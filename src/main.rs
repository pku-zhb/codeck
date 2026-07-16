mod app;
mod client;
mod clipboard;
mod lifecycle;
mod markdown;
mod model;
mod transcript;
mod ui;

use std::io::{self, stdout};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use app::{App, AttachRequest};
use clap::Parser;
use client::CodexClient;
use crossterm::cursor::Show;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;

#[derive(Debug, Parser)]
#[command(name = "codex-deck", version, about)]
struct Args {
    /// Working directory used for newly dispatched sessions.
    #[arg(short = 'C', long = "cd", value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Browse all unarchived Codex history instead of the managed lifecycle.
    #[arg(long)]
    all: bool,

    /// Verify daemon connectivity and print a short summary.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cwd = resolve_cwd(args.cwd)?;
    let mut client = CodexClient::connect()?;
    let mut app = App::new(cwd, args.all)?;
    app.begin(&mut client)?;

    if args.check {
        return run_check(app, client);
    }

    run_tui(app, client)
}

fn resolve_cwd(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path,
        None => std::env::current_dir().context("read current directory")?,
    };
    let canonical = path
        .canonicalize()
        .with_context(|| format!("working directory does not exist: {}", path.display()))?;
    if !canonical.is_dir() {
        bail!(
            "working directory is not a directory: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn run_check(mut app: App, mut client: CodexClient) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        while let Some(event) = client.try_recv() {
            app.handle_client_event(event, &mut client)?;
        }
        if app.initial_load_complete() {
            println!(
                "codex-deck: connected · {} session{}",
                app.sessions().len(),
                if app.sessions().len() == 1 { "" } else { "s" }
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("timed out waiting for Codex app-server")
}

fn run_tui(mut app: App, mut client: CodexClient) -> Result<()> {
    install_panic_restore_hook();
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut output = stdout();
    execute!(output, EnterAlternateScreen, EnableBracketedPaste)
        .context("enter terminal screen")?;

    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_event_loop(&mut terminal, &mut app, &mut client);
    restore_terminal(&mut terminal)?;
    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &mut CodexClient,
) -> Result<()> {
    let mut needs_draw = true;
    while !app.should_quit() {
        while let Some(incoming) = client.try_recv() {
            app.handle_client_event(incoming, client)?;
            needs_draw = true;
        }
        app.tick(client)?;

        if let Some(request) = app.take_attach_request() {
            let endpoint = client.endpoint().to_string();
            let status = run_attached_codex(terminal, &request, &endpoint)?;
            app.refresh_after_attach(&request.thread_id, client)?;
            if !status.success() {
                app.set_notice(format!("Attached Codex exited with {status}"));
            }
            needs_draw = true;
            continue;
        }

        if needs_draw {
            terminal.draw(|frame| ui::render(frame, app))?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(100)).context("poll terminal event")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) => app.handle_key(key, client)?,
                Event::Paste(text) => app.insert_paste(&text),
                Event::Resize(_, _) => {}
                _ => {}
            }
            needs_draw = true;
        }
    }
    Ok(())
}

fn run_attached_codex(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    request: &AttachRequest,
    endpoint: &str,
) -> Result<ExitStatus> {
    restore_terminal(terminal)?;
    let mut command = attached_codex_command(request, endpoint);
    let status = command.status().context("attach native Codex session");
    reenter_terminal(terminal)?;
    status
}

fn attached_codex_command(request: &AttachRequest, endpoint: &str) -> Command {
    let mut command = Command::new("codex");
    command
        .args(["resume", "--include-non-interactive", "--remote", endpoint])
        .arg(&request.thread_id);
    if request.cwd.is_dir() {
        command.current_dir(&request.cwd);
    }
    command
}

fn reenter_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode().context("re-enable terminal raw mode")?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )
    .context("re-enter terminal screen")?;
    force_full_redraw(terminal).context("redraw terminal after attach")?;
    Ok(())
}

fn force_full_redraw<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().context("disable terminal raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )
    .context("leave terminal screen")?;
    terminal.show_cursor().context("show terminal cursor")?;
    Ok(())
}

fn install_panic_restore_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen, Show);
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;
    use std::ffi::OsStr;

    #[test]
    fn attach_uses_native_tui_on_the_deck_app_server() {
        let request = AttachRequest {
            thread_id: "thread-123".to_string(),
            cwd: PathBuf::from("/tmp"),
        };
        let command = attached_codex_command(&request, "unix:///tmp/deck.sock");
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                OsStr::new("resume"),
                OsStr::new("--include-non-interactive"),
                OsStr::new("--remote"),
                OsStr::new("unix:///tmp/deck.sock"),
                OsStr::new("thread-123"),
            ]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("/tmp"))
        );
    }

    #[test]
    fn attach_return_forces_identical_content_to_be_redrawn() {
        let mut terminal = Terminal::new(TestBackend::new(12, 2)).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("deck"), frame.area()))
            .expect("first draw");
        terminal.backend_mut().clear().expect("simulate new screen");
        terminal
            .backend()
            .assert_buffer_lines(["            ", "            "]);

        force_full_redraw(&mut terminal).expect("invalidate buffers");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("deck"), frame.area()))
            .expect("redraw");
        terminal
            .backend()
            .assert_buffer_lines(["deck        ", "            "]);
    }
}
