use std::sync::{Arc, Mutex};
use std::time::Duration;

use spaghetti_common::ipc_protocol::*;
use spaghetti_engine::{ActivityReader, DeviceManager, FpsTracker, Router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Freshness window for the driver's last WriteMix timestamp — older than this
/// and the activity snapshot is considered stale (driver idle or gone).
const ACTIVITY_FRESHNESS_NS: u64 = 200_000_000;

const HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(10);
const CLIENT_DRAIN_LIMIT: Duration = Duration::from_secs(1);
const CLIENT_ABORT_LIMIT: Duration = Duration::from_secs(1);
const ROUTER_SHUTDOWN_LIMIT: Duration = Duration::from_secs(10);

/// Shared daemon state accessible by all client handlers.
pub struct DaemonState {
    pub router: Router,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
        }
    }
}

/// Per-server (not per-connection) activity tracking. Kept outside `DaemonState`
/// so the FPS window stays consistent across concurrent clients.
///
/// `reader` is behind a `Mutex` so the lazy-attach path (first successful open
/// after daemon starts before the driver has loaded) can update it safely.
pub struct ActivityState {
    pub reader: Mutex<Option<ActivityReader>>,
    pub fps: FpsTracker,
}

/// Claude: `devices` stays outside the `state` mutex because device
/// create/destroy waits on coreaudiod and must not block other requests.
struct Shared {
    state: Mutex<DaemonState>,
    devices: DeviceManager,
    activity: ActivityState,
    cancel: CancellationToken,
}

pub struct Server {
    listener: UnixListener,
    shared: Arc<Shared>,
    clients: JoinSet<()>,
    pub cancel: CancellationToken,
}

impl Server {
    pub async fn bind(
        activity_reader: Option<ActivityReader>,
        device_manager: DeviceManager,
    ) -> std::io::Result<Self> {
        // Remove stale socket file if it exists from a previous run
        let _ = std::fs::remove_file(SOCKET_PATH);
        let listener = UnixListener::bind(SOCKET_PATH)?;
        let cancel = CancellationToken::new();
        Ok(Self {
            listener,
            clients: JoinSet::new(),
            shared: Arc::new(Shared {
                state: Mutex::new(DaemonState::new()),
                devices: device_manager,
                activity: ActivityState {
                    reader: Mutex::new(activity_reader),
                    fps: FpsTracker::new(),
                },
                cancel: cancel.clone(),
            }),
            cancel,
        })
    }

    pub async fn run(&mut self) {
        let mut health_log = tokio::time::interval(HEALTH_LOG_INTERVAL);
        loop {
            tokio::select! {
                _ = health_log.tick() => lock_state(&self.shared.state).router.log_health(),
                accept = self.listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            self.clients.spawn(handle_client(stream, Arc::clone(&self.shared)));
                        }
                        Err(e) => tracing::error!("accept error: {e}"),
                    }
                }
                Some(result) = self.clients.join_next() => {
                    if let Err(error) = result {
                        tracing::error!(%error, "client handler failed");
                    }
                }
                () = self.shared.cancel.cancelled() => {
                    tracing::info!("server shutting down");
                    return;
                }
            }
        }
    }

    pub async fn shutdown(mut self) -> Result<(), String> {
        self.cancel.cancel();
        let clients = stop_clients(&mut self.clients, CLIENT_DRAIN_LIMIT, CLIENT_ABORT_LIMIT).await;
        let router = drop_with_timeout(self, ROUTER_SHUTDOWN_LIMIT).await;
        clients.and(router)
    }
}

async fn stop_clients(
    clients: &mut JoinSet<()>,
    drain_limit: Duration,
    abort_limit: Duration,
) -> Result<(), String> {
    if drain_clients(clients, drain_limit).await {
        return Ok(());
    }
    clients.abort_all();
    if drain_clients(clients, abort_limit).await {
        Ok(())
    } else {
        Err("client handlers did not stop before the abort deadline".into())
    }
}

async fn drain_clients(clients: &mut JoinSet<()>, limit: Duration) -> bool {
    tokio::time::timeout(limit, async {
        while let Some(result) = clients.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::error!(%error, "client handler failed during shutdown");
            }
        }
    })
    .await
    .is_ok()
}

async fn drop_with_timeout(value: impl Send + 'static, limit: Duration) -> Result<(), String> {
    let mut dropping = tokio::task::spawn_blocking(move || drop(value));
    match tokio::time::timeout(limit, &mut dropping).await {
        Ok(result) => result.map_err(|error| format!("router teardown failed: {error}")),
        Err(_) => {
            dropping.abort();
            Err(format!(
                "router teardown exceeded {:.0} seconds",
                limit.as_secs_f64()
            ))
        }
    }
}

async fn handle_client(mut stream: UnixStream, shared: Arc<Shared>) {
    let peer = stream.peer_addr().map(|a| format!("{a:?}")).unwrap_or_else(|_| "unknown".into());
    loop {
        let frame = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            frame = read_frame_async(&mut stream) => frame,
        };
        let response = match frame {
            Ok(buf) => match serde_json::from_slice::<Request>(&buf) {
                Ok(req) => dispatch(req, &shared).await,
                Err(e) => {
                    tracing::warn!(peer = %peer, "unrecognised request: {e}");
                    Response::Error {
                        message: format!("unknown or malformed request: {e}"),
                    }
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    tracing::warn!(peer = %peer, "read error: {e}");
                }
                return;
            }
        };

        if let Err(e) = write_message_async(&mut stream, &response).await {
            tracing::error!(peer = %peer, "write error: {e}");
            return;
        }
    }
}

async fn dispatch(request: Request, shared: &Shared) -> Response {
    let state = &shared.state;
    match request {
        Request::Status => {
            let st = lock_state(state);
            Response::Status(DaemonStatus {
                running: true,
                ipc_version: IPC_VERSION,
                source_count: st.router.source_count(),
                route_count: st.router.route_count(),
            })
        }
        Request::ListApps => {
            let apps = spaghetti_engine::list_audio_apps();
            Response::Apps(apps)
        }
        Request::CreateSource { source, tag } => {
            let mut st = lock_state(state);
            match st.router.create_source(source, tag) {
                Ok(source_id) => Response::SourceCreated { source_id },
                Err(message) => Response::Error { message },
            }
        }
        Request::DestroySource { source_id } => {
            let mut st = lock_state(state);
            match st.router.destroy_source(source_id) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::SetTapProcesses { source_id, pids } => {
            let mut st = lock_state(state);
            match st.router.set_tap_processes(source_id, pids) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::ListSources => {
            let st = lock_state(state);
            Response::Sources(st.router.list_sources())
        }
        Request::CreateRoute {
            source_id,
            destination,
            volume,
        } => {
            let mut st = lock_state(state);
            match st.router.create_route(source_id, destination, volume) {
                Ok(route_id) => Response::RouteCreated { route_id },
                Err(message) => Response::Error { message },
            }
        }
        Request::DestroyRoute { route_id } => {
            let mut st = lock_state(state);
            match st.router.destroy_route(route_id) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::ListRoutes => {
            let st = lock_state(state);
            Response::Routes(st.router.list_routes())
        }
        Request::SetVolume { route_id, volume } => {
            let mut st = lock_state(state);
            match st.router.set_volume(route_id, volume) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::SetRemoteDescription {
            peer,
            generation,
            sdp,
        } => {
            let st = lock_state(state);
            match st.router.set_remote_description(peer, generation, sdp) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::CreateDevice { name, channels } => {
            match shared.devices.create_device(&name, channels).await {
                Ok(info) => Response::DeviceCreated { device_uid: info.uid },
                Err(message) => Response::Error { message },
            }
        }
        Request::DestroyDevice { device_uid } => {
            let removed = {
                let st = lock_state(state);
                let in_use = st.router.routes_targeting(&device_uid);
                shared.devices.remove_device(&device_uid, &in_use)
            };
            match removed {
                Ok(()) => {
                    shared.devices.wait_until_removed(&device_uid).await;
                    Response::Ok
                }
                Err(message) => Response::Error { message },
            }
        }
        Request::ListDevices => Response::Devices(shared.devices.list_devices()),
        Request::GetActivity => Response::Activity(read_activity(&shared.activity)),
        Request::ListSystemDevices => Response::SystemDevices(spaghetti_engine::list_all_devices()),
        Request::ListAirPlayReceivers => match lock_state(state).router.list_airplay_receivers() {
            Ok(receivers) => Response::AirPlayReceivers(receivers),
            Err(message) => Response::Error { message },
        },
        Request::Shutdown => {
            tracing::info!("shutdown requested by client");
            shared.cancel.cancel();
            Response::Ok
        }
    }
}

/// Build an [`ActivityInfo`] response from the driver's shm snapshot.
///
/// Returns zeroed / inactive values when the shm region is unavailable.
/// If the region was not available at daemon start, attempts a lazy open on
/// each call — the driver may have loaded after the daemon started.
fn read_activity(activity: &ActivityState) -> ActivityInfo {
    let mut guard = match activity.reader.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Lazy attach: try opening now if we don't have a reader yet.
    if guard.is_none() {
        match ActivityReader::open() {
            Ok(r) => {
                tracing::info!("activity shm attached (lazy)");
                *guard = Some(r);
            }
            Err(_) => {
                return ActivityInfo {
                    active: false,
                    frames_per_second: 0,
                    peak_db: -96.0,
                    io_running: false,
                    total_frames: 0,
                };
            }
        }
    }

    let reader = guard.as_ref().expect("reader was just set");
    let snap = reader.snapshot();
    let fps = activity.fps.update(snap.frames_written);
    let stale = reader.is_stale(snap.last_write_host_time, ACTIVITY_FRESHNESS_NS);

    let peak_db = if snap.peak > 0.0 {
        let db = 20.0 * snap.peak.log10();
        if db < -96.0 { -96.0 } else { db }
    } else {
        -96.0
    };

    ActivityInfo {
        active: snap.io_running && !stale,
        frames_per_second: fps,
        peak_db,
        io_running: snap.io_running,
        total_frames: snap.frames_written,
    }
}

/// Recover from a poisoned mutex rather than panicking the daemon.
fn lock_state(state: &Mutex<DaemonState>) -> std::sync::MutexGuard<'_, DaemonState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("mutex was poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

/// Async version of length-prefixed message write.
async fn write_message_async(
    stream: &mut UnixStream,
    msg: &impl serde::Serialize,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    stream.flush().await
}

/// Read a length-prefixed frame, returning the raw bytes.
///
/// Only I/O failures (EOF, socket errors) are returned as errors — JSON
/// parsing is left to the caller so decode errors don't close the connection.
async fn read_frame_async(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    #[tokio::test]
    async fn client_draining_allows_an_in_flight_response_to_finish() {
        let done = Arc::new(AtomicBool::new(false));
        let mut clients = JoinSet::new();
        clients.spawn({
            let done = Arc::clone(&done);
            async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                done.store(true, Ordering::Release);
            }
        });
        stop_clients(
            &mut clients,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert!(done.load(Ordering::Acquire));
        assert!(clients.is_empty());
    }

    #[tokio::test]
    async fn an_idle_client_is_aborted_and_releases_its_shared_state() {
        let state = Arc::new(());
        let weak = Arc::downgrade(&state);
        let mut clients = JoinSet::new();
        clients.spawn(async move {
            std::future::pending::<()>().await;
            drop(state);
        });
        stop_clients(
            &mut clients,
            Duration::from_millis(5),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert!(clients.is_empty());
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn joining_an_uninterruptible_client_has_its_own_deadline() {
        let mut clients = JoinSet::new();
        let (started, waiting) = tokio::sync::oneshot::channel();
        clients.spawn_blocking(move || {
            started.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        waiting.await.unwrap();
        let before = Instant::now();
        let result = stop_clients(
            &mut clients,
            Duration::from_millis(10),
            Duration::from_millis(10),
        )
        .await;
        let elapsed = before.elapsed();
        assert!(result.unwrap_err().contains("abort deadline"));
        assert!(elapsed < Duration::from_millis(100), "waited {elapsed:?}");
        while clients.join_next().await.is_some() {}
    }

    struct SlowDrop {
        finished: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for SlowDrop {
        fn drop(&mut self) {
            std::thread::sleep(Duration::from_millis(150));
            let _ = self.finished.take().unwrap().send(());
        }
    }

    #[tokio::test]
    async fn router_teardown_has_a_deadline_even_when_drop_blocks() {
        let (finished, waiting) = tokio::sync::oneshot::channel();
        let before = Instant::now();
        let result = drop_with_timeout(
            SlowDrop {
                finished: Some(finished),
            },
            Duration::from_millis(10),
        )
        .await;
        let elapsed = before.elapsed();
        assert!(result.unwrap_err().contains("teardown exceeded"));
        assert!(elapsed < Duration::from_millis(100), "waited {elapsed:?}");
        waiting.await.unwrap();
        assert!(
            drop_with_timeout((), Duration::from_millis(100))
                .await
                .is_ok()
        );
    }
}
