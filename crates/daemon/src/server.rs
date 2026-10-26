use std::sync::{Arc, Mutex};

use spaghetti_common::ipc_protocol::*;
use spaghetti_engine::{ActivityReader, DeviceManager, FpsTracker, Router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

/// Freshness window for the driver's last WriteMix timestamp — older than this
/// and the activity snapshot is considered stale (driver idle or gone).
const ACTIVITY_FRESHNESS_NS: u64 = 200_000_000;

const HEALTH_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

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

    pub async fn run(&self) {
        let mut health_log = tokio::time::interval(HEALTH_LOG_INTERVAL);
        loop {
            tokio::select! {
                _ = health_log.tick() => lock_state(&self.shared.state).router.log_health(),
                accept = self.listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            tokio::spawn(handle_client(stream, Arc::clone(&self.shared)));
                        }
                        Err(e) => tracing::error!("accept error: {e}"),
                    }
                }
                () = self.shared.cancel.cancelled() => {
                    tracing::info!("server shutting down");
                    return;
                }
            }
        }
    }
}

async fn handle_client(mut stream: UnixStream, shared: Arc<Shared>) {
    let peer = stream.peer_addr().map(|a| format!("{a:?}")).unwrap_or_else(|_| "unknown".into());
    loop {
        let response = match read_frame_async(&mut stream).await {
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
