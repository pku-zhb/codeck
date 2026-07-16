use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value, json};

use crate::client::{ClientEvent, RpcSender};
use crate::clipboard::{image_paths_from_paste, save_clipboard_image};
use crate::lifecycle::LifecycleStore;
use crate::model::{ComposeTarget, Composer, MessageEntry, MessageKind, Session, SessionStatus};
use crate::transcript::{TAIL_PREVIEW_BYTES, load_bounded_preview_if_large};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const THREAD_SOURCE: &str = "codex-deck";

#[derive(Debug)]
enum PendingCall {
    Initialize,
    ThreadList {
        paginate: bool,
    },
    ThreadRead {
        thread_id: String,
    },
    ThreadStart {
        draft: PromptDraft,
    },
    ThreadResumeForReply {
        thread_id: String,
        draft: PromptDraft,
    },
    TurnStart {
        thread_id: String,
    },
    TurnSteer {
        thread_id: String,
    },
    TurnInterrupt {
        thread_id: String,
    },
    ThreadNameSet {
        thread_id: String,
        name: String,
    },
}

#[derive(Debug, Clone)]
struct InputOption {
    label: String,
    description: String,
}

#[derive(Debug, Clone)]
struct InputQuestion {
    id: String,
    header: String,
    question: String,
    options: Vec<InputOption>,
}

#[derive(Debug, Clone)]
enum PendingRequestKind {
    UserInput(Vec<InputQuestion>),
    CommandApproval,
    FileApproval,
    PermissionApproval,
    McpElicitation,
    Unsupported(String),
}

#[derive(Debug, Clone)]
struct PendingRequest {
    id: Value,
    thread_id: String,
    kind: PendingRequestKind,
}

#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub thread_id: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
struct PromptDraft {
    text: String,
    images: Vec<PathBuf>,
}

pub struct App {
    cwd: PathBuf,
    show_all: bool,
    lifecycle: LifecycleStore,
    initial_scan_seen: BTreeSet<String>,
    sessions: Vec<Session>,
    selected: usize,
    composer: Composer,
    new_draft: Composer,
    reply_drafts: HashMap<String, Composer>,
    pending_calls: HashMap<u64, PendingCall>,
    pending_requests: Vec<PendingRequest>,
    queued_replies: HashMap<String, String>,
    live_item_kinds: HashMap<(String, String), MessageKind>,
    initialized: bool,
    initial_load_complete: bool,
    list_inflight: bool,
    last_refresh: Instant,
    should_quit: bool,
    notice: String,
    scroll_back: usize,
    message_view_height: usize,
    attach_requested: Option<AttachRequest>,
    attach_right_armed: bool,
    rename_previous: Option<Composer>,
    rename_thread_id: Option<String>,
}

impl App {
    pub fn new(cwd: PathBuf, show_all: bool) -> Result<Self> {
        Ok(Self::with_lifecycle(
            cwd,
            show_all,
            LifecycleStore::load_default()?,
        ))
    }

    fn with_lifecycle(cwd: PathBuf, show_all: bool, lifecycle: LifecycleStore) -> Self {
        Self {
            cwd,
            show_all,
            lifecycle,
            initial_scan_seen: BTreeSet::new(),
            sessions: Vec::new(),
            selected: 0,
            composer: Composer::default(),
            new_draft: Composer::default(),
            reply_drafts: HashMap::new(),
            pending_calls: HashMap::new(),
            pending_requests: Vec::new(),
            queued_replies: HashMap::new(),
            live_item_kinds: HashMap::new(),
            initialized: false,
            initial_load_complete: false,
            list_inflight: false,
            last_refresh: Instant::now(),
            should_quit: false,
            notice: "Connecting to Codex…".to_string(),
            scroll_back: 0,
            message_view_height: 1,
            attach_requested: None,
            attach_right_armed: false,
            rename_previous: None,
            rename_thread_id: None,
        }
    }

    pub fn begin(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        let id = sender.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codex_deck",
                    "title": "Codex Deck",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )?;
        self.pending_calls.insert(id, PendingCall::Initialize);
        Ok(())
    }

    pub fn tick(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        if self.initialized
            && !self.list_inflight
            && self.last_refresh.elapsed() >= REFRESH_INTERVAL
        {
            self.request_thread_list(sender)?;
        }
        Ok(())
    }

    pub fn handle_client_event(
        &mut self,
        event: ClientEvent,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        match event {
            ClientEvent::Message(message) => self.handle_protocol_message(message, sender),
            ClientEvent::Warning(message) => {
                self.notice = message;
                Ok(())
            }
            ClientEvent::Disconnected(message) => {
                self.notice = format!("Disconnected: {message}");
                Ok(())
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, sender: &mut impl RpcSender) -> Result<()> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(());
        }

        let attach_right = key.code == KeyCode::Right
            && self.composer.text.is_empty()
            && self.composer.target != ComposeTarget::Rename
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !attach_right {
            self.attach_right_armed = false;
        }

        if key.modifiers.contains(KeyModifiers::SUPER) && key.code == KeyCode::Char('v') {
            self.paste_clipboard_image();
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return Ok(());
                }
                KeyCode::Char('n') => {
                    self.cancel_rename();
                    self.switch_to_new_draft();
                    self.notice = "New task".to_string();
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.start_rename();
                    return Ok(());
                }
                KeyCode::Char('u') => {
                    self.composer.text.clear();
                    self.composer.cursor = 0;
                    self.composer.images.clear();
                    return Ok(());
                }
                KeyCode::Char('v') => {
                    self.paste_clipboard_image();
                    return Ok(());
                }
                KeyCode::Char('t') => {
                    self.toggle_pin_selected()?;
                    return Ok(());
                }
                KeyCode::Char('x') => {
                    self.stop_or_remove_selected(sender)?;
                    return Ok(());
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Up => self.move_selection(-1, sender)?,
            KeyCode::Down => self.move_selection(1, sender)?,
            KeyCode::PageUp => {
                self.scroll_back = self
                    .scroll_back
                    .saturating_add(self.message_view_height.saturating_sub(1).max(1));
            }
            KeyCode::PageDown => {
                self.scroll_back = self
                    .scroll_back
                    .saturating_sub(self.message_view_height.saturating_sub(1).max(1));
            }
            KeyCode::Tab | KeyCode::BackTab => self.toggle_compose_target(),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.composer.insert("\n")
            }
            KeyCode::Enter if self.composer.target == ComposeTarget::Rename => {
                self.submit(sender)?
            }
            KeyCode::Enter => self.submit(sender)?,
            KeyCode::Backspace
                if self.composer.text.is_empty() && !self.composer.images.is_empty() =>
            {
                self.composer.images.pop();
                self.notice = "Removed last image".to_string();
            }
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right if attach_right && key.kind == KeyEventKind::Repeat => {}
            KeyCode::Right if attach_right => self.confirm_attach()?,
            KeyCode::Right => self.composer.move_right(),
            KeyCode::Home => self.composer.cursor = 0,
            KeyCode::End => self.composer.cursor = self.composer.text.len(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.composer.insert(&character.to_string())
            }
            _ => {}
        }
        Ok(())
    }

    pub fn insert_text(&mut self, text: &str) {
        self.attach_right_armed = false;
        self.composer.insert(text);
    }

    pub fn insert_paste(&mut self, text: &str) {
        if text.is_empty() {
            self.paste_clipboard_image();
            return;
        }
        let paths = image_paths_from_paste(text);
        if paths.is_empty() {
            self.insert_text(text);
        } else {
            self.add_images(paths);
        }
    }

    fn paste_clipboard_image(&mut self) {
        match save_clipboard_image() {
            Ok(Some(path)) => self.add_images(vec![path]),
            Ok(None) => {
                self.notice = "Clipboard does not contain an image".to_string();
            }
            Err(error) => {
                self.notice = format!("Could not paste clipboard image: {error:#}");
            }
        }
    }

    fn add_images(&mut self, paths: Vec<PathBuf>) {
        let before = self.composer.images.len();
        for path in paths {
            if !self.composer.images.contains(&path) {
                self.composer.images.push(path);
            }
        }
        let added = self.composer.images.len().saturating_sub(before);
        self.attach_right_armed = false;
        self.notice = if added == 0 {
            "Image is already attached".to_string()
        } else {
            format!(
                "Attached {added} image{} · Backspace on empty input removes the last one",
                if added == 1 { "" } else { "s" }
            )
        };
    }

    fn restore_draft(&mut self, draft: PromptDraft) {
        self.composer.text = draft.text;
        self.composer.cursor = self.composer.text.len();
        self.composer.images = draft.images;
    }

    pub fn take_attach_request(&mut self) -> Option<AttachRequest> {
        self.attach_requested.take()
    }

    pub fn refresh_after_attach(
        &mut self,
        thread_id: &str,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        self.select_session(thread_id);
        self.load_history(thread_id, sender)?;
        self.scroll_back = 0;
        if !matches!(
            self.notice.as_str(),
            "Large session: bounded preview" | "Transcript preview unavailable"
        ) {
            self.notice = "Returned from attached session".to_string();
        }
        Ok(())
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = notice.into();
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn selected_index(&self) -> usize {
        self.selected.min(self.sessions.len().saturating_sub(1))
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.sessions.get(self.selected_index())
    }

    pub fn composer(&self) -> &Composer {
        &self.composer
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    pub fn scroll_back(&self) -> usize {
        self.scroll_back
    }

    pub fn set_scroll_back(&mut self, value: usize) {
        self.scroll_back = value;
    }

    pub fn scroll_preview(&mut self, lines: isize) {
        if lines >= 0 {
            self.scroll_back = self.scroll_back.saturating_add(lines as usize);
        } else {
            self.scroll_back = self.scroll_back.saturating_sub(lines.unsigned_abs());
        }
    }

    pub fn set_message_view_height(&mut self, value: usize) {
        self.message_view_height = value.max(1);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn initial_load_complete(&self) -> bool {
        self.initial_load_complete
    }

    pub fn selected_has_pending_request(&self) -> bool {
        self.selected_session()
            .map(|session| self.has_pending_request(&session.id))
            .unwrap_or(false)
    }

    fn handle_protocol_message(
        &mut self,
        message: Value,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        let method = message.get("method").and_then(Value::as_str);
        let has_id = message.get("id").is_some();
        match (method, has_id) {
            (Some(_), true) => self.handle_server_request(message, sender),
            (Some(method), false) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                self.handle_notification(method, params, sender)
            }
            (None, true) => self.handle_response(message, sender),
            _ => Ok(()),
        }
    }

    fn handle_response(&mut self, message: Value, sender: &mut impl RpcSender) -> Result<()> {
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return Ok(());
        };
        let Some(call) = self.pending_calls.remove(&id) else {
            return Ok(());
        };
        if let Some(error) = message.get("error") {
            if matches!(call, PendingCall::ThreadList { .. }) {
                self.list_inflight = false;
            }
            if matches!(&call, PendingCall::ThreadNameSet { .. }) {
                self.cancel_rename();
            }
            self.notice = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex request failed")
                .to_string();
            return Ok(());
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);

        match call {
            PendingCall::Initialize => {
                sender.notify("initialized", json!({}))?;
                self.initialized = true;
                self.notice = "Connected".to_string();
                self.request_thread_list(sender)?;
            }
            PendingCall::ThreadList { paginate } => {
                self.merge_thread_list(&result);
                if paginate
                    && let Some(next_cursor) = result
                        .get("nextCursor")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                {
                    self.notice = format!("Loading {} sessions…", self.sessions.len());
                    self.request_thread_list_page(sender, Some(next_cursor), true)?;
                    return Ok(());
                }
                let first_load = !self.initial_load_complete;
                if first_load {
                    self.lifecycle.finish_initial_scan(&self.initial_scan_seen);
                }
                self.lifecycle.save().context("save session lifecycle")?;
                self.list_inflight = false;
                self.initial_load_complete = true;
                self.ensure_selected_history(sender)?;
                if first_load {
                    if self.sessions.is_empty() {
                        self.notice = "Type a task and press Enter".to_string();
                    } else {
                        self.notice.clear();
                    }
                }
            }
            PendingCall::ThreadRead { thread_id } => {
                if let Some(thread) = result.get("thread") {
                    self.apply_thread_history(&thread_id, thread);
                }
            }
            PendingCall::ThreadStart { draft } => {
                let thread = result
                    .get("thread")
                    .context("thread/start response missing thread")?;
                let session = Session::from_thread(thread)
                    .context("thread/start response has invalid thread")?;
                let thread_id = session.id.clone();
                self.track_session(&thread_id)?;
                self.merge_session(session);
                self.cache_current_draft();
                self.select_session(&thread_id);
                self.load_reply_draft(&thread_id);
                self.start_turn(&thread_id, &draft, sender)?;
            }
            PendingCall::ThreadResumeForReply { thread_id, draft } => {
                let thread = result.get("thread").unwrap_or(&Value::Null);
                self.apply_resumed_thread(&thread_id, thread);
                let status = thread
                    .get("status")
                    .map(SessionStatus::from_protocol)
                    .unwrap_or(SessionStatus::Completed);
                if status == SessionStatus::NeedsInput {
                    if !draft.images.is_empty() {
                        self.add_images(draft.images);
                    }
                    let prompt = draft.text;
                    if prompt.is_empty() {
                        self.notice =
                            "Session needs a text answer; attached images were kept".to_string();
                        return Ok(());
                    }
                    if self.has_pending_request(&thread_id) {
                        self.answer_pending_request(&thread_id, &prompt, sender)?;
                    } else {
                        self.queued_replies.insert(thread_id.clone(), prompt);
                        self.notice = "Waiting for Codex question details…".to_string();
                    }
                } else if status == SessionStatus::Working {
                    if let Some(turn_id) = active_turn_id(thread) {
                        self.steer_turn(&thread_id, &turn_id, &draft, sender)?;
                    } else {
                        self.notice = "Session is active; waiting for turn state".to_string();
                    }
                } else {
                    self.start_turn(&thread_id, &draft, sender)?;
                }
            }
            PendingCall::TurnStart { thread_id } => {
                if let Some(turn_id) = result
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                    && let Some(session) = self.session_mut(&thread_id)
                {
                    session.active_turn_id = Some(turn_id.to_string());
                    session.status = SessionStatus::Working;
                }
                self.notice = "Working in background".to_string();
                self.sort_sessions_preserving_selection();
            }
            PendingCall::TurnSteer { thread_id } => {
                self.notice = format!("Sent follow-up to {}", self.session_title(&thread_id));
            }
            PendingCall::TurnInterrupt { thread_id } => {
                self.notice = format!(
                    "Stopping {} · press Ctrl+X again after it completes to remove it",
                    self.session_title(&thread_id)
                );
            }
            PendingCall::ThreadNameSet { thread_id, name } => {
                if let Some(session) = self.session_mut(&thread_id) {
                    session.title = name.clone();
                }
                self.cancel_rename();
                self.notice = format!("Renamed session to {name}");
                self.sort_sessions_preserving_selection();
            }
        }
        Ok(())
    }

    fn handle_notification(
        &mut self,
        method: &str,
        params: Value,
        _sender: &mut impl RpcSender,
    ) -> Result<()> {
        match method {
            "thread/started" => {
                if let Some(thread) = params.get("thread")
                    && let Some(session) = Session::from_thread(thread)
                {
                    self.track_session(&session.id)?;
                    self.merge_session(session);
                }
            }
            "thread/status/changed" => {
                if let (Some(thread_id), Some(status)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params.get("status"),
                ) {
                    let status = SessionStatus::from_protocol(status);
                    if status.is_live() {
                        self.track_session(thread_id)?;
                    }
                    if let Some(session) = self.session_mut(thread_id) {
                        session.status = status;
                        self.sort_sessions_preserving_selection();
                    }
                }
            }
            "thread/name/updated" => {
                if let (Some(thread_id), Some(name)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params
                        .get("threadName")
                        .or_else(|| params.get("name"))
                        .and_then(Value::as_str),
                ) && let Some(session) = self.session_mut(thread_id)
                {
                    session.title = name.to_string();
                    self.sort_sessions_preserving_selection();
                }
            }
            "thread/deleted" | "thread/archived" => {
                if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                    let was_selected = self.selected_session().map(|session| session.id.as_str())
                        == Some(thread_id);
                    self.sessions.retain(|session| session.id != thread_id);
                    self.lifecycle.dismiss(thread_id);
                    self.lifecycle.save().context("save session lifecycle")?;
                    self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                    self.discard_reply_draft(thread_id, was_selected);
                }
            }
            "turn/started" => self.handle_turn_started(&params)?,
            "turn/completed" => self.handle_turn_completed(&params)?,
            "item/started" => self.handle_item(&params, false),
            "item/completed" => self.handle_item(&params, true),
            "item/agentMessage/delta" => self.handle_agent_delta(&params),
            "item/reasoning/summaryTextDelta" => self.handle_reasoning_delta(&params),
            "item/reasoning/summaryPartAdded" => self.handle_reasoning_part(&params),
            "error" | "warning" | "configWarning" | "deprecationNotice" => {
                self.notice = notification_message(&params).unwrap_or_else(|| method.to_string());
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_server_request(&mut self, message: Value, sender: &mut impl RpcSender) -> Result<()> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if thread_id.is_empty() {
            sender.respond_error(id, -32601, "codex-deck cannot route this request")?;
            self.notice = format!("Unsupported app-server request: {method}");
            return Ok(());
        }
        self.track_session(&thread_id)?;

        let kind = match method.as_str() {
            "item/tool/requestUserInput" => PendingRequestKind::UserInput(parse_questions(&params)),
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                PendingRequestKind::CommandApproval
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                PendingRequestKind::FileApproval
            }
            "item/permissions/requestApproval" => PendingRequestKind::PermissionApproval,
            "mcpServer/elicitation/request" => PendingRequestKind::McpElicitation,
            _ => PendingRequestKind::Unsupported(method.clone()),
        };

        let display = pending_request_display(&kind, &params);
        if let Some(session) = self.session_mut(&thread_id) {
            session.status = SessionStatus::NeedsInput;
            session.upsert_message(MessageEntry {
                id: format!("request-{}", rpc_id_label(&id)),
                kind: MessageKind::Question,
                text: display,
            });
        }
        self.pending_requests.push(PendingRequest {
            id,
            thread_id: thread_id.clone(),
            kind,
        });
        self.sort_sessions_preserving_selection();
        self.notice = "Selected session needs input".to_string();

        if let Some(reply) = self.queued_replies.remove(&thread_id) {
            self.answer_pending_request(&thread_id, &reply, sender)?;
        }
        Ok(())
    }

    fn request_thread_list(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        self.request_thread_list_page(sender, None, !self.initial_load_complete)
    }

    fn request_thread_list_page(
        &mut self,
        sender: &mut impl RpcSender,
        cursor: Option<String>,
        paginate: bool,
    ) -> Result<()> {
        let id = sender.request(
            "thread/list",
            json!({
                "cursor": cursor,
                "limit": 100,
                "sortKey": "recency_at",
                "sortDirection": "desc",
                "archived": false,
                "sourceKinds": ["cli", "vscode", "exec", "appServer"]
            }),
        )?;
        self.pending_calls
            .insert(id, PendingCall::ThreadList { paginate });
        self.list_inflight = true;
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn ensure_selected_history(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        let Some(session) = self.selected_session() else {
            return Ok(());
        };
        if session.history_loaded || self.pending_calls.values().any(|call| {
            matches!(call, PendingCall::ThreadRead { thread_id } if thread_id == &session.id)
        }) {
            return Ok(());
        }
        let thread_id = session.id.clone();
        self.load_history(&thread_id, sender)
    }

    fn load_history(&mut self, thread_id: &str, sender: &mut impl RpcSender) -> Result<()> {
        if self.load_bounded_history_if_large(thread_id) {
            return Ok(());
        }
        let id = sender.request(
            "thread/read",
            json!({ "threadId": thread_id, "includeTurns": true }),
        )?;
        self.pending_calls.insert(
            id,
            PendingCall::ThreadRead {
                thread_id: thread_id.to_string(),
            },
        );
        Ok(())
    }

    fn load_bounded_history_if_large(&mut self, thread_id: &str) -> bool {
        let Some(path) = self
            .session(thread_id)
            .and_then(|session| session.path.as_deref())
            .map(PathBuf::from)
        else {
            return false;
        };
        let preview = match load_bounded_preview_if_large(&path) {
            Ok(Some(preview)) => preview,
            Ok(None) => return false,
            Err(_) => {
                if let Some(session) = self.session_mut(thread_id) {
                    session.messages = vec![MessageEntry {
                        id: "bounded-preview-error".to_string(),
                        kind: MessageKind::System,
                        text: "Transcript preview is unavailable · attach to view the full history"
                            .to_string(),
                    }];
                    session.history_loaded = true;
                }
                self.notice = "Transcript preview unavailable".to_string();
                return true;
            }
        };

        let tail_mib = TAIL_PREVIEW_BYTES / (1024 * 1024);
        let file_mib = preview.file_bytes as f64 / (1024.0 * 1024.0);
        let mut messages = vec![MessageEntry {
            id: "bounded-preview".to_string(),
            kind: MessageKind::System,
            text: format!(
                "Large session ({file_mib:.1} MiB) · showing the latest {tail_mib} MiB · attach for full history"
            ),
        }];
        messages.extend(preview.messages);
        if let Some(session) = self.session_mut(thread_id) {
            messages.extend(
                session
                    .messages
                    .iter()
                    .filter(|message| message.kind == MessageKind::Question)
                    .cloned(),
            );
            session.messages = messages;
            session.history_loaded = true;
        }
        self.scroll_back = 0;
        self.notice = "Large session: bounded preview".to_string();
        true
    }

    fn submit(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        let text = self.composer.text.trim().to_string();
        let target = self.composer.target;
        if target == ComposeTarget::Rename {
            if text.is_empty() {
                return Ok(());
            }
            self.composer.take();
            let Some(thread_id) = self.rename_thread_id.clone() else {
                self.cancel_rename();
                self.notice = "Rename target is no longer available".to_string();
                return Ok(());
            };
            let id = sender.request(
                "thread/name/set",
                json!({ "threadId": thread_id, "name": text }),
            )?;
            self.pending_calls.insert(
                id,
                PendingCall::ThreadNameSet {
                    thread_id,
                    name: text,
                },
            );
            self.notice = "Renaming session…".to_string();
            return Ok(());
        }

        if target == ComposeTarget::Reply
            && let Some(thread_id) = self.selected_session().map(|session| session.id.clone())
            && self.has_pending_request(&thread_id)
        {
            if text.is_empty() {
                self.notice =
                    "This request needs a text answer; attached images were kept".to_string();
                return Ok(());
            }
            self.composer.take();
            self.cache_current_draft();
            self.answer_pending_request(&thread_id, &text, sender)?;
            return Ok(());
        }

        if text.is_empty() && self.composer.images.is_empty() {
            return Ok(());
        }
        self.composer.take();
        let draft = PromptDraft {
            text,
            images: self.composer.take_images(),
        };
        self.cache_current_draft();
        self.scroll_back = 0;

        match target {
            ComposeTarget::NewTask => {
                let id = sender.request(
                    "thread/start",
                    json!({
                        "cwd": self.cwd.to_string_lossy(),
                        "serviceName": "codex-deck",
                        "threadSource": THREAD_SOURCE,
                        "ephemeral": false
                    }),
                )?;
                self.pending_calls
                    .insert(id, PendingCall::ThreadStart { draft });
                self.notice = "Starting background session…".to_string();
            }
            ComposeTarget::Reply => {
                let Some(thread_id) = self.selected_session().map(|session| session.id.clone())
                else {
                    self.notice = "No session selected; switched to new task".to_string();
                    self.composer.target = ComposeTarget::NewTask;
                    self.restore_draft(draft);
                    self.cache_current_draft();
                    return Ok(());
                };
                self.track_session(&thread_id)?;

                let status = self
                    .session(&thread_id)
                    .map(|session| session.status)
                    .unwrap_or(SessionStatus::Completed);
                let active_turn_id = self
                    .session(&thread_id)
                    .and_then(|session| session.active_turn_id.clone());
                if status == SessionStatus::Working
                    && let Some(turn_id) = active_turn_id
                {
                    self.steer_turn(&thread_id, &turn_id, &draft, sender)?;
                } else {
                    let id = sender.request("thread/resume", json!({ "threadId": thread_id }))?;
                    self.pending_calls
                        .insert(id, PendingCall::ThreadResumeForReply { thread_id, draft });
                    self.notice = "Resuming session…".to_string();
                }
            }
            ComposeTarget::Rename => unreachable!("rename handled above"),
        }
        Ok(())
    }

    fn start_turn(
        &mut self,
        thread_id: &str,
        draft: &PromptDraft,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        let id = sender.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": prompt_input(draft),
                "summary": "auto"
            }),
        )?;
        self.pending_calls.insert(
            id,
            PendingCall::TurnStart {
                thread_id: thread_id.to_string(),
            },
        );
        if let Some(session) = self.session_mut(thread_id) {
            session.status = SessionStatus::Working;
            session.upsert_message(MessageEntry {
                id: format!("local-user-{id}"),
                kind: MessageKind::User,
                text: draft_display(draft),
            });
        }
        self.sort_sessions_preserving_selection();
        Ok(())
    }

    fn steer_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        draft: &PromptDraft,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        let id = sender.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": prompt_input(draft)
            }),
        )?;
        self.pending_calls.insert(
            id,
            PendingCall::TurnSteer {
                thread_id: thread_id.to_string(),
            },
        );
        if let Some(session) = self.session_mut(thread_id) {
            session.upsert_message(MessageEntry {
                id: format!("local-user-{id}"),
                kind: MessageKind::User,
                text: draft_display(draft),
            });
        }
        self.notice = "Sending follow-up…".to_string();
        Ok(())
    }

    fn answer_pending_request(
        &mut self,
        thread_id: &str,
        text: &str,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        let Some(index) = self
            .pending_requests
            .iter()
            .position(|request| request.thread_id == thread_id)
        else {
            self.notice = "Codex question details are not available yet".to_string();
            return Ok(());
        };
        let request = self.pending_requests.remove(index);
        let normalized = text.trim().to_ascii_lowercase();
        let result = match request.kind {
            PendingRequestKind::UserInput(questions) => {
                let parts: Vec<&str> = text.split('|').map(str::trim).collect();
                let mut answers = Map::new();
                for (index, question) in questions.iter().enumerate() {
                    let raw = parts
                        .get(index)
                        .copied()
                        .or_else(|| parts.last().copied())
                        .unwrap_or_default();
                    let answer = resolve_question_answer(question, raw);
                    answers.insert(question.id.clone(), json!({ "answers": [answer] }));
                }
                json!({ "answers": answers })
            }
            PendingRequestKind::CommandApproval => json!({
                "decision": approval_decision(&normalized)
            }),
            PendingRequestKind::FileApproval => json!({
                "decision": approval_decision(&normalized)
            }),
            PendingRequestKind::PermissionApproval => {
                json!({ "permissions": {}, "scope": "turn" })
            }
            // MCP form replies depend on the requested JSON schema. Until the deck has
            // a real form renderer, declining is safer than fabricating invalid content.
            PendingRequestKind::McpElicitation => json!({ "action": "decline" }),
            PendingRequestKind::Unsupported(method) => {
                sender.respond_error(
                    request.id,
                    -32601,
                    &format!("codex-deck does not support {method}"),
                )?;
                self.notice = format!("Rejected unsupported request: {method}");
                return Ok(());
            }
        };
        sender.respond(request.id, result)?;
        let answer_index = self.pending_requests.len();
        if let Some(session) = self.session_mut(thread_id) {
            session.status = SessionStatus::Working;
            session.upsert_message(MessageEntry {
                id: format!("local-answer-{answer_index}"),
                kind: MessageKind::User,
                text: text.to_string(),
            });
        }
        self.notice = "Response sent; session resumed".to_string();
        self.sort_sessions_preserving_selection();
        Ok(())
    }

    fn merge_thread_list(&mut self, result: &Value) {
        let selected_id = self.selected_session().map(|session| session.id.clone());
        if let Some(threads) = result.get("data").and_then(Value::as_array) {
            for thread in threads {
                if let Some(session) = Session::from_thread(thread) {
                    self.initial_scan_seen.insert(session.id.clone());
                    if self.should_track(&session) {
                        self.lifecycle.track(session.id.clone());
                    }
                    if self.should_include(&session) {
                        self.merge_session(session);
                    }
                }
            }
        }
        self.sort_sessions();
        if let Some(selected_id) = selected_id {
            self.select_session(&selected_id);
        }
    }

    fn merge_session(&mut self, incoming: Session) {
        if let Some(existing) = self.session_mut(&incoming.id) {
            existing.title = incoming.title;
            existing.preview = incoming.preview;
            existing.cwd = incoming.cwd;
            existing.path = incoming.path;
            existing.updated_at = incoming.updated_at;
            existing.source = incoming.source;
            existing.thread_source = incoming.thread_source;
            existing.status = incoming.status;
        } else {
            self.sessions.push(incoming);
        }
        self.sort_sessions_preserving_selection();
    }

    fn apply_thread_history(&mut self, thread_id: &str, thread: &Value) {
        let messages = messages_from_thread(thread);
        if let Some(session) = self.session_mut(thread_id) {
            if !messages.is_empty() {
                session.messages = messages;
            }
            session.history_loaded = true;
            if let Some(turn_id) = active_turn_id(thread) {
                session.active_turn_id = Some(turn_id);
            }
        }
        self.scroll_back = 0;
    }

    fn apply_resumed_thread(&mut self, thread_id: &str, thread: &Value) {
        if let Some(status) = thread.get("status")
            && let Some(session) = self.session_mut(thread_id)
        {
            session.status = SessionStatus::from_protocol(status);
            session.active_turn_id = active_turn_id(thread);
        }
        let messages = messages_from_thread(thread);
        if !messages.is_empty()
            && let Some(session) = self.session_mut(thread_id)
        {
            session.messages = messages;
            session.history_loaded = true;
        }
    }

    fn handle_turn_started(&mut self, params: &Value) -> Result<()> {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return Ok(());
        };
        self.track_session(thread_id)?;
        let turn_id = params
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(session) = self.session_mut(thread_id) {
            session.status = SessionStatus::Working;
            session.active_turn_id = turn_id;
            session.updated_at = unix_now();
        }
        self.sort_sessions_preserving_selection();
        Ok(())
    }

    fn handle_turn_completed(&mut self, params: &Value) -> Result<()> {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return Ok(());
        };
        self.track_session(thread_id)?;
        let turn = params.get("turn").unwrap_or(&Value::Null);
        let turn_status = turn.get("status").and_then(Value::as_str);
        if let Some(session) = self.session_mut(thread_id) {
            session.status = if turn_status == Some("failed") {
                SessionStatus::Failed
            } else {
                SessionStatus::Completed
            };
            session.active_turn_id = None;
            session.updated_at = unix_now();
            if let Some(error) = turn
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
            {
                session.upsert_message(MessageEntry {
                    id: format!(
                        "turn-error-{}",
                        turn.get("id").and_then(Value::as_str).unwrap_or("unknown")
                    ),
                    kind: MessageKind::System,
                    text: error.to_string(),
                });
            }
        }
        self.pending_requests
            .retain(|request| request.thread_id != thread_id);
        self.notice = match turn_status {
            Some("failed") => "Session failed".to_string(),
            Some("interrupted") => "Session interrupted".to_string(),
            _ => "Session completed".to_string(),
        };
        self.sort_sessions_preserving_selection();
        Ok(())
    }

    fn handle_item(&mut self, params: &Value, completed: bool) {
        let (Some(thread_id), Some(item)) = (
            params.get("threadId").and_then(Value::as_str),
            params.get("item"),
        ) else {
            return;
        };
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        if let Some(kind) = item_kind(item) {
            self.live_item_kinds
                .insert((thread_id.to_string(), item_id.to_string()), kind);
        }
        if let Some(entry) = message_from_item(item)
            && (!entry.text.is_empty() || completed)
            && let Some(session) = self.session_mut(thread_id)
        {
            if entry.kind == MessageKind::User {
                remove_matching_local_user(session, &entry.text);
            }
            session.upsert_message(entry);
            session.history_loaded = true;
        }
    }

    fn handle_agent_delta(&mut self, params: &Value) {
        let (Some(thread_id), Some(item_id), Some(delta)) = (
            params.get("threadId").and_then(Value::as_str),
            params.get("itemId").and_then(Value::as_str),
            params.get("delta").and_then(Value::as_str),
        ) else {
            return;
        };
        let kind = self
            .live_item_kinds
            .get(&(thread_id.to_string(), item_id.to_string()))
            .copied()
            .unwrap_or(MessageKind::Progress);
        if let Some(session) = self.session_mut(thread_id) {
            session.append_delta(item_id, kind, delta);
            session.history_loaded = true;
        }
        if self.selected_session().map(|session| session.id.as_str()) == Some(thread_id)
            && self.scroll_back == 0
        {
            self.scroll_back = 0;
        }
    }

    fn handle_reasoning_delta(&mut self, params: &Value) {
        let (Some(thread_id), Some(item_id), Some(delta)) = (
            params.get("threadId").and_then(Value::as_str),
            params.get("itemId").and_then(Value::as_str),
            params.get("delta").and_then(Value::as_str),
        ) else {
            return;
        };
        if let Some(session) = self.session_mut(thread_id) {
            session.append_delta(item_id, MessageKind::Thinking, delta);
            session.history_loaded = true;
        }
    }

    fn handle_reasoning_part(&mut self, params: &Value) {
        let (Some(thread_id), Some(item_id)) = (
            params.get("threadId").and_then(Value::as_str),
            params.get("itemId").and_then(Value::as_str),
        ) else {
            return;
        };
        if let Some(session) = self.session_mut(thread_id)
            && let Some(entry) = session
                .messages
                .iter_mut()
                .find(|entry| entry.id == item_id)
            && !entry.text.is_empty()
            && !entry.text.ends_with('\n')
        {
            entry.text.push('\n');
        }
    }

    fn move_selection(&mut self, delta: isize, sender: &mut impl RpcSender) -> Result<()> {
        if self.composer.target == ComposeTarget::Rename {
            self.cancel_rename();
        }
        if self.sessions.is_empty() {
            self.switch_to_new_draft();
            return Ok(());
        }
        let load_reply = self.composer.target == ComposeTarget::Reply
            || (self.composer.target == ComposeTarget::NewTask
                && self.composer.text.is_empty()
                && self.composer.images.is_empty());
        self.cache_current_draft();
        let last = self.sessions.len() - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(last)
        };
        if load_reply {
            self.load_selected_reply_draft();
        }
        self.scroll_back = 0;
        self.ensure_selected_history(sender)
    }

    fn request_attach(&mut self) -> Result<()> {
        self.attach_right_armed = false;
        let Some((thread_id, cwd)) = self
            .selected_session()
            .map(|session| (session.id.clone(), session.cwd.clone()))
        else {
            self.notice = "No session selected".to_string();
            return Ok(());
        };
        self.track_session(&thread_id)?;
        self.attach_requested = Some(AttachRequest {
            thread_id,
            cwd: if cwd.is_empty() {
                self.cwd.clone()
            } else {
                PathBuf::from(cwd)
            },
        });
        self.notice = "Attaching to native Codex…".to_string();
        Ok(())
    }

    fn confirm_attach(&mut self) -> Result<()> {
        if self.attach_right_armed {
            self.request_attach()
        } else {
            self.attach_right_armed = true;
            self.notice = "Press → again to attach".to_string();
            Ok(())
        }
    }

    fn toggle_compose_target(&mut self) {
        if self.composer.target == ComposeTarget::Rename {
            self.cancel_rename();
            self.notice = "Rename cancelled".to_string();
            return;
        }
        match self.composer.target {
            ComposeTarget::NewTask if !self.sessions.is_empty() => {
                self.cache_current_draft();
                self.load_selected_reply_draft();
            }
            ComposeTarget::NewTask => {}
            ComposeTarget::Reply => self.switch_to_new_draft(),
            ComposeTarget::Rename => unreachable!("rename handled above"),
        }
    }

    fn cache_current_draft(&mut self) {
        match self.composer.target {
            ComposeTarget::NewTask => {
                let mut draft = self.composer.clone();
                draft.target = ComposeTarget::NewTask;
                self.new_draft = draft;
            }
            ComposeTarget::Reply => {
                if let Some(thread_id) = self.selected_session().map(|session| session.id.clone()) {
                    let mut draft = self.composer.clone();
                    draft.target = ComposeTarget::Reply;
                    self.reply_drafts.insert(thread_id, draft);
                }
            }
            ComposeTarget::Rename => {}
        }
    }

    fn switch_to_new_draft(&mut self) {
        if self.composer.target != ComposeTarget::Rename {
            self.cache_current_draft();
        }
        self.composer = self.new_draft.clone();
        self.composer.target = ComposeTarget::NewTask;
    }

    fn load_selected_reply_draft(&mut self) {
        let Some(thread_id) = self.selected_session().map(|session| session.id.clone()) else {
            self.composer = self.new_draft.clone();
            self.composer.target = ComposeTarget::NewTask;
            return;
        };
        self.load_reply_draft(&thread_id);
    }

    fn load_reply_draft(&mut self, thread_id: &str) {
        self.composer = self
            .reply_drafts
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| Composer {
                target: ComposeTarget::Reply,
                ..Composer::default()
            });
        self.composer.target = ComposeTarget::Reply;
    }

    fn start_rename(&mut self) {
        let Some(session) = self.selected_session() else {
            self.notice = "No session selected".to_string();
            return;
        };
        let thread_id = session.id.clone();
        let title = session.title.clone();
        if self.composer.target != ComposeTarget::Rename {
            self.rename_previous = Some(self.composer.clone());
        }
        self.rename_thread_id = Some(thread_id);
        self.composer.target = ComposeTarget::Rename;
        self.composer.text = title;
        self.composer.cursor = self.composer.text.len();
        self.composer.images.clear();
        self.notice = "Edit the name and press Enter · Tab cancels".to_string();
    }

    fn cancel_rename(&mut self) {
        if let Some(previous) = self.rename_previous.take() {
            self.composer = previous;
        } else if self.composer.target == ComposeTarget::Rename {
            self.composer = Composer::default();
        }
        self.rename_thread_id = None;
    }

    fn should_track(&self, session: &Session) -> bool {
        self.lifecycle.contains(&session.id)
            || session.status.is_live()
            || (!self.lifecycle.is_initialized()
                && session.thread_source.as_deref() == Some(THREAD_SOURCE))
    }

    fn should_include(&self, session: &Session) -> bool {
        self.show_all || self.should_track(session)
    }

    fn track_session(&mut self, thread_id: &str) -> Result<()> {
        if !self.lifecycle.is_initialized() {
            self.initial_scan_seen.insert(thread_id.to_string());
        }
        self.lifecycle.track(thread_id.to_string());
        self.lifecycle.save().context("save session lifecycle")
    }

    fn toggle_pin_selected(&mut self) -> Result<()> {
        let Some(session) = self.selected_session() else {
            self.notice = "No session selected".to_string();
            return Ok(());
        };
        let thread_id = session.id.clone();
        let title = session.title.clone();
        let pinned = self.lifecycle.toggle_pin(&thread_id);
        self.lifecycle.save().context("save session lifecycle")?;
        self.sort_sessions_preserving_selection();
        self.select_session(&thread_id);
        self.notice = if pinned {
            format!("Pinned {title}")
        } else {
            format!("Unpinned {title}")
        };
        Ok(())
    }

    fn stop_or_remove_selected(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        if self.show_all {
            self.notice = "Remove is available in the lifecycle view, without --all".to_string();
            return Ok(());
        }
        let Some(session) = self.selected_session() else {
            self.notice = "No session selected".to_string();
            return Ok(());
        };
        if session.status.is_live() {
            let Some(turn_id) = session.active_turn_id.clone() else {
                self.notice = "Loading the active turn before stopping it…".to_string();
                self.ensure_selected_history(sender)?;
                return Ok(());
            };
            let thread_id = session.id.clone();
            let id = sender.request(
                "turn/interrupt",
                json!({ "threadId": thread_id, "turnId": turn_id }),
            )?;
            self.pending_calls
                .insert(id, PendingCall::TurnInterrupt { thread_id });
            self.notice = "Stopping session…".to_string();
            return Ok(());
        }
        let thread_id = session.id.clone();
        let was_selected =
            self.selected_session().map(|session| session.id.as_str()) == Some(thread_id.as_str());
        self.lifecycle.dismiss(&thread_id);
        self.lifecycle.save().context("save session lifecycle")?;
        self.sessions.retain(|session| session.id != thread_id);
        self.pending_requests
            .retain(|request| request.thread_id != thread_id);
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        self.discard_reply_draft(&thread_id, was_selected);
        self.scroll_back = 0;
        self.notice = "Reviewed session removed from the deck; Codex history was kept".to_string();
        self.ensure_selected_history(sender)
    }

    pub fn is_pinned(&self, thread_id: &str) -> bool {
        self.lifecycle.is_pinned(thread_id)
    }

    fn discard_reply_draft(&mut self, thread_id: &str, was_selected: bool) {
        self.reply_drafts.remove(thread_id);
        if self.rename_thread_id.as_deref() == Some(thread_id) {
            self.cancel_rename();
        }
        if was_selected && self.composer.target == ComposeTarget::Reply {
            self.load_selected_reply_draft();
        }
    }

    fn session(&self, thread_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|session| session.id == thread_id)
    }

    fn session_mut(&mut self, thread_id: &str) -> Option<&mut Session> {
        self.sessions
            .iter_mut()
            .find(|session| session.id == thread_id)
    }

    fn session_title(&self, thread_id: &str) -> String {
        self.session(thread_id)
            .map(|session| session.title.clone())
            .unwrap_or_else(|| "session".to_string())
    }

    fn select_session(&mut self, thread_id: &str) {
        if let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.id == thread_id)
        {
            self.selected = index;
        }
    }

    fn sort_sessions(&mut self) {
        let lifecycle = &self.lifecycle;
        self.sessions.sort_by(|left, right| {
            let left_group = if lifecycle.is_pinned(&left.id) {
                0
            } else if left.status.is_live() {
                1
            } else {
                2
            };
            let right_group = if lifecycle.is_pinned(&right.id) {
                0
            } else if right.status.is_live() {
                1
            } else {
                2
            };
            left_group
                .cmp(&right_group)
                .then_with(|| left.status.sort_rank().cmp(&right.status.sort_rank()))
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| left.title.cmp(&right.title))
        });
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
    }

    fn sort_sessions_preserving_selection(&mut self) {
        let selected_id = self.selected_session().map(|session| session.id.clone());
        self.sort_sessions();
        if let Some(selected_id) = selected_id {
            self.select_session(&selected_id);
        }
    }

    fn has_pending_request(&self, thread_id: &str) -> bool {
        self.pending_requests
            .iter()
            .any(|request| request.thread_id == thread_id)
    }
}

fn messages_from_thread(thread: &Value) -> Vec<MessageEntry> {
    let mut messages = Vec::new();
    let Some(turns) = thread.get("turns").and_then(Value::as_array) else {
        return messages;
    };
    for turn in turns {
        if let Some(items) = turn.get("items").and_then(Value::as_array) {
            for item in items {
                if let Some(entry) = message_from_item(item)
                    && !entry.text.trim().is_empty()
                {
                    messages.push(entry);
                }
            }
        }
        if let Some(error) = turn
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            messages.push(MessageEntry {
                id: format!(
                    "turn-error-{}",
                    turn.get("id").and_then(Value::as_str).unwrap_or("unknown")
                ),
                kind: MessageKind::System,
                text: error.to_string(),
            });
        }
    }
    messages
}

fn prompt_input(draft: &PromptDraft) -> Vec<Value> {
    let mut input = Vec::with_capacity(usize::from(!draft.text.is_empty()) + draft.images.len());
    if !draft.text.is_empty() {
        input.push(json!({
            "type": "text",
            "text": draft.text,
            "text_elements": []
        }));
    }
    input.extend(draft.images.iter().map(|path| {
        json!({
            "type": "localImage",
            "path": path.to_string_lossy()
        })
    }));
    input
}

fn draft_display(draft: &PromptDraft) -> String {
    let image_label = match draft.images.len() {
        0 => String::new(),
        1 => "🖼 1 image".to_string(),
        count => format!("🖼 {count} images"),
    };
    match (draft.text.is_empty(), image_label.is_empty()) {
        (false, false) => format!("{}\n\n{}", draft.text, image_label),
        (false, true) => draft.text.clone(),
        (true, false) => image_label,
        (true, true) => String::new(),
    }
}

fn message_from_item(item: &Value) -> Option<MessageEntry> {
    let item_type = item.get("type")?.as_str()?;
    let id = item.get("id")?.as_str()?.to_string();
    match item_type {
        "userMessage" => Some(MessageEntry {
            id,
            kind: MessageKind::User,
            text: user_input_text(item.get("content")),
        }),
        "agentMessage" => Some(MessageEntry {
            id,
            kind: match item.get("phase").and_then(Value::as_str) {
                Some("commentary") => MessageKind::Progress,
                Some("final_answer") | None => MessageKind::Final,
                Some(_) => MessageKind::Progress,
            },
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "reasoning" => {
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n");
            Some(MessageEntry {
                id,
                kind: MessageKind::Thinking,
                text: summary,
            })
        }
        "plan" => Some(MessageEntry {
            id,
            kind: MessageKind::Progress,
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => None,
    }
}

fn item_kind(item: &Value) -> Option<MessageKind> {
    match item.get("type").and_then(Value::as_str)? {
        "userMessage" => Some(MessageKind::User),
        "reasoning" => Some(MessageKind::Thinking),
        "plan" => Some(MessageKind::Progress),
        "agentMessage" => Some(match item.get("phase").and_then(Value::as_str) {
            Some("commentary") => MessageKind::Progress,
            Some("final_answer") | None => MessageKind::Final,
            Some(_) => MessageKind::Progress,
        }),
        _ => None,
    }
}

fn user_input_text(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text") => part
                .get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            Some("localImage") => Some("🖼 local image".to_string()),
            Some("image") => Some("🖼 image".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn active_turn_id(thread: &Value) -> Option<String> {
    thread
        .get("turns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_questions(params: &Value) -> Vec<InputQuestion> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            Some(InputQuestion {
                id: question.get("id")?.as_str()?.to_string(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or("Question")
                    .to_string(),
                question: question
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        Some(InputOption {
                            label: option.get("label")?.as_str()?.to_string(),
                            description: option
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect(),
            })
        })
        .collect()
}

fn pending_request_display(kind: &PendingRequestKind, params: &Value) -> String {
    match kind {
        PendingRequestKind::UserInput(questions) => questions
            .iter()
            .map(|question| {
                let options = question
                    .options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| {
                        if option.description.is_empty() {
                            format!("  {}. {}", index + 1, option.label)
                        } else {
                            format!("  {}. {} — {}", index + 1, option.label, option.description)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if options.is_empty() {
                    format!("{}: {}", question.header, question.question)
                } else {
                    format!("{}: {}\n{}", question.header, question.question, options)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        PendingRequestKind::CommandApproval => {
            let command = params
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("Run requested command");
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("Approve command?\n{command}\n{reason}\ny once · a session · n deny")
        }
        PendingRequestKind::FileApproval => {
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Codex requests a file change");
            format!("Approve file change?\n{reason}\ny once · a session · n deny")
        }
        PendingRequestKind::PermissionApproval => {
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Codex requests additional permissions");
            format!("{reason}\nAdvanced permission grants are denied by this version.")
        }
        PendingRequestKind::McpElicitation => params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP server requests input")
            .to_string(),
        PendingRequestKind::Unsupported(method) => {
            format!("Unsupported Codex request: {method}")
        }
    }
}

fn resolve_question_answer(question: &InputQuestion, raw: &str) -> String {
    if let Ok(index) = raw.parse::<usize>()
        && index > 0
        && let Some(option) = question.options.get(index - 1)
    {
        return option.label.clone();
    }
    question
        .options
        .iter()
        .find(|option| option.label.eq_ignore_ascii_case(raw))
        .map(|option| option.label.clone())
        .unwrap_or_else(|| raw.to_string())
}

fn approval_decision(normalized: &str) -> &'static str {
    match normalized {
        "a" | "always" | "session" => "acceptForSession",
        "y" | "yes" | "approve" | "allow" => "accept",
        "c" | "cancel" => "cancel",
        _ => "decline",
    }
}

fn rpc_id_label(id: &Value) -> String {
    id.as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| id.to_string())
}

fn notification_message(params: &Value) -> Option<String> {
    params
        .get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            params
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn remove_matching_local_user(session: &mut Session, text: &str) {
    let signature = user_message_signature(text);
    if let Some(index) = session.messages.iter().position(|entry| {
        entry.id.starts_with("local-user-") && user_message_signature(&entry.text) == signature
    }) {
        session.messages.remove(index);
    }
}

fn user_message_signature(text: &str) -> (String, bool) {
    let mut has_image = false;
    let mut body_lines = Vec::new();
    for line in text.lines() {
        if is_image_marker(line.trim()) {
            has_image = true;
        } else {
            body_lines.push(line);
        }
    }
    let body = body_lines.join("\n").trim().to_string();
    (body, has_image)
}

fn is_image_marker(line: &str) -> bool {
    let Some(label) = line.strip_prefix("🖼 ") else {
        return false;
    };
    if matches!(label, "image" | "local image") {
        return true;
    }
    let mut parts = label.split_whitespace();
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(count), Some("image" | "images"), None) if count.parse::<usize>().is_ok()
    )
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_STATE: AtomicU64 = AtomicU64::new(1);

    struct TestSender {
        next_id: u64,
        requests: Vec<(String, Value)>,
    }

    impl RpcSender for TestSender {
        fn request(&mut self, method: &str, params: Value) -> Result<u64> {
            self.next_id += 1;
            self.requests.push((method.to_string(), params));
            Ok(self.next_id)
        }

        fn notify(&mut self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn respond(&mut self, _id: Value, _result: Value) -> Result<()> {
            Ok(())
        }

        fn respond_error(&mut self, _id: Value, _code: i64, _message: &str) -> Result<()> {
            Ok(())
        }
    }

    fn test_app(show_all: bool) -> (App, PathBuf) {
        let sequence = NEXT_TEST_STATE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "codex-deck-app-lifecycle-{}-{sequence}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let lifecycle = LifecycleStore::for_test(path.clone());
        (
            App::with_lifecycle(PathBuf::from("/tmp"), show_all, lifecycle),
            path,
        )
    }

    fn test_sender() -> TestSender {
        TestSender {
            next_id: 0,
            requests: Vec::new(),
        }
    }

    fn test_session(id: &str, title: &str, status: SessionStatus) -> Session {
        Session {
            id: id.to_string(),
            title: title.to_string(),
            preview: String::new(),
            cwd: format!("/tmp/{id}"),
            path: None,
            updated_at: 0,
            source: "appServer".to_string(),
            thread_source: Some(THREAD_SOURCE.to_string()),
            status,
            active_turn_id: None,
            messages: Vec::new(),
            history_loaded: true,
        }
    }

    #[test]
    fn reads_thinking_and_final_in_one_ordered_stream() {
        let thread = json!({
            "turns": [{
                "id": "turn-1",
                "items": [
                    {"type":"userMessage","id":"u","content":[{"type":"text","text":"Do it","text_elements":[]}]},
                    {"type":"reasoning","id":"r","summary":["Inspecting", "Implementing"],"content":[]},
                    {"type":"agentMessage","id":"p","text":"Working","phase":"commentary","memoryCitation":null},
                    {"type":"agentMessage","id":"f","text":"Done","phase":"final_answer","memoryCitation":null}
                ],
                "error": null
            }]
        });
        let messages = messages_from_thread(&thread);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].kind, MessageKind::Thinking);
        assert_eq!(messages[2].kind, MessageKind::Progress);
        assert_eq!(messages[3].kind, MessageKind::Final);
    }

    #[test]
    fn numeric_question_answer_resolves_to_option_label() {
        let question = InputQuestion {
            id: "choice".to_string(),
            header: "Choice".to_string(),
            question: "Pick".to_string(),
            options: vec![
                InputOption {
                    label: "First".to_string(),
                    description: String::new(),
                },
                InputOption {
                    label: "Second".to_string(),
                    description: String::new(),
                },
            ],
        };
        assert_eq!(resolve_question_answer(&question, "2"), "Second");
    }

    #[test]
    fn attach_targets_the_selected_session_and_its_directory() {
        let (mut app, state_path) = test_app(true);
        app.sessions.push(Session {
            id: "thread-123".to_string(),
            title: "Test".to_string(),
            preview: String::new(),
            cwd: "/tmp/project".to_string(),
            path: None,
            updated_at: 0,
            source: "appServer".to_string(),
            thread_source: Some(THREAD_SOURCE.to_string()),
            status: SessionStatus::Completed,
            active_turn_id: None,
            messages: Vec::new(),
            history_loaded: true,
        });

        app.request_attach().expect("request attach");
        let request = app.take_attach_request().expect("attach request");
        assert_eq!(request.thread_id, "thread-123");
        assert_eq!(request.cwd, PathBuf::from("/tmp/project"));
        std::fs::remove_file(state_path).expect("remove lifecycle state");
    }

    #[test]
    fn attach_requires_two_right_arrow_presses_and_ignores_key_repeat() {
        let (mut app, state_path) = test_app(true);
        app.sessions
            .push(test_session("thread-123", "Test", SessionStatus::Completed));
        let mut sender = test_sender();

        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("empty enter");
        assert!(app.take_attach_request().is_none());

        app.handle_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("first right");
        assert!(app.take_attach_request().is_none());
        app.handle_key(
            KeyEvent::new_with_kind(KeyCode::Right, KeyModifiers::NONE, KeyEventKind::Repeat),
            &mut sender,
        )
        .expect("held right");
        assert!(app.take_attach_request().is_none());

        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("intervening key");
        app.handle_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("new first right");
        assert!(app.take_attach_request().is_none());
        app.handle_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("second right");

        assert_eq!(
            app.take_attach_request()
                .expect("confirmed attach")
                .thread_id,
            "thread-123"
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn reply_drafts_follow_their_selected_sessions() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "thread-a",
            "Session A",
            SessionStatus::Completed,
        ));
        app.sessions.push(test_session(
            "thread-b",
            "Session B",
            SessionStatus::Completed,
        ));
        let mut sender = test_sender();
        app.toggle_compose_target();
        app.insert_text("reply for A");
        app.composer.images.push(PathBuf::from("/tmp/a.png"));

        app.move_selection(1, &mut sender).expect("select B");

        assert_eq!(app.selected_session().expect("B").id, "thread-b");
        assert_eq!(app.composer.target, ComposeTarget::Reply);
        assert!(app.composer.text.is_empty());
        assert!(app.composer.images.is_empty());
        app.insert_text("reply for B");

        app.move_selection(-1, &mut sender).expect("return to A");

        assert_eq!(app.composer.text, "reply for A");
        assert_eq!(app.composer.images, vec![PathBuf::from("/tmp/a.png")]);
        app.move_selection(1, &mut sender).expect("return to B");
        assert_eq!(app.composer.text, "reply for B");
        app.submit(&mut sender).expect("submit B reply");
        assert_eq!(
            sender.requests.last(),
            Some(&("thread/resume".to_string(), json!({"threadId":"thread-b"})))
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn new_draft_is_global_and_separate_from_session_replies() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "thread-a",
            "Session A",
            SessionStatus::Completed,
        ));
        app.sessions.push(test_session(
            "thread-b",
            "Session B",
            SessionStatus::Completed,
        ));
        let mut sender = test_sender();
        app.insert_text("global new task");

        app.toggle_compose_target();
        assert!(app.composer.text.is_empty());
        app.insert_text("reply for A");
        app.toggle_compose_target();
        assert_eq!(app.composer.target, ComposeTarget::NewTask);
        assert_eq!(app.composer.text, "global new task");

        app.move_selection(1, &mut sender)
            .expect("select B without changing global draft");
        assert_eq!(app.composer.target, ComposeTarget::NewTask);
        assert_eq!(app.composer.text, "global new task");
        app.toggle_compose_target();
        assert_eq!(app.composer.target, ComposeTarget::Reply);
        assert!(app.composer.text.is_empty());

        app.move_selection(-1, &mut sender).expect("select A");
        assert_eq!(app.composer.text, "reply for A");
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn mouse_wheel_scroll_changes_only_preview_offset() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "thread-a",
            "Session A",
            SessionStatus::Completed,
        ));
        app.selected = 0;

        app.scroll_preview(3);
        assert_eq!(app.scroll_back, 3);
        assert_eq!(app.selected, 0);
        app.scroll_preview(-2);
        assert_eq!(app.scroll_back, 1);
        assert_eq!(app.selected, 0);
        app.scroll_preview(-10);
        assert_eq!(app.scroll_back, 0);
        assert_eq!(app.selected, 0);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn lifecycle_view_adopts_live_sessions_but_skips_old_history() {
        let (mut app, state_path) = test_app(false);
        app.merge_thread_list(&json!({
            "data": [
                {
                    "id": "old-thread",
                    "preview": "Old history",
                    "cwd": "/tmp/old",
                    "status": {"type": "idle"},
                    "source": "cli",
                    "threadSource": "cli"
                },
                {
                    "id": "live-thread",
                    "preview": "Current work",
                    "cwd": "/tmp/live",
                    "status": {"type": "active", "activeFlags": []},
                    "source": "appServer",
                    "threadSource": "cli"
                }
            ]
        }));

        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, "live-thread");
        assert!(app.lifecycle.contains("live-thread"));
        assert!(!app.lifecycle.contains("old-thread"));
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn completed_session_waits_for_review_then_leaves_only_the_deck() {
        let (mut app, state_path) = test_app(false);
        let session = Session::from_thread(&json!({
            "id": "review-thread",
            "preview": "Review me",
            "cwd": "/tmp/review",
            "status": {"type": "active", "activeFlags": []},
            "source": "appServer",
            "threadSource": "codex-deck"
        }))
        .expect("session");
        app.lifecycle.track(session.id.clone());
        app.sessions.push(session);
        app.sessions[0].status = SessionStatus::Completed;
        let mut sender = test_sender();

        app.stop_or_remove_selected(&mut sender)
            .expect("remove session");

        assert!(app.sessions.is_empty());
        assert!(!app.lifecycle.contains("review-thread"));
        assert!(app.notice.contains("Codex history was kept"));
        std::fs::remove_file(state_path).expect("remove lifecycle state");
    }

    #[test]
    fn pin_persists_and_moves_session_into_the_first_group() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "working-thread",
            "Working",
            SessionStatus::Working,
        ));
        app.sessions.push(test_session(
            "completed-thread",
            "Completed",
            SessionStatus::Completed,
        ));
        app.selected = 1;

        app.toggle_pin_selected().expect("pin session");

        assert_eq!(app.sessions[0].id, "completed-thread");
        assert!(app.lifecycle.is_pinned("completed-thread"));
        let restored = LifecycleStore::for_test(state_path.clone());
        assert!(restored.is_pinned("completed-thread"));
        std::fs::remove_file(state_path).expect("remove lifecycle state");
    }

    #[test]
    fn rename_uses_the_official_thread_name_rpc_and_restores_the_composer() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "rename-thread",
            "Old name",
            SessionStatus::Completed,
        ));
        app.composer.target = ComposeTarget::Reply;
        app.composer.text = "saved draft".to_string();
        app.composer.cursor = app.composer.text.len();
        app.composer.images.push(PathBuf::from("/tmp/draft.png"));
        app.start_rename();
        app.composer.text = "New name".to_string();
        app.composer.cursor = app.composer.text.len();
        let mut sender = test_sender();

        app.submit(&mut sender).expect("submit rename");

        assert_eq!(
            sender.requests.last(),
            Some(&(
                "thread/name/set".to_string(),
                json!({"threadId":"rename-thread","name":"New name"})
            ))
        );
        app.handle_response(json!({"id": 1, "result": {}}), &mut sender)
            .expect("handle rename response");
        assert_eq!(app.sessions[0].title, "New name");
        assert_eq!(app.composer.target, ComposeTarget::Reply);
        assert_eq!(app.composer.text, "saved draft");
        assert_eq!(app.composer.images, vec![PathBuf::from("/tmp/draft.png")]);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn image_only_reply_is_sent_as_local_image_input() {
        let (mut app, state_path) = test_app(false);
        let mut session = test_session("image-thread", "Image", SessionStatus::Working);
        session.active_turn_id = Some("turn-1".to_string());
        app.sessions.push(session);
        app.composer.target = ComposeTarget::Reply;
        app.composer.images.push(PathBuf::from("/tmp/pasted.png"));
        let mut sender = test_sender();

        app.submit(&mut sender).expect("submit image reply");

        assert_eq!(
            sender.requests.last(),
            Some(&(
                "turn/steer".to_string(),
                json!({
                    "threadId":"image-thread",
                    "expectedTurnId":"turn-1",
                    "input":[{"type":"localImage","path":"/tmp/pasted.png"}]
                })
            ))
        );
        assert!(app.composer.images.is_empty());
        assert_eq!(
            app.sessions[0]
                .messages
                .last()
                .expect("local user message")
                .text,
            "🖼 1 image"
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn history_parser_keeps_image_only_user_messages_visible() {
        assert_eq!(
            user_input_text(Some(&json!([
                {"type":"text","text":"Inspect this"},
                {"type":"localImage","path":"/tmp/pasted.png"}
            ]))),
            "Inspect this\n🖼 local image"
        );
    }

    #[test]
    fn official_image_message_replaces_its_optimistic_local_preview() {
        let mut session = test_session("image-thread", "Image", SessionStatus::Working);
        session.messages.push(MessageEntry {
            id: "local-user-1".to_string(),
            kind: MessageKind::User,
            text: "测试一下你是否真的收到了图\n\n🖼 1 image".to_string(),
        });

        remove_matching_local_user(&mut session, "测试一下你是否真的收到了图\n🖼 local image");

        assert!(session.messages.is_empty());
        assert_eq!(user_message_signature("🖼 2 images"), (String::new(), true));
    }

    #[test]
    fn ctrl_x_interrupts_a_working_turn_without_removing_the_session() {
        let (mut app, state_path) = test_app(false);
        let mut session = test_session("working-thread", "Working", SessionStatus::Working);
        session.active_turn_id = Some("turn-1".to_string());
        app.sessions.push(session);
        let mut sender = test_sender();

        app.stop_or_remove_selected(&mut sender)
            .expect("interrupt session");

        assert_eq!(app.sessions.len(), 1);
        assert_eq!(
            sender.requests.last(),
            Some(&(
                "turn/interrupt".to_string(),
                json!({"threadId":"working-thread","turnId":"turn-1"})
            ))
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn official_thread_name_notification_updates_the_title() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "rename-thread",
            "Old name",
            SessionStatus::Completed,
        ));
        let mut sender = test_sender();

        app.handle_notification(
            "thread/name/updated",
            json!({"threadId":"rename-thread","threadName":"Server name"}),
            &mut sender,
        )
        .expect("handle name notification");

        assert_eq!(app.sessions[0].title, "Server name");
        let _ = std::fs::remove_file(state_path);
    }
}
