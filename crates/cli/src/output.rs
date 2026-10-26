use spaghetti_common::ipc_protocol::{
    ActivityInfo, AppInfo, DaemonStatus, Destination, DeviceInfo, FlowState, FlowStatus, PeerRole,
    RouteInfo, SourceInfo, SourceSpec, SrtMode, SrtSocket, SystemDeviceInfo,
};
use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn print_logo() {
    print!("\n{}", include_str!("logo.ans").replace("{version}", VERSION));
}

pub fn print_error(msg: &str) {
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        eprintln!("\x1b[31merror:\x1b[0m {msg}");
    } else {
        eprintln!("error: {msg}");
    }
}

pub fn print_status(status: &DaemonStatus) {
    let state = if status.running { "running" } else { "stopped" };
    println!("Daemon:  {state}");
    println!("Sources: {}", status.source_count);
    println!("Routes:  {}", status.route_count);
}

pub fn print_apps(apps: &[AppInfo]) {
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

pub fn print_sources(sources: &[SourceInfo]) {
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

pub fn print_routes(routes: &[RouteInfo]) {
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

pub fn print_system_devices(devices: &[SystemDeviceInfo]) {
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

pub fn print_devices(devices: &[DeviceInfo]) {
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

/// Render a single status line for the monitor.
///
/// `tty` controls ANSI colour of the active dot; everything else is plain text.
pub fn format_activity_line(info: &ActivityInfo, tty: bool) -> String {
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
