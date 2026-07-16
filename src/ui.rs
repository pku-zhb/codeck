use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::markdown::{StyledLine, StyledSpan, apply_osc8_links, render_markdown};
use crate::model::{ComposeTarget, MessageEntry, MessageKind, PreviewVerbosity, SessionStatus};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if app.settings_open() {
        render_settings(frame, area, app.settings_selection());
        return;
    }

    let separator_height = u16::from(area.height >= 6);
    let max_input_height = area
        .height
        .saturating_sub(separator_height.saturating_mul(2))
        .saturating_sub(2)
        .max(1);
    let input_prefix = composer_prefix(
        app.composer().target,
        app.selected_has_pending_request(),
        app.composer().images.len(),
    );
    let input_height = composer_height(
        &input_prefix,
        &app.composer().text,
        area.width as usize,
        max_input_height,
    );
    let available = area
        .height
        .saturating_sub(input_height)
        .saturating_sub(separator_height.saturating_mul(2));
    let max_sessions = (available / 3).max(1);
    let desired_sessions = (app.sessions().len() + 3) as u16;
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

fn render_settings(frame: &mut Frame<'_>, area: Rect, selected: PreviewVerbosity) {
    let options = [
        (
            PreviewVerbosity::Full,
            "🧠 ",
            "Full",
            "thinking, progress, and final replies",
        ),
        (
            PreviewVerbosity::Progress,
            "💬 ",
            "Progress",
            "progress and final replies",
        ),
        (
            PreviewVerbosity::Final,
            "✅ ",
            "Final",
            "final replies only",
        ),
    ];
    let panel_height = 9u16.min(area.height);
    let panel = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(panel_height) / 2,
        area.width,
        panel_height,
    );
    let mut lines = vec![
        Line::from(Span::styled(
            "Codex Deck Settings",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(Span::styled(
            "Preview verbosity",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    let selected_index = options
        .iter()
        .position(|(verbosity, _, _, _)| *verbosity == selected)
        .unwrap_or_default();
    for (verbosity, emoji, label, description) in options {
        let active = verbosity == selected;
        let style = if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(if active { "› " } else { "  " }, style),
            Span::styled(emoji, style),
            Span::styled(format!("{label:<10}"), style),
            Span::styled(description, Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "↑↓ select · Enter save · ←← cancel · Ctrl+C close Deck",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), panel);

    let cursor_y = panel.y.saturating_add(3 + selected_index as u16);
    if cursor_y < panel.bottom() {
        frame.set_cursor_position((panel.x.min(panel.right().saturating_sub(1)), cursor_y));
    }
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
    let selected = app.selected_index();
    let groups = [
        (SessionGroup::Pinned, "Pinned", Color::Green),
        (SessionGroup::Working, "Working", Color::Green),
        (SessionGroup::Completed, "Completed", Color::Green),
    ];
    let mut rows = Vec::with_capacity(app.sessions().len() + groups.len());
    for (group, label, color) in groups {
        let count = app
            .sessions()
            .iter()
            .filter(|session| session_group(app.is_pinned(&session.id), session.status) == group)
            .count();
        rows.push((
            None,
            Line::from(vec![
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {count}"), Style::default().fg(Color::DarkGray)),
            ]),
        ));
        rows.extend(
            app.sessions()
                .iter()
                .enumerate()
                .filter(|(_, session)| {
                    session_group(app.is_pinned(&session.id), session.status) == group
                })
                .map(|(index, session)| {
                    (
                        Some(index),
                        session_line(session, index == selected, area.width as usize),
                    )
                }),
        );
    }

    let height = area.height as usize;
    let selected_row = rows
        .iter()
        .position(|(index, _)| *index == Some(selected))
        .unwrap_or_default();
    let offset = if selected_row >= height {
        selected_row + 1 - height
    } else {
        0
    };
    let lines = rows
        .into_iter()
        .skip(offset)
        .take(height)
        .map(|(_, line)| line)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionGroup {
    Pinned,
    Working,
    Completed,
}

fn session_group(pinned: bool, status: SessionStatus) -> SessionGroup {
    if pinned {
        SessionGroup::Pinned
    } else if status.is_live() {
        SessionGroup::Working
    } else {
        SessionGroup::Completed
    }
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
    let title_style = if selected {
        row_style.add_modifier(Modifier::BOLD)
    } else if session.status == SessionStatus::NeedsInput {
        row_style.fg(Color::Yellow)
    } else if session.status == SessionStatus::Failed {
        row_style.fg(Color::Red)
    } else {
        row_style
    };
    let right = session.leaf_directory();
    let prefix_width = UnicodeWidthStr::width(marker);
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
        Some(session) => message_lines(
            &session.messages,
            area.width as usize,
            app.preview_verbosity(),
        ),
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

fn message_lines(
    messages: &[MessageEntry],
    width: usize,
    verbosity: PreviewVerbosity,
) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    for message in messages
        .iter()
        .filter(|message| verbosity.includes(message.kind))
    {
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
    let prefix = composer_prefix(app.composer().target, pending, app.composer().images.len());
    let prefix_style = match app.composer().target {
        ComposeTarget::NewTask => Style::default().fg(Color::Green),
        ComposeTarget::Reply if pending => Style::default().fg(Color::Yellow),
        ComposeTarget::Reply => Style::default().fg(Color::Cyan),
        ComposeTarget::Rename => Style::default().fg(Color::Magenta),
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
            let rest = text.strip_prefix(&prefix).unwrap_or(&text).to_string();
            let mut spans = vec![Span::styled(prefix.clone(), prefix_style)];
            if rest.is_empty() && app.composer().text.is_empty() {
                spans.push(Span::styled(composer_hint(app), dim_style()));
            } else {
                spans.push(Span::raw(rest));
            }
            visible.push(Line::from(spans));
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

fn composer_prefix(target: ComposeTarget, pending: bool, image_count: usize) -> String {
    let base = match target {
        ComposeTarget::NewTask => "＋ new › ",
        ComposeTarget::Reply if pending => "？ answer › ",
        ComposeTarget::Reply => "↳ reply › ",
        ComposeTarget::Rename => "✎ rename › ",
    };
    if image_count == 0 {
        base.to_string()
    } else {
        format!("{}🖼{} ", base, image_count)
    }
}

fn composer_height(prefix: &str, text: &str, width: usize, maximum: u16) -> u16 {
    let display = format!("{prefix}{text}");
    let (lines, _, _) = layout_with_cursor(&display, display.len(), width.max(1));
    (lines.len().min(u16::MAX as usize) as u16).clamp(1, maximum.max(1))
}

fn composer_hint(app: &App) -> String {
    let base = "←← settings · →→ attach · ↑↓ select · Ctrl+V image · Ctrl+T pin · Ctrl+R rename · Ctrl+X stop/remove · Ctrl+C close";
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
    fn composer_starts_at_one_row_and_grows_with_wrapped_content() {
        assert_eq!(composer_height("＋ new › ", "", 40, 10), 1);
        assert_eq!(composer_height("＋ new › ", "1234567890", 10, 10), 2);
        assert_eq!(composer_height("＋ new › ", "\nsecond\nthird", 40, 2), 2);
        assert_eq!(
            composer_prefix(ComposeTarget::Reply, false, 2),
            "↳ reply › 🖼2 "
        );
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
        assert!(!line.spans.iter().any(|span| span.content.contains('●')));
    }

    #[test]
    fn pinned_group_takes_precedence_over_runtime_status() {
        assert_eq!(
            session_group(true, SessionStatus::Completed),
            SessionGroup::Pinned
        );
        assert_eq!(
            session_group(false, SessionStatus::NeedsInput),
            SessionGroup::Working
        );
        assert_eq!(
            session_group(false, SessionStatus::Failed),
            SessionGroup::Completed
        );
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
            PreviewVerbosity::Full,
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

    #[test]
    fn preview_verbosity_filters_only_assistant_detail_levels() {
        let messages = [
            MessageEntry {
                id: "user".to_string(),
                kind: MessageKind::User,
                text: "Prompt".to_string(),
            },
            MessageEntry {
                id: "thinking".to_string(),
                kind: MessageKind::Thinking,
                text: "Reasoning".to_string(),
            },
            MessageEntry {
                id: "progress".to_string(),
                kind: MessageKind::Progress,
                text: "Working".to_string(),
            },
            MessageEntry {
                id: "final".to_string(),
                kind: MessageKind::Final,
                text: "Done".to_string(),
            },
        ];

        let progress = message_lines(&messages, 60, PreviewVerbosity::Progress);
        let progress_text = progress
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(progress_text.contains("Prompt"));
        assert!(!progress_text.contains("Reasoning"));
        assert!(progress_text.contains("Working"));
        assert!(progress_text.contains("Done"));

        let final_only = message_lines(&messages, 60, PreviewVerbosity::Final);
        let final_text = final_only
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(final_text.contains("Prompt"));
        assert!(!final_text.contains("Reasoning"));
        assert!(!final_text.contains("Working"));
        assert!(final_text.contains("Done"));
    }
}
