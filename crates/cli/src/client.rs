use spaghetti_common::ipc_protocol::{read_message, write_message, Request, Response, SOCKET_PATH};
use std::os::unix::net::UnixStream;

pub fn connect() -> std::io::Result<UnixStream> {
    UnixStream::connect(SOCKET_PATH)
}

pub fn send_request(stream: &mut UnixStream, request: &Request) -> std::io::Result<Response> {
    write_message(stream, request)?;
    read_message(stream)
}
