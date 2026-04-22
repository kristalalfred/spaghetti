use std::sync::{Arc, Mutex};

use spaghetti_common::ipc_protocol::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::audio;
use crate::device_manager::DeviceManager;
use crate::router::Router;
use crate::tap::{self, TapManager};

/// Shared daemon state accessible by all client handlers.
pub struct DaemonState {
    pub tap_manager: TapManager,
    pub router: Router,
    pub device_manager: DeviceManager,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            tap_manager: TapManager::new(),
            router: Router::new(),
            device_manager: DeviceManager::new(),
        }
    }
}

pub struct Server {
    listener: UnixListener,
    state: Arc<Mutex<DaemonState>>,
    pub cancel: CancellationToken,
}

impl Server {
    pub async fn bind() -> std::io::Result<Self> {
        // Remove stale socket file if it exists from a previous run
        let _ = std::fs::remove_file(SOCKET_PATH);
        let listener = UnixListener::bind(SOCKET_PATH)?;
        Ok(Self {
            listener,
            state: Arc::new(Mutex::new(DaemonState::new())),
            cancel: CancellationToken::new(),
        })
    }

    pub async fn run(&self) {
        loop {
            tokio::select! {
                accept = self.listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let state = Arc::clone(&self.state);
                            let cancel = self.cancel.clone();
                            tokio::spawn(handle_client(stream, state, cancel));
                        }
                        Err(e) => tracing::error!("accept error: {e}"),
                    }
                }
                () = self.cancel.cancelled() => {
                    tracing::info!("server shutting down");
                    return;
                }
            }
        }
    }
}

async fn handle_client(
    mut stream: UnixStream,
    state: Arc<Mutex<DaemonState>>,
    cancel: CancellationToken,
) {
    loop {
        let request = match read_message_async(&mut stream).await {
            Ok(req) => req,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    return;
                }
                tracing::error!("read error: {e}");
                return;
            }
        };

        let response = dispatch(request, &state, &cancel);

        if let Err(e) = write_message_async(&mut stream, &response).await {
            tracing::error!("write error: {e}");
            return;
        }
    }
}

fn dispatch(request: Request, state: &Arc<Mutex<DaemonState>>, cancel: &CancellationToken) -> Response {
    match request {
        Request::Status => {
            let st = lock_state(state);
            Response::Status(DaemonStatus {
                running: true,
                tap_count: st.tap_manager.tap_count(),
                route_count: st.router.route_count(),
            })
        }
        Request::ListApps => {
            let apps = tap::list_audio_apps();
            Response::Apps(apps)
        }
        Request::CreateTap { pid } => {
            let mut st = lock_state(state);
            match st.tap_manager.create_tap(pid) {
                Ok(tap_id) => Response::TapCreated { tap_id },
                Err(message) => Response::Error { message },
            }
        }
        Request::DestroyTap { tap_id } => {
            let mut st = lock_state(state);
            match st.tap_manager.destroy_tap(tap_id) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::ListTaps => {
            let st = lock_state(state);
            Response::Taps(st.tap_manager.list_taps())
        }
        Request::CreateRoute {
            tap_id,
            device_uid,
            volume,
        } => {
            let mut st = lock_state(state);

            let aggregate_device_id = match st.tap_manager.get_tap(tap_id) {
                Some(tap) => tap.aggregate_device_id,
                None => {
                    return Response::Error {
                        message: format!("tap {tap_id} not found"),
                    }
                }
            };

            let dest_device_id = match audio::find_device_by_uid(&device_uid) {
                Some(id) => id,
                None => {
                    return Response::Error {
                        message: format!("device '{device_uid}' not found"),
                    }
                }
            };

            match st.router.create_route(
                tap_id,
                aggregate_device_id,
                &device_uid,
                dest_device_id,
                volume,
            ) {
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
        Request::CreateDevice { name, channels } => {
            let mut st = lock_state(state);
            let ch = channels.unwrap_or(2);
            match st.device_manager.create_device(&name, ch) {
                Ok(device_uid) => Response::DeviceCreated { device_uid },
                Err(message) => Response::Error { message },
            }
        }
        Request::DestroyDevice { device_uid } => {
            let mut st = lock_state(state);
            match st.device_manager.destroy_device(&device_uid) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::ListDevices => {
            let st = lock_state(state);
            Response::Devices(st.device_manager.list_devices())
        }
        Request::Shutdown => {
            tracing::info!("shutdown requested by client");
            cancel.cancel();
            Response::Ok
        }
    }
}

/// Recover from a poisoned mutex rather than panicking the daemon.
fn lock_state(state: &Arc<Mutex<DaemonState>>) -> std::sync::MutexGuard<'_, DaemonState> {
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

/// Async version of length-prefixed message read.
async fn read_message_async<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
