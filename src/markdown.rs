use std::num::NonZeroU16;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Debug, Default)]
pub struct StyledLine {
    pub spans: Vec<StyledSpan>,
}

#[derive(Clone, Debug)]
pub struct StyledSpan {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

impl StyledLine {
    pub fn from_span(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![StyledSpan {
                text: text.into(),
                style,
                link: None,
            }],
        }
    }

    pub fn to_ratatui(&self) -> Line<'static> {
        Line::from(
            self.spans
                .iter()
                .map(|span| Span::styled(span.text.clone(), span.style))
                .collect::<Vec<_>>(),
        )
    }

    fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.text.as_str()))
            .sum()
    }

    fn push(&mut self, text: impl Into<String>, style: Style, link: Option<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == style
            && last.link == link
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(StyledSpan { text, style, link });
    }
}

#[derive(Clone, Copy)]
struct ListState {
    next: Option<u64>,
}

struct MarkdownBuilder {
    lines: Vec<StyledLine>,
    current: StyledLine,
    style: Style,
    style_stack: Vec<Style>,
    link: Option<String>,
    link_stack: Vec<Option<String>>,
    lists: Vec<ListState>,
    quote_depth: usize,
    code_depth: usize,
    table_cell: usize,
}

impl MarkdownBuilder {
    fn new(base_style: Style) -> Self {
        Self {
            lines: Vec::new(),
            current: StyledLine::default(),
            style: base_style,
            style_stack: Vec::new(),
            link: None,
            link_stack: Vec::new(),
            lists: Vec::new(),
            quote_depth: 0,
            code_depth: 0,
            table_cell: 0,
        }
    }

    fn finish(mut self) -> Vec<StyledLine> {
        self.flush_line();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(StyledLine::default());
        }
        self.lines
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.current.spans.is_empty() && (self.quote_depth > 0 || self.code_depth > 0) {
                    self.push_block_prefix();
                }
            }
            Tag::Heading { level, .. } => {
                self.begin_block();
                self.push_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
                let marker = match level {
                    pulldown_cmark::HeadingLevel::H1 => "◆ ",
                    pulldown_cmark::HeadingLevel::H2 => "◇ ",
                    _ => "▸ ",
                };
                self.push(marker);
            }
            Tag::BlockQuote(_) => {
                self.begin_block();
                self.quote_depth += 1;
                self.push_block_prefix();
            }
            Tag::CodeBlock(kind) => {
                self.begin_block();
                self.code_depth += 1;
                self.push_style(Style::default().fg(Color::Yellow));
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.trim().is_empty()
                {
                    self.current.push(
                        format!("┌ {}", language.trim()),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                        None,
                    );
                    self.flush_line();
                }
                self.push_block_prefix();
            }
            Tag::List(start) => {
                self.begin_block();
                self.lists.push(ListState { next: start });
            }
            Tag::Item => {
                self.begin_block();
                let depth = self.lists.len().saturating_sub(1);
                self.push(&"  ".repeat(depth));
                let marker = self
                    .lists
                    .last_mut()
                    .and_then(|list| {
                        list.next.map(|next| {
                            list.next = Some(next + 1);
                            format!("{next}. ")
                        })
                    })
                    .unwrap_or_else(|| "• ".to_string());
                self.current
                    .push(marker, Style::default().fg(Color::Cyan), None);
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Superscript | Tag::Subscript => {
                self.push_style(Style::default().fg(Color::Cyan));
            }
            Tag::Link { dest_url, .. } => {
                self.push_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.link_stack.push(self.link.take());
                self.link = osc8_target(dest_url.as_ref());
            }
            Tag::Image { dest_url, .. } => {
                self.push("▧ ");
                self.push_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.link_stack.push(self.link.take());
                self.link = osc8_target(dest_url.as_ref());
            }
            Tag::FootnoteDefinition(label) => {
                self.begin_block();
                self.current.push(
                    format!("[^{label}] "),
                    Style::default().fg(Color::Cyan),
                    None,
                );
            }
            Tag::Table(_) => self.begin_block(),
            Tag::TableHead => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::TableRow => {
                self.begin_block();
                self.table_cell = 0;
            }
            Tag::TableCell => {
                if self.table_cell > 0 {
                    self.current
                        .push(" │ ", Style::default().fg(Color::DarkGray), None);
                }
                self.table_cell += 1;
            }
            Tag::DefinitionList => self.begin_block(),
            Tag::DefinitionListTitle => {
                self.begin_block();
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::DefinitionListDefinition => {
                self.begin_block();
                self.push("  — ");
            }
            Tag::HtmlBlock | Tag::MetadataBlock(_) => self.begin_block(),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item | TagEnd::FootnoteDefinition => self.flush_line(),
            TagEnd::Heading(_) => {
                self.flush_line();
                self.pop_style();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.code_depth = self.code_depth.saturating_sub(1);
                self.pop_style();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::TableHead
            | TagEnd::DefinitionListTitle => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.link = self.link_stack.pop().flatten();
                self.pop_style();
            }
            TagEnd::TableRow => self.flush_line(),
            TagEnd::Table => self.flush_line(),
            TagEnd::TableCell
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(text) => {
                let style = self.style.patch(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                );
                self.current
                    .push(text.to_string(), style, self.link.clone());
            }
            Event::InlineMath(text) => {
                self.current.push(
                    format!("${text}$"),
                    self.style.patch(Style::default().fg(Color::Yellow)),
                    self.link.clone(),
                );
            }
            Event::DisplayMath(text) => {
                self.begin_block();
                self.current.push(
                    format!("∑ {text}"),
                    self.style.patch(Style::default().fg(Color::Yellow)),
                    None,
                );
                self.flush_line();
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.current.push(
                    text.to_string(),
                    self.style.patch(Style::default().fg(Color::DarkGray)),
                    self.link.clone(),
                );
            }
            Event::FootnoteReference(label) => {
                self.current.push(
                    format!("[^{label}]"),
                    self.style.patch(Style::default().fg(Color::Cyan)),
                    None,
                );
            }
            Event::SoftBreak if self.code_depth > 0 || self.current_is_field_record() => {
                self.continue_line();
            }
            Event::SoftBreak => self.push(" "),
            Event::HardBreak => self.continue_line(),
            Event::Rule => {
                self.begin_block();
                self.current
                    .push("────────────", Style::default().fg(Color::DarkGray), None);
                self.flush_line();
            }
            Event::TaskListMarker(checked) => self.current.push(
                if checked { "☑ " } else { "☐ " },
                Style::default().fg(if checked {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
                None,
            ),
        }
    }

    fn push_style(&mut self, overlay: Style) {
        self.style_stack.push(self.style);
        self.style = self.style.patch(overlay);
    }

    fn pop_style(&mut self) {
        if let Some(style) = self.style_stack.pop() {
            self.style = style;
        }
    }

    fn push(&mut self, text: &str) {
        self.current
            .push(text.to_string(), self.style, self.link.clone());
    }

    fn push_text(&mut self, text: &str) {
        let parts = text.split('\n').collect::<Vec<_>>();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                self.flush_line();
                if index + 1 == parts.len() && part.is_empty() {
                    break;
                }
                self.push_block_prefix();
            }
            self.push(part);
        }
    }

    fn begin_block(&mut self) {
        if !self.current.spans.is_empty() {
            self.flush_line();
        }
    }

    fn flush_line(&mut self) {
        if self.current.spans.is_empty() {
            return;
        }
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn continue_line(&mut self) {
        self.flush_line();
        self.push_block_prefix();
    }

    fn current_is_field_record(&self) -> bool {
        let text = self
            .current
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        field_label_range(&text).is_some()
    }

    fn push_block_prefix(&mut self) {
        for _ in 0..self.quote_depth {
            self.current
                .push("│ ", Style::default().fg(Color::DarkGray), None);
        }
        if self.code_depth > 0 {
            self.current
                .push("│ ", Style::default().fg(Color::Yellow), None);
        }
    }
}

pub fn render_markdown(text: &str, base_style: Style, width: usize) -> Vec<StyledLine> {
    let options = Options::ENABLE_GFM
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
        | Options::ENABLE_MATH
        | Options::ENABLE_WIKILINKS;
    let mut builder = MarkdownBuilder::new(base_style);
    for event in Parser::new_ext(text, options) {
        builder.event(event);
    }
    let lines = wrap_lines(builder.finish(), width.max(1));
    highlight_field_labels(lines)
}

pub fn apply_osc8_links(frame: &mut Frame<'_>, area: Rect, lines: &[StyledLine]) {
    for (row, line) in lines.iter().take(area.height as usize).enumerate() {
        let mut x = area.x;
        let y = area.y.saturating_add(row as u16);
        for span in &line.spans {
            if let Some(target) = span.link.as_deref() {
                for grapheme in span.text.graphemes(true) {
                    let width = UnicodeWidthStr::width(grapheme).max(1) as u16;
                    if x.saturating_add(width) > area.right() {
                        break;
                    }
                    if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                        let linked = format!("\x1b]8;;{target}\x1b\\{grapheme}\x1b]8;;\x1b\\");
                        cell.set_symbol(&linked)
                            .set_diff_option(CellDiffOption::ForcedWidth(
                                NonZeroU16::new(width).expect("cell width"),
                            ));
                    }
                    x = x.saturating_add(width);
                }
            } else {
                x = x.saturating_add(UnicodeWidthStr::width(span.text.as_str()) as u16);
            }
            if x >= area.right() {
                break;
            }
        }
    }
}

fn wrap_lines(lines: Vec<StyledLine>, width: usize) -> Vec<StyledLine> {
    let mut output = Vec::new();
    for line in lines {
        if line.spans.is_empty() {
            output.push(line);
            continue;
        }
        let mut current = StyledLine::default();
        for span in line.spans {
            for token in span.text.split_word_bounds() {
                let token_width = UnicodeWidthStr::width(token);
                if token_width == 0 {
                    continue;
                }
                if token.trim().is_empty() && current.spans.is_empty() {
                    continue;
                }
                if token_width <= width {
                    if current.width() > 0 && current.width() + token_width > width {
                        output.push(std::mem::take(&mut current));
                        if token.trim().is_empty() {
                            continue;
                        }
                    }
                    current.push(token.to_string(), span.style, span.link.clone());
                    continue;
                }
                for grapheme in token.graphemes(true) {
                    let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
                    if current.width() > 0 && current.width() + grapheme_width > width {
                        output.push(std::mem::take(&mut current));
                    }
                    current.push(grapheme.to_string(), span.style, span.link.clone());
                }
            }
        }
        output.push(current);
    }
    output
}

fn highlight_field_labels(lines: Vec<StyledLine>) -> Vec<StyledLine> {
    lines
        .into_iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>();
            let Some((start, end)) = field_label_range(&text) else {
                return line;
            };
            highlight_byte_range(line, start, end)
        })
        .collect()
}

fn field_label_range(text: &str) -> Option<(usize, usize)> {
    let start = text
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))?
        .0;
    if UnicodeWidthStr::width(&text[..start]) > 4 {
        return None;
    }

    let mut label_width = 0;
    let mut has_label_char = false;
    for (index, ch) in text[start..].char_indices() {
        let absolute = start + index;
        if matches!(ch, ':' | '：') {
            if !has_label_char || label_width == 0 || label_width > 18 {
                return None;
            }
            let end = absolute + ch.len_utf8();
            if text[end..].starts_with("//") {
                return None;
            }
            return Some((start, end));
        }
        if !is_field_label_char(ch) {
            return None;
        }
        if !ch.is_whitespace() {
            has_label_char = true;
        }
        label_width += ch.width().unwrap_or_default();
    }
    None
}

fn is_field_label_char(ch: char) -> bool {
    ch.is_alphanumeric()
        || ch.is_whitespace()
        || matches!(ch, '_' | '-' | '/' | '&' | '+' | '#' | '.' | '%' | '$')
}

fn highlight_byte_range(line: StyledLine, start: usize, end: usize) -> StyledLine {
    let mut spans = Vec::new();
    let mut consumed = 0;
    for span in line.spans {
        let span_start = consumed;
        let span_end = span_start + span.text.len();
        consumed = span_end;

        if span_end <= start || span_start >= end {
            spans.push(span);
            continue;
        }

        let local_start = start.saturating_sub(span_start);
        let local_end = (end.min(span_end)).saturating_sub(span_start);

        if local_start > 0 {
            spans.push(StyledSpan {
                text: span.text[..local_start].to_string(),
                style: span.style,
                link: span.link.clone(),
            });
        }
        if local_start < local_end {
            spans.push(StyledSpan {
                text: span.text[local_start..local_end].to_string(),
                style: span.style.patch(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                link: span.link.clone(),
            });
        }
        if local_end < span.text.len() {
            spans.push(StyledSpan {
                text: span.text[local_end..].to_string(),
                style: span.style,
                link: span.link,
            });
        }
    }
    StyledLine { spans }
}

fn osc8_target(destination: &str) -> Option<String> {
    if destination.chars().any(char::is_control) {
        return None;
    }
    let destination = destination.trim();
    let target = if destination.starts_with('/') {
        format!("file://{destination}")
    } else if destination.starts_with("http://")
        || destination.starts_with("https://")
        || destination.starts_with("file://")
        || destination.starts_with("mailto:")
    {
        destination.to_string()
    } else {
        return None;
    };
    Some(target.replace(' ', "%20"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    #[test]
    fn markdown_styles_heading_emphasis_list_and_link() {
        let lines = render_markdown(
            "## Title\n\n- **bold** and *italic* [site](https://example.com)",
            Style::default(),
            80,
        );
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        assert!(spans.iter().any(|span| {
            span.text.contains("Title") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| span.text.contains("• ")));
        assert!(spans.iter().any(|span| {
            span.text.contains("bold") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.text.contains("italic") && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(spans.iter().any(|span| {
            span.text.contains("site") && span.link.as_deref() == Some("https://example.com")
        }));
    }

    #[test]
    fn markdown_highlights_field_labels_in_wrapped_records() {
        let lines = render_markdown(
            "公司：英伟达\n代码: NVDA\n核心逻辑：数据中心 GPU 需求持续超预期",
            Style::default(),
            80,
        );
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        for label in ["公司：", "代码:", "核心逻辑："] {
            let span = spans
                .iter()
                .find(|span| span.text == label)
                .expect("field label span");
            assert_eq!(span.style.fg, Some(Color::Cyan));
            assert!(span.style.add_modifier.contains(Modifier::BOLD));
        }
        let value = spans
            .iter()
            .find(|span| span.text.contains("英伟达"))
            .expect("value span");
        assert_ne!(value.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn markdown_does_not_highlight_urls_or_code_block_fields() {
        let lines = render_markdown(
            "https://example.com/path\n\n```text\n公司：英伟达\n```",
            Style::default(),
            80,
        );
        let highlighted = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.fg == Some(Color::Cyan))
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>();

        assert!(!highlighted.iter().any(|text| *text == "https:"));
        assert!(!highlighted.iter().any(|text| *text == "公司："));
    }

    #[test]
    fn osc8_link_is_stored_as_a_forced_width_cell() {
        let lines = render_markdown("[site](https://example.com)", Style::default(), 20);
        let mut terminal = Terminal::new(TestBackend::new(20, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                let rendered = lines.iter().map(StyledLine::to_ratatui).collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(rendered), frame.area());
                apply_osc8_links(frame, frame.area(), &lines);
            })
            .expect("draw");

        let cell = terminal.backend().buffer().cell((0, 0)).expect("cell");
        assert!(cell.symbol().contains("\x1b]8;;https://example.com"));
        assert!(matches!(cell.diff_option, CellDiffOption::ForcedWidth(_)));
    }

    #[test]
    fn osc8_link_preserves_wide_character_cell_positions() {
        let lines = render_markdown("[中文](file:///tmp/note.md)", Style::default(), 20);
        let mut terminal = Terminal::new(TestBackend::new(20, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                let rendered = lines.iter().map(StyledLine::to_ratatui).collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(rendered), frame.area());
                apply_osc8_links(frame, frame.area(), &lines);
            })
            .expect("draw");

        let first = terminal.backend().buffer().cell((0, 0)).expect("first");
        let second = terminal.backend().buffer().cell((2, 0)).expect("second");
        assert!(matches!(
            first.diff_option,
            CellDiffOption::ForcedWidth(width) if width.get() == 2
        ));
        assert!(second.symbol().contains("文"));
    }

    #[test]
    fn osc8_rejects_control_sequences_and_normalizes_file_paths() {
        assert_eq!(
            osc8_target("/tmp/My File.md"),
            Some("file:///tmp/My%20File.md".to_string())
        );
        assert_eq!(osc8_target("https://example.com/\u{1b}]8;;bad"), None);
    }
}
