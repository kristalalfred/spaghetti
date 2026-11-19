use spaghetti_common::ipc_protocol::{Request, Response, SOCKET_PATH, read_message, write_message};
use std::io::ErrorKind;
use std::path::Path;

pub use crate::transport::Stream;

pub const UNEXPECTED: &str = "unexpected response from daemon";

pub fn connect() -> Result<Stream, String> {
    crate::transport::connect(Path::new(SOCKET_PATH)).map_err(|e| match e.kind() {
        ErrorKind::ConnectionRefused | ErrorKind::NotFound => {
            format!(
                "Daemon not running. Start it with: {}",
                crate::daemon_mgmt::START
            )
        }
        _ => format!("Failed to connect to daemon: {e}"),
    })
}

pub fn request(stream: &mut Stream, request: &Request) -> Result<Response, String> {
    write_message(stream, request).map_err(|e| format!("send to daemon: {e}"))?;
    match read_message(stream).map_err(|e| format!("read from daemon: {e}"))? {
        Response::Error { message } => Err(message),
        response => Ok(response),
    }
}
