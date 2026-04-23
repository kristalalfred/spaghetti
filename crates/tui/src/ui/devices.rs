use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::state::{AppState, DeviceField, DevicesMode};

pub fn draw_devices(frame: &mut Frame, area: Rect, state: &AppState) {
    match state.devices_screen.mode {
        DevicesMode::Creating => {
            draw_device_list(frame, area, state);
            draw_create_form(frame, area, state);
        }
        DevicesMode::List => {
            draw_device_list(frame, area, state);
        }
    }
    draw_hints(frame, area, state);
}

fn draw_device_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Virtual Devices ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(ref devices) = state.devices else {
        frame.render_widget(Paragraph::new("…"), inner);
        return;
    };

    if devices.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "(no virtual devices)",
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
        Span::styled(format!("{:<38}", "UID"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<24}", "Name"), Style::default().fg(Color::DarkGray)),
        Span::styled("Ch", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let cursor = state
        .devices_screen
        .cursor
        .min(devices.len().saturating_sub(1));

    let armed_uid = state
        .devices_screen
        .armed_destroy
        .as_ref()
        .map(|(uid, _)| uid.as_str());

    let items: Vec<ListItem> = devices
        .iter()
        .enumerate()
        .map(|(i, dev)| {
            let armed = armed_uid == Some(dev.uid.as_str());
            let text = if armed {
                format!(
                    "{:<38}{:<24}{}  [press d again to destroy]",
                    dev.uid, dev.name, dev.channels,
                )
            } else {
                format!("{:<38}{:<24}{}", dev.uid, dev.name, dev.channels)
            };

            let style = if armed {
                Style::default().fg(Color::Yellow)
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

fn draw_create_form(frame: &mut Frame, area: Rect, state: &AppState) {
    let popup_area = centered_popup(area, 50, 8);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Create Virtual Device ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let form = &state.devices_screen.create_form;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Name label + input
            Constraint::Length(1), // spacer
            Constraint::Length(1), // Channels label + value
            Constraint::Length(1), // spacer
            Constraint::Length(1), // footer
        ])
        .split(inner);

    // --- Name field ---
    let name_label_style = if form.active_field == DeviceField::Name {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let name_value_style = if form.active_field == DeviceField::Name {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let name_display = if form.name.is_empty() {
        Span::styled("_", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(form.name.as_str(), name_value_style)
    };
    let name_line = Line::from(vec![
        Span::styled("Name      ", name_label_style),
        name_display,
    ]);
    frame.render_widget(Paragraph::new(name_line), chunks[0]);

    // --- Channels field ---
    let ch_label_style = if form.active_field == DeviceField::Channels {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let ch_value_style = if form.active_field == DeviceField::Channels {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let ch_line = Line::from(vec![
        Span::styled("Channels  ", ch_label_style),
        Span::styled(form.channels.to_string(), ch_value_style),
        Span::styled("  (↑/↓ or +/- to adjust, 1–64)", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(ch_line), chunks[2]);

    // --- Footer ---
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let footer = Line::from(vec![
        Span::styled("[Enter]", key),
        Span::styled(" Create  ", dim),
        Span::styled("[Esc]", key),
        Span::styled(" Cancel  ", dim),
        Span::styled("[Tab]", key),
        Span::styled(" Next field", dim),
    ]);
    frame.render_widget(Paragraph::new(footer), chunks[4]);
}

/// Build a centered popup rect from a parent area.
fn centered_popup(area: Rect, percent_width: u16, height: u16) -> Rect {
    let popup_width = (area.width * percent_width / 100).min(area.width.saturating_sub(4));
    let popup_height = height.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    Rect { x, y, width: popup_width, height: popup_height }
}

fn draw_hints(frame: &mut Frame, area: Rect, state: &AppState) {
    if state.notification.is_some() {
        return;
    }

    if matches!(state.devices_screen.mode, DevicesMode::Creating) {
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let spans = vec![
        Span::styled("j/k", key),
        Span::styled(" navigate  ", dim),
        Span::styled("c", key),
        Span::styled(" create  ", dim),
        Span::styled("d d", key),
        Span::styled(" destroy  ", dim),
        Span::styled("?", key),
        Span::styled(" help  ", dim),
        Span::styled("q", key),
        Span::styled(" quit", dim),
    ];

    let hint_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };
    frame.render_widget(Clear, hint_area);
    frame.render_widget(Paragraph::new(Line::from(spans)), hint_area);
}

// ─── Help content ─────────────────────────────────────────────────────────────

/// Keybindings shown in the help overlay for the Devices screen.
pub const DEVICES_HELP: &[(&str, &str)] = &[
    ("j / k", "navigate list"),
    ("c", "create virtual device"),
    ("d d", "destroy selected device (confirm)"),
];
