use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use ratatui::Frame;

use crate::state::{AppState, FocusedPane};

pub const APPS_HELP: &[(&str, &str)] = &[
    ("h / l", "switch between Apps and Taps panes"),
    ("j / k", "navigate list"),
    ("Enter", "create mirror tap (source keeps playing)"),
    ("m", "create muted tap (source silenced, redirect)"),
    ("d d", "destroy selected tap (confirm)"),
];

pub fn draw_apps(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    draw_apps_pane(frame, panes[0], state);
    draw_taps_pane(frame, panes[1], state);
    draw_hints(frame, area, state);
}

fn focused_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_apps_pane(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = state.apps_screen.focused == FocusedPane::Apps;
    let border_style = focused_border(focused);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" Apps ", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(ref apps) = state.apps else {
        frame.render_widget(Paragraph::new("…"), inner);
        return;
    };

    if apps.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("(no apps)", Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    // Sort by name case-insensitively.
    let mut sorted: Vec<_> = apps.iter().collect();
    sorted.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let tapped_pids: std::collections::HashSet<u32> = state
        .taps
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|t| t.pid)
        .collect();

    // Two rows: header + list.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let header = Line::from(vec![
        Span::styled("PID       ", Style::default().fg(Color::DarkGray)),
        Span::styled("Name", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let cursor = state.apps_screen.app_cursor.min(sorted.len().saturating_sub(1));
    let items: Vec<ListItem> = sorted
        .iter()
        .enumerate()
        .map(|(i, app)| {
            let tapped = tapped_pids.contains(&app.pid);
            let marker = if tapped { "* " } else { "  " };
            let text = format!("{:<8}  {}{}", app.pid, marker, app.name);

            let style = if i == cursor && focused {
                Style::default()
                    .bg(Color::White)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if i == cursor {
                Style::default().bg(Color::DarkGray)
            } else if tapped {
                Style::default().add_modifier(Modifier::BOLD)
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

fn draw_taps_pane(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = state.apps_screen.focused == FocusedPane::Taps;
    let border_style = focused_border(focused);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" Taps ", Style::default().add_modifier(Modifier::BOLD)))
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(ref taps) = state.taps else {
        frame.render_widget(Paragraph::new("…"), inner);
        return;
    };

    if taps.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("(no taps)", Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let header = Line::from(vec![
        Span::styled("ID    ", Style::default().fg(Color::DarkGray)),
        Span::styled("PID       ", Style::default().fg(Color::DarkGray)),
        Span::styled("Mode      ", Style::default().fg(Color::DarkGray)),
        Span::styled("Name", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let cursor = state
        .apps_screen
        .tap_cursor
        .min(taps.len().saturating_sub(1));

    let armed_tap_id = state
        .apps_screen
        .delete_armed
        .map(|(id, _)| id);

    let items: Vec<ListItem> = taps
        .iter()
        .enumerate()
        .map(|(i, tap)| {
            let armed = armed_tap_id == Some(tap.tap_id);
            let mode_label = format!("[{}]", tap.mode.label());
            let text = if armed {
                format!(
                    "{:<6}{:<10}{:<10}{}  [press d again to destroy]",
                    tap.tap_id, tap.pid, mode_label, tap.app_name
                )
            } else {
                format!("{:<6}{:<10}{:<10}{}", tap.tap_id, tap.pid, mode_label, tap.app_name)
            };

            let style = if armed {
                Style::default().fg(Color::Yellow)
            } else if i == cursor && focused {
                Style::default()
                    .bg(Color::White)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if i == cursor {
                Style::default().bg(Color::DarkGray)
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

fn draw_hints(frame: &mut Frame, area: Rect, state: &AppState) {
    // Hints overlay at the bottom-right corner of the body — 1-line paragraph.
    // We skip if there's an active notification (handled in status bar instead).
    if state.notification.is_some() {
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let spans = match state.apps_screen.focused {
        FocusedPane::Apps => vec![
            Span::styled("h/l", key),
            Span::styled(" switch pane  ", dim),
            Span::styled("j/k", key),
            Span::styled(" navigate  ", dim),
            Span::styled("Enter", key),
            Span::styled(" mirror tap  ", dim),
            Span::styled("m", key),
            Span::styled(" mute-source tap  ", dim),
            Span::styled("q", key),
            Span::styled(" quit", dim),
        ],
        FocusedPane::Taps => vec![
            Span::styled("h/l", key),
            Span::styled(" switch pane  ", dim),
            Span::styled("j/k", key),
            Span::styled(" navigate  ", dim),
            Span::styled("d d", key),
            Span::styled(" destroy tap  ", dim),
            Span::styled("q", key),
            Span::styled(" quit", dim),
        ],
    };

    // Place hint line at the bottom of the body area.
    let hint_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };

    frame.render_widget(Clear, hint_area);
    frame.render_widget(Paragraph::new(Line::from(spans)), hint_area);
}
