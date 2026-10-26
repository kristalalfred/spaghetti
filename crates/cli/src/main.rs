mod client;
mod daemon_mgmt;
mod output;

use clap::{Parser, Subcommand};
use client::UNEXPECTED;
use output::{
    Output, RouteJson, SourceJson, format_activity_line, print_apps, print_devices, print_error,
    print_logo, print_routes, print_sources, print_status, print_system_devices,
};
use serde_json::json;
use spaghetti_common::ipc_protocol::{
    Destination, Request, Response, SourceSpec, SrtMode, SrtSocket, TapMode,
};
use std::error::Error;
use std::io::{ErrorKind, IsTerminal, Write};
use std::os::unix::net::UnixStream;

#[derive(Parser)]
#[command(name = "spaghetti", about = "Route audio any way you like")]
struct Cli {
    /// Print JSON instead of text, one line per reading for monitor
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show daemon status
    Status,
    /// List audio-producing apps
    Apps,
    /// Tap an app's audio as a source (by PID or name)
    Tap {
        /// Process ID or app name to tap
        target: String,
        /// Silence the source while the tap is active (redirect mode)
        #[arg(short, long)]
        mute: bool,
    },
    /// Receive AAC in MPEG-TS over SRT as a source
    Receive {
        /// srt://:PORT to listen, or srt://HOST:PORT to call
        url: String,
        #[command(flatten)]
        srt: SrtArgs,
    },
    /// Remove a source and its routes
    Unsource {
        /// Source ID to remove
        source_id: u32,
    },
    /// List sources
    Sources,
    /// Route a source to an output device or an SRT peer
    Route {
        /// Source ID to route
        source_id: u32,
        /// Output device UID, srt://:PORT to listen, or srt://HOST:PORT to call
        #[arg(default_value = "SpaghettiDevice_UID")]
        destination: String,
        /// Initial volume (0.0-2.0)
        #[arg(short, long, default_value = "1.0")]
        volume: f32,
        #[command(flatten)]
        srt: SrtArgs,
    },
    /// Remove a route
    Unroute {
        /// Route ID to remove
        route_id: u32,
    },
    /// List active routes
    Routes,
    /// Set route volume
    Volume {
        /// Route ID
        route_id: u32,
        /// Volume level (0.0-2.0)
        level: f32,
    },
    /// Manage virtual devices
    Devices {
        #[command(subcommand)]
        action: Option<DeviceAction>,
    },
    /// Stream live driver activity (frames/s, peak dB)
    Monitor,
    /// List all Core Audio devices visible to the system
    SystemDevices,
    /// Shut down the daemon
    Shutdown,
    /// Manage the background daemon (macOS LaunchAgent)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

const DEFAULT_SRT_LATENCY_MS: u32 = 200;

#[derive(clap::Args)]
struct SrtArgs {
    /// SRT latency in milliseconds [default: 200]
    #[arg(long)]
    latency: Option<u32>,
    /// SRT passphrase, 10 to 79 characters; the peer must use the same one
    #[arg(long, env = "SPAGHETTI_SRT_PASSPHRASE", hide_env_values = true)]
    passphrase: Option<String>,
}

impl SrtArgs {
    fn is_set(&self) -> bool {
        self.latency.is_some() || self.passphrase.is_some()
    }

    fn socket(&self, url: &str) -> Result<SrtSocket, String> {
        let rest = url
            .strip_prefix("srt://")
            .ok_or_else(|| format!("'{url}' is not an srt:// URL"))?;
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| format!("'{url}' has no port"))?;
        let port: u16 = port
            .parse()
            .map_err(|_| format!("'{port}' is not a port"))?;
        let mode = if host.is_empty() {
            SrtMode::Listen { port }
        } else {
            SrtMode::Call {
                host: host.trim_start_matches('[').trim_end_matches(']').to_string(),
                port,
            }
        };
        Ok(SrtSocket {
            mode,
            latency_ms: self.latency.unwrap_or(DEFAULT_SRT_LATENCY_MS),
            passphrase: self.passphrase.clone(),
            key_size: None,
        })
    }
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Install the LaunchAgent so the daemon starts at login
    Install,
    /// Remove the LaunchAgent
    Uninstall,
    /// Start the daemon
    Start,
    /// Stop the daemon (stays stopped until `start` or next login)
    Stop,
    /// Restart the daemon (sends SIGTERM; launchd relaunches it)
    Restart,
    /// Show launchd status for the daemon
    Status,
    /// Show daemon logs
    Logs {
        /// Follow the log (like `tail -f`)
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show (default: 200)
        #[arg(short = 'n', long, default_value = "200")]
        lines: usize,
    },
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Create a new virtual device
    Create {
        /// Device name
        name: String,
        /// Number of channels
        #[arg(short, long, default_value = "2")]
        channels: u32,
    },
    /// Destroy a virtual device
    Destroy {
        /// Device UID to destroy
        uid: String,
    },
}

fn check_volume(volume: f32) -> Result<(), &'static str> {
    if (0.0..=2.0).contains(&volume) {
        Ok(())
    } else {
        Err("volume must be between 0.0 and 2.0")
    }
}

fn run_monitor(stream: UnixStream, out: Output) -> Result<(), Box<dyn Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(monitor_loop(stream, out));
    if out == Output::Text {
        let _ = writeln!(std::io::stdout());
    }
    result
}

async fn monitor_loop(mut stream: UnixStream, out: Output) -> Result<(), Box<dyn Error>> {
    // Blocking Unix socket, polled from a periodic task. Using the std socket
    // keeps the CLI deps minimal; each request/response is small so blocking
    // the single-threaded runtime for the round-trip is acceptable here.
    let tty = std::io::stdout().is_terminal();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stdout = std::io::stdout();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = ticker.tick() => {
                let Response::Activity(info) = client::request(&mut stream, &Request::GetActivity)? else {
                    return Err(UNEXPECTED.into());
                };
                let written = match out {
                    // \r + erase-to-end-of-line, then redraw without newline.
                    Output::Text => write!(stdout, "\r\x1b[K{}", format_activity_line(&info, tty)),
                    Output::Json => writeln!(stdout, "{}", serde_json::to_string(&info)?),
                };
                match written.and_then(|()| stdout.flush()) {
                    Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(()),
                    result => result?,
                }
            }
        }
    }
}

fn run_command(cmd: Command, out: Output) -> Result<(), Box<dyn Error>> {
    let mut stream = client::connect()?;

    match cmd {
        Command::Status => {
            let Response::Status(status) = client::request(&mut stream, &Request::Status)? else {
                return Err(UNEXPECTED.into());
            };
            out.emit(&status, || print_status(&status))?;
        }

        Command::Apps => {
            let Response::Apps(apps) = client::request(&mut stream, &Request::ListApps)? else {
                return Err(UNEXPECTED.into());
            };
            out.emit(&apps, || print_apps(&apps))?;
        }

        Command::Tap { target, mute } => {
            let mode = if mute { TapMode::MuteSourceWhenTapped } else { TapMode::Mirror };

            // Try parsing as PID first; fall back to name lookup.
            let pids = if let Ok(pid) = target.parse::<u32>() {
                vec![pid]
            } else {
                let Response::Apps(apps) = client::request(&mut stream, &Request::ListApps)? else {
                    return Err(UNEXPECTED.into());
                };
                let needle = target.to_lowercase();
                apps.iter()
                    .find(|a| a.name.to_lowercase() == needle)
                    .or_else(|| apps.iter().find(|a| a.name.to_lowercase().contains(&needle)))
                    .map(|app| app.pids.clone())
                    .ok_or_else(|| format!("no app matching '{target}' found"))?
            };

            create_source(&mut stream, out, SourceSpec::Tap { pids, mode })?;
        }

        Command::Receive { url, srt } => {
            create_source(&mut stream, out, SourceSpec::Srt(srt.socket(&url)?))?;
        }

        Command::Unsource { source_id } => {
            let Response::Ok =
                client::request(&mut stream, &Request::DestroySource { source_id })?
            else {
                return Err(UNEXPECTED.into());
            };
            out.note(format_args!("Source {source_id} removed."));
        }

        Command::Sources => {
            let Response::Sources(mut sources) =
                client::request(&mut stream, &Request::ListSources)?
            else {
                return Err(UNEXPECTED.into());
            };
            sources.sort_by_key(|s| s.source_id);
            let json: Vec<_> = sources.iter().map(SourceJson::from).collect();
            out.emit(&json, || print_sources(&sources))?;
        }

        Command::Route {
            source_id,
            destination,
            volume,
            srt,
        } => {
            check_volume(volume)?;
            if srt.is_set() && !destination.starts_with("srt://") {
                return Err("--latency and --passphrase apply only to srt:// destinations".into());
            }
            let destination = if destination.starts_with("srt://") {
                Destination::Srt(srt.socket(&destination)?)
            } else {
                Destination::Device { uid: destination }
            };
            let Response::RouteCreated { route_id } = client::request(
                &mut stream,
                &Request::CreateRoute {
                    source_id,
                    destination,
                    volume,
                },
            )?
            else {
                return Err(UNEXPECTED.into());
            };
            out.emit(&json!({ "route_id": route_id }), || {
                println!("Route created: id={route_id}")
            })?;
        }

        Command::Unroute { route_id } => {
            let Response::Ok = client::request(&mut stream, &Request::DestroyRoute { route_id })?
            else {
                return Err(UNEXPECTED.into());
            };
            out.note(format_args!("Route {route_id} removed."));
        }

        Command::Routes => {
            let Response::Routes(mut routes) = client::request(&mut stream, &Request::ListRoutes)?
            else {
                return Err(UNEXPECTED.into());
            };
            routes.sort_by_key(|r| r.route_id);
            let json: Vec<_> = routes.iter().map(RouteJson::from).collect();
            out.emit(&json, || print_routes(&routes))?;
        }

        Command::Volume { route_id, level } => {
            check_volume(level)?;
            let Response::Ok = client::request(
                &mut stream,
                &Request::SetVolume {
                    route_id,
                    volume: level,
                },
            )?
            else {
                return Err(UNEXPECTED.into());
            };
            out.note(format_args!("Volume for route {route_id} set to {level:.2}."));
        }

        Command::Devices { action } => match action {
            None => {
                let Response::Devices(devices) =
                    client::request(&mut stream, &Request::ListDevices)?
                else {
                    return Err(UNEXPECTED.into());
                };
                out.emit(&devices, || print_devices(&devices))?;
            }
            Some(DeviceAction::Create { name, channels }) => {
                let Response::DeviceCreated { device_uid } = client::request(
                    &mut stream,
                    &Request::CreateDevice {
                        name,
                        channels: Some(channels),
                    },
                )?
                else {
                    return Err(UNEXPECTED.into());
                };
                out.emit(&json!({ "uid": device_uid }), || {
                    println!("Device created: {device_uid}")
                })?;
            }
            Some(DeviceAction::Destroy { uid }) => {
                let Response::Ok =
                    client::request(&mut stream, &Request::DestroyDevice { device_uid: uid })?
                else {
                    return Err(UNEXPECTED.into());
                };
                out.note("Device destroyed.");
            }
        },

        Command::SystemDevices => {
            let Response::SystemDevices(devices) =
                client::request(&mut stream, &Request::ListSystemDevices)?
            else {
                return Err(UNEXPECTED.into());
            };
            out.emit(&devices, || print_system_devices(&devices))?;
        }

        Command::Monitor => return run_monitor(stream, out),

        Command::Shutdown => {
            let Response::Ok = client::request(&mut stream, &Request::Shutdown)? else {
                return Err(UNEXPECTED.into());
            };
            out.note("Daemon shut down.");
        }

        Command::Daemon { .. } => unreachable!("daemon commands handled in main"),
    }

    Ok(())
}

fn create_source(
    stream: &mut UnixStream,
    out: Output,
    source: SourceSpec,
) -> Result<(), Box<dyn Error>> {
    let Response::SourceCreated { source_id } =
        client::request(stream, &Request::CreateSource { source, tag: None })?
    else {
        return Err(UNEXPECTED.into());
    };
    out.emit(&json!({ "source_id": source_id }), || {
        println!("Source created: id={source_id}")
    })?;
    Ok(())
}

fn run_daemon(action: DaemonAction, out: Output) -> Result<(), String> {
    match action {
        DaemonAction::Install => daemon_mgmt::install(out),
        DaemonAction::Uninstall => daemon_mgmt::uninstall(out),
        DaemonAction::Start => daemon_mgmt::start(out),
        DaemonAction::Stop => daemon_mgmt::stop(out),
        DaemonAction::Restart => daemon_mgmt::restart(out),
        DaemonAction::Status => daemon_mgmt::status(out),
        DaemonAction::Logs { .. } if out == Output::Json => {
            Err("daemon logs has no JSON output".into())
        }
        DaemonAction::Logs { follow, lines } => daemon_mgmt::logs(follow, lines),
    }
}

fn main() {
    let cli = Cli::parse();
    let out = if cli.json { Output::Json } else { Output::Text };

    let result = match cli.command {
        None => {
            print_logo();
            println!("\nRun 'spaghetti --help' for usage.\n");
            Ok(())
        }
        // Daemon-management commands don't talk to the daemon socket; they
        // shell out to launchctl / tail. Handle them before `run_command` so
        // they work even when the daemon is not running.
        Some(Command::Daemon { action }) => run_daemon(action, out).map_err(Into::into),
        Some(cmd) => run_command(cmd, out),
    };
    if let Err(e) = result {
        print_error(&e.to_string());
        std::process::exit(1);
    }
}
