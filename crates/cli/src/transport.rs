//! Claude: the CLI's end of the daemon's IPC endpoint: a Unix socket, or on
//! Windows a named pipe, as a blocking stream.

use std::io;
use std::path::Path;

pub use imp::{Stream, connect};

#[cfg(test)]
pub use imp::fake;

#[cfg(unix)]
mod imp {
    use super::*;
    pub use std::os::unix::net::UnixStream as Stream;

    pub fn connect(path: &Path) -> io::Result<Stream> {
        Stream::connect(path)
    }

    #[cfg(test)]
    pub mod fake {
        use std::io::Write;
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;
        use std::thread::JoinHandle;
        use std::time::Duration;

        use spaghetti_common::ipc_protocol::{Request, read_message};

        /// Claude: a socket path of this process's own, removed when dropped.
        pub struct Endpoint(PathBuf);

        impl Endpoint {
            pub fn new(name: &str) -> Self {
                let path = std::env::temp_dir()
                    .join(format!("spaghetti-test-{}-{name}.sock", std::process::id()));
                let _ = std::fs::remove_file(&path);
                Self(path)
            }

            pub fn path(&self) -> &std::path::Path {
                &self.0
            }

            pub fn listen(&self) -> Listener {
                Listener(UnixListener::bind(&self.0).unwrap())
            }
        }

        impl Drop for Endpoint {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        pub struct Listener(UnixListener);

        impl Listener {
            /// Claude: takes one client and one request from it, then answers
            /// with `answer`'s bytes, or with none for `silence`.
            pub fn answer_once(self, answer: Option<Vec<u8>>, silence: Duration) -> JoinHandle<()> {
                std::thread::spawn(move || {
                    let (mut stream, _) = self.0.accept().unwrap();
                    let _: Request = read_message(&mut stream).unwrap();
                    match answer {
                        Some(bytes) => stream.write_all(&bytes).unwrap(),
                        None => std::thread::sleep(silence),
                    }
                })
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::{Duration, Instant};

    pub use std::fs::File as Stream;

    const ERROR_PIPE_BUSY: i32 = 231;
    const BUSY_WAIT: Duration = Duration::from_secs(1);

    /// Claude: every instance is busy only between the daemon accepting a
    /// client and creating the next one, so a busy pipe is retried briefly.
    pub fn connect(path: &Path) -> io::Result<Stream> {
        let started = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
            {
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && started.elapsed() < BUSY_WAIT =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => return result,
            }
        }
    }

    #[cfg(test)]
    pub mod fake {
        use std::path::{Path, PathBuf};
        use std::thread::JoinHandle;
        use std::time::Duration;

        use spaghetti_common::ipc_protocol::Request;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
        use tokio::runtime::Runtime;

        /// Claude: a pipe name of this process's own.
        pub struct Endpoint(PathBuf);

        impl Endpoint {
            pub fn new(name: &str) -> Self {
                Self(PathBuf::from(format!(
                    r"\\.\pipe\spaghetti-test-{}-{name}",
                    std::process::id()
                )))
            }

            pub fn path(&self) -> &Path {
                &self.0
            }

            pub fn listen(&self) -> Listener {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let server = {
                    let _entered = runtime.enter();
                    ServerOptions::new()
                        .first_pipe_instance(true)
                        .create(&self.0)
                        .unwrap()
                };
                Listener { runtime, server }
            }
        }

        pub struct Listener {
            runtime: Runtime,
            server: NamedPipeServer,
        }

        impl Listener {
            /// Claude: takes one client and one request from it, then answers
            /// with `answer`'s bytes, or with none for `silence`.
            pub fn answer_once(self, answer: Option<Vec<u8>>, silence: Duration) -> JoinHandle<()> {
                let Self {
                    runtime,
                    mut server,
                } = self;
                std::thread::spawn(move || {
                    runtime.block_on(async move {
                        server.connect().await.unwrap();
                        let mut length = [0u8; 4];
                        server.read_exact(&mut length).await.unwrap();
                        let mut request = vec![0u8; u32::from_be_bytes(length) as usize];
                        server.read_exact(&mut request).await.unwrap();
                        let _: Request = serde_json::from_slice(&request).unwrap();
                        match answer {
                            Some(bytes) => {
                                server.write_all(&bytes).await.unwrap();
                                server.flush().await.unwrap();
                            }
                            None => tokio::time::sleep(silence).await,
                        }
                    })
                })
            }
        }
    }
}
