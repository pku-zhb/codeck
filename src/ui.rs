use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, HistoryPickerView, MenuTab, SkillPickerView};
use crate::markdown::{StyledLine, StyledSpan, apply_osc8_links, render_markdown};
use crate::model::{
    ComposeTarget, ComposerToken, ComposerTokenKind, MessageEntry, MessageKind, PreviewVerbosity,
    SessionStatus,
};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if app.settings_open() {
        render_menu(
            frame,
            area,
            app.menu_tab(),
            app.settings_selection(),
            app.history_picker_view().as_ref(),
        );
        return;
    }

    let skill_picker = app.skill_picker_view();
    let skill_picker_height = skill_picker
        .as_ref()
        .map(|picker| (picker.items.len().clamp(1, 6) + 1) as u16)
        .unwrap_or_default();
    let separator_height = u16::from(area.height >= 6);
    let max_input_height = area
        .height
        .saturating_sub(separator_height.saturating_mul(2))
        .saturating_sub(2)
        .max(1);
    let input_prefix = composer_prefix(app.composer().target, app.selected_has_pending_request());
    let input_height = composer_height(
        &input_prefix,
        &app.composer().text,
        area.width as usize,
        max_input_height,
    );
    let available = area
        .height
        .saturating_sub(input_height)
        .saturating_sub(skill_picker_height)
        .saturating_sub(separator_height.saturating_mul(2));
    let max_sessions = (available / 3).max(1);
    let desired_sessions = app.sessions().len() as u16;
    let session_height = desired_sessions.min(max_sessions).min(available);
    let chunks = Layout::vertical([
        Constraint::Length(session_height),
        Constraint::Length(separator_height),
        Constraint::Min(0),
        Constraint::Length(skill_picker_height),
        Constraint::Length(separator_height),
        Constraint::Length(input_height),
    ])
    .split(area);

    render_sessions(frame, chunks[0], app);
    render_separator(frame, chunks[1]);
    render_messages(frame, chunks[2], app);
    if let Some(picker) = &skill_picker {
        render_skill_picker(frame, chunks[3], picker);
    }
    render_separator(frame, chunks[4]);
    render_composer(frame, chunks[5], app);
}

fn render_skill_picker(frame: &mut Frame<'_>, area: Rect, picker: &SkillPickerView) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "$ Skills",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  ↑↓ select · Enter/Tab insert",
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    if picker.items.is_empty() {
        lines.push(Line::from(Span::styled(
            if picker.loading {
                "  Loading skills…"
            } else {
                "  No matching skills"
            },
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let viewport = area.height.saturating_sub(1) as usize;
        let offset = picker.selected.saturating_add(1).saturating_sub(viewport);
        for (index, skill) in picker.items.iter().enumerate().skip(offset).take(viewport) {
            let selected = index == picker.selected;
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let scope = if skill.scope.is_empty() {
                String::new()
            } else {
                format!("  {}", skill.scope)
            };
            lines.push(Line::from(vec![
                Span::styled(if selected { "› " } else { "  " }, style),
                Span::styled(format!("${:<24}", skill.name), style),
                Span::styled(&skill.description, Style::default().fg(Color::Gray)),
                Span::styled(scope, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_menu(
    frame: &mut Frame<'_>,
    area: Rect,
    tab: MenuTab,
    selected: PreviewVerbosity,
    history: Option<&HistoryPickerView>,
) {
    let panel = if area.height > 4 {
        Rect::new(area.x, area.y + 1, area.width, area.height - 2)
    } else {
        area
    };
    let mut lines = vec![menu_tabs(tab), Line::default()];
    match tab {
        MenuTab::Resume => render_resume_tab(frame, panel, history, &mut lines),
        MenuTab::Settings => render_settings_tab(frame, panel, selected, &mut lines),
    }
}

fn menu_tabs(selected: MenuTab) -> Line<'static> {
    let active = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        Span::styled(
            "Codex Deck  ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Resume",
            if selected == MenuTab::Resume {
                active
            } else {
                inactive
            },
        ),
        Span::styled("  ", inactive),
        Span::styled(
            "Settings",
            if selected == MenuTab::Settings {
                active
            } else {
                inactive
            },
        ),
        Span::styled("   Tab switch", inactive),
    ])
}

fn render_settings_tab(
    frame: &mut Frame<'_>,
    panel: Rect,
    selected: PreviewVerbosity,
    lines: &mut Vec<Line<'static>>,
) {
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
    lines.push(Line::from(Span::styled(
        "Preview verbosity",
        Style::default().add_modifier(Modifier::BOLD),
    )));
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
    while lines.len() + 1 < panel.height as usize {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        "↑↓ select · Enter save · Tab switch · ←← close · Ctrl+C close Deck",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines.clone())), panel);

    let cursor_y = panel.y.saturating_add(3 + selected_index as u16);
    if cursor_y < panel.bottom() {
        frame.set_cursor_position((panel.x.min(panel.right().saturating_sub(1)), cursor_y));
    }
}

fn render_resume_tab(
    frame: &mut Frame<'_>,
    panel: Rect,
    history: Option<&HistoryPickerView>,
    lines: &mut Vec<Line<'static>>,
) {
    let query = history.map(|view| view.query.as_str()).unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(
            "Find › ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(query.to_string()),
    ]));
    lines.push(Line::default());

    let items = history
        .map(|view| view.items.as_slice())
        .unwrap_or_default();
    let selected = history.map(|view| view.selected).unwrap_or_default();
    let viewport = panel.height.saturating_sub(5) as usize;
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            if query.is_empty() {
                "  No sessions outside the Deck"
            } else {
                "  No matching sessions"
            },
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let offset = selected.saturating_add(1).saturating_sub(viewport.max(1));
        lines.extend(items.iter().enumerate().skip(offset).take(viewport).map(
            |(index, session)| {
                session_line(session, false, index == selected, panel.width as usize)
            },
        ));
    }
    while lines.len() + 1 < panel.height as usize {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        "type to filter · ↑↓ select · Enter add · Tab switch · ←← close",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines.clone())), panel);

    let prefix_width = UnicodeWidthStr::width("Find › ");
    let query_width = UnicodeWidthStr::width(query);
    let cursor_x = panel
        .x
        .saturating_add((prefix_width + query_width) as u16)
        .min(panel.right().saturating_sub(1));
    let cursor_y = panel
        .y
        .saturating_add(2)
        .min(panel.bottom().saturating_sub(1));
    frame.set_cursor_position((cursor_x, cursor_y));
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
    let rows = app
        .sessions()
        .iter()
        .enumerate()
        .map(|(index, session)| {
            session_line(
                session,
                app.is_pinned(&session.id),
                index == selected,
                area.width as usize,
            )
        })
        .collect::<Vec<_>>();

    let height = area.height as usize;
    let offset = if selected >= height {
        selected + 1 - height
    } else {
        0
    };
    let lines = rows
        .into_iter()
        .skip(offset)
        .take(height)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn session_line(
    session: &crate::model::Session,
    pinned: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let row_style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let pin = if pinned { "📌 " } else { "   " };
    let working = session.status == SessionStatus::Working;
    let lamp = if working { "● " } else { "  " };
    let lamp_style = if working {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    };
    let right = session.leaf_directory();
    let prefix_width = UnicodeWidthStr::width(pin) + UnicodeWidthStr::width(lamp);
    let right_width = UnicodeWidthStr::width(right.as_str());
    let title_budget = width
        .saturating_sub(prefix_width)
        .saturating_sub(right_width)
        .saturating_sub(1);
    let title = truncate_display(&session.title, title_budget);
    let used = prefix_width + UnicodeWidthStr::width(title.as_str()) + right_width;
    let spacer = " ".repeat(width.saturating_sub(used));

    Line::from(vec![
        Span::raw(pin.to_string()),
        Span::styled(lamp.to_string(), lamp_style),
        Span::styled(title, row_style),
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
    let prefix = composer_prefix(app.composer().target, pending);
    let prefix_style = match app.composer().target {
        ComposeTarget::NewTask => Style::default().fg(Color::Green),
        ComposeTarget::Reply if pending => Style::default().fg(Color::Yellow),
        ComposeTarget::Reply => Style::default().fg(Color::Cyan),
        ComposeTarget::Rename => Style::default().fg(Color::Magenta),
    }
    .add_modifier(Modifier::BOLD);
    let (all_lines, cursor_row, cursor_col) = styled_composer_layout(
        &prefix,
        &app.composer().text,
        app.composer().cursor,
        &app.composer().tokens,
        prefix_style,
        area.width.max(1) as usize,
    );
    let offset = cursor_row.saturating_sub(area.height.saturating_sub(1) as usize);

    let mut visible = Vec::new();
    for row in 0..area.height as usize {
        let source_row = offset + row;
        let mut line = all_lines.get(source_row).cloned().unwrap_or_default();
        if source_row == 0 && app.composer().text.is_empty() {
            line.spans
                .push(Span::styled(composer_hint(app), dim_style()));
        }
        visible.push(line);
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

fn composer_prefix(target: ComposeTarget, pending: bool) -> String {
    match target {
        ComposeTarget::NewTask => "＋ new › ",
        ComposeTarget::Reply if pending => "？ answer › ",
        ComposeTarget::Reply => "↳ reply › ",
        ComposeTarget::Rename => "✎ rename › ",
    }
    .to_string()
}

fn composer_height(prefix: &str, text: &str, width: usize, maximum: u16) -> u16 {
    let display = format!("{prefix}{text}");
    let (lines, _, _) = layout_with_cursor(&display, display.len(), width.max(1));
    (lines.len().min(u16::MAX as usize) as u16).clamp(1, maximum.max(1))
}

fn composer_hint(app: &App) -> String {
    let base = "$ skills · ←← menu · →→ attach · ↑↓ select · Ctrl+V image · Ctrl+T pin · Ctrl+R rename · Ctrl+X pause/remove · Ctrl+C close";
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

fn styled_composer_layout(
    prefix: &str,
    text: &str,
    cursor: usize,
    tokens: &[ComposerToken],
    prefix_style: Style,
    width: usize,
) -> (Vec<Line<'static>>, usize, usize) {
    let width = width.max(1);
    let display = format!("{prefix}{text}");
    let cursor_byte = prefix.len() + cursor;
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut row = 0usize;
    let mut column = 0usize;
    let mut cursor_position = None;

    for (byte, grapheme) in display.grapheme_indices(true) {
        if byte == cursor_byte {
            cursor_position = Some((row, column));
        }
        if grapheme == "\n" {
            lines.push(Vec::new());
            row += 1;
            column = 0;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if column > 0 && column + grapheme_width > width {
            lines.push(Vec::new());
            row += 1;
            column = 0;
        }
        let style = if byte < prefix.len() {
            prefix_style
        } else {
            token_style_at(tokens, byte - prefix.len()).unwrap_or_default()
        };
        push_styled_grapheme(&mut lines[row], grapheme, style);
        column += grapheme_width;
        if column >= width {
            lines.push(Vec::new());
            row += 1;
            column = 0;
        }
    }
    if cursor_byte == display.len() {
        cursor_position = Some((row, column));
    }
    let (cursor_row, cursor_col) = cursor_position.unwrap_or((row, column));
    (
        lines.into_iter().map(Line::from).collect(),
        cursor_row,
        cursor_col,
    )
}

fn token_style_at(tokens: &[ComposerToken], byte: usize) -> Option<Style> {
    tokens
        .iter()
        .find(|token| token.start <= byte && byte < token.style_end)
        .map(|token| match &token.kind {
            ComposerTokenKind::Skill(_) => Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            ComposerTokenKind::Image(_) => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        })
}

fn push_styled_grapheme(spans: &mut Vec<Span<'static>>, grapheme: &str, style: Style) {
    if let Some(previous) = spans.last_mut()
        && previous.style == style
    {
        previous.content.to_mut().push_str(grapheme);
        return;
    }
    spans.push(Span::styled(grapheme.to_string(), style));
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
        assert_eq!(composer_prefix(ComposeTarget::Reply, false), "↳ reply › ");
    }

    #[test]
    fn composer_tokens_use_ansi_foreground_without_background() {
        let mut composer = crate::model::Composer::default();
        composer.insert("$doc");
        composer.replace_with_skill(
            0,
            crate::model::SkillReference {
                name: "documents".to_string(),
                path: "/tmp/documents/SKILL.md".to_string(),
            },
        );
        composer.attach_image(std::path::PathBuf::from("/tmp/chart.png"));

        let (lines, _, _) = styled_composer_layout(
            "＋ new › ",
            &composer.text,
            composer.cursor,
            &composer.tokens,
            Style::default().fg(Color::Green),
            80,
        );
        let skill = lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("$documents"))
            .expect("styled skill token");
        let image = lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("[Image #1]"))
            .expect("styled image token");

        assert_eq!(skill.style.fg, Some(Color::Magenta));
        assert_eq!(image.style.fg, Some(Color::Yellow));
        assert!(skill.style.bg.is_none());
        assert!(image.style.bg.is_none());
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

        let line = session_line(&session, false, true, 40);
        assert_eq!(line.style.fg, Some(Color::Cyan));
        assert!(line.style.add_modifier.contains(Modifier::BOLD));
        assert!(!line.style.add_modifier.contains(Modifier::REVERSED));
        assert!(line.style.bg.is_none());
        assert!(!line.spans.iter().any(|span| span.content.contains('›')));
        assert_eq!(line.spans[0].content, "   ");
        assert_eq!(line.spans[1].content, "● ");
        assert_eq!(line.spans[1].style.fg, Some(Color::Green));
        assert!(
            !line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::SLOW_BLINK)
        );
    }

    #[test]
    fn non_working_sessions_reserve_lamp_space_without_showing_a_lamp() {
        for status in [
            SessionStatus::NeedsInput,
            SessionStatus::Completed,
            SessionStatus::Failed,
        ] {
            let session = Session {
                id: "thread".to_string(),
                title: "Idle session".to_string(),
                preview: String::new(),
                cwd: "/tmp/project".to_string(),
                path: None,
                updated_at: 0,
                source: "appServer".to_string(),
                thread_source: Some("codex-deck".to_string()),
                status,
                active_turn_id: None,
                messages: Vec::new(),
                history_loaded: true,
            };

            let line = session_line(&session, false, false, 40);
            assert_eq!(line.spans[0].content, "   ");
            assert_eq!(line.spans[1].content, "  ");
            assert!(!line.spans.iter().any(|span| span.content.contains('●')));
            assert!(!line.spans.iter().any(|span| span.content.contains('›')));
            assert_eq!(line.spans[2].style.fg, None);
        }
    }

    #[test]
    fn pinned_session_uses_reserved_left_slot() {
        let session = Session {
            id: "thread".to_string(),
            title: "Pinned session".to_string(),
            preview: String::new(),
            cwd: "/tmp/project".to_string(),
            path: None,
            updated_at: 0,
            source: "appServer".to_string(),
            thread_source: Some("codex-deck".to_string()),
            status: SessionStatus::Completed,
            active_turn_id: None,
            messages: Vec::new(),
            history_loaded: true,
        };

        let line = session_line(&session, true, false, 40);
        assert_eq!(line.spans[0].content, "📌 ");
        assert_eq!(line.spans[4].content, "project");
        assert_eq!(
            UnicodeWidthStr::width(line.spans[0].content.as_ref()),
            UnicodeWidthStr::width("   ")
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
