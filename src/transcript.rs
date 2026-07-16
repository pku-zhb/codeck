use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::model::{MessageEntry, MessageKind};

pub const FULL_HISTORY_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
pub const TAIL_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = TAIL_PREVIEW_BYTES as usize;

pub struct BoundedPreview {
    pub file_bytes: u64,
    pub messages: Vec<MessageEntry>,
}

pub fn load_bounded_preview_if_large(path: &Path) -> io::Result<Option<BoundedPreview>> {
    let file_bytes = path.metadata()?.len();
    if file_bytes <= FULL_HISTORY_LIMIT_BYTES {
        return Ok(None);
    }

    let start = file_bytes.saturating_sub(TAIL_PREVIEW_BYTES);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let read_bytes = file_bytes.saturating_sub(start).min(TAIL_PREVIEW_BYTES);
    let mut bytes = Vec::with_capacity(read_bytes as usize);
    file.take(TAIL_PREVIEW_BYTES).read_to_end(&mut bytes)?;
    let messages = parse_tail(&bytes, start > 0);
    Ok(Some(BoundedPreview {
        file_bytes,
        messages,
    }))
}

fn parse_tail(bytes: &[u8], discard_leading_partial: bool) -> Vec<MessageEntry> {
    let bytes = if discard_leading_partial {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| &bytes[index + 1..])
            .unwrap_or_default()
    } else {
        bytes
    };
    let mut messages = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || line.len() > MAX_RECORD_BYTES {
            continue;
        }
        let header = String::from_utf8_lossy(&line[..line.len().min(512)]);
        if is_noisy_record(&header) {
            continue;
        }
        let Ok(envelope) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(payload) = envelope.get("payload") else {
            continue;
        };
        let payload_type = payload.get("type").and_then(Value::as_str);
        match envelope.get("type").and_then(Value::as_str) {
            Some("event_msg") => parse_event(payload_type, payload, &mut messages),
            Some("response_item") => parse_response_item(payload_type, payload, &mut messages),
            _ => {}
        }
    }
    messages
}

fn is_noisy_record(header: &str) -> bool {
    [
        "custom_tool_call",
        "custom_tool_call_output",
        "function_call",
        "function_call_output",
        "local_shell_call",
        "local_shell_call_output",
    ]
    .iter()
    .any(|kind| header.contains(&format!("\"type\":\"{kind}\"")))
}

fn parse_event(payload_type: Option<&str>, payload: &Value, messages: &mut Vec<MessageEntry>) {
    match payload_type {
        Some("task_started") => {}
        Some("user_message") => push_text(
            messages,
            MessageKind::User,
            payload.get("message").and_then(Value::as_str),
        ),
        Some("agent_message") => {
            let kind = match payload.get("phase").and_then(Value::as_str) {
                Some("commentary") => MessageKind::Progress,
                _ => MessageKind::Final,
            };
            push_text(
                messages,
                kind,
                payload.get("message").and_then(Value::as_str),
            );
        }
        Some("task_complete") => push_text(
            messages,
            MessageKind::Final,
            payload.get("last_agent_message").and_then(Value::as_str),
        ),
        Some("turn_aborted") => push_text(
            messages,
            MessageKind::System,
            Some("Turn aborted while this session was in the background"),
        ),
        _ => {}
    }
}

fn parse_response_item(
    payload_type: Option<&str>,
    payload: &Value,
    messages: &mut Vec<MessageEntry>,
) {
    match payload_type {
        Some("message") => {
            let kind = match payload.get("role").and_then(Value::as_str) {
                Some("user") => MessageKind::User,
                Some("assistant")
                    if payload.get("phase").and_then(Value::as_str) == Some("commentary") =>
                {
                    MessageKind::Progress
                }
                Some("assistant") => MessageKind::Final,
                _ => return,
            };
            let text = text_from_value(payload.get("content"));
            push_text(messages, kind, text.as_deref());
        }
        Some("reasoning") => {
            let text = text_from_value(payload.get("summary"));
            push_text(messages, MessageKind::Thinking, text.as_deref());
        }
        Some("agent_message") => {
            let kind = if payload.get("phase").and_then(Value::as_str) == Some("commentary") {
                MessageKind::Progress
            } else {
                MessageKind::Final
            };
            let text = payload
                .get("message")
                .or_else(|| payload.get("text"))
                .and_then(Value::as_str);
            push_text(messages, kind, text);
        }
        _ => {}
    }
}

fn text_from_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(text) => Some(text.as_str()),
                    Value::Object(object) => ["text", "input_text", "output_text"]
                        .iter()
                        .find_map(|key| object.get(*key).and_then(Value::as_str)),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn push_text(messages: &mut Vec<MessageEntry>, kind: MessageKind, text: Option<&str>) {
    let Some(text) = normalize_text(text) else {
        return;
    };
    if messages
        .iter()
        .rev()
        .take(4)
        .any(|message| message.kind == kind && message.text == text)
    {
        return;
    }
    messages.push(MessageEntry {
        id: format!("tail-{}", messages.len()),
        kind,
        text,
    });
}

fn normalize_text(text: Option<&str>) -> Option<String> {
    let text = text?.replace("\r\n", "\n").replace('\r', "\n");
    let text = text
        .split("<oai-mem-citation>")
        .next()
        .unwrap_or(&text)
        .trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn line(envelope_type: &str, payload: Value) -> String {
        json!({ "type": envelope_type, "payload": payload }).to_string()
    }

    #[test]
    fn bounded_tail_keeps_all_user_visible_turns_in_the_window() {
        let data = [
            "partial-record".to_string(),
            line(
                "event_msg",
                json!({ "type": "user_message", "message": "上一轮任务" }),
            ),
            line(
                "event_msg",
                json!({ "type": "task_complete", "last_agent_message": "上一轮完成" }),
            ),
            line("event_msg", json!({ "type": "task_started" })),
            line(
                "response_item",
                json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "最新任务" }] }),
            ),
            line(
                "response_item",
                json!({ "type": "custom_tool_call_output", "output": "noise" }),
            ),
            line(
                "response_item",
                json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": "正在检查" }] }),
            ),
            line(
                "event_msg",
                json!({ "type": "agent_message", "phase": "commentary", "message": "完成一半" }),
            ),
            line(
                "event_msg",
                json!({ "type": "task_complete", "last_agent_message": "已经完成" }),
            ),
        ]
        .join("\n");

        let messages = parse_tail(data.as_bytes(), true);
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[0].text, "上一轮任务");
        assert_eq!(messages[1].text, "上一轮完成");
        assert_eq!(messages[2].kind, MessageKind::User);
        assert_eq!(messages[3].kind, MessageKind::Thinking);
        assert_eq!(messages[4].kind, MessageKind::Progress);
        assert_eq!(messages[5].kind, MessageKind::Final);
        assert_eq!(messages[5].text, "已经完成");
    }

    #[test]
    fn small_rollouts_keep_using_the_official_history_api() {
        let path =
            std::env::temp_dir().join(format!("codeck-small-transcript-{}", std::process::id()));
        std::fs::write(&path, b"{}\n").expect("write fixture");
        let result = load_bounded_preview_if_large(&path).expect("inspect fixture");
        std::fs::remove_file(path).expect("remove fixture");
        assert!(result.is_none());
    }

    #[test]
    fn bounded_tail_keeps_more_than_the_previous_message_and_text_caps() {
        assert_eq!(TAIL_PREVIEW_BYTES, 64 * 1024 * 1024);
        let long_text = "x".repeat(13_000);
        let mut records = (0..30)
            .map(|index| {
                line(
                    "event_msg",
                    json!({ "type": "user_message", "message": format!("message-{index}") }),
                )
            })
            .collect::<Vec<_>>();
        records.push(line(
            "event_msg",
            json!({ "type": "agent_message", "message": long_text }),
        ));

        let messages = parse_tail(records.join("\n").as_bytes(), false);

        assert_eq!(messages.len(), 31);
        assert_eq!(messages.last().expect("long message").text.len(), 13_000);
    }

    #[test]
    fn oversized_rollout_reads_only_its_bounded_tail() {
        let path =
            std::env::temp_dir().join(format!("codeck-large-transcript-{}", std::process::id()));
        let mut file = File::create(&path).expect("create fixture");
        file.set_len(FULL_HISTORY_LIMIT_BYTES + 1)
            .expect("make sparse fixture");
        file.seek(SeekFrom::End(0)).expect("seek fixture");
        writeln!(file, "discarded partial record").expect("write partial");
        writeln!(
            file,
            "{}",
            line(
                "event_msg",
                json!({ "type": "agent_message", "phase": "final_answer", "message": "bounded result" })
            )
        )
        .expect("write message");
        drop(file);

        let preview = load_bounded_preview_if_large(&path)
            .expect("read fixture")
            .expect("bounded preview");
        std::fs::remove_file(path).expect("remove fixture");
        assert_eq!(preview.messages.len(), 1);
        assert_eq!(preview.messages[0].kind, MessageKind::Final);
        assert_eq!(preview.messages[0].text, "bounded result");
    }
}
