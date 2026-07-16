use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::model::{ComposeTarget, MessageEntry, MessageKind, SessionStatus};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let input_height = area.height.min(2);
    let available = area.height.saturating_sub(input_height);
    let max_sessions = (available / 3).max(1);
    let desired_sessions = app.sessions().len().max(1) as u16;
    let session_height = desired_sessions.min(max_sessions).min(available);
    let chunks = Layout::vertical([
        Constraint::Length(session_height),
        Constraint::Min(0),
        Constraint::Length(input_height),
    ])
    .split(area);

    render_sessions(frame, chunks[0], app);
    render_messages(frame, chunks[1], app);
    render_composer(frame, chunks[2], app);
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if app.sessions().is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No sessions · type a task and press Enter",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let selected = app.selected_index();
    let height = area.height as usize;
    let offset = if selected >= height {
        selected + 1 - height
    } else {
        0
    };
    let lines = app
        .sessions()
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(index, session)| session_line(session, index == selected, area.width as usize))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn session_line(session: &crate::model::Session, selected: bool, width: usize) -> Line<'static> {
    let row_style = if selected {
        Style::default().bg(Color::Rgb(38, 48, 45))
    } else {
        Style::default()
    };
    let marker = if selected { "› " } else { "  " };
    let dot_style = match session.status {
        SessionStatus::NeedsInput => Style::default().fg(Color::Yellow),
        SessionStatus::Working => Style::default().fg(Color::Green),
        SessionStatus::Completed => Style::default().fg(Color::Cyan),
        SessionStatus::Failed => Style::default().fg(Color::Red),
    };
    let title_style = if selected {
        row_style.add_modifier(Modifier::BOLD)
    } else {
        row_style
    };
    let right = session.leaf_directory();
    let prefix_width = UnicodeWidthStr::width(marker) + 2;
    let right_width = UnicodeWidthStr::width(right.as_str());
    let title_budget = width
        .saturating_sub(prefix_width)
        .saturating_sub(right_width)
        .saturating_sub(1);
    let title = truncate_display(&session.title, title_budget);
    let used = prefix_width + UnicodeWidthStr::width(title.as_str()) + right_width;
    let spacer = " ".repeat(width.saturating_sub(used));

    Line::from(vec![
        Span::styled(marker.to_string(), row_style),
        Span::styled("● ", dot_style.patch(row_style)),
        Span::styled(title, title_style),
        Span::styled(spacer, row_style),
        Span::styled(right, row_style.fg(Color::DarkGray)),
    ])
    .style(row_style)
}

fn render_messages(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    app.set_message_view_height(area.height as usize);
    let mut lines = match app.selected_session() {
        Some(session) if !session.history_loaded && session.messages.is_empty() => {
            vec![Line::from(Span::styled(
                "  Loading conversation…",
                dim_style(),
            ))]
        }
        Some(session) if session.messages.is_empty() => vec![Line::from(Span::styled(
            "  No conversation content yet",
            dim_style(),
        ))],
        Some(session) => message_lines(&session.messages, area.width as usize),
        None => vec![Line::from(Span::styled(
            "  New tasks will appear here and keep running after you close the deck.",
            dim_style(),
        ))],
    };
    if lines.is_empty() {
        lines.push(Line::default());
    }

    let viewport = area.height as usize;
    let max_start = lines.len().saturating_sub(viewport);
    let scroll_back = app.scroll_back().min(max_start);
    app.set_scroll_back(scroll_back);
    let start = max_start.saturating_sub(scroll_back);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((start.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn message_lines(messages: &[MessageEntry], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for message in messages {
        let (prefix, prefix_style, text_style) = message_style(message.kind);
        let wrapped = wrap_message(&message.text, prefix, width.max(1));
        for (index, line) in wrapped.into_iter().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(line, text_style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line, text_style),
                ]));
            }
        }
        lines.push(Line::default());
    }
    lines
}

fn message_style(kind: MessageKind) -> (&'static str, Style, Style) {
    match kind {
        MessageKind::User => (
            "› ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        MessageKind::Thinking => (
            "🧠 ",
            Style::default().fg(Color::Magenta),
            Style::default().fg(Color::Gray),
        ),
        MessageKind::Progress => ("💬 ", Style::default().fg(Color::Blue), Style::default()),
        MessageKind::Final => ("✅ ", Style::default().fg(Color::Green), Style::default()),
        MessageKind::Question => (
            "❓ ",
            Style::default().fg(Color::Yellow),
            Style::default().fg(Color::Yellow),
        ),
        MessageKind::System => (
            "⚠ ",
            Style::default().fg(Color::Red),
            Style::default().fg(Color::Red),
        ),
    }
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let pending = app.selected_has_pending_request();
    let prefix = match app.composer().target {
        ComposeTarget::NewTask => "＋ new › ",
        ComposeTarget::Reply if pending => "？ answer › ",
        ComposeTarget::Reply => "↳ reply › ",
    };
    let prefix_style = match app.composer().target {
        ComposeTarget::NewTask => Style::default().fg(Color::Green),
        ComposeTarget::Reply if pending => Style::default().fg(Color::Yellow),
        ComposeTarget::Reply => Style::default().fg(Color::Cyan),
    }
    .add_modifier(Modifier::BOLD);
    let display = format!("{}{}", prefix, app.composer().text);
    let cursor_byte = prefix.len() + app.composer().cursor;
    let (all_lines, cursor_row, cursor_col) =
        layout_with_cursor(&display, cursor_byte, area.width.max(1) as usize);
    let offset = cursor_row.saturating_sub(area.height.saturating_sub(1) as usize);

    let mut visible = Vec::new();
    for row in 0..area.height as usize {
        let source_row = offset + row;
        let text = all_lines.get(source_row).cloned().unwrap_or_default();
        if source_row == 0 {
            let rest = text.strip_prefix(prefix).unwrap_or(&text).to_string();
            visible.push(Line::from(vec![
                Span::styled(prefix.to_string(), prefix_style),
                Span::raw(rest),
            ]));
        } else if text.is_empty() && row == 1 && app.composer().text.is_empty() {
            visible.push(Line::from(Span::styled(composer_hint(app), dim_style())));
        } else {
            visible.push(Line::from(text));
        }
    }

    frame.render_widget(Paragraph::new(Text::from(visible)), area);
    let cursor_y = area
        .y
        .saturating_add(cursor_row.saturating_sub(offset) as u16)
        .min(area.bottom().saturating_sub(1));
    let cursor_x = area
        .x
        .saturating_add(cursor_col as u16)
        .min(area.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn composer_hint(app: &App) -> String {
    let base = "Enter/→ attach · Tab new/reply · ↑↓ select · PgUp/PgDn scroll · Ctrl+C close";
    if app.notice().is_empty() {
        base.to_string()
    } else {
        format!("{} · {}", app.notice(), base)
    }
}

fn wrap_message(text: &str, prefix: &str, width: usize) -> Vec<String> {
    let first_width = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
    let next_width = width.saturating_sub(2).max(1);
    let mut output = Vec::new();
    let mut first = true;
    for logical_line in text.split('\n') {
        let budget = if first { first_width } else { next_width };
        let mut wrapped = wrap_graphemes(logical_line, budget);
        if wrapped.is_empty() {
            wrapped.push(String::new());
        }
        output.append(&mut wrapped);
        first = false;
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

fn wrap_graphemes(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if current_width > 0 && current_width + grapheme_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn layout_with_cursor(text: &str, cursor_byte: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut row = 0usize;
    let mut column = 0usize;
    let mut cursor = None;

    for (byte, grapheme) in text.grapheme_indices(true) {
        if byte == cursor_byte {
            cursor = Some((row, column));
        }
        if grapheme == "\n" {
            lines.push(String::new());
            row += 1;
            column = 0;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if column > 0 && column + grapheme_width > width {
            lines.push(String::new());
            row += 1;
            column = 0;
        }
        lines[row].push_str(grapheme);
        column += grapheme_width;
        if column >= width {
            lines.push(String::new());
            row += 1;
            column = 0;
        }
    }
    if cursor_byte == text.len() {
        cursor = Some((row, column));
    }
    let (cursor_row, cursor_col) = cursor.unwrap_or((row, column));
    (lines, cursor_row, cursor_col)
}

fn truncate_display(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if used + grapheme_width + 1 > width {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result.push('…');
    result
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_cjk_width() {
        assert_eq!(truncate_display("中文标题", 5), "中文…");
    }

    #[test]
    fn composer_cursor_wraps_to_second_row() {
        let (lines, row, column) = layout_with_cursor("123456", 6, 4);
        assert_eq!(lines[0], "1234");
        assert_eq!(lines[1], "56");
        assert_eq!((row, column), (1, 2));
    }
}
