use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value, json};

use crate::client::{ClientEvent, RpcSender};
use crate::model::{ComposeTarget, Composer, MessageEntry, MessageKind, Session, SessionStatus};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const THREAD_SOURCE: &str = "codex-deck";

#[derive(Debug)]
enum PendingCall {
    Initialize,
    ThreadList { paginate: bool },
    ThreadRead { thread_id: String },
    ThreadStart { prompt: String },
    ThreadResumeForReply { thread_id: String, prompt: String },
    TurnStart { thread_id: String },
    TurnSteer { thread_id: String },
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

pub struct App {
    cwd: PathBuf,
    include_all: bool,
    sessions: Vec<Session>,
    selected: usize,
    composer: Composer,
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
}

impl App {
    pub fn new(cwd: PathBuf, include_all: bool) -> Self {
        Self {
            cwd,
            include_all,
            sessions: Vec::new(),
            selected: 0,
            composer: Composer::default(),
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

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return Ok(());
                }
                KeyCode::Char('n') => {
                    self.composer.target = ComposeTarget::NewTask;
                    self.notice = "New task".to_string();
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    if !self.sessions.is_empty() {
                        self.composer.target = ComposeTarget::Reply;
                        self.notice = "Reply to selected session".to_string();
                    }
                    return Ok(());
                }
                KeyCode::Char('u') => {
                    self.composer.text.clear();
                    self.composer.cursor = 0;
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
            KeyCode::Enter if self.composer.text.is_empty() => self.request_attach(),
            KeyCode::Enter => self.submit(sender)?,
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right if self.composer.text.is_empty() => self.request_attach(),
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
        self.composer.insert(text);
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
        self.scroll_back = 0;
        self.notice = "Returned from attached session".to_string();
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
                self.list_inflight = false;
                self.initial_load_complete = true;
                self.ensure_selected_history(sender)?;
                if first_load {
                    self.notice = if self.sessions.is_empty() {
                        "Type a task and press Enter".to_string()
                    } else {
                        format!("{} sessions", self.sessions.len())
                    };
                }
            }
            PendingCall::ThreadRead { thread_id } => {
                if let Some(thread) = result.get("thread") {
                    self.apply_thread_history(&thread_id, thread);
                }
            }
            PendingCall::ThreadStart { prompt } => {
                let thread = result
                    .get("thread")
                    .context("thread/start response missing thread")?;
                let session = Session::from_thread(thread)
                    .context("thread/start response has invalid thread")?;
                let thread_id = session.id.clone();
                self.merge_session(session);
                self.select_session(&thread_id);
                self.composer.target = ComposeTarget::Reply;
                self.start_turn(&thread_id, &prompt, sender)?;
            }
            PendingCall::ThreadResumeForReply { thread_id, prompt } => {
                let thread = result.get("thread").unwrap_or(&Value::Null);
                self.apply_resumed_thread(&thread_id, thread);
                let status = thread
                    .get("status")
                    .map(SessionStatus::from_protocol)
                    .unwrap_or(SessionStatus::Completed);
                if status == SessionStatus::NeedsInput {
                    if self.has_pending_request(&thread_id) {
                        self.answer_pending_request(&thread_id, &prompt, sender)?;
                    } else {
                        self.queued_replies.insert(thread_id.clone(), prompt);
                        self.notice = "Waiting for Codex question details…".to_string();
                    }
                } else if status == SessionStatus::Working {
                    if let Some(turn_id) = active_turn_id(thread) {
                        self.steer_turn(&thread_id, &turn_id, &prompt, sender)?;
                    } else {
                        self.notice = "Session is active; waiting for turn state".to_string();
                    }
                } else {
                    self.start_turn(&thread_id, &prompt, sender)?;
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
                    && self.should_include(&session)
                {
                    self.merge_session(session);
                }
            }
            "thread/status/changed" => {
                if let (Some(thread_id), Some(status)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params.get("status"),
                ) && let Some(session) = self.session_mut(thread_id)
                {
                    session.status = SessionStatus::from_protocol(status);
                    self.sort_sessions_preserving_selection();
                }
            }
            "thread/name/updated" => {
                if let (Some(thread_id), Some(name)) = (
                    params.get("threadId").and_then(Value::as_str),
                    params.get("name").and_then(Value::as_str),
                ) && let Some(session) = self.session_mut(thread_id)
                {
                    session.title = name.to_string();
                }
            }
            "thread/deleted" | "thread/archived" => {
                if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                    self.sessions.retain(|session| session.id != thread_id);
                    self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                }
            }
            "turn/started" => self.handle_turn_started(&params),
            "turn/completed" => self.handle_turn_completed(&params),
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
        let source_kinds = if self.include_all {
            json!(["cli", "vscode", "exec", "appServer"])
        } else {
            json!(["appServer"])
        };
        let id = sender.request(
            "thread/list",
            json!({
                "cursor": cursor,
                "limit": 100,
                "sortKey": "recency_at",
                "sortDirection": "desc",
                "archived": false,
                "sourceKinds": source_kinds
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
        let id = sender.request(
            "thread/read",
            json!({ "threadId": thread_id, "includeTurns": true }),
        )?;
        self.pending_calls
            .insert(id, PendingCall::ThreadRead { thread_id });
        Ok(())
    }

    fn submit(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        let text = self.composer.text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        let target = self.composer.target;
        self.composer.take();
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
                    .insert(id, PendingCall::ThreadStart { prompt: text });
                self.notice = "Starting background session…".to_string();
            }
            ComposeTarget::Reply => {
                let Some(thread_id) = self.selected_session().map(|session| session.id.clone())
                else {
                    self.notice = "No session selected; switched to new task".to_string();
                    self.composer.target = ComposeTarget::NewTask;
                    self.composer.insert(&text);
                    return Ok(());
                };

                if self.has_pending_request(&thread_id) {
                    self.answer_pending_request(&thread_id, &text, sender)?;
                    return Ok(());
                }

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
                    self.steer_turn(&thread_id, &turn_id, &text, sender)?;
                } else {
                    let id = sender.request("thread/resume", json!({ "threadId": thread_id }))?;
                    self.pending_calls.insert(
                        id,
                        PendingCall::ThreadResumeForReply {
                            thread_id,
                            prompt: text,
                        },
                    );
                    self.notice = "Resuming session…".to_string();
                }
            }
        }
        Ok(())
    }

    fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        let id = sender.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": prompt,
                    "text_elements": []
                }],
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
                text: prompt.to_string(),
            });
        }
        self.sort_sessions_preserving_selection();
        Ok(())
    }

    fn steer_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        prompt: &str,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        let id = sender.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": [{
                    "type": "text",
                    "text": prompt,
                    "text_elements": []
                }]
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
                text: prompt.to_string(),
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
                if let Some(session) = Session::from_thread(thread)
                    && self.should_include(&session)
                {
                    self.merge_session(session);
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

    fn handle_turn_started(&mut self, params: &Value) {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return;
        };
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
    }

    fn handle_turn_completed(&mut self, params: &Value) {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return;
        };
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
        if self.sessions.is_empty() {
            self.composer.target = ComposeTarget::NewTask;
            return Ok(());
        }
        let last = self.sessions.len() - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(last)
        };
        if self.composer.text.is_empty() {
            self.composer.target = ComposeTarget::Reply;
        }
        self.scroll_back = 0;
        self.ensure_selected_history(sender)
    }

    fn request_attach(&mut self) {
        let Some((thread_id, cwd)) = self
            .selected_session()
            .map(|session| (session.id.clone(), session.cwd.clone()))
        else {
            self.notice = "No session selected".to_string();
            return;
        };
        self.attach_requested = Some(AttachRequest {
            thread_id,
            cwd: if cwd.is_empty() {
                self.cwd.clone()
            } else {
                PathBuf::from(cwd)
            },
        });
        self.notice = "Attaching to native Codex…".to_string();
    }

    fn toggle_compose_target(&mut self) {
        self.composer.target = match self.composer.target {
            ComposeTarget::NewTask if !self.sessions.is_empty() => ComposeTarget::Reply,
            ComposeTarget::NewTask => ComposeTarget::NewTask,
            ComposeTarget::Reply => ComposeTarget::NewTask,
        };
    }

    fn should_include(&self, session: &Session) -> bool {
        self.include_all || session.thread_source.as_deref() == Some(THREAD_SOURCE)
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
        self.sessions.sort_by(|left, right| {
            left.status
                .sort_rank()
                .cmp(&right.status.sort_rank())
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
        .filter_map(|part| {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                part.get("text").and_then(Value::as_str)
            } else {
                None
            }
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
    if let Some(index) = session
        .messages
        .iter()
        .position(|entry| entry.id.starts_with("local-user-") && entry.text.trim() == text.trim())
    {
        session.messages.remove(index);
    }
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
        let mut app = App::new(PathBuf::from("/tmp"), true);
        app.sessions.push(Session {
            id: "thread-123".to_string(),
            title: "Test".to_string(),
            preview: String::new(),
            cwd: "/tmp/project".to_string(),
            updated_at: 0,
            source: "appServer".to_string(),
            thread_source: Some(THREAD_SOURCE.to_string()),
            status: SessionStatus::Completed,
            active_turn_id: None,
            messages: Vec::new(),
            history_loaded: true,
        });

        app.request_attach();
        let request = app.take_attach_request().expect("attach request");
        assert_eq!(request.thread_id, "thread-123");
        assert_eq!(request.cwd, PathBuf::from("/tmp/project"));
    }
}
