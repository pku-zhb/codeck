use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionStatus {
    NeedsInput,
    Working,
    Completed,
    Failed,
}

impl SessionStatus {
    pub fn from_protocol(value: &Value) -> Self {
        match value.get("type").and_then(Value::as_str) {
            Some("active") => {
                let flags = value
                    .get("activeFlags")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str);
                if flags
                    .into_iter()
                    .any(|flag| matches!(flag, "waitingOnApproval" | "waitingOnUserInput"))
                {
                    Self::NeedsInput
                } else {
                    Self::Working
                }
            }
            Some("systemError") => Self::Failed,
            Some("idle" | "notLoaded") | None => Self::Completed,
            Some(_) => Self::Completed,
        }
    }

    pub fn sort_rank(self) -> u8 {
        match self {
            Self::NeedsInput => 0,
            Self::Working => 1,
            Self::Failed => 2,
            Self::Completed => 3,
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, Self::NeedsInput | Self::Working)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Thinking,
    Progress,
    Final,
    Question,
    System,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewVerbosity {
    #[default]
    Full,
    Progress,
    Final,
}

impl PreviewVerbosity {
    pub fn includes(self, kind: MessageKind) -> bool {
        match kind {
            MessageKind::Thinking => self == Self::Full,
            MessageKind::Progress => self != Self::Final,
            MessageKind::User
            | MessageKind::Final
            | MessageKind::Question
            | MessageKind::System => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    pub id: String,
    pub kind: MessageKind,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub preview: String,
    pub cwd: String,
    pub path: Option<String>,
    pub updated_at: i64,
    pub source: String,
    pub thread_source: Option<String>,
    pub status: SessionStatus,
    pub active_turn_id: Option<String>,
    pub messages: Vec<MessageEntry>,
    pub history_loaded: bool,
}

impl Session {
    pub fn from_thread(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let preview = value
            .get("preview")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let title = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .or_else(|| preview.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("Untitled session")
            .trim()
            .to_string();
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let updated_at = value
            .get("recencyAt")
            .or_else(|| value.get("updatedAt"))
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let source = source_label(value.get("source"));
        let thread_source = value
            .get("threadSource")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let status = SessionStatus::from_protocol(value.get("status").unwrap_or(&Value::Null));

        Some(Self {
            id,
            title,
            preview,
            cwd,
            path,
            updated_at,
            source,
            thread_source,
            status,
            active_turn_id: None,
            messages: Vec::new(),
            history_loaded: false,
        })
    }

    pub fn leaf_directory(&self) -> String {
        Path::new(&self.cwd)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.cwd)
            .to_string()
    }

    pub fn upsert_message(&mut self, entry: MessageEntry) {
        if let Some(existing) = self.messages.iter_mut().find(|item| item.id == entry.id) {
            *existing = entry;
        } else {
            self.messages.push(entry);
        }
    }

    pub fn append_delta(&mut self, id: &str, kind: MessageKind, delta: &str) {
        if let Some(existing) = self.messages.iter_mut().find(|item| item.id == id) {
            existing.text.push_str(delta);
            return;
        }
        self.messages.push(MessageEntry {
            id: id.to_string(),
            kind,
            text: delta.to_string(),
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeTarget {
    NewTask,
    Reply,
    Rename,
}

#[derive(Debug, Clone)]
pub struct Composer {
    pub text: String,
    pub cursor: usize,
    pub target: ComposeTarget,
    pub images: Vec<PathBuf>,
    pub skills: Vec<SkillReference>,
    pub tokens: Vec<ComposerToken>,
    next_image_number: usize,
    next_paste_number: usize,
    vertical_column: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReference {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerToken {
    pub start: usize,
    pub style_end: usize,
    pub end: usize,
    pub kind: ComposerTokenKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerTokenKind {
    Skill(SkillReference),
    Image(PathBuf),
    PastedText(String),
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            target: ComposeTarget::NewTask,
            images: Vec::new(),
            skills: Vec::new(),
            tokens: Vec::new(),
            next_image_number: 1,
            next_paste_number: 1,
            vertical_column: None,
        }
    }
}

impl Composer {
    pub fn insert(&mut self, text: &str) {
        self.vertical_column = None;
        let position = self.cursor;
        self.shift_tokens_for_insertion(position, text.len());
        self.text.insert_str(position, text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        self.vertical_column = None;
        if self.cursor == 0 {
            return;
        }
        if let Some(index) = self
            .tokens
            .iter()
            .position(|token| token.end == self.cursor)
        {
            self.remove_token(index);
            return;
        }
        let previous = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.delete_plain_range(previous, self.cursor);
    }

    pub fn delete(&mut self) {
        self.vertical_column = None;
        if self.cursor >= self.text.len() {
            return;
        }
        if let Some(index) = self
            .tokens
            .iter()
            .position(|token| token.start == self.cursor)
        {
            self.remove_token(index);
            return;
        }
        let next = self.text[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
        self.delete_plain_range(self.cursor, next);
    }

    pub fn move_left(&mut self) {
        self.vertical_column = None;
        if let Some(token) = self.tokens.iter().find(|token| token.end == self.cursor) {
            self.cursor = token.start;
            return;
        }
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    pub fn move_right(&mut self) {
        self.vertical_column = None;
        if self.cursor >= self.text.len() {
            return;
        }
        if let Some(token) = self.tokens.iter().find(|token| token.start == self.cursor) {
            self.cursor = token.end;
            return;
        }
        self.cursor = self.text[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
    }

    pub fn move_to_start(&mut self) {
        self.vertical_column = None;
        self.cursor = 0;
    }

    pub fn move_to_end(&mut self) {
        self.vertical_column = None;
        self.cursor = self.text.len();
    }

    pub fn move_vertical(&mut self, direction: isize, width: usize, prefix_width: usize) -> bool {
        let positions = self.visual_cursor_positions(width, prefix_width);
        let Some((_, current_row, current_column)) = positions
            .iter()
            .find(|(byte, _, _)| *byte == self.cursor)
            .copied()
        else {
            return false;
        };
        let Some(target_row) = current_row.checked_add_signed(direction) else {
            return false;
        };
        let desired_column = self.vertical_column.unwrap_or(current_column);
        let Some((target, _, _)) = positions
            .into_iter()
            .filter(|(_, row, _)| *row == target_row)
            .min_by_key(|(_, _, column)| column.abs_diff(desired_column))
        else {
            return false;
        };
        self.cursor = target;
        self.vertical_column = Some(desired_column);
        true
    }

    pub fn prompt_text(&self) -> String {
        let mut text = self.text.clone();
        let mut replacements = self
            .tokens
            .iter()
            .filter(|token| {
                matches!(
                    &token.kind,
                    ComposerTokenKind::Image(_) | ComposerTokenKind::PastedText(_)
                )
            })
            .collect::<Vec<_>>();
        replacements.sort_by_key(|token| std::cmp::Reverse(token.start));
        for token in replacements {
            let replacement = match &token.kind {
                ComposerTokenKind::Image(_) => "",
                ComposerTokenKind::PastedText(value) => value,
                ComposerTokenKind::Skill(_) => continue,
            };
            text.replace_range(token.start..token.end, replacement);
        }
        text
    }

    pub fn reset(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.images.clear();
        self.skills.clear();
        self.tokens.clear();
        self.next_image_number = 1;
        self.next_paste_number = 1;
        self.vertical_column = None;
    }

    pub fn clear_text_keep_images(&mut self) {
        let images = self.images.clone();
        self.reset();
        for image in images {
            self.attach_image(image);
        }
    }

    pub fn attach_image(&mut self, path: PathBuf) -> bool {
        if self.images.contains(&path) {
            return false;
        }
        let placeholder = format!("[Image #{}] ", self.next_image_number);
        self.next_image_number = self.next_image_number.saturating_add(1);
        let start = self.cursor;
        self.insert(&placeholder);
        let end = self.cursor;
        self.tokens.push(ComposerToken {
            start,
            style_end: end.saturating_sub(1),
            end,
            kind: ComposerTokenKind::Image(path.clone()),
        });
        self.tokens.sort_by_key(|token| token.start);
        self.images.push(path);
        true
    }

    pub fn attach_pasted_text(&mut self, text: String) -> bool {
        if text.is_empty() {
            return false;
        }
        let line_count = text.split('\n').count();
        let detail = if line_count > 1 {
            format!("{line_count} lines")
        } else {
            format!("{} chars", text.chars().count())
        };
        let placeholder = format!("[Pasted text #{} +{detail}] ", self.next_paste_number);
        self.next_paste_number = self.next_paste_number.saturating_add(1);
        let start = self.cursor;
        self.insert(&placeholder);
        let end = self.cursor;
        self.tokens.push(ComposerToken {
            start,
            style_end: end.saturating_sub(1),
            end,
            kind: ComposerTokenKind::PastedText(text),
        });
        self.tokens.sort_by_key(|token| token.start);
        true
    }

    pub fn replace_with_skill(&mut self, start: usize, skill: SkillReference) {
        self.vertical_column = None;
        if start > self.cursor || self.cursor > self.text.len() {
            return;
        }
        let replacement = format!("${} ", skill.name);
        let style_end = start + replacement.len().saturating_sub(1);
        self.replace_plain_range(start, self.cursor, &replacement);
        let end = self.cursor;
        if !self
            .skills
            .iter()
            .any(|existing| existing.path == skill.path)
        {
            self.skills.push(skill.clone());
        }
        self.tokens.push(ComposerToken {
            start,
            style_end,
            end,
            kind: ComposerTokenKind::Skill(skill),
        });
        self.tokens.sort_by_key(|token| token.start);
    }

    pub fn replace_plain_text_before_cursor(&mut self, start: usize, replacement: &str) {
        self.vertical_column = None;
        if start > self.cursor || self.cursor > self.text.len() {
            return;
        }
        self.replace_plain_range(start, self.cursor, replacement);
    }

    fn visual_cursor_positions(
        &self,
        width: usize,
        prefix_width: usize,
    ) -> Vec<(usize, usize, usize)> {
        let width = width.max(1);
        let mut positions = Vec::new();
        let mut row = prefix_width / width;
        let mut column = prefix_width % width;
        for (byte, grapheme) in self.text.grapheme_indices(true) {
            if self.is_cursor_stop(byte) {
                positions.push((byte, row, column));
            }
            if grapheme == "\n" {
                row += 1;
                column = 0;
                continue;
            }
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if column > 0 && column + grapheme_width > width {
                row += 1;
                column = 0;
            }
            column += grapheme_width;
            if column >= width {
                row += 1;
                column = 0;
            }
        }
        if self.is_cursor_stop(self.text.len()) {
            positions.push((self.text.len(), row, column));
        }
        positions
    }

    fn is_cursor_stop(&self, byte: usize) -> bool {
        !self
            .tokens
            .iter()
            .any(|token| token.start < byte && byte < token.end)
    }

    fn shift_tokens_for_insertion(&mut self, position: usize, length: usize) {
        for token in &mut self.tokens {
            if token.start >= position {
                token.start = token.start.saturating_add(length);
                token.style_end = token.style_end.saturating_add(length);
                token.end = token.end.saturating_add(length);
            }
        }
    }

    fn delete_plain_range(&mut self, start: usize, end: usize) {
        self.text.replace_range(start..end, "");
        let removed = end.saturating_sub(start);
        for token in &mut self.tokens {
            if token.start >= end {
                token.start = token.start.saturating_sub(removed);
                token.style_end = token.style_end.saturating_sub(removed);
                token.end = token.end.saturating_sub(removed);
            }
        }
        self.cursor = start;
    }

    fn replace_plain_range(&mut self, start: usize, end: usize, replacement: &str) {
        self.text.replace_range(start..end, replacement);
        let removed = end.saturating_sub(start);
        let added = replacement.len();
        for token in &mut self.tokens {
            if token.start >= end {
                if added >= removed {
                    let shift = added - removed;
                    token.start = token.start.saturating_add(shift);
                    token.style_end = token.style_end.saturating_add(shift);
                    token.end = token.end.saturating_add(shift);
                } else {
                    let shift = removed - added;
                    token.start = token.start.saturating_sub(shift);
                    token.style_end = token.style_end.saturating_sub(shift);
                    token.end = token.end.saturating_sub(shift);
                }
            }
        }
        self.cursor = start + added;
    }

    fn remove_token(&mut self, index: usize) {
        let token = self.tokens.remove(index);
        let removed = token.end.saturating_sub(token.start);
        self.text.replace_range(token.start..token.end, "");
        for remaining in &mut self.tokens {
            if remaining.start >= token.end {
                remaining.start = remaining.start.saturating_sub(removed);
                remaining.style_end = remaining.style_end.saturating_sub(removed);
                remaining.end = remaining.end.saturating_sub(removed);
            }
        }
        match token.kind {
            ComposerTokenKind::Image(path) => {
                if !self.tokens.iter().any(
                    |remaining| matches!(&remaining.kind, ComposerTokenKind::Image(value) if value == &path),
                ) {
                    self.images.retain(|image| image != &path);
                }
            }
            ComposerTokenKind::Skill(skill) => {
                if !self.tokens.iter().any(
                    |remaining| matches!(&remaining.kind, ComposerTokenKind::Skill(value) if value.path == skill.path),
                ) {
                    self.skills.retain(|value| value.path != skill.path);
                }
            }
            ComposerTokenKind::PastedText(_) => {}
        }
        self.cursor = token.start;
    }
}

fn source_label(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(source)) => source.clone(),
        Some(Value::Object(map)) => map
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string()),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn protocol_status_prefers_waiting_flags() {
        assert_eq!(
            SessionStatus::from_protocol(&json!({
                "type": "active",
                "activeFlags": ["waitingOnUserInput"]
            })),
            SessionStatus::NeedsInput
        );
        assert_eq!(
            SessionStatus::from_protocol(&json!({
                "type": "active",
                "activeFlags": []
            })),
            SessionStatus::Working
        );
        assert_eq!(
            SessionStatus::from_protocol(&json!({ "type": "idle" })),
            SessionStatus::Completed
        );
    }

    #[test]
    fn composer_edits_cjk_and_emoji_by_grapheme() {
        let mut composer = Composer::default();
        composer.insert("你好🧠");
        composer.backspace();
        assert_eq!(composer.text, "你好");
        composer.move_left();
        composer.delete();
        assert_eq!(composer.text, "你");
    }

    #[test]
    fn composer_moves_vertically_across_wrapped_visual_lines() {
        let mut composer = Composer::default();
        composer.insert("abcdefghij");

        assert!(composer.move_vertical(-1, 6, 2));
        assert_eq!(composer.cursor, 4);
        assert!(composer.move_vertical(1, 6, 2));
        assert_eq!(composer.cursor, composer.text.len());
    }

    #[test]
    fn composer_treats_selected_skill_as_an_atomic_token() {
        let mut composer = Composer::default();
        composer.insert("$doc");
        composer.replace_with_skill(
            0,
            SkillReference {
                name: "documents".to_string(),
                path: "/tmp/documents/SKILL.md".to_string(),
            },
        );
        assert_eq!(composer.text, "$documents ");
        assert_eq!(composer.skills.len(), 1);
        assert_eq!(composer.tokens.len(), 1);

        composer.move_left();
        assert_eq!(composer.cursor, 0);
        composer.move_right();
        assert_eq!(composer.cursor, composer.text.len());
        composer.backspace();
        assert!(composer.text.is_empty());
        assert!(composer.skills.is_empty());
        assert!(composer.tokens.is_empty());
    }

    #[test]
    fn composer_replaces_plain_command_text_before_the_cursor() {
        let mut composer = Composer::default();
        composer.insert("/c");

        composer.replace_plain_text_before_cursor(0, "/clear");

        assert_eq!(composer.text, "/clear");
        assert_eq!(composer.cursor, "/clear".len());
        assert!(composer.tokens.is_empty());
    }

    #[test]
    fn composer_inserts_images_at_the_cursor_and_strips_placeholders_from_prompt() {
        let mut composer = Composer::default();
        composer.insert("before after");
        composer.cursor = "before ".len();
        let image = PathBuf::from("/tmp/chart.png");

        assert!(composer.attach_image(image.clone()));
        assert_eq!(composer.text, "before [Image #1] after");
        assert_eq!(composer.prompt_text(), "before after");
        assert_eq!(composer.images, vec![image]);

        composer.backspace();
        assert_eq!(composer.text, "before after");
        assert!(composer.images.is_empty());
        assert!(composer.tokens.is_empty());
    }

    #[test]
    fn composer_shifts_atomic_tokens_when_editing_before_them() {
        let mut composer = Composer::default();
        composer.attach_image(PathBuf::from("/tmp/chart.png"));
        composer.cursor = 0;
        composer.insert("look ");

        assert_eq!(composer.tokens[0].start, "look ".len());
        composer.cursor = composer.tokens[0].start;
        composer.delete();
        assert_eq!(composer.text, "look ");
        assert!(composer.images.is_empty());
    }

    #[test]
    fn composer_collapses_pasted_text_but_expands_it_for_the_prompt() {
        let mut composer = Composer::default();
        composer.insert("before after");
        composer.cursor = "before ".len();
        let pasted = "first line\nsecond line".to_string();

        assert!(composer.attach_pasted_text(pasted.clone()));
        assert_eq!(composer.text, "before [Pasted text #1 +2 lines] after");
        assert_eq!(composer.prompt_text(), format!("before {pasted}after"));

        composer.backspace();
        assert_eq!(composer.text, "before after");
        assert!(composer.tokens.is_empty());
    }
}
