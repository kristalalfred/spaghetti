use std::fmt::{self, Write as _};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use socket2::{Domain, Socket, Type};
use tracing::debug;

use crate::hap;

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
    let Some(message) = decode_message(buf)? else {
        return Ok(None);
    };
    let mut parts = message.first_line.splitn(3, ' ');
    let protocol = parts.next().unwrap_or_default();
    let status = parts.next().and_then(|s| s.parse().ok());
    let (true, Some(status)) = (protocol.contains('/'), status) else {
        return Err(format!("malformed status line {:?}", message.first_line));
    };
    let reason = parts.next().unwrap_or_default().to_owned();
    Ok(Some((
        Response {
            status,
            reason,
            headers: message.headers,
            body: message.body,
        },
        message.len,
    )))
}

/// Claude: a request a receiver sends, as over an AirPlay 2 event connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncomingRequest {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) protocol: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl IncomingRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) fn decode_request(buf: &[u8]) -> Result<Option<(IncomingRequest, usize)>, String> {
    let Some(message) = decode_message(buf)? else {
        return Ok(None);
    };
    let parts: Vec<&str> = message.first_line.split(' ').collect();
    let [method, uri, protocol] = parts[..] else {
        return Err(format!("malformed request line {:?}", message.first_line));
    };
    if !protocol.contains('/') {
        return Err(format!("malformed request line {:?}", message.first_line));
    }
    Ok(Some((
        IncomingRequest {
            method: method.to_owned(),
            uri: uri.to_owned(),
            protocol: protocol.to_owned(),
            headers: message.headers,
            body: message.body,
        },
        message.len,
    )))
}

struct Message {
    first_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    len: usize,
}

fn decode_message(buf: &[u8]) -> Result<Option<Message>, String> {
    let Some(head_len) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD {
            Err("message headers too long".into())
        } else {
            Ok(None)
        };
    };
    let head = String::from_utf8_lossy(&buf[..head_len]);
    let mut lines = head.split("\r\n");
    let first_line = lines.next().unwrap_or_default().to_owned();
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
        return Err(format!("message body of {body_len} bytes is too long"));
    }
    let body_start = head_len + 4;
    if buf.len() < body_start + body_len {
        return Ok(None);
    }
    Ok(Some(Message {
        first_line,
        headers,
        body: buf[body_start..body_start + body_len].to_vec(),
        len: body_start + body_len,
    }))
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
    channel: Option<hap::Channel>,
    /// Claude: a request went out only in part, so the receiver can make
    /// nothing more of the connection.
    broken: bool,
}

impl Connection {
    /// Claude: connects to the first of `addresses` that answers, giving each
    /// `timeout` but none past `deadline`.
    pub(crate) fn open(
        addresses: &[SocketAddr],
        timeout: Duration,
        deadline: Instant,
        common: Vec<(&'static str, String)>,
        abort: &dyn Fn() -> bool,
    ) -> Result<Connection, OpenError> {
        let mut failures = Vec::new();
        let mut transient = false;
        for address in addresses {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                failures.push(format!("{address} (not tried in time)"));
                continue;
            }
            match connect(*address, timeout.min(left), abort).and_then(|stream| {
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
                        channel: None,
                        broken: false,
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

    /// Claude: from now on, every request is sealed and every response opened
    /// with `keys`, as an AirPlay 2 receiver expects once paired.
    pub(crate) fn encrypt(&mut self, keys: &hap::Keys) {
        self.channel = Some(hap::Channel::new(keys));
    }

    /// Claude: sends a request and returns its CSeq, giving up at `deadline`
    /// or soon after `abort` turns true. A request given up on part sent
    /// breaks the connection.
    pub(crate) fn send(
        &mut self,
        request: &Request,
        deadline: Instant,
        abort: &dyn Fn() -> bool,
    ) -> Result<u32, String> {
        if self.broken {
            return Err(format!(
                "could not send {}: an earlier request went out only in part",
                request.method
            ));
        }
        self.cseq += 1;
        let mut bytes = encode_request(request, self.cseq, &self.common);
        debug!(peer = %self.peer, request = %described(&bytes), "RTSP request");
        if let Some(channel) = &mut self.channel {
            bytes = channel.seal(&bytes);
        }
        let mut sent = 0;
        while sent < bytes.len() {
            let failed = |reason: String| format!("could not send {} ({reason})", request.method);
            let left = deadline.saturating_duration_since(Instant::now());
            let given_up = if abort() {
                Some("stopped".to_string())
            } else if left.is_zero() {
                Some(failed("the receiver took no more in time".into()))
            } else {
                None
            };
            if let Some(reason) = given_up {
                self.broken = sent > 0;
                return Err(reason);
            }
            self.stream
                .set_write_timeout(Some(left.min(ABORT_CHECK)))
                .map_err(|e| failed(e.to_string()))?;
            match self.stream.write(&bytes[sent..]) {
                Ok(0) => {
                    self.broken = true;
                    return Err(failed("the connection closed".into()));
                }
                Ok(n) => sent += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    self.broken = true;
                    return Err(failed(e.to_string()));
                }
            }
        }
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
        let deadline = Instant::now() + timeout;
        let cseq = self.send(request, deadline, abort)?;
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
            Ok(n) => match &mut self.channel {
                Some(channel) => channel
                    .open(&chunk[..n], &mut self.received)
                    .map_err(|e| ReadError::Failed(format!("the RTSP connection failed ({e})"))),
                None => {
                    self.received.extend_from_slice(&chunk[..n]);
                    Ok(())
                }
            },
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
        debug!(peer = %self.peer, response = %described(&self.received[..len]), "RTSP response");
        self.received.drain(..len);
        Ok(Some(response))
    }
}

/// Claude: connects within `timeout` on the calling thread, checking `abort`
/// as it waits; an attempt given up closes its socket at once, so it never
/// connects later. Nothing is attempted once `abort` is true.
pub(crate) fn connect(
    address: SocketAddr,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> std::io::Result<TcpStream> {
    if abort() {
        return Err(ErrorKind::Interrupted.into());
    }
    let deadline = Instant::now() + timeout;
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_nonblocking(true)?;
    match socket.connect(&address.into()) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(ErrorKind::TimedOut, "connection timed out"));
            }
            if writable(socket.as_raw_fd(), left.min(ABORT_CHECK)) {
                if let Some(e) = socket.take_error()? {
                    return Err(e);
                }
                break;
            }
            if abort() {
                return Err(ErrorKind::Interrupted.into());
            }
        },
        Err(e) => return Err(e),
    }
    socket.set_nonblocking(false)?;
    Ok(socket.into())
}

fn writable(fd: RawFd, timeout: Duration) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(&mut pollfd, 1, ms) };
    ready > 0 && pollfd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0
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

pub(crate) const PLIST: &str = "application/x-apple-binary-plist";

/// Claude: a message as the debug log shows it: its head, and its body only as
/// far as it holds no secret. Text bodies print whole, binary plists print with
/// every data value as its length, since they carry keys such as `shk`, and
/// any other body, such as pairing's SRP values and proofs, prints as its
/// length.
pub(crate) fn described(message: &[u8]) -> String {
    let head_len = message
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(message.len(), |p| p + 4);
    let (head, body) = message.split_at(head_len);
    let mut text = String::from_utf8_lossy(head).into_owned();
    if body.is_empty() {
        return text;
    }
    let content_type = text
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("Content-Type"))
        .map(|(_, value)| value.trim().to_ascii_lowercase());
    match content_type.as_deref() {
        Some(t) if t.starts_with("text/") || t == "application/sdp" => {
            text.push_str(&String::from_utf8_lossy(body));
        }
        Some(PLIST) => match plist::Value::from_reader(io::Cursor::new(body)) {
            Ok(value) => redacted(&value, &mut text),
            Err(_) => {
                let _ = write!(text, "<{} bytes that are not a plist>", body.len());
            }
        },
        _ => {
            let _ = write!(text, "<{} bytes>", body.len());
        }
    }
    text
}

fn redacted(value: &plist::Value, out: &mut String) {
    use plist::Value;
    match value {
        Value::Dictionary(entries) => {
            out.push('{');
            for (i, (key, value)) in entries.iter().enumerate() {
                let _ = write!(out, "{}{key}: ", if i == 0 { "" } else { ", " });
                redacted(value, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, value) in items.iter().enumerate() {
                out.push_str(if i == 0 { "" } else { ", " });
                redacted(value, out);
            }
            out.push(']');
        }
        Value::Data(data) => {
            let _ = write!(out, "<{} bytes>", data.len());
        }
        Value::String(string) => {
            let _ = write!(out, "{string:?}");
        }
        Value::Integer(integer) => {
            let _ = write!(out, "{integer}");
        }
        Value::Real(real) => {
            let _ = write!(out, "{real}");
        }
        Value::Boolean(boolean) => {
            let _ = write!(out, "{boolean}");
        }
        _ => out.push_str("<other>"),
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

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
    fn requests_parse_once_complete() {
        let message = b"POST /command RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 2\r\n\r\nabPOST";
        assert_eq!(decode_request(&message[..40]), Ok(None));
        let (request, len) = decode_request(message).unwrap().unwrap();
        assert_eq!(len, message.len() - 4);
        assert_eq!(
            (
                request.method.as_str(),
                request.uri.as_str(),
                request.protocol.as_str()
            ),
            ("POST", "/command", "RTSP/1.0")
        );
        assert_eq!(
            (request.header("cseq"), request.body.as_slice()),
            (Some("3"), &b"ab"[..])
        );
        assert!(decode_request(b"POST /command\r\n\r\n").is_err());
        assert!(decode_request(b"RTSP/1.0 200 OK\r\n\r\n").is_err());
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
    fn an_encrypted_connection_seals_requests_and_opens_split_and_coalesced_responses() {
        use crate::hap::tests::{reversed, test_keys};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let keys = test_keys();
        let server_keys = reversed(&keys);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut channel = hap::Channel::new(&server_keys);
            let (mut wire, mut plaintext) = (Vec::new(), Vec::new());
            let mut requests = 0;
            while requests < 2 {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                wire.extend_from_slice(&chunk[..n]);
                channel.open(&chunk[..n], &mut plaintext).unwrap();
                requests = plaintext.windows(4).filter(|w| w == b"\r\n\r\n").count();
            }
            let first = channel.seal(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n");
            for byte in &first {
                stream.write_all(&[*byte]).unwrap();
                stream.flush().unwrap();
            }
            let body = vec![b'x'; 1500];
            let mut second = format!(
                "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            second.extend_from_slice(&body);
            let unrelated = b"RTSP/1.0 200 OK\r\nCSeq: 9\r\nContent-Length: 0\r\n\r\n";
            let coalesced = [channel.seal(unrelated), channel.seal(&second)].concat();
            stream.write_all(&coalesced).unwrap();
            (wire, String::from_utf8(plaintext).unwrap())
        });

        let mut connection = Connection::open(
            &[address],
            Duration::from_secs(1),
            Instant::now() + Duration::from_secs(1),
            vec![],
            &|| false,
        )
        .map_err(|e| e.to_string())
        .unwrap();
        connection.encrypt(&keys);
        let request = |method| Request {
            method,
            uri: "*",
            headers: vec![("X-Apple-HKP", "4".into())],
            body: Some(("application/octet-stream", &[7u8; 1100][..])),
        };
        let soon = Instant::now() + Duration::from_secs(1);
        connection
            .send(&request("OPTIONS"), soon, &|| false)
            .unwrap();
        let response = connection
            .exchange(&request("POST"), Duration::from_secs(2), &|| false)
            .unwrap();
        assert_eq!((response.cseq(), response.body.len()), (Some(2), 1500));
        let (wire, plaintext) = server.join().unwrap();
        assert!(plaintext.starts_with("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nX-Apple-HKP: 4\r\n"));
        assert!(plaintext.contains("POST * RTSP/1.0\r\nCSeq: 2\r\n"));
        assert!(!wire.windows(4).any(|w| w == b"RTSP" || w == b"CSeq"));
        assert_eq!(wire.len(), plaintext.len() + 18 * 4);
    }

    /// Claude: a request far larger than the kernel buffers on both ends, to
    /// a peer that never reads it.
    #[test]
    fn a_stalled_write_gives_up_when_stopped_or_late_and_breaks_the_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let open = || {
            Connection::open(
                &[address],
                Duration::from_secs(1),
                Instant::now() + Duration::from_secs(1),
                vec![],
                &|| false,
            )
            .map_err(|e| e.to_string())
            .unwrap()
        };
        let body = vec![0u8; 32 << 20];
        let large = Request {
            method: "POST",
            uri: "*",
            headers: vec![],
            body: Some(("application/octet-stream", &body)),
        };
        let small = Request {
            method: "OPTIONS",
            uri: "*",
            headers: vec![],
            body: None,
        };
        let later = || Instant::now() + Duration::from_secs(5);

        let mut stopped = open();
        let stop_at = Instant::now() + Duration::from_millis(200);
        let sent = stopped.send(&large, later(), &|| Instant::now() >= stop_at);
        let after = Instant::now().duration_since(stop_at);
        assert_eq!(sent, Err("stopped".into()));
        assert!(after < ABORT_CHECK + Duration::from_millis(50), "{after:?}");
        assert!(stopped.send(&small, later(), &|| false).is_err());

        let mut late = open();
        let deadline = Instant::now() + Duration::from_millis(200);
        let sent = late.send(&large, deadline, &|| false).unwrap_err();
        let after = Instant::now().duration_since(deadline);
        assert!(sent.contains("took no more in time"), "{sent}");
        assert!(after < ABORT_CHECK + Duration::from_millis(50), "{after:?}");
        let refused = late.exchange(&small, Duration::from_secs(1), &|| false);
        assert!(refused.unwrap_err().contains("only in part"));
    }

    #[test]
    fn the_log_shows_no_secret_bodies() {
        assert_eq!(
            described(b"POST /pair-setup RTSP/1.0\r\nContent-Type: application/octet-stream\r\n\r\n\x06\x01\x03\x03\x02\xab\xcd"),
            "POST /pair-setup RTSP/1.0\r\nContent-Type: application/octet-stream\r\n\r\n<7 bytes>"
        );
        assert_eq!(
            described(b"RTSP/1.0 200 OK\r\n\r\n\x01\xff"),
            "RTSP/1.0 200 OK\r\n\r\n<2 bytes>"
        );
        let shk = [0x5A; 32];
        let mut stream = plist::Dictionary::new();
        stream.insert("shk".into(), plist::Value::Data(shk.to_vec()));
        stream.insert("type".into(), 96.into());
        let mut setup = plist::Dictionary::new();
        setup.insert(
            "streams".into(),
            vec![plist::Value::Dictionary(stream)].into(),
        );
        setup.insert("timingProtocol".into(), "NTP".into());
        setup.insert("isMedia".into(), true.into());
        let mut body = Vec::new();
        plist::Value::Dictionary(setup)
            .to_writer_binary(&mut body)
            .unwrap();
        let message = [
            format!("SETUP rtsp://a/1 RTSP/1.0\r\nContent-Type: {PLIST}\r\n\r\n").as_bytes(),
            &body,
        ]
        .concat();
        let text = described(&message);
        assert!(
            text.ends_with(
                "{streams: [{shk: <32 bytes>, type: 96}], timingProtocol: \"NTP\", isMedia: true}"
            ),
            "{text}"
        );
        let hex: String = shk.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!text.to_ascii_lowercase().contains(&hex) && !text.contains('Z'));
        assert!(
            described(b"ANNOUNCE x RTSP/1.0\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n")
                .ends_with("v=0\r\n")
        );
    }
}
