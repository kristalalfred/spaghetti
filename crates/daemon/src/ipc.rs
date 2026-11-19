//! Claude: where clients reach the daemon, at `SOCKET_PATH`: a Unix socket, or
//! on Windows a named pipe with one server instance per client.

use std::io;

use spaghetti_common::ipc_protocol::SOCKET_PATH;

pub use imp::{Listener, Stream};

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::net::UnixListener;
    pub use tokio::net::UnixStream as Stream;

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind() -> io::Result<Self> {
            let _ = std::fs::remove_file(SOCKET_PATH);
            UnixListener::bind(SOCKET_PATH).map(Self)
        }

        pub async fn accept(&mut self) -> io::Result<(Stream, String)> {
            let (stream, peer) = self.0.accept().await?;
            Ok((stream, format!("{peer:?}")))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(SOCKET_PATH);
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    pub use tokio::net::windows::named_pipe::NamedPipeServer as Stream;
    use tokio::net::windows::named_pipe::ServerOptions;

    pub struct Listener {
        waiting: Stream,
    }

    impl Listener {
        pub fn bind() -> io::Result<Self> {
            let waiting = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(SOCKET_PATH)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::PermissionDenied => io::Error::new(
                        error.kind(),
                        format!("{SOCKET_PATH} is taken; is another spaghettid running? ({error})"),
                    ),
                    _ => error,
                })?;
            Ok(Self { waiting })
        }

        /// Claude: a client that connected before a failure to create the next
        /// instance is returned by the next call.
        pub async fn accept(&mut self) -> io::Result<(Stream, String)> {
            self.waiting.connect().await?;
            let next = ServerOptions::new()
                .reject_remote_clients(true)
                .create(SOCKET_PATH)?;
            Ok((
                std::mem::replace(&mut self.waiting, next),
                "pipe client".into(),
            ))
        }
    }
}
