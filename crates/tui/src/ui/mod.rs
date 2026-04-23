use std::io::Stdout;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::state::{AppState, ConnectionStatus, NotificationKind, Screen};

pub mod apps;
pub mod dashboard;
pub mod devices;
pub mod help;
pub mod routes;

pub fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &AppState,
) -> anyhow::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        // Three rows: tab bar (1), body (fill), status bar (1).
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        draw_tab_bar(frame, chunks[0], state);

        match state.current_screen {
            Screen::Dashboard => dashboard::draw_dashboard(frame, chunks[1], state),
            Screen::Apps => apps::draw_apps(frame, chunks[1], state),
            Screen::Routes => routes::draw_routes(frame, chunks[1], state),
            Screen::Devices => devices::draw_devices(frame, chunks[1], state),
        }

        draw_status_bar(frame, chunks[2], state);

        if state.show_help {
            help::draw_help_overlay(frame, chunks[1], state);
        }
    })?;
    Ok(())
}

fn draw_tab_bar(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    // Left: tab labels with number shortcuts. Right: connection status.
    let tab_line: Vec<Span> = Screen::ALL
        .iter()
        .enumerate()
        .flat_map(|(i, &screen)| {
            let active = screen == state.current_screen;
            let style = if active {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let label = format!(" [{}] {} ", i + 1, screen.label());
            let mut spans = vec![Span::styled(label, style)];
            if i < Screen::ALL.len() - 1 {
                spans.push(Span::raw(" "));
            }
            spans
        })
        .collect();

    let conn_text = match state.connection_status {
        ConnectionStatus::Connected => Span::styled(" ONLINE ", Style::default().fg(Color::Green)),
        ConnectionStatus::Disconnected => {
            Span::styled(" OFFLINE ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        }
    };

    // Split tab bar: tabs on left, status on right.
    // Compute status width to right-align it.
    let status_width = conn_text.content.len() as u16;
    let tabs_width = area.width.saturating_sub(status_width);

    let tab_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(tabs_width), Constraint::Length(status_width)])
        .split(area);

    frame.render_widget(Paragraph::new(Line::from(tab_line)), tab_chunks[0]);
    frame.render_widget(Paragraph::new(conn_text), tab_chunks[1]);
}

fn draw_status_bar(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    // Notification takes priority over the connection status hint.
    if let Some(ref notif) = state.notification {
        let color = match notif.kind {
            NotificationKind::Info => Color::Green,
            NotificationKind::Error => Color::Red,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(&notif.message, Style::default().fg(color))),
            area,
        );
        return;
    }

    // Offline hint — brief reminder so the status bar isn't silent.
    if state.connection_status == ConnectionStatus::Disconnected {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Daemon offline — reconnecting…",
                Style::default().fg(Color::DarkGray),
            )),
            area,
        );
    }
}
