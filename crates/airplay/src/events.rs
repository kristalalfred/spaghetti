use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::hap;
use crate::rtsp::{self, IncomingRequest};

const READ_SLICE: Duration = Duration::from_millis(50);
const RETRY: Duration = Duration::from_millis(100);

/// Claude: the connection an AirPlay 2 receiver sends its own requests over,
/// opened by the sender to the receiver's `eventPort`. A thread of its own
/// answers every request with 200 OK, as pyatv's event channel does, so that
/// no receiver waits on an answer while the stream renders or handshakes.
pub(crate) struct EventChannel {
    stop: Arc<AtomicBool>,
    socket: TcpStream,
    thread: Option<JoinHandle<()>>,
}

impl EventChannel {
    /// Claude: connects within `timeout`, trying again while the port refuses,
    /// since some receivers open it only after answering the SETUP that names
    /// it (pyatv found this with airplay2-receiver).
    pub(crate) fn connect(
        address: SocketAddr,
        keys: &hap::Keys,
        timeout: Duration,
        abort: &dyn Fn() -> bool,
    ) -> Result<EventChannel, String> {
        let deadline = Instant::now() + timeout;
        let socket = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rtsp::connect(address, remaining.max(RETRY), abort) {
                Ok(socket) => break socket,
                Err(e) if e.kind() == ErrorKind::Interrupted => return Err("stopped".into()),
                Err(e)
                    if e.kind() == ErrorKind::ConnectionRefused
                        && Instant::now() + RETRY < deadline =>
                {
                    thread::sleep(RETRY);
                }
                Err(e) => {
                    return Err(format!(
                        "could not connect to its event port {address} ({e})"
                    ));
                }
            }
        };
        socket
            .set_read_timeout(Some(READ_SLICE))
            .and_then(|()| socket.set_nodelay(true))
            .map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("airplay-events".into())
            .spawn({
                let socket = socket.try_clone().map_err(|e| e.to_string())?;
                let (channel, stop) = (hap::Channel::new(keys), stop.clone());
                move || serve(socket, channel, &stop)
            })
            .map_err(|e| format!("could not start the event thread ({e})"))?;
        debug!(%address, "AirPlay event connection opened");
        Ok(EventChannel {
            stop,
            socket,
            thread: Some(thread),
        })
    }
}

impl Drop for EventChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut socket: TcpStream, mut channel: hap::Channel, stop: &AtomicBool) {
    let mut plaintext = Vec::new();
    let mut chunk = [0u8; 4096];
    while !stop.load(Ordering::Acquire) {
        let received = match socket.read(&mut chunk) {
            Ok(0) => {
                if !stop.load(Ordering::Acquire) {
                    info!("the AirPlay receiver closed its event connection");
                }
                return;
            }
            Ok(n) => &chunk[..n],
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                if !stop.load(Ordering::Acquire) {
                    debug!(error = %e, "AirPlay event connection failed");
                }
                return;
            }
        };
        if let Err(reason) = channel.open(received, &mut plaintext) {
            warn!(%reason, "AirPlay event connection failed");
            return;
        }
        loop {
            let (request, len) = match rtsp::decode_request(&plaintext) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => break,
                Err(reason) => {
                    warn!(%reason, "AirPlay event connection sent a malformed request");
                    return;
                }
            };
            debug!(request = %rtsp::described(&plaintext[..len]), "AirPlay event");
            plaintext.drain(..len);
            if let Err(e) = socket.write_all(&channel.seal(&answer(&request))) {
                debug!(error = %e, "could not answer an AirPlay event");
                return;
            }
        }
    }
}

/// Claude: 200 OK in the request's protocol, echoing its CSeq and Server and
/// saying the sender adds no audio latency, as pyatv answers.
fn answer(request: &IncomingRequest) -> Vec<u8> {
    let mut answer = format!("{} 200 OK\r\n", request.protocol);
    if let Some(cseq) = request.header("CSeq") {
        answer.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    if let Some(server) = request.header("Server") {
        answer.push_str(&format!("Server: {server}\r\n"));
    }
    answer.push_str("Audio-Latency: 0\r\nContent-Length: 0\r\n\r\n");
    answer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_answered_with_200_in_their_protocol() {
        let (request, _) = rtsp::decode_request(
            b"POST /command RTSP/1.0\r\nCSeq: 7\r\nServer: AirTunes/366.0\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            String::from_utf8(answer(&request)).unwrap(),
            "RTSP/1.0 200 OK\r\nCSeq: 7\r\nServer: AirTunes/366.0\r\nAudio-Latency: 0\r\nContent-Length: 0\r\n\r\n"
        );
        let (request, _) = rtsp::decode_request(b"GET /x HTTP/1.1\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(answer(&request)).unwrap(),
            "HTTP/1.1 200 OK\r\nAudio-Latency: 0\r\nContent-Length: 0\r\n\r\n"
        );
    }
}
