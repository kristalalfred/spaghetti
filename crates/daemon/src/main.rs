use std::fs;

use spaghetti_engine::{ActivityReader, DeviceManager};
use tokio::signal;
use tokio::signal::unix::SignalKind;
use tracing_subscriber::EnvFilter;

mod server;

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();

    tracing::info!("spaghettid starting...");

    // Bootstrap the device config file before anything else. On a fresh
    // install this writes the default entry so the HAL plugin can publish
    // `SpaghettiDevice_UID` when it loads.
    let device_manager = DeviceManager::new();

    // Activity shm is a soft dependency — daemon keeps running if the driver's
    // shm region is unavailable (reads will just return defaults).
    let activity_reader = match ActivityReader::open() {
        Ok(r) => {
            tracing::info!("activity shm opened: {}", spaghetti_common::ACTIVITY_SHM_NAME);
            Some(r)
        }
        Err(e) => {
            tracing::warn!("activity shm unavailable: {e}");
            None
        }
    };

    let server = server::Server::bind(activity_reader, device_manager).await?;
    tracing::info!("Listening on {}", spaghetti_common::ipc_protocol::SOCKET_PATH);

    // Probe for the virtual device in the background so we log whether the
    // driver is installed without blocking the socket bind.
    tokio::task::spawn_blocking(|| match spaghetti_engine::find_device_by_uid("SpaghettiDevice_UID") {
        Some(id) => tracing::info!("Found Spaghetti device: object_id={id}"),
        None => tracing::warn!(
            "Spaghetti virtual device not found — is the driver installed and coreaudiod loaded it?"
        ),
    });

    let mut sigterm = signal::unix::signal(SignalKind::terminate())?;

    // Run server until a shutdown request, Ctrl-C, or SIGTERM (launchd stop).
    tokio::select! {
        _ = server.run() => {},
        _ = signal::ctrl_c() => {
            tracing::info!("Shutting down (SIGINT)...");
        }
        _ = sigterm.recv() => {
            tracing::info!("Shutting down (SIGTERM)...");
        }
        () = server.cancel.cancelled() => {
            tracing::info!("Shutting down...");
        }
    }

    let _ = fs::remove_file(spaghetti_common::ipc_protocol::SOCKET_PATH);

    Ok(())
}
