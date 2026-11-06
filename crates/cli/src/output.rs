use serde::Serialize;
use spaghetti_common::ipc_protocol::{
    ActivityInfo, AirPlayReceiverInfo, AirPlaySupport, AppInfo, DaemonStatus, Destination,
    DeviceInfo, FlowState, FlowStatus, PeerRole, RouteInfo, SourceInfo, SourceSpec, SrtMode,
    SrtSocket, WebRtcPeer,
};
use spaghetti_engine::SystemDevice;
use std::fmt::Display;
use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Text,
    Json,
}

impl Output {
    pub fn emit(self, json: &impl Serialize, text: impl FnOnce()) -> serde_json::Result<()> {
        match self {
            Output::Text => text(),
            Output::Json => println!("{}", serde_json::to_string(json)?),
        }
        Ok(())
    }

    pub fn note(self, message: impl Display) {
        if self == Output::Text {
            println!("{message}");
        }
    }
}

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
    for (source, kind) in sources.iter().zip(&kinds) {
        println!(
            "{:<6} {:<kind_w$} {:<label_w$} {}",
            source.source_id,
            kind,
            source.label,
            state_label(matches!(source.spec, SourceSpec::WebRtc(_)), &source.status)
        );
    }
}

fn destination_label(destination: &Destination) -> String {
    match destination {
        Destination::Device {
            uid,
            delay_ms,
            latency_offset_ms,
        } => scheduled_label(uid, *delay_ms, *latency_offset_ms),
        Destination::AirPlay {
            receiver,
            delay_ms,
            latency_offset_ms,
        } => scheduled_label(
            &format!("airplay://{receiver}"),
            Some(*delay_ms),
            *latency_offset_ms,
        ),
        Destination::Srt(socket) => srt_label(socket),
        Destination::WebRtc(peer) => format!("webrtc {} ({})", peer.link, role_label(peer.role)),
    }
}

fn scheduled_label(target: &str, delay_ms: Option<u32>, latency_offset_ms: Option<i32>) -> String {
    use std::fmt::Write;
    let mut label = target.to_string();
    if let Some(delay) = delay_ms {
        write!(label, " +{delay} ms").unwrap();
    }
    if let Some(offset) = latency_offset_ms {
        write!(label, " (latency offset {offset:+} ms)").unwrap();
    }
    label
}

pub fn print_routes(routes: &[RouteInfo]) {
    if routes.is_empty() {
        println!("No active routes.");
        return;
    }
    let destinations: Vec<String> = routes
        .iter()
        .map(|route| destination_label(&route.destination))
        .collect();
    let dest_w = col_width(destinations.iter().map(String::as_str), 30);
    println!(
        "{:<6} {:<7} {:<dest_w$} {:<7} {:<8} STATE",
        "ID", "SOURCE", "DESTINATION", "VOLUME", "RECEIVER"
    );
    println!("{}", "-".repeat(dest_w + 43));
    for (route, destination) in routes.iter().zip(&destinations) {
        let receiver = route.receiver_volume.map_or("-".into(), |volume| format!("{volume:.2}"));
        println!(
            "{:<6} {:<7} {:<dest_w$} {:<7.2} {:<8} {}",
            route.route_id,
            route.source_id,
            destination,
            route.volume,
            receiver,
            state_label(
                matches!(route.destination, Destination::WebRtc(_)),
                &route.status
            )
        );
    }
}

pub fn print_system_devices(devices: &[SystemDevice]) {
    print!("{}", format_system_devices(devices));
}

fn format_system_devices(devices: &[SystemDevice]) -> String {
    use std::fmt::Write;
    if devices.is_empty() {
        return "No audio devices found.\n".into();
    }
    let uid_w = col_width(devices.iter().map(|d| d.uid.as_str()), 40);
    let name_w = col_width(devices.iter().map(|d| d.name.as_str()), 24);
    let channels = |count: u32| match count {
        0 => "-".to_string(),
        count => count.to_string(),
    };
    let mut text = format!(
        "{:<uid_w$} {:<name_w$} {:>3} {:>3}\n",
        "UID", "NAME", "IN", "OUT"
    );
    text.push_str(&"-".repeat(uid_w + name_w + 8));
    text.push('\n');
    for d in devices {
        let _ = writeln!(
            text,
            "{:<uid_w$} {:<name_w$} {:>3} {:>3}",
            d.uid,
            d.name,
            channels(d.input_channels),
            channels(d.output_channels)
        );
    }
    text
}

pub fn print_airplay_receivers(receivers: &[AirPlayReceiverInfo]) {
    print!("{}", format_airplay_receivers(receivers));
}

fn format_airplay_receivers(receivers: &[AirPlayReceiverInfo]) -> String {
    use std::fmt::Write;
    if receivers.is_empty() {
        return "No AirPlay receivers found.\n".into();
    }
    let name_w = col_width(receivers.iter().map(|r| r.name.as_str()), 24);
    let mut text = format!("{:<17} {:<name_w$} {:<20} SUPPORT\n", "ID", "NAME", "MODEL");
    for receiver in receivers {
        let support = match &receiver.support {
            AirPlaySupport::Supported => "supported",
            AirPlaySupport::Unsupported { reason } => reason,
        };
        writeln!(
            text,
            "{:<17} {:<name_w$} {:<20} {}",
            receiver.id,
            receiver.name,
            receiver.model.as_deref().unwrap_or("-"),
            support
        )
        .unwrap();
    }
    text
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

#[derive(Serialize)]
pub struct SourceJson<'a> {
    source_id: u32,
    label: &'a str,
    tag: Option<&'a str>,
    #[serde(flatten)]
    spec: EndpointJson<'a>,
    status: FlowJson<'a>,
}

impl<'a> From<&'a SourceInfo> for SourceJson<'a> {
    fn from(source: &'a SourceInfo) -> Self {
        SourceJson {
            source_id: source.source_id,
            label: &source.label,
            tag: source.tag.as_deref(),
            spec: match &source.spec {
                SourceSpec::Tap { pids, mode } => EndpointJson::Tap {
                    pids,
                    mode: mode.label(),
                },
                SourceSpec::Srt(socket) => EndpointJson::srt(socket),
                SourceSpec::WebRtc(peer) => EndpointJson::webrtc(peer),
            },
            status: FlowJson::from(&source.status),
        }
    }
}

#[derive(Serialize)]
pub struct RouteJson<'a> {
    route_id: u32,
    source_id: u32,
    destination: EndpointJson<'a>,
    volume: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver_volume: Option<f32>,
    status: FlowJson<'a>,
}

impl<'a> From<&'a RouteInfo> for RouteJson<'a> {
    fn from(route: &'a RouteInfo) -> Self {
        RouteJson {
            route_id: route.route_id,
            source_id: route.source_id,
            destination: match &route.destination {
                Destination::Device {
                    uid,
                    delay_ms,
                    latency_offset_ms,
                } => EndpointJson::Device {
                    uid,
                    delay_ms: *delay_ms,
                    latency_offset_ms: *latency_offset_ms,
                },
                Destination::AirPlay {
                    receiver,
                    delay_ms,
                    latency_offset_ms,
                } => EndpointJson::AirPlay {
                    receiver,
                    delay_ms: *delay_ms,
                    latency_offset_ms: *latency_offset_ms,
                },
                Destination::Srt(socket) => EndpointJson::srt(socket),
                Destination::WebRtc(peer) => EndpointJson::webrtc(peer),
            },
            volume: route.volume,
            receiver_volume: route.receiver_volume,
            status: FlowJson::from(&route.status),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EndpointJson<'a> {
    Tap {
        pids: &'a [u32],
        mode: &'static str,
    },
    Device {
        uid: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        latency_offset_ms: Option<i32>,
    },
    #[serde(rename = "airplay")]
    AirPlay {
        receiver: &'a str,
        delay_ms: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        latency_offset_ms: Option<i32>,
    },
    Srt {
        url: String,
        latency_ms: u32,
        encrypted: bool,
    },
    #[serde(rename = "webrtc")]
    WebRtc {
        link: &'a str,
        generation: u32,
        role: &'static str,
    },
}

impl<'a> EndpointJson<'a> {
    fn srt(socket: &SrtSocket) -> Self {
        EndpointJson::Srt {
            url: srt_label(socket),
            latency_ms: socket.latency_ms,
            encrypted: socket.passphrase.is_some(),
        }
    }

    fn webrtc(peer: &'a WebRtcPeer) -> Self {
        EndpointJson::WebRtc {
            link: &peer.link,
            generation: peer.generation,
            role: role_label(peer.role),
        }
    }
}

#[derive(Serialize)]
struct FlowJson<'a> {
    state: &'static str,
    bytes: u64,
    peer: Option<&'a str>,
    relayed: bool,
    packets_lost: u64,
    packets_retransmitted: u64,
}

impl<'a> From<&'a FlowStatus> for FlowJson<'a> {
    fn from(status: &'a FlowStatus) -> Self {
        FlowJson {
            state: flow_label(status.state),
            bytes: status.bytes,
            peer: status.peer.as_deref(),
            relayed: status.relayed,
            packets_lost: status.packets_lost,
            packets_retransmitted: status.packets_retransmitted,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use spaghetti_common::ipc_protocol::{TapMode, TurnServer};

    fn flowing(peer: Option<&str>) -> FlowStatus {
        FlowStatus {
            state: FlowState::Flowing,
            bytes: 4096,
            peer: peer.map(String::from),
            relayed: false,
            packets_lost: 1,
            packets_retransmitted: 2,
        }
    }

    const FLOWING: &str = r#"{"state":"flowing","bytes":4096,"peer":null,"relayed":false,"packets_lost":1,"packets_retransmitted":2}"#;

    fn source_json(spec: SourceSpec, tag: Option<&str>) -> String {
        let source = SourceInfo {
            source_id: 3,
            spec,
            tag: tag.map(String::from),
            label: "Music".into(),
            status: flowing(None),
            local_description: None,
        };
        serde_json::to_string(&SourceJson::from(&source)).unwrap()
    }

    fn route_json(destination: Destination) -> String {
        route_json_with(destination, None)
    }

    fn route_json_with(destination: Destination, receiver_volume: Option<f32>) -> String {
        let route = RouteInfo {
            route_id: 5,
            source_id: 3,
            destination,
            volume: 0.8,
            status: flowing(None),
            receiver_volume,
            local_description: Some("v=0\r\n".into()),
        };
        serde_json::to_string(&RouteJson::from(&route)).unwrap()
    }

    #[test]
    fn a_source_lists_its_kind_beside_its_id() {
        assert_eq!(
            source_json(
                SourceSpec::Tap {
                    pids: vec![501, 502],
                    mode: TapMode::MuteSourceWhenTapped,
                },
                Some("music"),
            ),
            format!(
                r#"{{"source_id":3,"label":"Music","tag":"music","kind":"tap","pids":[501,502],"mode":"muted","status":{FLOWING}}}"#
            ),
        );
    }

    #[test]
    fn an_srt_endpoint_shows_its_url_but_not_its_passphrase() {
        let json = source_json(
            SourceSpec::Srt(SrtSocket {
                mode: SrtMode::Call {
                    host: "studio.local".into(),
                    port: 9000,
                },
                latency_ms: 200,
                passphrase: Some("shared secret 1".into()),
                key_size: None,
            }),
            None,
        );
        assert_eq!(
            json,
            format!(
                r#"{{"source_id":3,"label":"Music","tag":null,"kind":"srt","url":"srt://studio.local:9000","latency_ms":200,"encrypted":true,"status":{FLOWING}}}"#
            ),
        );
    }

    #[test]
    fn a_route_nests_its_destination() {
        assert_eq!(
            route_json(Destination::Device {
                uid: "SpaghettiDevice_UID".into(),
                delay_ms: None,
                latency_offset_ms: None
            }),
            format!(
                r#"{{"route_id":5,"source_id":3,"destination":{{"kind":"device","uid":"SpaghettiDevice_UID"}},"volume":0.8,"status":{FLOWING}}}"#
            ),
        );
    }

    #[test]
    fn a_webrtc_endpoint_shows_its_link_but_not_its_turn_credential() {
        let json = route_json(Destination::WebRtc(WebRtcPeer {
            link: "link-7".into(),
            generation: 2,
            role: PeerRole::Offer,
            stun: vec!["stun.l.google.com:19302".into()],
            turn: vec![TurnServer {
                address: "turn.example.net:3478".into(),
                username: "1767225600:link-7".into(),
                credential: "c2VjcmV0".into(),
            }],
        }));
        assert_eq!(
            json,
            format!(
                r#"{{"route_id":5,"source_id":3,"destination":{{"kind":"webrtc","link":"link-7","generation":2,"role":"offer"}},"volume":0.8,"status":{FLOWING}}}"#
            ),
        );
    }
    #[test]
    fn airplay_routes_show_the_receiver_and_required_delay_in_text_and_json() {
        let destination = Destination::AirPlay {
            receiver: "C4:38:75:04:5E:80".into(),
            delay_ms: 2500,
            latency_offset_ms: None,
        };
        assert_eq!(
            destination_label(&destination),
            "airplay://C4:38:75:04:5E:80 +2500 ms"
        );
        assert_eq!(
            route_json(destination.clone()),
            format!(
                r#"{{"route_id":5,"source_id":3,"destination":{{"kind":"airplay","receiver":"C4:38:75:04:5E:80","delay_ms":2500}},"volume":0.8,"status":{FLOWING}}}"#
            )
        );
        assert_eq!(
            route_json_with(destination, Some(0.4)),
            format!(
                r#"{{"route_id":5,"source_id":3,"destination":{{"kind":"airplay","receiver":"C4:38:75:04:5E:80","delay_ms":2500}},"volume":0.8,"receiver_volume":0.4,"status":{FLOWING}}}"#
            )
        );
    }

    #[test]
    fn latency_offsets_are_visible_in_route_text_and_json() {
        for (kind, endpoint, target) in [
            ("Device", "uid", "Speakers"),
            ("AirPlay", "receiver", "C4:38:75:04:5E:80"),
        ] {
            for offset in [-47, 83, 0] {
                let destination: Destination = serde_json::from_value(serde_json::json!({
                    kind: { endpoint: target, "delay_ms": 2500, "latency_offset_ms": offset }
                }))
                .unwrap();
                let text = destination_label(&destination);
                assert!(
                    text.contains(&format!("latency offset {offset:+} ms")),
                    "{text}"
                );
                let json: serde_json::Value =
                    serde_json::from_str(&route_json(destination)).unwrap();
                assert_eq!(json["destination"]["latency_offset_ms"], offset);
            }
        }
    }

    #[test]
    fn device_listings_show_input_and_output_channels() {
        let device = |uid: &str, input_channels, output_channels| SystemDevice {
            uid: uid.into(),
            name: uid.to_lowercase(),
            input_channels,
            output_channels,
        };
        let text = format_system_devices(&[
            device("Microphone", 1, 0),
            device("Speakers", 0, 2),
            device("Interface", 8, 4),
        ]);
        let rows: Vec<Vec<&str>> = text
            .lines()
            .map(|line| line.split_whitespace().collect())
            .collect();
        assert_eq!(rows[0], ["UID", "NAME", "IN", "OUT"]);
        assert_eq!(
            rows[2..],
            [
                ["Microphone", "microphone", "1", "-"],
                ["Speakers", "speakers", "-", "2"],
                ["Interface", "interface", "8", "4"],
            ]
        );
    }

    #[test]
    fn receiver_listings_show_support_reasons_and_models() {
        let receivers = vec![
            AirPlayReceiverInfo {
                id: "C4:38:75:04:5E:80".into(),
                name: "Kitchen".into(),
                model: Some("Speaker".into()),
                support: AirPlaySupport::Supported,
            },
            AirPlayReceiverInfo {
                id: "C4:38:75:04:5E:81".into(),
                name: "Living room".into(),
                model: None,
                support: AirPlaySupport::Unsupported {
                    reason: "pairing required".into(),
                },
            },
        ];
        let text = format_airplay_receivers(&receivers);
        for expected in [
            "Kitchen",
            "Speaker",
            "supported",
            "C4:38:75:04:5E:81",
            "Living room",
            "pairing required",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        let json = serde_json::to_value(&receivers).unwrap();
        assert_eq!(json[0]["support"], "Supported");
        assert_eq!(
            json[1]["support"]["Unsupported"]["reason"],
            "pairing required"
        );
        assert_eq!(
            format_airplay_receivers(&[]),
            "No AirPlay receivers found.\n"
        );
    }
}
