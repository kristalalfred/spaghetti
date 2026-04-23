use std::fs;

use tokio::signal;
use tokio::signal::unix::SignalKind;
use tracing_subscriber::EnvFilter;

mod activity;
mod audio;
mod device_manager;
mod mixer;
mod router;
mod server;
mod tap;

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

    // Verify the Spaghetti virtual device exists.
    let device_id = audio::find_device_by_uid("SpaghettiDevice_UID")
        .ok_or_else(|| anyhow::anyhow!("Spaghetti virtual device not found. Is the driver installed?"))?;
    tracing::info!("Found Spaghetti device: object_id={device_id}");

    // Activity shm is a soft dependency — daemon keeps running if the driver's
    // shm region is unavailable (reads will just return defaults).
    let activity_reader = match activity::ActivityReader::open() {
        Ok(r) => {
            tracing::info!("activity shm opened: {}", spaghetti_common::ACTIVITY_SHM_NAME);
            Some(r)
        }
        Err(e) => {
            tracing::warn!("activity shm unavailable: {e}");
            None
        }
    };

    // Start the control socket server.
    let server = server::Server::bind(activity_reader).await?;
    tracing::info!("Listening on {}", spaghetti_common::ipc_protocol::SOCKET_PATH);

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
