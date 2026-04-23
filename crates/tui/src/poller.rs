use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::time::{interval, MissedTickBehavior};

use spaghetti_common::async_client::IpcClient;
use spaghetti_common::ipc_protocol::{Request, Response};

use crate::state::{AppState, ConnectionStatus};

// 150ms activity tick; slower bulk polls on the 1s path.
const ACTIVITY_INTERVAL: Duration = Duration::from_millis(150);
const BULK_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

pub async fn run_poller(state: Arc<RwLock<AppState>>) {
    loop {
        match IpcClient::connect().await {
            Ok(mut client) => {
                state.write().await.connection_status = ConnectionStatus::Connected;

                if let Err(()) = poll_loop(&mut client, Arc::clone(&state)).await {
                    let mut s = state.write().await;
                    s.connection_status = ConnectionStatus::Disconnected;
                    s.clear_polled_data();
                }
            }
            Err(_) => {
                // Daemon not running yet.
            }
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// Returns `Err(())` when the connection is lost.
async fn poll_loop(client: &mut IpcClient, state: Arc<RwLock<AppState>>) -> Result<(), ()> {
    let mut activity_tick = interval(ACTIVITY_INTERVAL);
    activity_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut bulk_tick = interval(BULK_INTERVAL);
    bulk_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Consume the immediate first tick so we don't double-fire at t=0.
    activity_tick.tick().await;
    bulk_tick.tick().await;

    loop {
        tokio::select! {
            _ = activity_tick.tick() => {
                fetch_activity(client, Arc::clone(&state)).await?;
            }
            _ = bulk_tick.tick() => {
                fetch_bulk(client, Arc::clone(&state)).await?;
            }
        }
    }
}

async fn fetch_activity(client: &mut IpcClient, state: Arc<RwLock<AppState>>) -> Result<(), ()> {
    match client.send(Request::GetActivity).await {
        Ok(Response::Activity(info)) => {
            state.write().await.activity = Some(info);
            Ok(())
        }
        // Daemon rejected the request — show a badge but keep the connection alive.
        Ok(Response::Error { message }) => {
            state
                .write()
                .await
                .notify_daemon_rejection(format!("daemon: {message}"));
            Ok(())
        }
        // Transport failure — reconnect.
        Err(_) => Err(()),
        // Unexpected variant — treat like a rejection; the connection is still healthy.
        Ok(unexpected) => {
            state
                .write()
                .await
                .notify_daemon_rejection(format!(
                    "unexpected response to GetActivity: {unexpected:?}"
                ));
            Ok(())
        }
    }
}

async fn fetch_bulk(client: &mut IpcClient, state: Arc<RwLock<AppState>>) -> Result<(), ()> {
    // Each arm returns Err(()) only on a true transport failure.
    // Daemon rejections and unexpected variants are surfaced as notifications
    // but do not tear down the connection.
    macro_rules! fetch {
        ($req:expr, $pat:pat, $body:expr) => {{
            match client.send($req).await {
                Ok($pat) => {
                    $body;
                }
                Ok(Response::Error { message }) => {
                    state
                        .write()
                        .await
                        .notify_daemon_rejection(format!("daemon: {message}"));
                }
                Err(_) => return Err(()),
                Ok(unexpected) => {
                    state
                        .write()
                        .await
                        .notify_daemon_rejection(format!(
                            "unexpected response to {}: {unexpected:?}",
                            stringify!($req),
                        ));
                }
            }
        }};
    }

    fetch!(Request::Status, Response::Status(s), {
        state.write().await.daemon_status = Some(s);
    });
    fetch!(Request::ListApps, Response::Apps(list), {
        state.write().await.apps = Some(list);
    });
    fetch!(Request::ListTaps, Response::Taps(list), {
        state.write().await.taps = Some(list);
    });
    fetch!(Request::ListRoutes, Response::Routes(list), {
        state.write().await.routes = Some(list);
    });
    fetch!(Request::ListDevices, Response::Devices(list), {
        state.write().await.devices = Some(list);
    });
    fetch!(Request::ListSystemDevices, Response::SystemDevices(list), {
        state.write().await.system_devices = list;
    });

    Ok(())
}
