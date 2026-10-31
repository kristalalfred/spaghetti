use std::fmt::{self, Write as _};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::debug;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const ABORT_CHECK: Duration = Duration::from_millis(50);

pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) uri: &'a str,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: Option<(&'static str, &'a [u8])>,
}

pub(crate) fn encode_request(
    request: &Request,
    cseq: u32,
    common: &[(&'static str, String)],
) -> Vec<u8> {
    let mut head = format!(
        "{} {} RTSP/1.0\r\nCSeq: {cseq}\r\n",
        request.method, request.uri
    );
    for (name, value) in common.iter().chain(&request.headers) {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if let Some((content_type, body)) = request.body {
        let _ = write!(
            head,
            "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    if let Some((_, body)) = request.body {
        bytes.extend_from_slice(body);
    }
    bytes
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) reason: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn cseq(&self) -> Option<u32> {
        self.header("CSeq")?.parse().ok()
    }
}

/// Claude: the first complete response in `buf` and how many bytes it took, or
/// `None` while it is incomplete.
pub(crate) fn decode_response(buf: &[u8]) -> Result<Option<(Response, usize)>, String> {
    let Some(head_len) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD {
            Err("response headers too long".into())
        } else {
            Ok(None)
        };
    };
    let head = String::from_utf8_lossy(&buf[..head_len]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let protocol = parts.next().unwrap_or_default();
    let status = parts.next().and_then(|s| s.parse().ok());
    let (true, Some(status)) = (protocol.contains('/'), status) else {
        return Err(format!("malformed status line {status_line:?}"));
    };
    let reason = parts.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let body_len = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
        .map(|(_, v)| {
            v.parse::<usize>()
                .map_err(|_| format!("malformed Content-Length {v:?}"))
        })
        .transpose()?
        .unwrap_or(0);
    if body_len > MAX_BODY {
        return Err(format!("response body of {body_len} bytes is too long"));
    }
    let body_start = head_len + 4;
    if buf.len() < body_start + body_len {
        return Ok(None);
    }
    let body = buf[body_start..body_start + body_len].to_vec();
    Ok(Some((
        Response {
            status,
            reason,
            headers,
            body,
        },
        body_start + body_len,
    )))
}

/// Claude: an RTSP connection to a receiver. Requests carry the common headers
/// and, once set, the session.
pub(crate) struct Connection {
    stream: TcpStream,
    local: SocketAddr,
    peer: SocketAddr,
    received: Vec<u8>,
    cseq: u32,
    common: Vec<(&'static str, String)>,
}

impl Connection {
    pub(crate) fn open(
        addresses: &[SocketAddr],
        timeout: Duration,
        common: Vec<(&'static str, String)>,
        abort: &dyn Fn() -> bool,
    ) -> Result<Connection, OpenError> {
        let mut failures = Vec::new();
        let mut transient = false;
        for address in addresses {
            match connect(*address, timeout, abort).and_then(|stream| {
                stream.set_nodelay(true)?;
                Ok((stream.local_addr()?, stream))
            }) {
                Ok((local, stream)) => {
                    debug!(%address, %local, "RTSP connected");
                    return Ok(Connection {
                        stream,
                        local,
                        peer: *address,
                        received: Vec::new(),
                        cseq: 0,
                        common,
                    });
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => return Err(OpenError::Stopped),
                Err(e) => {
                    transient |= is_transient(&e);
                    failures.push(format!("{address} ({e})"));
                }
            }
        }
        if failures.is_empty() {
            return Err(OpenError::Failed("it has no address".into()));
        }
        let reason = format!("could not connect to {}", failures.join(", "));
        Err(if transient {
            OpenError::Transient(reason)
        } else {
            OpenError::Failed(reason)
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub(crate) fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    pub(crate) fn common_headers(&self) -> &[(&'static str, String)] {
        &self.common
    }

    pub(crate) fn add_header(&mut self, name: &'static str, value: String) {
        self.common.push((name, value));
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Claude: sends a request and returns its CSeq.
    pub(crate) fn send(&mut self, request: &Request) -> Result<u32, String> {
        self.cseq += 1;
        let bytes = encode_request(request, self.cseq, &self.common);
        debug!(peer = %self.peer, request = %printable(&bytes), "RTSP request");
        self.stream
            .write_all(&bytes)
            .map_err(|e| format!("could not send {} ({e})", request.method))?;
        Ok(self.cseq)
    }

    /// Claude: sends a request and waits for its answer, giving up after
    /// `timeout` or soon after `abort` turns true.
    pub(crate) fn exchange(
        &mut self,
        request: &Request,
        timeout: Duration,
        abort: &dyn Fn() -> bool,
    ) -> Result<Response, String> {
        if abort() {
            return Err("stopped".into());
        }
        self.stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let cseq = self.send(request)?;
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(response) = self.next_response()? {
                if response.cseq() == Some(cseq) {
                    return Ok(response);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "no answer to {} within {timeout:?}",
                    request.method
                ));
            }
            if abort() {
                return Err("stopped".into());
            }
            self.stream
                .set_read_timeout(Some(remaining.min(ABORT_CHECK)))
                .map_err(|e| e.to_string())?;
            match self.read_ready() {
                Err(ReadError::TimedOut) => {}
                Err(ReadError::Failed(e)) => return Err(e),
                Ok(()) => {}
            }
        }
    }

    /// Claude: reads what has arrived; call it once the socket is readable, or
    /// with a read timeout set.
    pub(crate) fn read_ready(&mut self) -> Result<(), ReadError> {
        let mut chunk = [0; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(ReadError::Failed(
                "the receiver closed the RTSP connection".into(),
            )),
            Ok(n) => {
                self.received.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Err(ReadError::TimedOut)
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => Ok(()),
            Err(e) => Err(ReadError::Failed(format!(
                "the RTSP connection failed ({e})"
            ))),
        }
    }

    pub(crate) fn next_response(&mut self) -> Result<Option<Response>, String> {
        let Some((response, len)) = decode_response(&self.received)? else {
            return Ok(None);
        };
        debug!(peer = %self.peer, response = %printable(&self.received[..len]), "RTSP response");
        self.received.drain(..len);
        Ok(Some(response))
    }
}

/// Claude: connects on a thread of its own so that `abort` can cut the wait
/// short; an abandoned attempt ends by itself within `timeout`.
fn connect(
    address: SocketAddr,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> std::io::Result<TcpStream> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("airplay-connect".into())
        .spawn(move || {
            let _ = sender.send(TcpStream::connect_timeout(&address, timeout));
        })?;
    loop {
        match receiver.recv_timeout(ABORT_CHECK) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) if !abort() => {}
            Err(RecvTimeoutError::Timeout) => return Err(ErrorKind::Interrupted.into()),
            Err(RecvTimeoutError::Disconnected) => return Err(ErrorKind::Other.into()),
        }
    }
}

pub(crate) enum OpenError {
    Stopped,
    /// Claude: an address failed as a receiver's does while it wakes from
    /// standby or starts its RTSP server.
    Transient(String),
    Failed(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Stopped => f.write_str("stopped"),
            OpenError::Transient(reason) | OpenError::Failed(reason) => f.write_str(reason),
        }
    }
}

fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionRefused
            | ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::NetworkDown
            | ErrorKind::TimedOut
    ) || error.raw_os_error() == Some(libc::EHOSTDOWN)
}

pub(crate) enum ReadError {
    TimedOut,
    Failed(String),
}

fn printable(message: &[u8]) -> String {
    let head_len = message
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(message.len(), |p| p + 4);
    let (head, body) = message.split_at(head_len);
    let mut text = String::from_utf8_lossy(head).into_owned();
    match std::str::from_utf8(body) {
        Ok(body) => text.push_str(body),
        Err(_) => body.iter().for_each(|b| {
            let _ = write!(text, "{b:02x}");
        }),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_put_cseq_first_then_common_headers_then_the_body() {
        let request = Request {
            method: "ANNOUNCE",
            uri: "rtsp://192.0.2.2/1234",
            headers: vec![("X-Extra", "1".into())],
            body: Some(("application/sdp", b"v=0\r\n")),
        };
        let common = [
            ("User-Agent", "spaghetti/0.1".to_owned()),
            ("Session", "7".to_owned()),
        ];
        assert_eq!(
            String::from_utf8(encode_request(&request, 3, &common)).unwrap(),
            "ANNOUNCE rtsp://192.0.2.2/1234 RTSP/1.0\r\nCSeq: 3\r\nUser-Agent: spaghetti/0.1\r\nSession: 7\r\n\
             X-Extra: 1\r\nContent-Type: application/sdp\r\nContent-Length: 5\r\n\r\nv=0\r\n"
        );
        let options = Request {
            method: "OPTIONS",
            uri: "*",
            headers: vec![],
            body: None,
        };
        assert_eq!(
            encode_request(&options, 1, &[]),
            b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n"
        );
    }

    #[test]
    fn responses_parse_once_complete_with_case_insensitive_headers() {
        let message = b"RTSP/1.0 200 OK\r\nCSeq: 4\r\naudio-latency: 11025\r\nContent-Length: 3\r\n\r\nabcRTSP/1.0 4";
        for len in [10, 68, 69, 71] {
            assert_eq!(decode_response(&message[..len]), Ok(None));
        }
        let (response, len) = decode_response(message).unwrap().unwrap();
        assert_eq!(len, message.len() - 10);
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.cseq(), Some(4));
        assert_eq!(response.header("Audio-Latency"), Some("11025"));
        assert_eq!(response.body, b"abc");

        let (response, _) = decode_response(b"RTSP/1.0 453 Not Enough Bandwidth\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            (response.status, response.reason.as_str()),
            (453, "Not Enough Bandwidth")
        );
        assert!(decode_response(b"garbage\r\n\r\n").is_err());
        assert!(decode_response(b"RTSP/1.0 200 OK\r\nContent-Length: x\r\n\r\n").is_err());
    }

    #[test]
    fn connections_failing_as_a_waking_receiver_does_are_transient() {
        let os = io::Error::from_raw_os_error;
        for error in [
            os(libc::ECONNREFUSED),
            os(libc::EHOSTUNREACH),
            os(libc::EHOSTDOWN),
            os(libc::ENETUNREACH),
            os(libc::ENETDOWN),
            os(libc::ETIMEDOUT),
            io::Error::new(ErrorKind::TimedOut, "connection timed out"),
        ] {
            assert!(is_transient(&error), "{error}");
        }
        for error in [os(libc::EADDRNOTAVAIL), os(libc::EACCES), os(libc::EINVAL)] {
            assert!(!is_transient(&error), "{error}");
        }
    }

    #[test]
    fn binary_bodies_print_as_hex() {
        assert_eq!(
            printable(b"POST /a RTSP/1.0\r\n\r\n\x01\xff"),
            "POST /a RTSP/1.0\r\n\r\n01ff"
        );
    }
}
