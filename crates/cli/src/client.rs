use spaghetti_common::ipc_protocol::{read_message, write_message, Request, Response, SOCKET_PATH};
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;

pub const UNEXPECTED: &str = "unexpected response from daemon";

pub fn connect() -> Result<UnixStream, String> {
    UnixStream::connect(SOCKET_PATH).map_err(|e| match e.kind() {
        ErrorKind::ConnectionRefused | ErrorKind::NotFound => {
            "Daemon not running. Start it with: spaghetti daemon start".to_string()
        }
        _ => format!("Failed to connect to daemon: {e}"),
    })
}

pub fn request(stream: &mut UnixStream, request: &Request) -> Result<Response, String> {
    write_message(stream, request).map_err(|e| format!("send to daemon: {e}"))?;
    match read_message(stream).map_err(|e| format!("read from daemon: {e}"))? {
        Response::Error { message } => Err(message),
        response => Ok(response),
    }
}
