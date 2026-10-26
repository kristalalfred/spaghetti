mod client;
mod daemon_mgmt;

use clap::{Parser, Subcommand, ValueEnum};
use spaghetti_common::ipc_protocol::{
    ActivityInfo, AppInfo, Destination, DeviceInfo, FlowState, FlowStatus, PeerRef, PeerRole, Request,
    Response, RouteInfo, SourceInfo, SourceSpec, SrtMode, SrtSocket, SystemDeviceInfo, TapMode,
    TurnServer, WebRtcPeer,
};
use std::io::{IsTerminal, Read, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Tap an app's audio as a source (by PID or name)
    Tap {
        /// Process ID or app name to tap
        target: String,
        /// Silence the source while the tap is active (redirect mode)
        #[arg(short, long)]
        mute: bool,
    },
    /// Receive AAC in MPEG-TS over SRT, or Opus over WebRTC, as a source
    Receive {
        /// srt://:PORT to listen, srt://HOST:PORT to call, or webrtc to answer an offer
        url: String,
        #[command(flatten)]
        srt: SrtArgs,
        #[command(flatten)]
        webrtc: WebRtcArgs,
    },
    /// Remove a source and its routes
    Unsource {
        /// Source ID to remove
        source_id: u32,
    },
    /// List sources
    Sources,
    /// Route a source to an output device, an SRT peer or a WebRTC peer
    Route {
        /// Source ID to route
        source_id: u32,
        /// Output device UID, srt://:PORT to listen, srt://HOST:PORT to call, or webrtc to offer
        #[arg(default_value = "SpaghettiDevice_UID")]
        destination: String,
        /// Initial volume (0.0-2.0)
        #[arg(short, long, default_value = "1.0")]
        volume: f32,
        #[command(flatten)]
        srt: SrtArgs,
        #[command(flatten)]
        webrtc: WebRtcArgs,
    },
    /// Print a WebRTC peer's offer or answer
    Sdp {
        peer: PeerKind,
        /// Source or route ID
        id: u32,
    },
    /// Give a WebRTC peer the remote offer or answer, from a file or - for stdin
    Accept {
        peer: PeerKind,
        /// Source or route ID
        id: u32,
        file: String,
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

#[derive(clap::Args)]
struct WebRtcArgs {
    /// STUN server for a webrtc peer, repeatable
    #[arg(long, value_name = "HOST:PORT")]
    stun: Vec<String>,
    /// TURN server for a webrtc peer, repeatable
    #[arg(long, value_name = "HOST:PORT,USERNAME,CREDENTIAL")]
    turn: Vec<String>,
}

impl WebRtcArgs {
    fn is_set(&self) -> bool {
        !self.stun.is_empty() || !self.turn.is_empty()
    }

    /// Claude: a peer of a link this CLI names itself; its first generation.
    fn peer(&self, role: PeerRole) -> Result<WebRtcPeer, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        Ok(WebRtcPeer {
            link: format!("cli-{nanos:x}"),
            generation: 1,
            role,
            stun: self.stun.clone(),
            turn: self
                .turn
                .iter()
                .map(|server| turn_server(server))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// Claude: the error never repeats the value; it holds the credential.
fn turn_server(value: &str) -> Result<TurnServer, String> {
    match value.splitn(3, ',').collect::<Vec<_>>()[..] {
        [address, username, credential]
            if !address.is_empty() && !username.is_empty() && !credential.is_empty() =>
        {
            Ok(TurnServer {
                address: address.to_string(),
                username: username.to_string(),
                credential: credential.to_string(),
            })
        }
        _ => Err("--turn takes HOST:PORT,USERNAME,CREDENTIAL".to_string()),
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PeerKind {
    Source,
    Route,
}

const LOCAL_DESCRIPTION_WAIT: Duration = Duration::from_secs(3);

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

fn print_apps(apps: &[AppInfo]) {
    if apps.is_empty() {
        println!("No audio apps detected.");
        return;
    }
    let name_w = col_width(apps.iter().map(|a| a.name.as_str()), 24);
    println!("{:<name_w$} {:<16} BUNDLE", "NAME", "PIDS");
    println!("{}", "-".repeat(name_w + 40));
    for app in apps {
        let pids = app.pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        println!("{:<name_w$} {:<16} {}", app.name, pids, app.bundle_id.as_deref().unwrap_or("-"));
    }
}

fn flow_label(state: FlowState) -> &'static str {
    match state {
        FlowState::Idle => "idle",
        FlowState::Connecting => "connecting",
        FlowState::Connected => "connected",
        FlowState::Flowing => "flowing",
        FlowState::Stalled => "stalled",
        FlowState::Unreachable => "unreachable",
        FlowState::Lost => "lost",
    }
}

fn state_label(webrtc: bool, status: &FlowStatus) -> String {
    let state = flow_label(status.state);
    match (webrtc, &status.peer) {
        (true, Some(_)) if status.relayed => format!("{state} (relayed)"),
        (true, Some(_)) => format!("{state} (direct)"),
        _ => state.to_string(),
    }
}

fn srt_label(socket: &SrtSocket) -> String {
    match &socket.mode {
        SrtMode::Listen { port } => format!("srt://:{port}"),
        SrtMode::Call { host, port } => format!("srt://{host}:{port}"),
    }
}

fn role_label(role: PeerRole) -> &'static str {
    match role {
        PeerRole::Offer => "offer",
        PeerRole::Answer => "answer",
    }
}

fn print_sources(sources: &[SourceInfo]) {
    if sources.is_empty() {
        println!("No sources.");
        return;
    }
    let mut sources: Vec<_> = sources.iter().collect();
    sources.sort_by_key(|s| s.source_id);
    let kinds: Vec<String> = sources
        .iter()
        .map(|source| match &source.spec {
            SourceSpec::Tap { mode, .. } => format!("tap ({})", mode.label()),
            SourceSpec::Srt(_) => "srt".to_string(),
            SourceSpec::WebRtc(peer) => format!("webrtc ({})", role_label(peer.role)),
        })
        .collect();
    let kind_w = col_width(kinds.iter().map(String::as_str), 12);
    let label_w = col_width(sources.iter().map(|s| s.label.as_str()), 24);
    println!("{:<6} {:<kind_w$} {:<label_w$} STATE", "ID", "KIND", "LABEL");
    println!("{}", "-".repeat(kind_w + label_w + 20));
    for (source, kind) in sources.into_iter().zip(&kinds) {
        println!(
            "{:<6} {:<kind_w$} {:<label_w$} {}",
            source.source_id,
            kind,
            source.label,
            state_label(matches!(source.spec, SourceSpec::WebRtc(_)), &source.status)
        );
    }
}

fn print_routes(routes: &[RouteInfo]) {
    if routes.is_empty() {
        println!("No active routes.");
        return;
    }
    let mut routes: Vec<_> = routes.iter().collect();
    routes.sort_by_key(|r| r.route_id);
    let destinations: Vec<String> = routes
        .iter()
        .map(|route| match &route.destination {
            Destination::Device { uid } => uid.clone(),
            Destination::Srt(socket) => srt_label(socket),
            Destination::WebRtc(peer) => format!("webrtc {} ({})", peer.link, role_label(peer.role)),
        })
        .collect();
    let dest_w = col_width(destinations.iter().map(String::as_str), 30);
    println!("{:<6} {:<7} {:<dest_w$} {:<7} STATE", "ID", "SOURCE", "DESTINATION", "VOLUME");
    println!("{}", "-".repeat(dest_w + 34));
    for (route, destination) in routes.into_iter().zip(&destinations) {
        println!(
            "{:<6} {:<7} {:<dest_w$} {:<7.2} {}",
            route.route_id,
            route.source_id,
            destination,
            route.volume,
            state_label(
                matches!(route.destination, Destination::WebRtc(_)),
                &route.status
            )
        );
    }
}

fn print_system_devices(devices: &[SystemDeviceInfo]) {
    if devices.is_empty() {
        println!("No audio devices found.");
        return;
    }
    let uid_w = col_width(devices.iter().map(|d| d.uid.as_str()), 40);
    let name_w = col_width(devices.iter().map(|d| d.name.as_str()), 24);
    println!("{:<uid_w$} {:<name_w$} {:<8} TYPE", "UID", "NAME", "CHANNELS");
    println!("{}", "-".repeat(uid_w + name_w + 16));
    for d in devices {
        let kind = if d.is_output { "output" } else { "input" };
        println!("{:<uid_w$} {:<name_w$} {:<8} {}", d.uid, d.name, d.channels, kind);
    }
}

fn print_devices(devices: &[DeviceInfo]) {
    if devices.is_empty() {
        println!("No virtual devices.");
        return;
    }
    let uid_w = col_width(devices.iter().map(|d| d.uid.as_str()), 40);
    let name_w = col_width(devices.iter().map(|d| d.name.as_str()), 24);
    println!("{:<uid_w$} {:<name_w$} {:<8} FLAGS", "UID", "NAME", "CHANNELS");
    println!("{}", "-".repeat(uid_w + name_w + 31));
    for d in devices {
        let flags = if d.is_default { "(default, protected)" } else { "" };
        println!("{:<uid_w$} {:<name_w$} {:<8} {}", d.uid, d.name, d.channels, flags);
    }
}

fn col_width<'a>(values: impl Iterator<Item = &'a str>, min: usize) -> usize {
    values.map(|v| v.chars().count()).max().unwrap_or(0).max(min)
}

fn print_error(msg: &str) {
    // Use red if the terminal likely supports ANSI colors
    if std::env::var("TERM").is_ok() && std::env::var("NO_COLOR").is_err() {
        eprintln!("\x1b[31merror:\x1b[0m {msg}");
    } else {
        eprintln!("error: {msg}");
    }
}

/// Render a single status line for the monitor.
///
/// `tty` controls ANSI colour of the active dot; everything else is plain text.
fn format_activity_line(info: &ActivityInfo, tty: bool) -> String {
    let (dot, io_str) = if info.active {
        (if tty { "\x1b[32m●\x1b[0m" } else { "●" }, "on ")
    } else if tty {
        ("\x1b[90m○\x1b[0m", "off")
    } else {
        ("○", "off")
    };

    let peak = if info.active || info.peak_db > -96.0 {
        format!("{:>5.1} dB", info.peak_db)
    } else {
        "--.- dB".to_string()
    };

    format!(
        "[spaghetti] active: {dot}  io: {io}   frames/s: {fps:<6}  peak: {peak}   total: {total}",
        io = io_str,
        fps = info.frames_per_second,
        total = info.total_frames,
    )
}

fn run_monitor() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(monitor_loop())
}

async fn monitor_loop() -> Result<(), Box<dyn std::error::Error>> {
    // Blocking Unix socket, polled from a periodic task. Using the std socket
    // keeps the CLI deps minimal; each request/response is small so blocking
    // the single-threaded runtime for the round-trip is acceptable here.
    let tty = std::io::stdout().is_terminal();
    let mut stream = client::connect().map_err(|e| -> Box<dyn std::error::Error> {
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            "Daemon not running. Start it with: spaghettid".into()
        } else {
            format!("Failed to connect to daemon: {e}").into()
        }
    })?;

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stdout = std::io::stdout();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(());
            }
            _ = ticker.tick() => {
                let response = client::send_request(&mut stream, &Request::GetActivity)?;
                match response {
                    Response::Activity(info) => {
                        let line = format_activity_line(&info, tty);
                        // \r + erase-to-end-of-line, then redraw without newline.
                        write!(stdout, "\r\x1b[K{line}")?;
                        stdout.flush()?;
                    }
                    Response::Error { message } => {
                        println!();
                        print_error(&message);
                        std::process::exit(1);
                    }
                    _ => {
                        println!();
                        print_error("unexpected response from daemon");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

fn run_command(cmd: Command) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = client::connect().map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            "Daemon not running. Start it with: spaghetti daemon start".to_string()
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
                    println!("Daemon:  {state}");
                    println!("Sources: {}", s.source_count);
                    println!("Routes:  {}", s.route_count);
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

        Command::Tap { target, mute } => {
            let mode = if mute { TapMode::MuteSourceWhenTapped } else { TapMode::Mirror };

            // Try parsing as PID first; fall back to name lookup.
            let pids = if let Ok(pid) = target.parse::<u32>() {
                vec![pid]
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
                    .find(|a| a.name.to_lowercase() == needle)
                    .or_else(|| apps.iter().find(|a| a.name.to_lowercase().contains(&needle)));

                match matched {
                    Some(app) => app.pids.clone(),
                    None => {
                        print_error(&format!("no app matching '{target}' found"));
                        std::process::exit(1);
                    }
                }
            };

            let source = SourceSpec::Tap { pids, mode };
            create_source(&mut stream, source)?;
        }

        Command::Receive { url, srt, webrtc } => {
            let source = if url == "webrtc" {
                if srt.is_set() {
                    print_error("--latency and --passphrase apply only to srt:// sources");
                    std::process::exit(1);
                }
                SourceSpec::WebRtc(webrtc.peer(PeerRole::Answer).unwrap_or_else(|e| {
                    print_error(&e);
                    std::process::exit(1);
                }))
            } else if webrtc.is_set() {
                print_error("--stun and --turn apply only to webrtc sources");
                std::process::exit(1);
            } else {
                SourceSpec::Srt(srt.socket(&url).unwrap_or_else(|e| {
                    print_error(&e);
                    std::process::exit(1);
                }))
            };
            create_source(&mut stream, source)?;
        }

        Command::Unsource { source_id } => {
            let response =
                client::send_request(&mut stream, &Request::DestroySource { source_id })?;
            match response {
                Response::Ok => println!("Source {source_id} removed."),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Sources => {
            let response = client::send_request(&mut stream, &Request::ListSources)?;
            match response {
                Response::Sources(sources) => print_sources(&sources),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Route {
            source_id,
            destination,
            volume,
            srt,
            webrtc,
        } => {
            if !(0.0..=2.0).contains(&volume) {
                print_error("volume must be between 0.0 and 2.0");
                std::process::exit(1);
            }
            if srt.is_set() && !destination.starts_with("srt://") {
                print_error("--latency and --passphrase apply only to srt:// destinations");
                std::process::exit(1);
            }
            if webrtc.is_set() && destination != "webrtc" {
                print_error("--stun and --turn apply only to webrtc destinations");
                std::process::exit(1);
            }
            let destination = if destination.starts_with("srt://") {
                Destination::Srt(srt.socket(&destination).unwrap_or_else(|e| {
                    print_error(&e);
                    std::process::exit(1);
                }))
            } else if destination == "webrtc" {
                Destination::WebRtc(webrtc.peer(PeerRole::Offer).unwrap_or_else(|e| {
                    print_error(&e);
                    std::process::exit(1);
                }))
            } else {
                Destination::Device { uid: destination }
            };
            let link = match &destination {
                Destination::WebRtc(peer) => format!(" link={}", peer.link),
                _ => String::new(),
            };
            let response = client::send_request(
                &mut stream,
                &Request::CreateRoute {
                    source_id,
                    destination,
                    volume,
                },
            )?;
            match response {
                Response::RouteCreated { route_id } => {
                    println!("Route created: id={route_id}{link}");
                }
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Sdp { peer, id } => {
            let deadline = Instant::now() + LOCAL_DESCRIPTION_WAIT;
            loop {
                let (_, description) = find_peer(&mut stream, peer, id)?;
                if let Some(sdp) = description {
                    print!("{sdp}");
                    break;
                }
                if Instant::now() >= deadline {
                    print_error(&format!("{} {id} has no local description yet", peer_noun(peer)));
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        Command::Accept { peer, id, file } => {
            let mut sdp = String::new();
            let read = if file == "-" {
                std::io::stdin().read_to_string(&mut sdp).map(|_| ())
            } else {
                std::fs::read_to_string(&file).map(|text| sdp = text)
            };
            if let Err(e) = read {
                print_error(&format!("cannot read {file}: {e}"));
                std::process::exit(1);
            }
            let (generation, _) = find_peer(&mut stream, peer, id)?;
            let peer = match peer {
                PeerKind::Source => PeerRef::Source(id),
                PeerKind::Route => PeerRef::Route(id),
            };
            let request = Request::SetRemoteDescription {
                peer,
                generation,
                sdp,
            };
            match client::send_request(&mut stream, &request)? {
                Response::Ok => println!("Remote description applied."),
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

        Command::SystemDevices => {
            let response =
                client::send_request(&mut stream, &Request::ListSystemDevices)?;
            match response {
                Response::SystemDevices(devices) => print_system_devices(&devices),
                Response::Error { message } => {
                    print_error(&message);
                    std::process::exit(1);
                }
                _ => print_error("unexpected response from daemon"),
            }
        }

        Command::Monitor => {
            // Monitor owns its own connection lifecycle and loops until Ctrl-C,
            // so hand off to a dedicated function rather than fitting it into
            // the single-request shape of the other commands.
            drop(stream);
            return run_monitor();
        }

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

        Command::Daemon { .. } => unreachable!("daemon commands handled in main"),
    }

    Ok(())
}

fn peer_noun(peer: PeerKind) -> &'static str {
    match peer {
        PeerKind::Source => "source",
        PeerKind::Route => "route",
    }
}

/// Claude: a WebRTC source's or route's generation and local description, or
/// an exit with the reason it is not one.
fn find_peer(
    stream: &mut std::os::unix::net::UnixStream,
    peer: PeerKind,
    id: u32,
) -> Result<(u32, Option<String>), Box<dyn std::error::Error>> {
    let found = match (peer, client::send_request(stream, &request_listing(peer))?) {
        (PeerKind::Source, Response::Sources(sources)) => {
            sources.into_iter().find(|s| s.source_id == id).map(|s| match s.spec {
                SourceSpec::WebRtc(spec) => Some((spec.generation, s.local_description)),
                _ => None,
            })
        }
        (PeerKind::Route, Response::Routes(routes)) => {
            routes.into_iter().find(|r| r.route_id == id).map(|r| match r.destination {
                Destination::WebRtc(spec) => Some((spec.generation, r.local_description)),
                _ => None,
            })
        }
        (_, Response::Error { message }) => {
            print_error(&message);
            std::process::exit(1);
        }
        _ => return Err("unexpected response from daemon".into()),
    };
    let noun = peer_noun(peer);
    match found {
        Some(Some(found)) => Ok(found),
        Some(None) => {
            print_error(&format!("{noun} {id} is not a WebRTC peer"));
            std::process::exit(1);
        }
        None => {
            print_error(&format!("{noun} {id} not found"));
            std::process::exit(1);
        }
    }
}

fn request_listing(peer: PeerKind) -> Request {
    match peer {
        PeerKind::Source => Request::ListSources,
        PeerKind::Route => Request::ListRoutes,
    }
}

fn create_source(
    stream: &mut std::os::unix::net::UnixStream,
    source: SourceSpec,
) -> Result<(), Box<dyn std::error::Error>> {
    let link = match &source {
        SourceSpec::WebRtc(peer) => format!(" link={}", peer.link),
        _ => String::new(),
    };
    match client::send_request(stream, &Request::CreateSource { source, tag: None })? {
        Response::SourceCreated { source_id } => println!("Source created: id={source_id}{link}"),
        Response::Error { message } => {
            print_error(&message);
            std::process::exit(1);
        }
        _ => print_error("unexpected response from daemon"),
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
        // Daemon-management commands don't talk to the daemon socket; they
        // shell out to launchctl / tail. Handle them before `run_command` so
        // they work even when the daemon is not running.
        Some(Command::Daemon { action }) => {
            let result = match action {
                DaemonAction::Install => daemon_mgmt::install(),
                DaemonAction::Uninstall => daemon_mgmt::uninstall(),
                DaemonAction::Start => daemon_mgmt::start(),
                DaemonAction::Stop => daemon_mgmt::stop(),
                DaemonAction::Restart => daemon_mgmt::restart(),
                DaemonAction::Status => daemon_mgmt::status(),
                DaemonAction::Logs { follow, lines } => daemon_mgmt::logs(follow, lines),
            };
            if let Err(e) = result {
                print_error(&e);
                std::process::exit(1);
            }
        }
        Some(cmd) => {
            if let Err(e) = run_command(cmd) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_server_takes_its_credential_whole_and_never_repeats_it() {
        let cli = Cli::parse_from([
            "spaghetti",
            "route",
            "4",
            "webrtc",
            "--turn",
            "turn.example.net:3478,1767225600:link,se,cr,et",
            "--turn",
            "192.0.2.1:3478,user,pass",
        ]);
        let Some(Command::Route { webrtc, .. }) = cli.command else {
            panic!("not a route");
        };
        let peer = webrtc.peer(PeerRole::Offer).unwrap();
        assert_eq!(peer.turn.len(), 2);
        assert_eq!(peer.turn[0].address, "turn.example.net:3478");
        assert_eq!(peer.turn[0].username, "1767225600:link");
        assert_eq!(peer.turn[0].credential, "se,cr,et");
        assert_eq!(peer.turn[1].credential, "pass");

        for value in ["turn.example.net:3478,hunter2", "turn.example.net:3478,,hunter2"] {
            let error = turn_server(value).unwrap_err();
            assert!(!error.contains("hunter2"), "{error}");
        }
    }
}
