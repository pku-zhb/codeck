use std::path::Path;

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

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
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            target: ComposeTarget::NewTask,
        }
    }
}

impl Composer {
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
        self.text.drain(self.cursor..next);
    }

    pub fn move_left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.cursor = self.text[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
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
}
