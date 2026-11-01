mod client;
mod daemon_mgmt;
mod output;

use clap::{Parser, Subcommand};
use client::UNEXPECTED;
use output::{
    Output, RouteJson, SourceJson, format_activity_line, print_airplay_receivers, print_apps,
    print_devices, print_error, print_logo, print_routes, print_sources, print_status,
    print_system_devices,
};
use serde_json::json;
use spaghetti_common::ipc_protocol::{
    AirPlayReceiverInfo, Destination, Request, Response, SourceSpec, SrtMode, SrtSocket, TapMode,
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
    #[command(about = "Route a source to a device, AirPlay receiver, or SRT peer")]
    Route {
        /// Source ID to route
        source_id: u32,
        #[arg(
            default_value = "SpaghettiDevice_UID",
            help = "Device UID, airplay://NAME or airplay://ID, or srt://HOST:PORT"
        )]
        destination: String,
        /// Initial volume (0.0-2.0)
        #[arg(short, long, default_value = "1.0")]
        volume: f32,
        /// Play each frame this many milliseconds after its capture, in step with other routes given the same delay
        #[arg(short, long)]
        delay: Option<u32>,
        #[arg(
            long = "latency-offset",
            value_name = "MS",
            allow_negative_numbers = true,
            help = "Milliseconds the output sounds later than reported; negative means earlier (requires --delay)"
        )]
        latency_offset_ms: Option<i32>,
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
    #[command(name = "airplay", about = "List AirPlay receivers")]
    AirPlay,
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

fn route_destination(
    target: &str,
    delay: Option<u32>,
    latency_offset_ms: Option<i32>,
    srt: &SrtArgs,
    receivers: impl FnOnce() -> Result<Vec<AirPlayReceiverInfo>, String>,
) -> Result<Destination, String> {
    if latency_offset_ms.is_some() {
        if target.starts_with("srt://") {
            return Err(
                "--latency-offset applies only to output devices and AirPlay receivers".into(),
            );
        }
        if delay.is_none() {
            return Err("--latency-offset requires --delay <ms>".into());
        }
    }
    if srt.is_set() && !target.starts_with("srt://") {
        return Err("--latency and --passphrase apply only to srt:// destinations".into());
    }
    if let Some(name) = target.strip_prefix("airplay://") {
        let delay_ms = delay.ok_or("airplay:// destinations require --delay <ms>")?;
        if name.is_empty() {
            return Err("airplay:// requires a receiver name or id".into());
        }
        if name.parse::<airplay::ReceiverId>().is_ok() {
            return Ok(Destination::AirPlay {
                receiver: name.to_string(),
                delay_ms,
                latency_offset_ms,
            });
        }
        let receivers = receivers()?;
        let matches: Vec<_> = receivers.iter().filter(|r| r.name == name).collect();
        let receiver = match matches.as_slice() {
            [] => return Err(format!("AirPlay receiver name '{name}' not found")),
            [receiver] => receiver.id.clone(),
            _ => {
                let mut ids: Vec<_> = matches.iter().map(|r| r.id.as_str()).collect();
                ids.sort_unstable();
                return Err(format!(
                    "AirPlay name '{name}' is ambiguous; use an id: {}",
                    ids.join(", ")
                ));
            }
        };
        Ok(Destination::AirPlay {
            receiver,
            delay_ms,
            latency_offset_ms,
        })
    } else if target.starts_with("srt://") {
        if delay.is_some() {
            return Err("--delay applies only to output devices and AirPlay receivers".into());
        }
        Ok(Destination::Srt(srt.socket(target)?))
    } else {
        Ok(Destination::Device {
            uid: target.to_string(),
            delay_ms: delay,
            latency_offset_ms,
        })
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
            delay,
            latency_offset_ms,
            srt,
        } => {
            check_volume(volume)?;
            let destination =
                route_destination(&destination, delay, latency_offset_ms, &srt, || {
                    match client::request(&mut stream, &Request::ListAirPlayReceivers)
                        .map_err(|error| error.to_string())?
                    {
                        Response::AirPlayReceivers(receivers) => Ok(receivers),
                        _ => Err(UNEXPECTED.into()),
                    }
                })?;
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

        Command::AirPlay => {
            let Response::AirPlayReceivers(receivers) =
                client::request(&mut stream, &Request::ListAirPlayReceivers)?
            else {
                return Err(UNEXPECTED.into());
            };
            out.emit(&receivers, || print_airplay_receivers(&receivers))?;
        }

        Command::Monitor => return run_monitor(stream, out),

        Command::Shutdown => {
            let Response::Ok = client::request(&mut stream, &Request::Shutdown)? else {
                return Err(UNEXPECTED.into());
            };
            out.note("Daemon shut down.");
        }

        Command::Daemon { .. } | Command::SystemDevices => {
            unreachable!("commands without the daemon are handled in main")
        }
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

fn list_system_devices(out: Output) -> Result<(), Box<dyn Error>> {
    let devices = spaghetti_engine::list_all_devices();
    let listed: Vec<_> = devices
        .iter()
        .map(|device| {
            json!({
                "uid": device.uid,
                "name": device.name,
                "input_channels": device.input_channels,
                "output_channels": device.output_channels,
            })
        })
        .collect();
    out.emit(&listed, || print_system_devices(&devices))?;
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
        Some(Command::SystemDevices) => list_system_devices(out),
        Some(cmd) => run_command(cmd, out),
    };
    if let Err(e) = result {
        print_error(&e.to_string());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spaghetti_common::ipc_protocol::AirPlaySupport;

    fn srt() -> SrtArgs {
        SrtArgs {
            latency: None,
            passphrase: None,
        }
    }

    fn receiver(id: &str, name: &str) -> AirPlayReceiverInfo {
        AirPlayReceiverInfo {
            id: id.into(),
            name: name.into(),
            model: None,
            support: AirPlaySupport::Supported,
        }
    }

    #[test]
    fn signed_latency_offsets_parse_for_device_and_airplay_routes() {
        for target in ["Speakers", "airplay://C4:38:75:04:5E:80"] {
            for offset in ["-47", "83", "0"] {
                let cli = Cli::try_parse_from([
                    "spaghetti",
                    "route",
                    "7",
                    target,
                    "--delay",
                    "2500",
                    "--latency-offset",
                    offset,
                ])
                .unwrap();
                let Some(Command::Route {
                    destination,
                    delay,
                    latency_offset_ms,
                    srt,
                    ..
                }) = cli.command
                else {
                    panic!("expected route");
                };
                let offset: i32 = offset.parse().unwrap();
                assert_eq!(latency_offset_ms, Some(offset));
                let destination =
                    route_destination(&destination, delay, latency_offset_ms, &srt, || {
                        panic!("unexpected listing")
                    })
                    .unwrap();
                let json = serde_json::to_value(destination).unwrap();
                let kind = if target.starts_with("airplay://") {
                    "AirPlay"
                } else {
                    "Device"
                };
                assert_eq!(json[kind]["latency_offset_ms"], offset);
                assert_eq!(json[kind]["delay_ms"], 2500);
            }
        }
    }

    #[test]
    fn latency_offsets_require_a_scheduled_device_or_airplay_destination() {
        for target in ["Speakers", "airplay://Kitchen"] {
            for offset in [-47, 0, 83] {
                let error = route_destination(target, None, Some(offset), &srt(), || {
                    panic!("unexpected listing")
                })
                .unwrap_err();
                assert!(
                    error.contains("--latency-offset requires --delay"),
                    "{error}"
                );
            }
        }
        for delay in [None, Some(2500)] {
            let error = route_destination("srt://:9000", delay, Some(-47), &srt(), || {
                panic!("unexpected listing")
            })
            .unwrap_err();
            assert!(error.contains("--latency-offset applies only"), "{error}");
        }
    }

    #[test]
    fn airplay_list_and_route_commands_parse() {
        let cli = Cli::try_parse_from(["spaghetti", "airplay", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Some(Command::AirPlay)));
        let cli = Cli::try_parse_from([
            "spaghetti",
            "route",
            "7",
            "airplay://Living room",
            "--delay",
            "2500",
        ])
        .unwrap();
        let Some(Command::Route {
            source_id,
            destination,
            delay,
            srt,
            ..
        }) = cli.command
        else {
            panic!("expected route");
        };
        assert_eq!(source_id, 7);
        let destination = route_destination(&destination, delay, None, &srt, || {
            Ok(vec![receiver("C4:38:75:04:5E:80", "Living room")])
        })
        .unwrap();
        assert_eq!(
            destination,
            Destination::AirPlay {
                receiver: "C4:38:75:04:5E:80".into(),
                delay_ms: 2500,
                latency_offset_ms: None
            }
        );
    }

    #[test]
    fn airplay_requires_a_delay_and_name_before_listing() {
        for (target, delay, expected) in [
            ("airplay://Kitchen", None, "require --delay"),
            ("airplay://", Some(2500), "requires a receiver"),
        ] {
            let error =
                route_destination(target, delay, None, &srt(), || panic!("unexpected listing"))
                    .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn airplay_names_match_exactly_and_ambiguous_names_list_ids() {
        let list = || {
            Ok(vec![
                receiver("C4:38:75:04:5E:80", "Kitchen"),
                receiver("C4:38:75:04:5E:81", "Kitchen"),
            ])
        };
        let error =
            route_destination("airplay://Kitchen", Some(2500), None, &srt(), list).unwrap_err();
        assert!(error.contains("ambiguous"));
        assert!(error.contains("C4:38:75:04:5E:80, C4:38:75:04:5E:81"));
        for target in ["kitchen", "Kit", "Absent"] {
            let error = route_destination(
                &format!("airplay://{target}"),
                Some(2500),
                None,
                &srt(),
                list,
            )
            .unwrap_err();
            assert!(error.contains("not found"), "{error}");
        }
    }

    #[test]
    fn explicit_ids_reach_the_daemon_unchanged_without_listing() {
        for id in [
            "C4:38:75:04:5E:99",
            "c4:38:75:04:5e:99",
            "C43875045E99",
            "c43875045e99",
        ] {
            let destination =
                route_destination(&format!("airplay://{id}"), Some(2500), None, &srt(), || {
                    panic!("an explicit receiver id must not list receivers")
                })
                .unwrap();
            assert_eq!(
                destination,
                Destination::AirPlay {
                    receiver: id.into(),
                    delay_ms: 2500,
                    latency_offset_ms: None
                }
            );
        }
    }

    #[test]
    fn an_explicit_id_takes_precedence_over_a_matching_name() {
        let target = "C4:38:75:04:5E:80";
        let listed = std::cell::Cell::new(false);
        let destination = route_destination(
            &format!("airplay://{target}"),
            Some(2500),
            None,
            &srt(),
            || {
                listed.set(true);
                Ok(vec![
                    receiver(target, "Kitchen"),
                    receiver("C4:38:75:04:5E:81", target),
                ])
            },
        )
        .unwrap();
        assert_eq!(
            destination,
            Destination::AirPlay {
                receiver: target.into(),
                delay_ms: 2500,
                latency_offset_ms: None
            }
        );
        assert!(!listed.get());
    }

    #[test]
    fn support_is_checked_by_the_stream_and_discovery_errors_are_reported() {
        let mut unsupported = receiver("C4:38:75:04:5E:80", "Kitchen");
        unsupported.support = AirPlaySupport::Unsupported {
            reason: "pairing required".into(),
        };
        let destination = route_destination("airplay://Kitchen", Some(2500), None, &srt(), || {
            Ok(vec![unsupported])
        })
        .unwrap();
        assert_eq!(
            destination,
            Destination::AirPlay {
                receiver: "C4:38:75:04:5E:80".into(),
                delay_ms: 2500,
                latency_offset_ms: None
            }
        );
        assert_eq!(
            route_destination("airplay://Kitchen", Some(2500), None, &srt(), || Err(
                "discovery failed".into()
            ))
            .unwrap_err(),
            "discovery failed"
        );
    }

    #[test]
    fn existing_destinations_do_not_list_airplay_and_keep_their_delay_rules() {
        assert_eq!(
            route_destination("Speakers", Some(2500), None, &srt(), || panic!("listing")).unwrap(),
            Destination::Device {
                uid: "Speakers".into(),
                delay_ms: Some(2500),
                latency_offset_ms: None
            }
        );
        assert!(
            route_destination("srt://:9000", Some(2500), None, &srt(), || panic!(
                "listing"
            ))
            .is_err()
        );
        assert!(matches!(
            route_destination("srt://:9000", None, None, &srt(), || panic!("listing")).unwrap(),
            Destination::Srt(_)
        ));
        let options = SrtArgs {
            latency: Some(200),
            passphrase: None,
        };
        assert!(
            route_destination("airplay://Kitchen", Some(2500), None, &options, || panic!(
                "listing"
            ))
            .unwrap_err()
            .contains("only to srt://")
        );
    }
}
