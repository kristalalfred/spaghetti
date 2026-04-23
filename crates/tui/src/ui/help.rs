use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::state::{AppState, Screen};

/// Global keybindings shown at the bottom of every help overlay.
pub const GLOBAL_HELP: &[(&str, &str)] = &[
    ("1 / 2 / 3 / 4", "switch to Dashboard / Apps / Routes / Devices"),
    ("Tab / Shift+Tab", "cycle screens"),
    ("?", "toggle this help"),
    ("q", "quit"),
];

/// Render key-description pairs into styled `Line`s.
pub fn render_help_lines<'a>(
    screen_keys: &'a [(&'a str, &'a str)],
    global_keys: &'a [(&'a str, &'a str)],
) -> Vec<Line<'a>> {
    let key_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let header = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled("Screen", header)));
    for (k, v) in screen_keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}"), key_style),
            Span::styled(*v, dim),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Global", header)));
    for (k, v) in global_keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}"), key_style),
            Span::styled(*v, dim),
        ]));
    }

    lines
}

/// Draw the help overlay on top of whatever is already rendered.
pub fn draw_help_overlay(frame: &mut Frame, area: Rect, state: &AppState) {
    // Build screen-specific content first to know the height needed.
    let screen_keys: &[(&str, &str)] = match state.current_screen {
        Screen::Dashboard => crate::ui::dashboard::DASHBOARD_HELP,
        Screen::Apps => crate::ui::apps::APPS_HELP,
        Screen::Routes => crate::ui::routes::ROUTES_HELP,
        Screen::Devices => crate::ui::devices::DEVICES_HELP,
    };

    let row_count = screen_keys.len() + GLOBAL_HELP.len() + 4; // headers + blank line
    let height = (row_count as u16 + 2).min(area.height.saturating_sub(2)); // +2 for border
    let popup_area = centered_popup(area, 55, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Keybindings ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = render_help_lines(screen_keys, GLOBAL_HELP);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered_popup(area: Rect, percent_width: u16, height: u16) -> Rect {
    let popup_width = (area.width * percent_width / 100).min(area.width.saturating_sub(4));
    let popup_height = height.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    Rect { x, y, width: popup_width, height: popup_height }
}
