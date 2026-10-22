use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use coreaudio_sys::AudioGetCurrentHostTime;
use futures::{SinkExt, StreamExt};
use spaghetti_common::ipc_protocol::{FlowStatus, SrtMode, SrtSocket};
use srt_tokio::options::{KeySize, Passphrase};

use crate::flow::{FlowMeter, Link};

const RETRY_AFTER: Duration = Duration::from_millis(500);

/// Claude: an SRT socket spec checked against what srt-tokio accepts, so
/// connecting cannot fail on the options themselves.
#[derive(Clone)]
pub struct SrtEndpoint {
    mode: SrtMode,
    latency: Duration,
    encryption: Option<(KeySize, Passphrase)>,
}

impl SrtEndpoint {
    pub fn parse(spec: &SrtSocket) -> Result<Self, String> {
        let encryption = match &spec.passphrase {
            None => None,
            Some(passphrase) => {
                let key_size = u16::from(spec.key_size.unwrap_or(32))
                    .try_into()
                    .map_err(|_| "SRT key size must be 16, 24 or 32 bytes".to_string())?;
                let passphrase = passphrase
                    .clone()
                    .try_into()
                    .map_err(|_| "SRT passphrase must have 10 to 79 characters".to_string())?;
                Some((key_size, passphrase))
            }
        };
        if let SrtMode::Call { host, .. } = &spec.mode
            && host.trim().is_empty()
        {
            return Err("an SRT caller needs a host".into());
        }
        Ok(Self {
            mode: spec.mode.clone(),
            latency: Duration::from_millis(u64::from(spec.latency_ms)),
            encryption,
        })
    }

    pub fn label(&self) -> String {
        match &self.mode {
            SrtMode::Listen { port } => format!("srt://:{port}"),
            SrtMode::Call { host, port } => format!("srt://{host}:{port}"),
        }
    }

    async fn connect(&self) -> io::Result<srt_tokio::SrtSocket> {
        let mut builder = srt_tokio::SrtSocket::builder().latency(self.latency);
        if let Some((key_size, passphrase)) = &self.encryption {
            let (key_size, passphrase) = (*key_size, passphrase.clone());
            builder = builder.set(move |options| {
                options.encryption.key_size = key_size;
                options.encryption.passphrase = Some(passphrase);
            });
        }
        match &self.mode {
            SrtMode::Listen { port } => {
                builder
                    .listen_on(SocketAddr::from(([0, 0, 0, 0], *port)))
                    .await
            }
            SrtMode::Call { host, port } => {
                let remote = tokio::net::lookup_host((host.as_str(), *port))
                    .await?
                    .next()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SRT host has no address"))?;
                builder.call(remote, None).await
            }
        }
    }
}

enum Connection {
    Connecting,
    Connected {
        peer: SocketAddr,
        packets_lost: u64,
        packets_retransmitted: u64,
    },
}

struct Shared {
    meter: FlowMeter,
    connection: Mutex<Connection>,
}

impl Shared {
    fn connected(&self, peer: SocketAddr) {
        self.meter.restart();
        *self.connection.lock().unwrap() = Connection::Connected {
            peer,
            packets_lost: 0,
            packets_retransmitted: 0,
        };
        tracing::info!(%peer, "SRT connected");
    }

    fn disconnected(&self) {
        *self.connection.lock().unwrap() = Connection::Connecting;
    }

    fn counters(&self, lost: u64, retransmitted: u64) {
        if let Connection::Connected {
            packets_lost,
            packets_retransmitted,
            ..
        } = &mut *self.connection.lock().unwrap()
        {
            *packets_lost = lost;
            *packets_retransmitted = retransmitted;
        }
    }
}

/// Claude: one SRT connection that is re-established whenever it drops. A
/// listener waits for the next caller; a caller dials again.
pub struct SrtLink {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl SrtLink {
    /// Claude: sends each message as one SRT packet, released at the receiver one
    /// latency after its origin instant. Messages that arrive while no peer is
    /// connected are dropped, so a sender never builds a backlog.
    pub fn send(
        endpoint: SrtEndpoint,
        mut messages: tokio::sync::mpsc::Receiver<(Instant, Bytes)>,
    ) -> Self {
        let shared = Arc::new(Shared {
            meter: FlowMeter::new(),
            connection: Mutex::new(Connection::Connecting),
        });
        let state = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            loop {
                let Some(mut socket) = connect_draining(&endpoint, &mut messages).await else {
                    return;
                };
                state.connected(socket.settings().remote);
                let mut statistics = socket.statistics().clone();
                loop {
                    tokio::select! {
                        message = messages.recv() => {
                            let Some((origin, message)) = message else { return };
                            let len = message.len() as u64;
                            let origin = origin.max(socket.settings().socket_start_time);
                            if socket.send((origin, message)).await.is_err() {
                                break;
                            }
                            state.meter.record(len, unsafe { AudioGetCurrentHostTime() });
                        }
                        sample = statistics.next() => match sample {
                            Some(sample) => state.counters(sample.tx_loss_data, sample.tx_retransmit_data),
                            None => break,
                        },
                        incoming = socket.next() => if !matches!(incoming, Some(Ok(_))) { break },
                    }
                }
                state.disconnected();
                tracing::info!(endpoint = endpoint.label(), "SRT sender disconnected");
            }
        });
        Self { shared, task }
    }

    /// Claude: hands every received packet's payload to `payloads`, dropping
    /// it when the consumer is behind.
    pub fn receive(endpoint: SrtEndpoint, payloads: std::sync::mpsc::SyncSender<Bytes>) -> Self {
        let shared = Arc::new(Shared {
            meter: FlowMeter::new(),
            connection: Mutex::new(Connection::Connecting),
        });
        let state = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            loop {
                let mut socket = match endpoint.connect().await {
                    Ok(socket) => socket,
                    Err(e) => {
                        tracing::debug!(endpoint = endpoint.label(), error = %e, "SRT receiver connect failed");
                        tokio::time::sleep(RETRY_AFTER).await;
                        continue;
                    }
                };
                state.connected(socket.settings().remote);
                let mut statistics = socket.statistics().clone();
                loop {
                    tokio::select! {
                        incoming = socket.next() => match incoming {
                            Some(Ok((_, payload))) => {
                                state.meter.record(payload.len() as u64, unsafe { AudioGetCurrentHostTime() });
                                if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = payloads.try_send(payload) {
                                    return;
                                }
                            }
                            _ => break,
                        },
                        sample = statistics.next() => match sample {
                            Some(sample) => state.counters(sample.rx_loss_data, sample.rx_retransmit_data),
                            None => break,
                        },
                    }
                }
                state.disconnected();
                tracing::info!(endpoint = endpoint.label(), "SRT receiver disconnected");
            }
        });
        Self { shared, task }
    }

    pub fn status(&self) -> FlowStatus {
        let link = match &*self.shared.connection.lock().unwrap() {
            Connection::Connecting => Link::Connecting,
            Connection::Connected {
                peer,
                packets_lost,
                packets_retransmitted,
            } => Link::Connected {
                peer: Some(peer.to_string()),
                packets_lost: *packets_lost,
                packets_retransmitted: *packets_retransmitted,
            },
        };
        self.shared.meter.status(link)
    }
}

impl Drop for SrtLink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Claude: connects, retrying on failure, while discarding messages so the
/// producer never blocks. Returns `None` once the producer is gone.
async fn connect_draining(
    endpoint: &SrtEndpoint,
    messages: &mut tokio::sync::mpsc::Receiver<(Instant, Bytes)>,
) -> Option<srt_tokio::SrtSocket> {
    loop {
        let attempt = endpoint.connect();
        tokio::pin!(attempt);
        let result = loop {
            tokio::select! {
                result = &mut attempt => break result,
                message = messages.recv() => { message?; }
            }
        };
        match result {
            Ok(socket) => return Some(socket),
            Err(e) => {
                tracing::debug!(endpoint = endpoint.label(), error = %e, "SRT sender connect failed");
                let retry = tokio::time::sleep(RETRY_AFTER);
                tokio::pin!(retry);
                loop {
                    tokio::select! {
                        () = &mut retry => break,
                        message = messages.recv() => { message?; }
                    }
                }
            }
        }
    }
}
