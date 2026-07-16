use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value, json};

use crate::client::{ClientEvent, RpcSender};
use crate::clipboard::{image_paths_from_paste, save_clipboard_image};
use crate::lifecycle::LifecycleStore;
use crate::model::{
    ComposeTarget, Composer, MessageEntry, MessageKind, PreviewVerbosity, Session, SessionStatus,
    SkillReference,
};
use crate::transcript::{TAIL_PREVIEW_BYTES, load_bounded_preview_if_large};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const THREAD_SOURCE: &str = "codeck";
const LEGACY_THREAD_SOURCE: &str = "codex-deck";

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
    SkillsList {
        cwd: String,
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
    skills: Vec<SkillReference>,
    composer: Composer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: String,
    pub scope: String,
}

#[derive(Debug, Clone)]
struct SkillPicker {
    cwd: String,
    query_start: usize,
    query: String,
    selected: usize,
}

#[derive(Debug, Clone)]
struct HistoryPicker {
    query: String,
    selected: usize,
}

#[derive(Debug, Clone)]
pub struct HistoryPickerView {
    pub query: String,
    pub selected: usize,
    pub items: Vec<Session>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTab {
    Resume,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlXAction {
    Pause,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CtrlXConfirmation {
    thread_id: String,
    action: CtrlXAction,
}

#[derive(Debug, Clone)]
pub struct SkillPickerView {
    pub items: Vec<SkillMetadata>,
    pub selected: usize,
    pub loading: bool,
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
    history_sessions: Vec<Session>,
    history_picker: Option<HistoryPicker>,
    skills_by_cwd: HashMap<String, Vec<SkillMetadata>>,
    skills_inflight: BTreeSet<String>,
    skill_picker: Option<SkillPicker>,
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
    settings_open: bool,
    menu_tab: MenuTab,
    settings_selection: PreviewVerbosity,
    settings_left_armed: bool,
    force_full_redraw: bool,
    ctrl_x_armed: Option<CtrlXConfirmation>,
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
        let settings_selection = lifecycle.preview_verbosity();
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
            history_sessions: Vec::new(),
            history_picker: None,
            skills_by_cwd: HashMap::new(),
            skills_inflight: BTreeSet::new(),
            skill_picker: None,
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
            settings_open: false,
            menu_tab: MenuTab::Resume,
            settings_selection,
            settings_left_armed: false,
            force_full_redraw: false,
            ctrl_x_armed: None,
            rename_previous: None,
            rename_thread_id: None,
        }
    }

    pub fn begin(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        let id = sender.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codeck",
                    "title": "Codeck",
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

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        let is_ctrl_x =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x');
        if !is_ctrl_x {
            self.ctrl_x_armed = None;
        }

        if self.settings_open {
            return self.handle_settings_key(key, sender);
        }

        if self.handle_skill_picker_key(key) {
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
        let settings_left = key.code == KeyCode::Left
            && self.composer.text.is_empty()
            && self.composer.target != ComposeTarget::Rename
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !settings_left {
            self.settings_left_armed = false;
        }

        if key.modifiers.contains(KeyModifiers::SUPER) && key.code == KeyCode::Char('v') {
            self.skill_picker = None;
            self.paste_clipboard_image();
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('n') => {
                    self.skill_picker = None;
                    self.cancel_rename();
                    self.switch_to_new_draft();
                    self.notice = "New task".to_string();
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.skill_picker = None;
                    self.start_rename();
                    return Ok(());
                }
                KeyCode::Char('u') => {
                    self.composer.reset();
                    self.skill_picker = None;
                    return Ok(());
                }
                KeyCode::Char('v') => {
                    self.skill_picker = None;
                    self.paste_clipboard_image();
                    return Ok(());
                }
                KeyCode::Char('t') => {
                    self.skill_picker = None;
                    self.toggle_pin_selected()?;
                    return Ok(());
                }
                KeyCode::Char('x') => {
                    self.skill_picker = None;
                    self.confirm_stop_or_remove(key.kind, sender)?;
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
            KeyCode::Left if settings_left && key.kind == KeyEventKind::Repeat => {}
            KeyCode::Left if settings_left => self.confirm_settings()?,
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
        self.sync_skill_picker(sender)?;
        Ok(())
    }

    pub fn insert_text(&mut self, text: &str) {
        self.attach_right_armed = false;
        self.skill_picker = None;
        self.composer.insert(text);
    }

    pub fn insert_paste(&mut self, text: &str) {
        if self.settings_open {
            if self.menu_tab == MenuTab::Resume
                && let Some(picker) = &mut self.history_picker
            {
                picker.query.push_str(text);
                picker.selected = 0;
            }
            return;
        }
        self.skill_picker = None;
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
            self.composer.attach_image(path);
        }
        let added = self.composer.images.len().saturating_sub(before);
        self.attach_right_armed = false;
        self.notice = if added == 0 {
            "Image is already attached".to_string()
        } else {
            format!(
                "Attached {added} image{} · Backspace removes the image token",
                if added == 1 { "" } else { "s" }
            )
        };
    }

    fn restore_draft(&mut self, draft: PromptDraft) {
        self.composer = draft.composer;
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

    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn preview_verbosity(&self) -> PreviewVerbosity {
        self.lifecycle.preview_verbosity()
    }

    pub fn settings_selection(&self) -> PreviewVerbosity {
        self.settings_selection
    }

    pub fn menu_tab(&self) -> MenuTab {
        self.menu_tab
    }

    pub fn take_force_full_redraw(&mut self) -> bool {
        std::mem::take(&mut self.force_full_redraw)
    }

    pub fn skill_picker_view(&self) -> Option<SkillPickerView> {
        let picker = self.skill_picker.as_ref()?;
        let items = self
            .skills_by_cwd
            .get(&picker.cwd)
            .into_iter()
            .flatten()
            .filter(|skill| skill_matches(skill, &picker.query))
            .cloned()
            .collect::<Vec<_>>();
        Some(SkillPickerView {
            selected: picker.selected.min(items.len().saturating_sub(1)),
            loading: self.skills_inflight.contains(&picker.cwd),
            items,
        })
    }

    pub fn history_picker_view(&self) -> Option<HistoryPickerView> {
        if !self.settings_open || self.menu_tab != MenuTab::Resume {
            return None;
        }
        let picker = self.history_picker.as_ref()?;
        let items = self
            .history_sessions
            .iter()
            .filter(|session| history_session_matches(session, &picker.query))
            .cloned()
            .collect::<Vec<_>>();
        Some(HistoryPickerView {
            query: picker.query.clone(),
            selected: picker.selected.min(items.len().saturating_sub(1)),
            items,
        })
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
            if let PendingCall::SkillsList { cwd } = &call {
                self.skills_inflight.remove(cwd);
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
                let cwd = self.cwd.to_string_lossy().into_owned();
                self.request_skills(&cwd, false, sender)?;
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
                    "Pausing {} · press Ctrl+X twice after it completes to remove it",
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
            PendingCall::SkillsList { cwd } => {
                self.skills_inflight.remove(&cwd);
                self.skills_by_cwd
                    .insert(cwd, skills_from_list_response(&result));
                self.clamp_skill_picker_selection();
            }
        }
        Ok(())
    }

    fn handle_notification(
        &mut self,
        method: &str,
        params: Value,
        sender: &mut impl RpcSender,
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
                    let mut completed_transition = false;
                    if let Some(session) = self.session_mut(thread_id) {
                        completed_transition = session.status.is_live() && !status.is_live();
                        session.status = status;
                        if completed_transition {
                            session.history_loaded = false;
                        }
                    }
                    self.sort_sessions_preserving_selection();
                    if completed_transition {
                        self.ensure_selected_history(sender)?;
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
                    self.history_sessions
                        .retain(|session| session.id != thread_id);
                    self.lifecycle.dismiss(thread_id);
                    self.lifecycle.save().context("save session lifecycle")?;
                    self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                    self.discard_reply_draft(thread_id, was_selected);
                }
            }
            "turn/started" => self.handle_turn_started(&params)?,
            "turn/completed" => self.handle_turn_completed(&params, sender)?,
            "item/started" => self.handle_item(&params, false),
            "item/completed" => self.handle_item(&params, true),
            "item/agentMessage/delta" => self.handle_agent_delta(&params),
            "item/reasoning/summaryTextDelta" => self.handle_reasoning_delta(&params),
            "item/reasoning/summaryPartAdded" => self.handle_reasoning_part(&params),
            "skills/changed" => {
                self.skills_by_cwd.clear();
                if let Some(cwd) = self.skill_picker.as_ref().map(|picker| picker.cwd.clone()) {
                    self.request_skills(&cwd, true, sender)?;
                }
            }
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
            sender.respond_error(id, -32601, "codeck cannot route this request")?;
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
        let target = self.composer.target;
        let text = if target == ComposeTarget::Rename {
            self.composer.text.trim().to_string()
        } else {
            self.composer.prompt_text().trim().to_string()
        };
        if target == ComposeTarget::Rename {
            if text.is_empty() {
                return Ok(());
            }
            self.composer.reset();
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
            self.composer.clear_text_keep_images();
            self.cache_current_draft();
            self.answer_pending_request(&thread_id, &text, sender)?;
            return Ok(());
        }

        if text.is_empty() && self.composer.images.is_empty() {
            return Ok(());
        }
        let composer = self.composer.clone();
        let draft = PromptDraft {
            text,
            images: composer.images.clone(),
            skills: composer.skills.clone(),
            composer,
        };
        self.composer.reset();
        self.cache_current_draft();
        self.scroll_back = 0;

        match target {
            ComposeTarget::NewTask => {
                let id = sender.request(
                    "thread/start",
                    json!({
                        "cwd": self.cwd.to_string_lossy(),
                        "serviceName": "codeck",
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
                    self.restore_draft(draft);
                    self.composer.target = ComposeTarget::NewTask;
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
            // MCP form replies depend on the requested JSON schema. Until Codeck has
            // a real form renderer, declining is safer than fabricating invalid content.
            PendingRequestKind::McpElicitation => json!({ "action": "decline" }),
            PendingRequestKind::Unsupported(method) => {
                sender.respond_error(
                    request.id,
                    -32601,
                    &format!("codeck does not support {method}"),
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
                    let should_track = self.should_track(&session);
                    if should_track {
                        self.lifecycle.track(session.id.clone());
                    }
                    if self.show_all || should_track {
                        self.merge_session(session);
                    } else {
                        self.merge_history_session(session);
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
        self.history_sessions
            .retain(|session| session.id != incoming.id);
        if let Some(existing) = self.session_mut(&incoming.id) {
            let completed_transition = existing.status.is_live() && !incoming.status.is_live();
            existing.title = incoming.title;
            existing.preview = incoming.preview;
            existing.cwd = incoming.cwd;
            existing.path = incoming.path;
            existing.updated_at = incoming.updated_at;
            existing.source = incoming.source;
            existing.thread_source = incoming.thread_source;
            existing.status = incoming.status;
            if completed_transition {
                existing.history_loaded = false;
            }
        } else {
            self.sessions.push(incoming);
        }
        self.sort_sessions_preserving_selection();
    }

    fn merge_history_session(&mut self, mut incoming: Session) {
        incoming.messages.clear();
        incoming.active_turn_id = None;
        incoming.history_loaded = false;
        if let Some(existing) = self
            .history_sessions
            .iter_mut()
            .find(|session| session.id == incoming.id)
        {
            *existing = incoming;
        } else {
            self.history_sessions.push(incoming);
        }
        self.history_sessions
            .sort_by_key(|session| std::cmp::Reverse(session.updated_at));
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

    fn handle_turn_completed(&mut self, params: &Value, sender: &mut impl RpcSender) -> Result<()> {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return Ok(());
        };
        self.track_session(thread_id)?;
        let turn = params.get("turn").unwrap_or(&Value::Null);
        let turn_status = turn.get("status").and_then(Value::as_str);
        let completed_entries = turn
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(message_from_item)
            .collect::<Vec<_>>();
        if let Some(session) = self.session_mut(thread_id) {
            session.status = if turn_status == Some("failed") {
                SessionStatus::Failed
            } else {
                SessionStatus::Completed
            };
            session.active_turn_id = None;
            session.updated_at = unix_now();
            for entry in completed_entries {
                if entry.kind == MessageKind::User {
                    remove_matching_local_user(session, &entry.text);
                }
                session.upsert_message(entry);
            }
            session.history_loaded = true;
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
        self.pending_calls.retain(|_, call| {
            !matches!(call, PendingCall::ThreadRead { thread_id: pending } if pending == thread_id)
        });
        self.load_history(thread_id, sender)?;
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

    fn confirm_settings(&mut self) -> Result<()> {
        if self.settings_left_armed {
            self.settings_left_armed = false;
            self.attach_right_armed = false;
            self.settings_selection = self.lifecycle.preview_verbosity();
            self.menu_tab = MenuTab::Resume;
            self.history_picker = Some(HistoryPicker {
                query: String::new(),
                selected: 0,
            });
            self.settings_open = true;
            self.force_full_redraw = true;
            self.notice.clear();
        } else {
            self.settings_left_armed = true;
            self.notice = "Press ← again for settings".to_string();
        }
        Ok(())
    }

    fn handle_skill_picker_key(&mut self, key: KeyEvent) -> bool {
        if self.skill_picker.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Up => {
                if let Some(picker) = &mut self.skill_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                true
            }
            KeyCode::Down => {
                let item_count = self
                    .skill_picker_view()
                    .map(|view| view.items.len())
                    .unwrap_or_default();
                if let Some(picker) = &mut self.skill_picker {
                    picker.selected = picker
                        .selected
                        .saturating_add(1)
                        .min(item_count.saturating_sub(1));
                }
                true
            }
            KeyCode::Enter
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
                ) =>
            {
                let loading = self.skill_picker_view().is_some_and(|view| view.loading);
                let handled = self.accept_selected_skill() || loading;
                if !handled {
                    self.skill_picker = None;
                }
                handled
            }
            KeyCode::Tab => {
                self.accept_selected_skill();
                true
            }
            _ => false,
        }
    }

    fn accept_selected_skill(&mut self) -> bool {
        let Some(picker) = self.skill_picker.clone() else {
            return false;
        };
        let Some(skill) = self
            .skill_picker_view()
            .and_then(|view| view.items.get(view.selected).cloned())
        else {
            return false;
        };
        let name = skill.name.clone();
        self.composer.replace_with_skill(
            picker.query_start,
            SkillReference {
                name: skill.name,
                path: skill.path,
            },
        );
        self.skill_picker = None;
        self.notice = format!("Selected ${name}");
        true
    }

    fn sync_skill_picker(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        if self.composer.target == ComposeTarget::Rename
            || (self.composer.target == ComposeTarget::Reply
                && self
                    .selected_session()
                    .is_some_and(|session| self.has_pending_request(&session.id)))
        {
            self.skill_picker = None;
            return Ok(());
        }
        let Some((query_start, query)) =
            active_skill_query(&self.composer.text, self.composer.cursor)
        else {
            self.skill_picker = None;
            return Ok(());
        };
        let cwd = self.skill_context_cwd();
        match &mut self.skill_picker {
            Some(picker) if picker.cwd == cwd && picker.query_start == query_start => {
                if picker.query != query {
                    picker.query = query;
                    picker.selected = 0;
                }
            }
            _ => {
                self.skill_picker = Some(SkillPicker {
                    cwd: cwd.clone(),
                    query_start,
                    query,
                    selected: 0,
                });
            }
        }
        if !self.skills_by_cwd.contains_key(&cwd) {
            self.request_skills(&cwd, false, sender)?;
        }
        self.clamp_skill_picker_selection();
        Ok(())
    }

    fn clamp_skill_picker_selection(&mut self) {
        let item_count = self
            .skill_picker_view()
            .map(|view| view.items.len())
            .unwrap_or_default();
        if let Some(picker) = &mut self.skill_picker {
            picker.selected = picker.selected.min(item_count.saturating_sub(1));
        }
    }

    fn skill_context_cwd(&self) -> String {
        if self.composer.target == ComposeTarget::Reply
            && let Some(cwd) = self
                .selected_session()
                .map(|session| session.cwd.as_str())
                .filter(|cwd| !cwd.is_empty())
        {
            return cwd.to_string();
        }
        self.cwd.to_string_lossy().into_owned()
    }

    fn request_skills(
        &mut self,
        cwd: &str,
        force_reload: bool,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        if !self.skills_inflight.insert(cwd.to_string()) {
            return Ok(());
        }
        let id = match sender.request(
            "skills/list",
            json!({ "cwds": [cwd], "forceReload": force_reload }),
        ) {
            Ok(id) => id,
            Err(error) => {
                self.skills_inflight.remove(cwd);
                return Err(error);
            }
        };
        self.pending_calls.insert(
            id,
            PendingCall::SkillsList {
                cwd: cwd.to_string(),
            },
        );
        Ok(())
    }

    fn handle_settings_key(&mut self, key: KeyEvent, sender: &mut impl RpcSender) -> Result<()> {
        let settings_left = key.code == KeyCode::Left
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !settings_left {
            self.settings_left_armed = false;
        }

        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.menu_tab = match self.menu_tab {
                MenuTab::Resume => MenuTab::Settings,
                MenuTab::Settings => MenuTab::Resume,
            };
            self.settings_left_armed = false;
            self.force_full_redraw = true;
            return Ok(());
        }

        if settings_left && key.kind == KeyEventKind::Repeat {
            return Ok(());
        }
        if settings_left && self.settings_left_armed {
            self.settings_open = false;
            self.settings_left_armed = false;
            self.history_picker = None;
            self.settings_selection = self.lifecycle.preview_verbosity();
            self.force_full_redraw = true;
            self.notice = "Menu closed".to_string();
            return Ok(());
        }
        if settings_left {
            self.settings_left_armed = true;
            return Ok(());
        }

        if self.menu_tab == MenuTab::Resume {
            return self.handle_resume_tab_key(key, sender);
        }

        match key.code {
            KeyCode::Up => {
                let previous = self.settings_selection;
                self.settings_selection = match self.settings_selection {
                    PreviewVerbosity::Full => PreviewVerbosity::Full,
                    PreviewVerbosity::Progress => PreviewVerbosity::Full,
                    PreviewVerbosity::Final => PreviewVerbosity::Progress,
                };
                self.force_full_redraw |= self.settings_selection != previous;
            }
            KeyCode::Down => {
                let previous = self.settings_selection;
                self.settings_selection = match self.settings_selection {
                    PreviewVerbosity::Full => PreviewVerbosity::Progress,
                    PreviewVerbosity::Progress => PreviewVerbosity::Final,
                    PreviewVerbosity::Final => PreviewVerbosity::Final,
                };
                self.force_full_redraw |= self.settings_selection != previous;
            }
            KeyCode::Enter => {
                self.lifecycle
                    .set_preview_verbosity(self.settings_selection);
                self.lifecycle.save().context("save Codeck settings")?;
                self.settings_open = false;
                self.settings_left_armed = false;
                self.history_picker = None;
                self.force_full_redraw = true;
                self.scroll_back = 0;
                self.notice = format!(
                    "Preview verbosity: {}",
                    preview_verbosity_name(self.settings_selection)
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_resume_tab_key(&mut self, key: KeyEvent, sender: &mut impl RpcSender) -> Result<()> {
        match key.code {
            KeyCode::Up => {
                if let Some(picker) = &mut self.history_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                let item_count = self
                    .history_picker_view()
                    .map(|view| view.items.len())
                    .unwrap_or_default();
                if let Some(picker) = &mut self.history_picker {
                    picker.selected = picker
                        .selected
                        .saturating_add(1)
                        .min(item_count.saturating_sub(1));
                }
            }
            KeyCode::Enter => self.add_selected_history_session(sender)?,
            KeyCode::Backspace => {
                if let Some(picker) = &mut self.history_picker {
                    picker.query.pop();
                    picker.selected = 0;
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = &mut self.history_picker {
                    picker.query.clear();
                    picker.selected = 0;
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if let Some(picker) = &mut self.history_picker {
                    picker.query.push(character);
                    picker.selected = 0;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn add_selected_history_session(&mut self, sender: &mut impl RpcSender) -> Result<()> {
        let Some(session) = self
            .history_picker_view()
            .and_then(|view| view.items.get(view.selected).cloned())
        else {
            self.notice = "No matching session to add".to_string();
            return Ok(());
        };
        let thread_id = session.id.clone();
        let title = session.title.clone();
        if self.composer.target == ComposeTarget::Rename {
            self.cancel_rename();
        } else {
            self.cache_current_draft();
        }
        self.track_session(&thread_id)?;
        self.merge_session(session);
        self.select_session(&thread_id);
        self.load_reply_draft(&thread_id);
        self.settings_open = false;
        self.settings_left_armed = false;
        self.history_picker = None;
        self.force_full_redraw = true;
        self.scroll_back = 0;
        self.ensure_selected_history(sender)?;
        self.notice = format!("Added {title} · type a reply and press Enter to resume");
        Ok(())
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
            .unwrap_or_else(|| {
                let mut composer = Composer::default();
                composer.target = ComposeTarget::Reply;
                composer
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
        self.composer.reset();
        self.composer.text = title;
        self.composer.cursor = self.composer.text.len();
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
                && matches!(
                    session.thread_source.as_deref(),
                    Some(THREAD_SOURCE | LEGACY_THREAD_SOURCE)
                ))
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
                self.notice = "Loading the active turn before pausing it…".to_string();
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
            self.notice = "Pausing session…".to_string();
            return Ok(());
        }
        let removed_session = session.clone();
        let thread_id = session.id.clone();
        let was_selected =
            self.selected_session().map(|session| session.id.as_str()) == Some(thread_id.as_str());
        self.lifecycle.dismiss(&thread_id);
        self.lifecycle.save().context("save session lifecycle")?;
        self.sessions.retain(|session| session.id != thread_id);
        self.merge_history_session(removed_session);
        self.pending_requests
            .retain(|request| request.thread_id != thread_id);
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        self.discard_reply_draft(&thread_id, was_selected);
        self.scroll_back = 0;
        self.notice = "Reviewed session removed from Codeck; Codex history was kept".to_string();
        self.ensure_selected_history(sender)
    }

    fn confirm_stop_or_remove(
        &mut self,
        key_kind: KeyEventKind,
        sender: &mut impl RpcSender,
    ) -> Result<()> {
        if key_kind == KeyEventKind::Repeat {
            return Ok(());
        }
        if self.show_all {
            self.ctrl_x_armed = None;
            self.notice = "Remove is available in the lifecycle view, without --all".to_string();
            return Ok(());
        }
        let Some(session) = self.selected_session() else {
            self.ctrl_x_armed = None;
            self.notice = "No session selected".to_string();
            return Ok(());
        };
        let action = if session.status.is_live() {
            CtrlXAction::Pause
        } else {
            CtrlXAction::Remove
        };
        let confirmation = CtrlXConfirmation {
            thread_id: session.id.clone(),
            action,
        };
        let title = session.title.clone();
        if self.ctrl_x_armed.as_ref() == Some(&confirmation) {
            self.ctrl_x_armed = None;
            return self.stop_or_remove_selected(sender);
        }
        self.ctrl_x_armed = Some(confirmation);
        self.notice = match action {
            CtrlXAction::Pause => format!("Press Ctrl+X again to pause {title}"),
            CtrlXAction::Remove => format!(
                "Press Ctrl+X again to remove {title} from Codeck · Codex history will be kept"
            ),
        };
        Ok(())
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
            let left_pin_rank = u8::from(!lifecycle.is_pinned(&left.id));
            let right_pin_rank = u8::from(!lifecycle.is_pinned(&right.id));
            left_pin_rank
                .cmp(&right_pin_rank)
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

fn preview_verbosity_name(verbosity: PreviewVerbosity) -> &'static str {
    match verbosity {
        PreviewVerbosity::Full => "Full",
        PreviewVerbosity::Progress => "Progress",
        PreviewVerbosity::Final => "Final",
    }
}

fn active_skill_query(text: &str, cursor: usize) -> Option<(usize, String)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    if text[cursor..]
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    let prefix = &text[..cursor];
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    let query = prefix.get(start..)?.strip_prefix('$')?;
    Some((start, query.to_string()))
}

fn skill_matches(skill: &SkillMetadata, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    skill.name.to_ascii_lowercase().contains(&query)
        || skill.description.to_ascii_lowercase().contains(&query)
}

fn history_session_matches(session: &Session, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || session.title.to_lowercase().contains(&query)
        || session.cwd.to_lowercase().contains(&query)
        || session.preview.to_lowercase().contains(&query)
        || session.id.to_lowercase().contains(&query)
}

fn skills_from_list_response(result: &Value) -> Vec<SkillMetadata> {
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("skills").and_then(Value::as_array))
        .flatten()
        .filter(|skill| {
            skill
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .filter_map(|skill| {
            let name = skill.get("name")?.as_str()?.to_string();
            let path = skill.get("path")?.as_str()?.to_string();
            let description = skill
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| skill.get("shortDescription").and_then(Value::as_str))
                .or_else(|| {
                    skill
                        .get("interface")
                        .and_then(|interface| interface.get("shortDescription"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .to_string();
            let scope = skill
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(SkillMetadata {
                name,
                description,
                path,
                scope,
            })
        })
        .collect()
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
    let mut input = Vec::with_capacity(
        usize::from(!draft.text.is_empty()) + draft.skills.len() + draft.images.len(),
    );
    if !draft.text.is_empty() {
        input.push(json!({
            "type": "text",
            "text": draft.text,
            "text_elements": []
        }));
    }
    input.extend(draft.skills.iter().map(|skill| {
        json!({
            "type": "skill",
            "name": skill.name,
            "path": skill.path
        })
    }));
    input.extend(draft.images.iter().map(|path| {
        json!({
            "type": "localImage",
            "path": path.to_string_lossy()
        })
    }));
    input
}

fn draft_display(draft: &PromptDraft) -> String {
    let display = draft.composer.text.trim();
    if !display.is_empty() {
        return display.to_string();
    }
    (1..=draft.images.len())
        .map(|number| format!("[Image #{number}]"))
        .collect::<Vec<_>>()
        .join(" ")
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
    let mut image_number = 0usize;
    let mut parts = Vec::new();
    for part in content.and_then(Value::as_array).into_iter().flatten() {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            Some("localImage" | "image") => {
                image_number = image_number.saturating_add(1);
                parts.push(format!("[Image #{image_number}]"));
            }
            _ => {}
        }
    }
    parts.join("\n")
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
        let (line, inline_image) = strip_inline_image_markers(line);
        has_image |= inline_image;
        if is_legacy_image_marker(line.trim()) {
            has_image = true;
        } else {
            body_lines.push(line);
        }
    }
    let body = body_lines
        .join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (body, has_image)
}

fn strip_inline_image_markers(line: &str) -> (String, bool) {
    const PREFIX: &str = "[Image #";
    let mut output = String::new();
    let mut remaining = line;
    let mut found = false;
    while let Some(start) = remaining.find(PREFIX) {
        output.push_str(&remaining[..start]);
        let marker = &remaining[start + PREFIX.len()..];
        let Some(end) = marker.find(']') else {
            output.push_str(&remaining[start..]);
            return (output, found);
        };
        if marker[..end].parse::<usize>().is_ok() {
            found = true;
            remaining = &marker[end + 1..];
        } else {
            output.push_str(PREFIX);
            remaining = marker;
        }
    }
    output.push_str(remaining);
    (output, found)
}

fn is_legacy_image_marker(line: &str) -> bool {
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
            "codeck-app-lifecycle-{}-{sequence}.json",
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
    fn settings_requires_two_left_presses_and_persists_preview_verbosity() {
        let (mut app, state_path) = test_app(true);
        let mut sender = test_sender();

        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("first left");
        assert!(!app.settings_open());
        app.handle_key(
            KeyEvent::new_with_kind(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Repeat),
            &mut sender,
        )
        .expect("held left");
        assert!(!app.settings_open());
        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("second left");
        assert!(app.settings_open());
        assert_eq!(app.menu_tab(), MenuTab::Resume);
        assert!(app.take_force_full_redraw());
        assert_eq!(app.settings_selection(), PreviewVerbosity::Full);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut sender)
            .expect("switch to settings");
        assert_eq!(app.menu_tab(), MenuTab::Settings);
        assert!(app.take_force_full_redraw());
        app.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("select progress");
        assert!(app.take_force_full_redraw());
        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("save settings");
        assert!(!app.settings_open());
        assert_eq!(app.preview_verbosity(), PreviewVerbosity::Progress);

        let restored = LifecycleStore::for_test(state_path.clone());
        assert_eq!(restored.preview_verbosity(), PreviewVerbosity::Progress);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn resume_menu_adds_a_history_session_then_resumes_it_in_codeck() {
        let (mut app, state_path) = test_app(false);
        app.merge_history_session(test_session(
            "history-thread",
            "Legacy research",
            SessionStatus::Completed,
        ));
        let mut sender = test_sender();

        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("first left");
        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("open menu");
        assert_eq!(app.menu_tab(), MenuTab::Resume);
        assert_eq!(
            app.history_picker_view().expect("resume tab").items.len(),
            1
        );
        for character in "research".chars() {
            app.handle_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut sender,
            )
            .expect("filter history");
        }
        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("add history session");

        assert!(!app.settings_open());
        assert_eq!(
            app.selected_session().expect("added session").id,
            "history-thread"
        );
        assert_eq!(app.composer.target, ComposeTarget::Reply);
        assert!(app.lifecycle.contains("history-thread"));
        assert!(app.history_sessions.is_empty());

        app.insert_text("Continue this work");
        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("resume inside Codeck");
        assert_eq!(
            sender.requests.last(),
            Some(&(
                "thread/resume".to_string(),
                json!({"threadId":"history-thread"})
            ))
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn left_arrow_keeps_editing_nonempty_composer() {
        let (mut app, state_path) = test_app(true);
        let mut sender = test_sender();
        app.insert_text("abc");

        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("move left once");
        app.handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("move left twice");

        assert!(!app.settings_open());
        assert_eq!(app.composer.cursor, 1);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn dollar_opens_codex_skill_picker_and_sends_structured_skill_input() {
        let (mut app, state_path) = test_app(true);
        let mut sender = test_sender();

        app.handle_key(
            KeyEvent::new(KeyCode::Char('$'), KeyModifiers::NONE),
            &mut sender,
        )
        .expect("type dollar");
        assert_eq!(app.composer.text, "$");
        assert!(app.skill_picker_view().expect("picker").loading);
        assert_eq!(sender.requests[0].0, "skills/list");
        assert_eq!(sender.requests[0].1["cwds"], json!(["/tmp"]));
        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("ignore enter while skills load");
        assert_eq!(sender.requests.len(), 1);
        assert_eq!(app.composer.text, "$");

        app.handle_client_event(
            ClientEvent::Message(json!({
                "id": 1,
                "result": {
                    "data": [{
                        "cwd": "/tmp",
                        "skills": [
                            {
                                "name": "documents",
                                "description": "Create and edit documents",
                                "path": "/tmp/skills/documents/SKILL.md",
                                "scope": "user",
                                "enabled": true
                            },
                            {
                                "name": "disabled",
                                "description": "Hidden",
                                "path": "/tmp/skills/disabled/SKILL.md",
                                "scope": "user",
                                "enabled": false
                            }
                        ],
                        "errors": []
                    }]
                }
            })),
            &mut sender,
        )
        .expect("skills response");
        let picker = app.skill_picker_view().expect("loaded picker");
        assert!(!picker.loading);
        assert_eq!(picker.items.len(), 1);
        assert_eq!(picker.items[0].name, "documents");

        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("select skill");
        assert!(app.skill_picker_view().is_none());
        assert_eq!(app.composer.text, "$documents ");
        assert_eq!(app.composer.skills.len(), 1);

        app.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("submit skill prompt");
        let draft = app
            .pending_calls
            .values()
            .find_map(|call| match call {
                PendingCall::ThreadStart { draft } => Some(draft),
                _ => None,
            })
            .expect("pending thread with skill draft");
        assert_eq!(
            prompt_input(draft),
            vec![
                json!({
                    "type": "text",
                    "text": "$documents",
                    "text_elements": []
                }),
                json!({
                    "type": "skill",
                    "name": "documents",
                    "path": "/tmp/skills/documents/SKILL.md"
                })
            ]
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn skill_query_requires_a_token_boundary_and_filters_as_you_type() {
        assert_eq!(active_skill_query("$doc", 4), Some((0, "doc".to_string())));
        assert_eq!(
            active_skill_query("please $doc", 11),
            Some((7, "doc".to_string()))
        );
        assert_eq!(active_skill_query("cost$doc", 8), None);
        assert_eq!(active_skill_query("$document rest", 4), None);

        let skill = SkillMetadata {
            name: "documents".to_string(),
            description: "Word and Google Docs".to_string(),
            path: "/tmp/documents/SKILL.md".to_string(),
            scope: "user".to_string(),
        };
        assert!(skill_matches(&skill, "doc"));
        assert!(skill_matches(&skill, "google"));
        assert!(!skill_matches(&skill, "slides"));
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
        assert_eq!(app.history_sessions.len(), 1);
        assert_eq!(app.history_sessions[0].id, "old-thread");
        assert!(app.lifecycle.contains("live-thread"));
        assert!(!app.lifecycle.contains("old-thread"));
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn initial_lifecycle_adopts_sessions_created_by_legacy_name() {
        let (mut app, state_path) = test_app(false);
        app.merge_thread_list(&json!({
            "data": [{
                "id": "legacy-thread",
                "preview": "Pre-rename session",
                "cwd": "/tmp/legacy",
                "status": {"type": "idle"},
                "source": "appServer",
                "threadSource": LEGACY_THREAD_SOURCE
            }]
        }));

        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, "legacy-thread");
        assert!(app.lifecycle.contains("legacy-thread"));
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn completed_session_waits_for_review_then_leaves_only_codeck() {
        let (mut app, state_path) = test_app(false);
        let session = Session::from_thread(&json!({
            "id": "review-thread",
            "preview": "Review me",
            "cwd": "/tmp/review",
            "status": {"type": "active", "activeFlags": []},
            "source": "appServer",
            "threadSource": THREAD_SOURCE
        }))
        .expect("session");
        app.lifecycle.track(session.id.clone());
        app.sessions.push(session);
        app.sessions[0].status = SessionStatus::Completed;
        let mut sender = test_sender();

        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut sender,
        )
        .expect("arm removal");
        assert_eq!(app.sessions.len(), 1);
        assert!(app.notice.contains("Press Ctrl+X again"));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut sender,
        )
        .expect("confirm removal");

        assert!(app.sessions.is_empty());
        assert_eq!(app.history_sessions[0].id, "review-thread");
        assert!(!app.lifecycle.contains("review-thread"));
        assert!(app.notice.contains("Codex history was kept"));
        std::fs::remove_file(state_path).expect("remove lifecycle state");
    }

    #[test]
    fn pin_persists_and_moves_session_to_the_top() {
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
    fn sessions_sort_by_pin_then_status_then_latest_reply() {
        let (mut app, state_path) = test_app(false);
        let mut pinned = test_session("pinned", "Pinned", SessionStatus::Completed);
        pinned.updated_at = 1;
        let mut older_working =
            test_session("older-working", "Older working", SessionStatus::Working);
        older_working.updated_at = 10;
        let mut newer_working =
            test_session("newer-working", "Newer working", SessionStatus::Working);
        newer_working.updated_at = 20;
        let mut completed = test_session("completed", "Completed", SessionStatus::Completed);
        completed.updated_at = 30;
        app.sessions = vec![completed, older_working, pinned, newer_working];
        app.lifecycle.toggle_pin("pinned");
        app.lifecycle.save().expect("save lifecycle state");

        app.sort_sessions();

        assert_eq!(
            app.sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["pinned", "newer-working", "older-working", "completed"]
        );
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
        app.add_images(vec![PathBuf::from("/tmp/pasted.png")]);
        assert_eq!(app.composer.text, "[Image #1] ");
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
            "[Image #1]"
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn inline_image_placeholder_is_not_sent_as_prompt_text() {
        let (mut app, state_path) = test_app(false);
        let mut session = test_session("image-thread", "Image", SessionStatus::Working);
        session.active_turn_id = Some("turn-1".to_string());
        app.sessions.push(session);
        app.composer.target = ComposeTarget::Reply;
        app.insert_text("look please");
        app.composer.cursor = "look ".len();
        app.add_images(vec![PathBuf::from("/tmp/pasted.png")]);
        assert_eq!(app.composer.text, "look [Image #1] please");
        let mut sender = test_sender();

        app.submit(&mut sender).expect("submit image reply");

        assert_eq!(
            sender.requests.last(),
            Some(&(
                "turn/steer".to_string(),
                json!({
                    "threadId":"image-thread",
                    "expectedTurnId":"turn-1",
                    "input":[
                        {"type":"text","text":"look please","text_elements":[]},
                        {"type":"localImage","path":"/tmp/pasted.png"}
                    ]
                })
            ))
        );
        assert_eq!(
            app.sessions[0]
                .messages
                .last()
                .expect("inline image preview")
                .text,
            "look [Image #1] please"
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
            "Inspect this\n[Image #1]"
        );
    }

    #[test]
    fn official_image_message_replaces_its_optimistic_local_preview() {
        let mut session = test_session("image-thread", "Image", SessionStatus::Working);
        session.messages.push(MessageEntry {
            id: "local-user-1".to_string(),
            kind: MessageKind::User,
            text: "测试一下[Image #1]你是否真的收到了图".to_string(),
        });

        remove_matching_local_user(&mut session, "测试一下你是否真的收到了图\n[Image #1]");

        assert!(session.messages.is_empty());
        assert_eq!(
            user_message_signature("[Image #1] [Image #2]"),
            (String::new(), true)
        );
    }

    #[test]
    fn turn_completed_merges_final_and_requests_authoritative_history() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "completed-thread",
            "Completed",
            SessionStatus::Working,
        ));
        let mut sender = test_sender();

        app.handle_notification(
            "turn/completed",
            json!({
                "threadId": "completed-thread",
                "turn": {
                    "id": "turn-1",
                    "status": "completed",
                    "items": [{
                        "id": "final-1",
                        "type": "agentMessage",
                        "phase": "final_answer",
                        "text": "The final answer"
                    }]
                }
            }),
            &mut sender,
        )
        .expect("complete turn");

        assert_eq!(app.sessions[0].status, SessionStatus::Completed);
        assert!(
            app.sessions[0].messages.iter().any(|entry| {
                entry.kind == MessageKind::Final && entry.text == "The final answer"
            })
        );
        assert_eq!(
            sender.requests.last(),
            Some(&(
                "thread/read".to_string(),
                json!({"threadId":"completed-thread","includeTurns":true})
            ))
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn completed_status_transition_refreshes_selected_history() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "completed-thread",
            "Completed",
            SessionStatus::Working,
        ));
        let mut sender = test_sender();

        app.handle_notification(
            "thread/status/changed",
            json!({
                "threadId": "completed-thread",
                "status": {"type": "idle"}
            }),
            &mut sender,
        )
        .expect("complete thread");

        assert_eq!(app.sessions[0].status, SessionStatus::Completed);
        assert!(!app.sessions[0].history_loaded);
        assert_eq!(
            sender.requests.last(),
            Some(&(
                "thread/read".to_string(),
                json!({"threadId":"completed-thread","includeTurns":true})
            ))
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn polled_completion_marks_unselected_history_stale() {
        let (mut app, state_path) = test_app(false);
        app.sessions.push(test_session(
            "selected-thread",
            "Selected",
            SessionStatus::Working,
        ));
        app.sessions.push(test_session(
            "background-thread",
            "Background",
            SessionStatus::Working,
        ));
        let incoming = test_session("background-thread", "Background", SessionStatus::Completed);

        app.merge_session(incoming);

        let background = app
            .session("background-thread")
            .expect("background session");
        assert_eq!(background.status, SessionStatus::Completed);
        assert!(!background.history_loaded);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn ctrl_x_interrupts_a_working_turn_without_removing_the_session() {
        let (mut app, state_path) = test_app(false);
        let mut session = test_session("working-thread", "Working", SessionStatus::Working);
        session.active_turn_id = Some("turn-1".to_string());
        app.sessions.push(session);
        let mut sender = test_sender();

        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut sender,
        )
        .expect("arm pause");
        assert!(sender.requests.is_empty());
        assert!(app.notice.contains("Press Ctrl+X again"));
        app.handle_key(
            KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            ),
            &mut sender,
        )
        .expect("ignore held key");
        assert!(sender.requests.is_empty());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut sender,
        )
        .expect("confirm pause");

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
    fn ctrl_x_confirmation_is_cancelled_by_an_intervening_key() {
        let (mut app, state_path) = test_app(false);
        let mut session = test_session("working-thread", "Working", SessionStatus::Working);
        session.active_turn_id = Some("turn-1".to_string());
        app.sessions.push(session);
        let mut sender = test_sender();
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

        app.handle_key(ctrl_x, &mut sender).expect("arm pause");
        app.handle_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut sender,
        )
        .expect("intervening key");
        app.handle_key(ctrl_x, &mut sender).expect("re-arm pause");

        assert!(sender.requests.is_empty());
        assert!(app.notice.contains("Press Ctrl+X again"));
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
