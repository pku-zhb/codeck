use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::markdown::{StyledLine, StyledSpan, apply_osc8_links, render_markdown};
use crate::model::{ComposeTarget, MessageEntry, MessageKind, SessionStatus};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let input_height = area.height.min(2);
    let separator_height = u16::from(area.height >= 6);
    let available = area
        .height
        .saturating_sub(input_height)
        .saturating_sub(separator_height.saturating_mul(2));
    let max_sessions = (available / 3).max(1);
    let desired_sessions = app.sessions().len().max(1) as u16;
    let session_height = desired_sessions.min(max_sessions).min(available);
    let chunks = Layout::vertical([
        Constraint::Length(session_height),
        Constraint::Length(separator_height),
        Constraint::Min(0),
        Constraint::Length(separator_height),
        Constraint::Length(input_height),
    ])
    .split(area);

    render_sessions(frame, chunks[0], app);
    render_separator(frame, chunks[1]);
    render_messages(frame, chunks[2], app);
    render_separator(frame, chunks[3]);
    render_composer(frame, chunks[4], app);
}

fn render_separator(frame: &mut Frame<'_>, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Color::DarkGray),
        )),
        area,
    );
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
        Span::styled("● ", row_style.patch(dot_style)),
        Span::styled(title, title_style),
        Span::styled(spacer, row_style),
        Span::styled(
            right,
            if selected {
                row_style.remove_modifier(Modifier::BOLD)
            } else {
                row_style.fg(Color::DarkGray)
            },
        ),
    ])
    .style(row_style)
}

fn render_messages(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    app.set_message_view_height(area.height as usize);
    let mut lines: Vec<StyledLine> = match app.selected_session() {
        Some(session) if !session.history_loaded && session.messages.is_empty() => {
            vec![StyledLine::from_span(
                "  Loading conversation…",
                dim_style(),
            )]
        }
        Some(session) if session.messages.is_empty() => vec![StyledLine::from_span(
            "  No conversation content yet",
            dim_style(),
        )],
        Some(session) => message_lines(&session.messages, area.width as usize),
        None => vec![StyledLine::from_span(
            "  New tasks will appear here and keep running after you close the deck.",
            dim_style(),
        )],
    };
    if lines.is_empty() {
        lines.push(StyledLine::default());
    }

    let viewport = area.height as usize;
    let max_start = lines.len().saturating_sub(viewport);
    let scroll_back = app.scroll_back().min(max_start);
    app.set_scroll_back(scroll_back);
    let start = max_start.saturating_sub(scroll_back);
    let end = (start + viewport).min(lines.len());
    let visible = &lines[start..end];
    let rendered = visible
        .iter()
        .map(StyledLine::to_ratatui)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(rendered)), area);
    apply_osc8_links(frame, area, visible);
}

fn message_lines(messages: &[MessageEntry], width: usize) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    for message in messages {
        let (prefix, prefix_style, text_style) = message_style(message.kind);
        let gutter_width = UnicodeWidthStr::width(prefix).max(1);
        let content_width = width.saturating_sub(gutter_width).max(1);
        let markdown = render_markdown(&message.text, text_style, content_width);
        for (index, line) in markdown.into_iter().enumerate() {
            let gutter = if index == 0 {
                prefix.to_string()
            } else {
                format!("│{}", " ".repeat(gutter_width.saturating_sub(1)))
            };
            let mut spans = vec![StyledSpan {
                text: gutter,
                style: if index == 0 {
                    prefix_style
                } else {
                    prefix_style.remove_modifier(Modifier::BOLD)
                },
                link: None,
            }];
            spans.extend(line.spans);
            lines.push(StyledLine { spans });
        }
        lines.push(StyledLine::default());
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
    let base = "Enter/→ attach · Tab new/reply · ↑↓ select · Del reviewed · Ctrl+C close";
    if app.notice().is_empty() {
        base.to_string()
    } else {
        format!("{} · {}", app.notice(), base)
    }
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
    use crate::model::Session;

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

    #[test]
    fn selected_session_uses_ansi_foreground_without_background() {
        let session = Session {
            id: "thread".to_string(),
            title: "Readable selection".to_string(),
            preview: String::new(),
            cwd: "/tmp/project".to_string(),
            path: None,
            updated_at: 0,
            source: "appServer".to_string(),
            thread_source: Some("codex-deck".to_string()),
            status: SessionStatus::Working,
            active_turn_id: None,
            messages: Vec::new(),
            history_loaded: true,
        };

        let line = session_line(&session, true, 40);
        assert_eq!(line.style.fg, Some(Color::Cyan));
        assert!(line.style.add_modifier.contains(Modifier::BOLD));
        assert!(!line.style.add_modifier.contains(Modifier::REVERSED));
        assert!(line.style.bg.is_none());
    }

    #[test]
    fn message_stream_keeps_role_gutter_and_markdown_styles() {
        let lines = message_lines(
            &[MessageEntry {
                id: "final".to_string(),
                kind: MessageKind::Final,
                text: "**Done**\n\n- [link](https://example.com)".to_string(),
            }],
            60,
        );
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        assert!(spans.iter().any(|span| span.text == "✅ "));
        assert!(spans.iter().any(|span| {
            span.text.contains("Done") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.text.contains("link") && span.link.as_deref() == Some("https://example.com")
        }));
    }
}
