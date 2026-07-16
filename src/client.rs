use std::fs::{self, OpenOptions};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

#[derive(Debug)]
pub enum ClientEvent {
    Message(Value),
    Warning(String),
    Disconnected(String),
}

pub trait RpcSender {
    fn request(&mut self, method: &str, params: Value) -> Result<u64>;
    fn notify(&mut self, method: &str, params: Value) -> Result<()>;
    fn respond(&mut self, id: Value, result: Value) -> Result<()>;
    fn respond_error(&mut self, id: Value, code: i64, message: &str) -> Result<()>;
}

pub struct CodexClient {
    outgoing: Sender<ClientCommand>,
    incoming: Receiver<ClientEvent>,
    next_id: u64,
    endpoint: String,
}

enum ClientCommand {
    Json(Value),
    Close,
}

impl CodexClient {
    pub fn connect() -> Result<Self> {
        let socket = ensure_app_server()?;
        let websocket = connect_websocket(&socket)?;
        let endpoint = format!("unix://{}", socket.display());
        let (event_tx, incoming) = mpsc::channel();
        let (outgoing, command_rx) = mpsc::channel();
        thread::spawn(move || run_websocket(websocket, command_rx, event_tx));

        Ok(Self {
            outgoing,
            incoming,
            next_id: 1,
            endpoint,
        })
    }

    pub fn try_recv(&self) -> Option<ClientEvent> {
        self.incoming.try_recv().ok()
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn send(&mut self, message: Value) -> Result<()> {
        self.outgoing
            .send(ClientCommand::Json(message))
            .context("send app-server request")
    }
}

impl RpcSender for CodexClient {
    fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "method": method, "id": id, "params": params }))?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({ "method": method, "params": params }))
    }

    fn respond(&mut self, id: Value, result: Value) -> Result<()> {
        self.send(json!({ "id": id, "result": result }))
    }

    fn respond_error(&mut self, id: Value, code: i64, message: &str) -> Result<()> {
        self.send(json!({
            "id": id,
            "error": { "code": code, "message": message }
        }))
    }
}

impl Drop for CodexClient {
    fn drop(&mut self) {
        let _ = self.outgoing.send(ClientCommand::Close);
    }
}

fn connect_websocket(socket: &Path) -> Result<WebSocket<UnixStream>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
                if !process_from_state_is_alive()? {
                    return Err(error).context("Codex app-server exited before accepting clients");
                }
            }
            Err(error) => {
                return Err(error).with_context(|| format!("connect to {}", socket.display()));
            }
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .context("set app-server read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("set app-server write timeout")?;
    let request = "ws://localhost/"
        .into_client_request()
        .context("build app-server WebSocket request")?;
    let (websocket, _) =
        tungstenite::client(request, stream).context("complete app-server WebSocket handshake")?;
    Ok(websocket)
}

fn run_websocket(
    mut websocket: WebSocket<UnixStream>,
    commands: Receiver<ClientCommand>,
    events: Sender<ClientEvent>,
) {
    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                ClientCommand::Json(value) => {
                    let text = match serde_json::to_string(&value) {
                        Ok(text) => text,
                        Err(error) => {
                            let _ = events.send(ClientEvent::Warning(format!(
                                "failed to encode app-server request: {error}"
                            )));
                            continue;
                        }
                    };
                    if let Err(error) = websocket.send(Message::Text(text.into())) {
                        let _ = events.send(ClientEvent::Disconnected(format!(
                            "app-server write failed: {error}"
                        )));
                        return;
                    }
                }
                ClientCommand::Close => {
                    let _ = websocket.close(None);
                    return;
                }
            }
        }

        match websocket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                Ok(message) => {
                    if events.send(ClientEvent::Message(message)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = events.send(ClientEvent::Warning(format!(
                        "invalid app-server JSON: {error}"
                    )));
                }
            },
            Ok(Message::Ping(payload)) => {
                let _ = websocket.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => {
                let _ = events.send(ClientEvent::Disconnected(
                    "app-server connection closed".to_string(),
                ));
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed) => {
                let _ = events.send(ClientEvent::Disconnected(
                    "app-server connection closed".to_string(),
                ));
                return;
            }
            Err(error) => {
                let _ = events.send(ClientEvent::Disconnected(format!(
                    "app-server read failed: {error}"
                )));
                return;
            }
        }
    }
}

fn ensure_app_server() -> Result<PathBuf> {
    let state_dir = state_dir()?;
    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    let socket = state_dir.join("app-server.sock");
    let pid_path = state_dir.join("app-server.pid");

    if let Some(pid) = read_pid(&pid_path)
        && process_is_alive(pid)
        && socket.exists()
    {
        return Ok(socket);
    }

    remove_if_exists(&socket)?;
    remove_if_exists(&pid_path)?;
    let log_path = state_dir.join("app-server.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let errors = log.try_clone().context("clone app-server log handle")?;
    let endpoint = format!("unix://{}", socket.display());
    let mut command = Command::new("codex");
    command
        .args(["app-server", "--listen"])
        .arg(endpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors));
    // The app-server must survive both codex-deck and its launching terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = command.spawn().context("start detached Codex app-server")?;
    let pid = child.id();
    fs::write(&pid_path, format!("{pid}\n"))
        .with_context(|| format!("write {}", pid_path.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socket.exists() && process_is_alive(pid) {
            return Ok(socket);
        }
        thread::sleep(Duration::from_millis(50));
    }

    let log_tail = fs::read_to_string(&log_path).unwrap_or_default();
    let tail = log_tail.lines().rev().take(8).collect::<Vec<_>>();
    bail!(
        "Codex app-server did not create {}{}",
        socket.display(),
        if tail.is_empty() {
            String::new()
        } else {
            format!(
                "\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )
        }
    )
}

pub(crate) fn state_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex-deck"))
}

fn process_from_state_is_alive() -> Result<bool> {
    Ok(read_pid(&state_dir()?.join("app-server.pid"))
        .map(process_is_alive)
        .unwrap_or(false))
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove stale {}", path.display())),
    }
}
