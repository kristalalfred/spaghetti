use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;

use crate::state::{AppState, ConnectionStatus};

pub const DASHBOARD_HELP: &[(&str, &str)] = &[
    ("(no screen keys)", "read-only overview"),
];

const DB_FLOOR: f32 = -96.0;

/// Format an integer with thousands separators (e.g. 1_234_567 → "1,234,567").
fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Map peak_db to 0–100 bar percentage. Clamps to [0, 100].
fn db_to_pct(db: f32) -> u16 {
    let clamped = db.clamp(DB_FLOOR, 0.0);
    let ratio = (clamped - DB_FLOOR) / (-DB_FLOOR);
    (ratio * 100.0).round() as u16
}

/// Green below -12 dB, amber -12..-3, red -3..0.
fn bar_color(db: f32) -> Color {
    if db >= -3.0 {
        Color::Red
    } else if db >= -12.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub fn draw_dashboard(frame: &mut Frame, area: Rect, state: &AppState) {
    let offline = state.connection_status == ConnectionStatus::Disconnected;

    // When offline, reserve the first row for a banner.
    let body = if offline {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " OFFLINE — daemon not running, reconnecting\u{2026}",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            )),
            chunks[0],
        );
        chunks[1]
    } else {
        area
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // daemon card
            Constraint::Length(5), // activity meter
            Constraint::Min(0),    // spacer
            Constraint::Length(1), // hints
        ])
        .split(body);

    draw_daemon_card(frame, chunks[0], state, offline);
    draw_activity_meter(frame, chunks[1], state, offline);
    draw_hints(frame, chunks[3]);
}

fn placeholder(offline: bool) -> String {
    if offline { "—".into() } else { "…".into() }
}

fn draw_daemon_card(frame: &mut Frame, area: Rect, state: &AppState, offline: bool) {
    let have_status = state.daemon_status.is_some();

    let tap_count = state
        .daemon_status
        .as_ref()
        .map(|s| s.tap_count.to_string())
        .unwrap_or_else(|| placeholder(offline));

    let route_count = state
        .daemon_status
        .as_ref()
        .map(|s| s.route_count.to_string())
        .unwrap_or_else(|| placeholder(offline));

    let device_count = match &state.devices {
        _ if offline => placeholder(offline),
        None => "…".to_string(),
        Some(devs) => devs.len().to_string(),
    };

    let sys_count = if offline || (!have_status && state.system_devices.is_empty()) {
        placeholder(offline)
    } else {
        state.system_devices.len().to_string()
    };

    let kv = Style::default().fg(Color::DarkGray);
    let val = Style::default().add_modifier(Modifier::BOLD);
    let sep = Span::raw("   ");

    let line1 = Line::from(vec![
        Span::styled("Taps ", kv),
        Span::styled(tap_count, val),
        sep.clone(),
        Span::styled("Routes ", kv),
        Span::styled(route_count, val),
    ]);

    let line2 = Line::from(vec![
        Span::styled("Devices ", kv),
        Span::styled(device_count, val),
        sep,
        Span::styled("System devices ", kv),
        Span::styled(sys_count, val),
    ]);

    let card = Paragraph::new(vec![Line::from(""), line1, line2])
        .block(Block::default().borders(Borders::ALL).title(" Daemon "));

    frame.render_widget(card, area);
}

fn draw_activity_meter(frame: &mut Frame, area: Rect, state: &AppState, offline: bool) {
    let block = Block::default().borders(Borders::ALL).title(" Activity ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // gauge
            Constraint::Length(1), // readout
            Constraint::Min(0),
        ])
        .split(inner);

    if offline {
        frame.render_widget(Paragraph::new("offline"), inner_chunks[0]);
        return;
    }

    let Some(ref activity) = state.activity else {
        frame.render_widget(Paragraph::new("…"), inner_chunks[0]);
        return;
    };

    let pct = db_to_pct(activity.peak_db);
    let color = bar_color(activity.peak_db);

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(color).bg(Color::DarkGray))
        .percent(pct)
        .label("");

    frame.render_widget(gauge, inner_chunks[0]);

    let state_label = if activity.active { "ACTIVE" } else { "IDLE" };
    let readout = format!(
        "{:.1} dB   {} fps   total: {} frames   [{}]",
        activity.peak_db,
        activity.frames_per_second,
        fmt_thousands(activity.total_frames),
        state_label,
    );

    let readout_style = if activity.active {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    frame.render_widget(
        Paragraph::new(Span::styled(readout, readout_style)),
        inner_chunks[1],
    );
}

fn draw_hints(frame: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let spans = vec![
        Span::styled("[1]", key),
        Span::styled(" Dashboard  ", dim),
        Span::styled("[2]", key),
        Span::styled(" Apps  ", dim),
        Span::styled("[3]", key),
        Span::styled(" Routes  ", dim),
        Span::styled("[4]", key),
        Span::styled(" Devices  ", dim),
        Span::styled("[?]", key),
        Span::styled(" Help  ", dim),
        Span::styled("[q]", key),
        Span::styled(" Quit", dim),
    ];

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
