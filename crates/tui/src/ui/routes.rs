use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::state::{AppState, CreateField, RoutesMode};

pub const ROUTES_HELP: &[(&str, &str)] = &[
    ("j / k", "navigate list"),
    ("Enter", "enter volume-edit mode for selected route"),
    ("← / → (or h/l)", "adjust volume ±0.05"),
    ("H / L", "adjust volume ±0.25"),
    ("0 / 1", "mute / unity volume"),
    ("Enter / Esc", "exit volume-edit mode"),
    ("c", "open create-route form"),
    ("d d", "destroy selected route (confirm)"),
];

pub fn draw_routes(frame: &mut Frame, area: Rect, state: &AppState) {
    match &state.routes_screen.mode {
        RoutesMode::Creating => {
            draw_route_list(frame, area, state);
            draw_create_form(frame, area, state);
        }
        _ => {
            draw_route_list(frame, area, state);
        }
    }
    draw_hints(frame, area, state);
}

fn draw_route_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" Routes ", Style::default().add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(ref routes) = state.routes else {
        frame.render_widget(Paragraph::new("…"), inner);
        return;
    };

    if routes.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "(no routes)",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let header = Line::from(vec![
        Span::styled(format!("{:<6}", "ID"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<6}", "Tap"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<18}", "App"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<20}", "Device"), Style::default().fg(Color::DarkGray)),
        Span::styled("Volume", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let cursor = state
        .routes_screen
        .cursor
        .min(routes.len().saturating_sub(1));

    let armed_id = state.routes_screen.armed_destroy.map(|(id, _)| id);

    let items: Vec<ListItem> = routes
        .iter()
        .enumerate()
        .map(|(i, route)| {
            let app_name = resolve_app_name(route.tap_id, state);
            let device_name = resolve_device_name(&route.device_uid, state);

            let editing = matches!(
                state.routes_screen.mode,
                RoutesMode::Editing { route_id } if route_id == route.route_id
            );
            let armed = armed_id == Some(route.route_id);

            let vol_bar = render_volume_bar(route.volume);
            let vol_label = format!("{:.0}%", route.volume * 100.0);

            let text = if armed {
                format!(
                    "{:<6}{:<6}{:<18}{:<20}  [press d again to destroy]",
                    route.route_id, route.tap_id, app_name, device_name,
                )
            } else {
                format!(
                    "{:<6}{:<6}{:<18}{:<20}{}  {}",
                    route.route_id, route.tap_id, app_name, device_name, vol_bar, vol_label,
                )
            };

            let style = if armed {
                Style::default().fg(Color::Yellow)
            } else if editing {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if i == cursor {
                Style::default()
                    .bg(Color::White)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(text).style(style)
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(cursor));

    frame.render_stateful_widget(List::new(items), chunks[1], &mut list_state);
}

/// Resolve a tap_id to its app_name from state.taps.
fn resolve_app_name(tap_id: u32, state: &AppState) -> String {
    state
        .taps
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|t| t.tap_id == tap_id)
        .map(|t| t.app_name.clone())
        .unwrap_or_else(|| "—".to_string())
}

/// Resolve a device UID to its human-readable name.
/// Prefers system_devices (authoritative), falls back to daemon-managed devices,
/// then the raw UID as last resort.
pub fn resolve_device_name(uid: &str, state: &AppState) -> String {
    if let Some(dev) = state.system_devices.iter().find(|d| d.uid == uid) {
        return dev.name.clone();
    }
    if let Some(dev) = state
        .devices
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|d| d.uid == uid)
    {
        return dev.name.clone();
    }
    uid.to_string()
}

/// Render a 20-cell unicode volume bar for range [0.0, 2.0].
/// Each cell = 0.1 of volume. The midpoint (1.0 = cell 10) is marked with a
/// colour change: cells 0–9 are green, cells 10–19 are amber.
/// Full cell = '█', half cell = '▌', empty = ' '.
pub fn render_volume_bar(volume: f32) -> String {
    const CELLS: usize = 20;
    const CELL_SIZE: f32 = 0.1; // each cell covers 0.1 volume

    let vol = volume.clamp(0.0, 2.0);
    let filled_f = vol / CELL_SIZE; // e.g. 1.5 → 15.0

    let mut bar = String::with_capacity(CELLS + 2);
    bar.push('[');
    for i in 0..CELLS {
        let cell_start = i as f32;
        let remainder = filled_f - cell_start;
        if remainder >= 1.0 {
            bar.push('█');
        } else if remainder >= 0.5 {
            bar.push('▌');
        } else {
            bar.push(' ');
        }
    }
    bar.push(']');
    bar
}

fn draw_create_form(frame: &mut Frame, area: Rect, state: &AppState) {
    // Centered popup: 60% wide, 14 rows tall.
    let popup_area = centered_popup(area, 60, 14);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Create Route ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let form = &state.routes_screen.create_form;

    let taps = state.taps.as_deref().unwrap_or(&[]);
    let output_devices: Vec<_> = state
        .system_devices
        .iter()
        .filter(|d| d.is_output)
        .collect();

    // No taps yet — show informational message.
    if taps.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "No taps yet — go to Apps screen to create one.",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "[Esc] Cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(msg, inner);
        return;
    }

    // Layout: tap picker, device picker, volume row, footer.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Tap label
            Constraint::Min(3),    // Tap list
            Constraint::Length(1), // Device label
            Constraint::Min(3),    // Device list
            Constraint::Length(1), // Volume label + bar
            Constraint::Length(1), // spacer
            Constraint::Length(1), // footer
        ])
        .split(inner);

    // --- Tap picker ---
    let tap_label_style = if form.active_field == CreateField::Tap {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Span::styled("Tap", tap_label_style)),
        chunks[0],
    );

    let tap_items: Vec<ListItem> = taps
        .iter()
        .enumerate()
        .map(|(i, tap)| {
            let selected = i == form.selected_tap_idx;
            let active = form.active_field == CreateField::Tap;
            let text = format!("{:<6}{}", tap.tap_id, tap.app_name);
            let style = if selected && active {
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let mut tap_state = ListState::default();
    tap_state.select(Some(form.selected_tap_idx.min(taps.len().saturating_sub(1))));
    frame.render_stateful_widget(List::new(tap_items), chunks[1], &mut tap_state);

    // --- Device picker ---
    let dev_label_style = if form.active_field == CreateField::Device {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Span::styled("Device", dev_label_style)),
        chunks[2],
    );

    if output_devices.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "(no output devices)",
                Style::default().fg(Color::DarkGray),
            )),
            chunks[3],
        );
    } else {
        let dev_items: Vec<ListItem> = output_devices
            .iter()
            .enumerate()
            .map(|(i, dev)| {
                let selected = i == form.selected_device_idx;
                let active = form.active_field == CreateField::Device;
                let style = if selected && active {
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else if selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                ListItem::new(dev.name.clone()).style(style)
            })
            .collect();

        let mut dev_state = ListState::default();
        dev_state.select(Some(
            form.selected_device_idx
                .min(output_devices.len().saturating_sub(1)),
        ));
        frame.render_stateful_widget(List::new(dev_items), chunks[3], &mut dev_state);
    }

    // --- Volume row ---
    let vol_label_style = if form.active_field == CreateField::Volume {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let vol_bar = render_volume_bar(form.volume);
    let vol_pct = format!("{:.0}%", form.volume * 100.0);
    let vol_line = Line::from(vec![
        Span::styled("Volume  ", vol_label_style),
        Span::raw(format!("{vol_bar}  {vol_pct}")),
    ]);
    frame.render_widget(Paragraph::new(vol_line), chunks[4]);

    // --- Footer ---
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let footer = Line::from(vec![
        Span::styled("[Enter]", key),
        Span::styled(" Create  ", dim),
        Span::styled("[Esc]", key),
        Span::styled(" Cancel  ", dim),
        Span::styled("[Tab]", key),
        Span::styled(" Next field  ", dim),
        Span::styled("[Shift+Tab]", key),
        Span::styled(" Prev field", dim),
    ]);
    frame.render_widget(Paragraph::new(footer), chunks[6]);
}

/// Build a centered popup rect from a parent area.
fn centered_popup(area: Rect, percent_width: u16, height: u16) -> Rect {
    let popup_width = (area.width * percent_width / 100).min(area.width.saturating_sub(4));
    let popup_height = height.min(area.height.saturating_sub(4));

    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;

    Rect {
        x,
        y,
        width: popup_width,
        height: popup_height,
    }
}

fn draw_hints(frame: &mut Frame, area: Rect, state: &AppState) {
    if state.notification.is_some() {
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let spans = match &state.routes_screen.mode {
        RoutesMode::List => vec![
            Span::styled("j/k", key),
            Span::styled(" navigate  ", dim),
            Span::styled("Enter", key),
            Span::styled(" edit volume  ", dim),
            Span::styled("c", key),
            Span::styled(" create  ", dim),
            Span::styled("d d", key),
            Span::styled(" destroy  ", dim),
            Span::styled("q", key),
            Span::styled(" quit", dim),
        ],
        RoutesMode::Editing { .. } => vec![
            Span::styled("←/→", key),
            Span::styled(" ±0.05  ", dim),
            Span::styled("H/L", key),
            Span::styled(" ±0.25  ", dim),
            Span::styled("0", key),
            Span::styled(" mute  ", dim),
            Span::styled("1", key),
            Span::styled(" unity  ", dim),
            Span::styled("Enter/Esc", key),
            Span::styled(" done", dim),
        ],
        RoutesMode::Creating => {
            // Footer is rendered inside the popup; skip the global hint bar.
            return;
        }
    };

    let hint_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };

    frame.render_widget(Clear, hint_area);
    frame.render_widget(Paragraph::new(Line::from(spans)), hint_area);
}
