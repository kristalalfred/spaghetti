use tokio::signal;

mod audio;
mod device_manager;
mod mixer;
mod router;
mod server;
mod tap;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("spaghettid starting...");

    // Verify the Spaghetti virtual device exists
    let device_id = audio::find_device_by_uid("SpaghettiDevice_UID")
        .ok_or_else(|| anyhow::anyhow!("Spaghetti virtual device not found. Is the driver installed?"))?;
    tracing::info!("Found Spaghetti device: object_id={device_id}");

    // Start the control socket server
    let server = server::Server::bind().await?;
    tracing::info!("Listening on {}", spaghetti_common::ipc_protocol::SOCKET_PATH);

    // Run server until either a shutdown request or Ctrl-C
    tokio::select! {
        _ = server.run() => {},
        _ = signal::ctrl_c() => {
            tracing::info!("Shutting down...");
        }
        () = server.cancel.cancelled() => {
            tracing::info!("Shutting down...");
        }
    }

    // Cleanup socket
    let _ = std::fs::remove_file(spaghetti_common::ipc_protocol::SOCKET_PATH);
    Ok(())
}
