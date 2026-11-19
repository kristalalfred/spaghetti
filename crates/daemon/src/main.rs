use std::future::Future;

use tokio::signal;
use tracing_subscriber::EnvFilter;

mod driver;
mod ipc;
mod server;

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn main() -> anyhow::Result<()> {
    init_logging();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

async fn run() -> anyhow::Result<()> {
    tracing::info!("spaghettid starting...");

    let driver = driver::Driver::open();
    driver.look_for_device();

    let mut server = server::Server::bind(driver)?;
    tracing::info!(
        "Listening on {}",
        spaghetti_common::ipc_protocol::SOCKET_PATH
    );

    let terminated = termination()?;

    let cancel = server.cancel.clone();

    // Run server until a shutdown request, Ctrl-C, or termination (launchd stop).
    tokio::select! {
        _ = server.run() => {},
        _ = signal::ctrl_c() => {
            tracing::info!("Shutting down (SIGINT)...");
        }
        signal = terminated => {
            tracing::info!("Shutting down ({signal})...");
        }
        () = cancel.cancelled() => {
            tracing::info!("Shutting down...");
        }
    }

    server.shutdown().await.map_err(anyhow::Error::msg)?;

    Ok(())
}

#[cfg(unix)]
fn termination() -> std::io::Result<impl Future<Output = &'static str>> {
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    Ok(async move {
        sigterm.recv().await;
        "SIGTERM"
    })
}

#[cfg(windows)]
fn termination() -> std::io::Result<impl Future<Output = &'static str>> {
    let mut close = signal::windows::ctrl_close()?;
    let mut shutdown = signal::windows::ctrl_shutdown()?;
    Ok(async move {
        tokio::select! {
            _ = close.recv() => "console closed",
            _ = shutdown.recv() => "system shutdown",
        }
    })
}
