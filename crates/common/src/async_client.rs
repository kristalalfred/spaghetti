use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::ipc_protocol::{Request, Response, SOCKET_PATH};

/// Async IPC client for communicating with the spaghetti daemon.
///
/// Uses the same length-prefixed JSON framing as the sync client (4-byte BE u32
/// length followed by JSON body).
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    pub async fn connect() -> std::io::Result<Self> {
        let stream = UnixStream::connect(SOCKET_PATH).await?;
        Ok(Self { stream })
    }

    pub async fn send(&mut self, req: Request) -> std::io::Result<Response> {
        let json = serde_json::to_vec(&req)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let len = (json.len() as u32).to_be_bytes();
        self.stream.write_all(&len).await?;
        self.stream.write_all(&json).await?;
        self.stream.flush().await?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await?;
        serde_json::from_slice(&buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
