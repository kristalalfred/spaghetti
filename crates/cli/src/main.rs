mod client;

use clap::{Parser, Subcommand};
use spaghetti_common::ipc_protocol::{AppInfo, DeviceInfo, Request, Response, RouteInfo, TapInfo};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_logo() {
    // Color codes
    let y = "\x1b[38;2;245;195;60m";   // golden pasta
    let y2 = "\x1b[38;2;220;170;40m";  // darker pasta
    let y3 = "\x1b[38;2;255;210;80m";  // bright pasta highlight
    let r = "\x1b[38;2;200;45;25m";    // tomato sauce
    let r2 = "\x1b[38;2;160;30;15m";   // darker sauce
    let g = "\x1b[38;2;60;160;50m";    // basil green
    let w = "\x1b[38;5;253m";          // plate rim
    let d = "\x1b[38;5;243m";          // plate shadow
    let f = "\x1b[38;5;249m";          // fork silver
    let f2 = "\x1b[38;5;253m";         // fork highlight
    let dim = "\x1b[2m";               // dim
    let b = "\x1b[1m";                 // bold
    let res = "\x1b[0m";               // reset

    let art = format!(
        r#"
{f2}        ┃{res}
{f}       ━╋━{res}       {y3}╭─╮{y}╭──╮  ╭─╮{res}
{f}        ┃{res}      {y}╭─╯{y3}╰╯{y2}╭╮╰╮╭╯ ╰╮{res}
{f}        ┃{res}     {y2}╰╮ ╭╯{y}╰╯ ╰╯{y3}╭╯╭─╯{res}
{f}        ┃{res}      {y}╰╮╰─╮{y2}╭──╮{y3}╰╮│{y}╭╮{res}
{f}        ┃{res}     {y3}╭─╯{y2}╭╯╰╮{y}╭╯{y2}╭╯╰╯{y}╰╮{res}
{f}        ┃{res}     {y}│{y2}╭╯{r}▓▓{y}╰╯{r}▓▓▓{y2}╰╮{y3}╭─╯{res}
{f2}       ╭┸╮{res}   {y2}╰╯{r}▓▓▓▓▓▓▓▓▓{y}╰╯{res}
{f}       ┃ ┃{res}    {r}▓▓▓{r2}▓▓{g}❦{r}▓▓▓▓▓▓{res}
{f}       ┃ ┃{res}   {w}╭───────────────╮{res}
{f}       ┃ ┃{res}   {w}│{r}▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓{w}│{res}   {b}Spaghetti{res} {dim}{VERSION}{res}
{f}       ┃ ┃{res}   {w}╰┬─────────────┬╯{res}   {dim}route audio any way you like{res}
{f}       ╰━╯{res}    {d}╰─────────────╯{res}
"#
    );
    print!("{art}");
}

#[derive(Parser)]
#[command(name = "spaghetti", about = "Route audio any way you like")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show daemon status
    Status,
    /// List audio-producing apps
    Apps,
    /// Create a tap on an app (by PID or name)
    Tap {
        /// Process ID or app name to tap
        target: String,
    },
    /// Remove a tap
    Untap {
        /// Tap ID to remove
        tap_id: u32,
    },
    /// List active taps
    Taps,
    /// Route a tap to a virtual device
    Route {
        /// Tap ID to route
        tap_id: u32,
        /// Destination device UID
        #[arg(default_value = "SpaghettiDevice_UID")]
        device: String,
        /// Initial volume (0.0-1.0)
        #[arg(short, long, default_value = "1.0")]
        volume: f32,
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
        /// Volume level (0.0-1.0)
        level: f32,
    },
    /// Manage virtual devices
    Devices {
        #[command(subcommand)]
        action: Option<DeviceAction>,
    },
    /// Shut down the daemon
    Shutdown,
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

fn print_apps(apps: &[AppInfo]) {
    if apps.is_empty() {
        println!("No audio apps detected.");
        return;
    }
    println!("{:<8} {}", "PID", "NAME");
    println!("{}", "-".repeat(40));
    for app in apps {
        println!("{:<8} {}", app.pid, app.name);
    }
}

fn print_taps(taps: &[TapInfo]) {
    if taps.is_empty() {
        println!("No active taps.");
        return;
    }
    println!("{:<6} {:<8} {}", "ID", "PID", "APP NAME");
    println!("{}", "-".repeat(40));
    for tap in taps {
        println!("{:<6} {:<8} {}", tap.tap_id, tap.pid, tap.app_name);
    }
}

fn print_routes(routes: &[RouteInfo]) {
    if routes.is_empty() {
        println!("No active routes.");
        return;
    }
    println!("{:<6} {:<6} {:<30} {}", "ID", "TAP", "DEVICE", "VOLUME");
    println!("{}", "-".repeat(55));
    for route in routes {
        println!(
            "{:<6} {:<6} {:<30} {:.2}",
            route.route_id, route.tap_id, route.device_uid, route.volume
        );
    }
}

fn print_devices(devices: &[DeviceInfo]) {
    if devices.is_empty() {
        println!("No virtual devices.");
        return;
    }
    println!("{:<40} {:<24} {}", "UID", "NAME", "CHANNELS");
    println!("{}", "-".repeat(70));
    for d in devices {
        println!("{:<40} {:<24} {}", d.uid, d.name, d.channels);
    }
}

fn print_error(msg: &str) {
    // Use red if the terminal likely supports ANSI colors
    if std::env::var("TERM").is_ok() && std::env::var("NO_COLOR").is_err() {
        eprintln!("\x1b[31merror:\x1b[0m {msg}");
    } else {
        eprintln!("error: {msg}");
    }
}

fn run_command(cmd: Command) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = client::connect().map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            "Daemon not running. Start it with: spaghettid".to_string()
        } else {
            format!("Failed to connect to daemon: {e}")
        }
    })?;

    match cmd {
        Command::Status => {
            let response = client::send_request(&mut stream, &Request::Status)?;
            match response {
                Response::Status(s) => {
                    let state = if s.running { "running" } else { "stopped" };
                    println!("Daemon: {state}");
                    println!("Taps:   {}", s.tap_count);
                    println!("Routes: {}", s.route_count);
                }
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Apps => {
            let response = client::send_request(&mut stream, &Request::ListApps)?;
            match response {
                Response::Apps(apps) => print_apps(&apps),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Tap { target } => {
            // Try parsing as PID first; fall back to name lookup.
            let pid = if let Ok(pid) = target.parse::<u32>() {
                pid
            } else {
                let response = client::send_request(&mut stream, &Request::ListApps)?;
                let apps = match response {
                    Response::Apps(apps) => apps,
                    Response::Error { message } => {
                        print_error(&message);
                        std::process::exit(1);
                    }
                    _ => {
                        print_error("unexpected response from daemon");
                        std::process::exit(1);
                    }
                };

                let needle = target.to_lowercase();
                let matched = apps
                    .iter()
                    .find(|a| a.name.to_lowercase().contains(&needle));

                match matched {
                    Some(app) => app.pid,
                    None => {
                        print_error(&format!("no app matching '{target}' found"));
                        std::process::exit(1);
                    }
                }
            };

            // Need a fresh stream after the ListApps round-trip consumed it.
            // (A single Unix socket stream is half-duplex per request here.)
            // Re-use the same stream for the first case; open a new one for
            // the name-lookup path.
            let response = client::send_request(&mut stream, &Request::CreateTap { pid })?;
            match response {
                Response::TapCreated { tap_id } => println!("Tap created: id={tap_id}"),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Untap { tap_id } => {
            let response = client::send_request(&mut stream, &Request::DestroyTap { tap_id })?;
            match response {
                Response::Ok => println!("Tap {tap_id} removed."),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Taps => {
            let response = client::send_request(&mut stream, &Request::ListTaps)?;
            match response {
                Response::Taps(taps) => print_taps(&taps),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Route { tap_id, device, volume } => {
            if !(0.0..=2.0).contains(&volume) {
                print_error("volume must be between 0.0 and 2.0");
                std::process::exit(1);
            }
            let response = client::send_request(
                &mut stream,
                &Request::CreateRoute {
                    tap_id,
                    device_uid: device,
                    volume,
                },
            )?;
            match response {
                Response::RouteCreated { route_id } => {
                    println!("Route created: id={route_id}");
                }
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Unroute { route_id } => {
            let response =
                client::send_request(&mut stream, &Request::DestroyRoute { route_id })?;
            match response {
                Response::Ok => println!("Route {route_id} removed."),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Routes => {
            let response = client::send_request(&mut stream, &Request::ListRoutes)?;
            match response {
                Response::Routes(routes) => print_routes(&routes),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Volume { route_id, level } => {
            if !(0.0..=2.0).contains(&level) {
                print_error("volume must be between 0.0 and 2.0");
                std::process::exit(1);
            }
            let response = client::send_request(
                &mut stream,
                &Request::SetVolume {
                    route_id,
                    volume: level,
                },
            )?;
            match response {
                Response::Ok => println!("Volume for route {route_id} set to {level:.2}."),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Devices { action } => match action {
            None => {
                let response =
                    client::send_request(&mut stream, &Request::ListDevices)?;
                match response {
                    Response::Devices(devices) => print_devices(&devices),
                    Response::Error { message } => {
                        print_error(&message);
                        std::process::exit(1);
                    }
                    _ => print_error("unexpected response from daemon"),
                }
            }
            Some(DeviceAction::Create { name, channels }) => {
                let response = client::send_request(
                    &mut stream,
                    &Request::CreateDevice {
                        name,
                        channels: Some(channels),
                    },
                )?;
                match response {
                    Response::DeviceCreated { device_uid } => {
                        println!("Device created: {device_uid}");
                    }
                    Response::Error { message } => {
                        print_error(&message);
                        std::process::exit(1);
                    }
                    _ => print_error("unexpected response from daemon"),
                }
            }
            Some(DeviceAction::Destroy { uid }) => {
                let response = client::send_request(
                    &mut stream,
                    &Request::DestroyDevice { device_uid: uid },
                )?;
                match response {
                    Response::Ok => println!("Device destroyed."),
                    Response::Error { message } => {
                        print_error(&message);
                        std::process::exit(1);
                    }
                    _ => print_error("unexpected response from daemon"),
                }
            }
        },

        Command::Shutdown => {
            let response = client::send_request(&mut stream, &Request::Shutdown)?;
            match response {
                Response::Ok => println!("Daemon shut down."),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }
    }

    Ok(())
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        None => {
            print_logo();
            println!("\nRun 'spaghetti --help' for usage.\n");
        }
        Some(cmd) => {
            if let Err(e) = run_command(cmd) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}
