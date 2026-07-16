mod app;
mod client;
mod model;
mod ui;

use std::io::{self, stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use app::App;
use clap::Parser;
use client::CodexClient;
use crossterm::cursor::Show;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

#[derive(Debug, Parser)]
#[command(name = "codex-deck", version, about)]
struct Args {
    /// Working directory used for newly dispatched sessions.
    #[arg(short = 'C', long = "cd", value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Show only sessions created by codex-deck.
    #[arg(long)]
    managed_only: bool,

    /// Verify daemon connectivity and print a short summary.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cwd = resolve_cwd(args.cwd)?;
    let mut client = CodexClient::connect()?;
    let mut app = App::new(cwd, !args.managed_only);
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
        if needs_draw {
            terminal.draw(|frame| ui::render(frame, app))?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(100)).context("poll terminal event")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) => app.handle_key(key, client)?,
                Event::Paste(text) => app.insert_text(&text),
                Event::Resize(_, _) => {}
                _ => {}
            }
            needs_draw = true;
        }
    }
    Ok(())
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
