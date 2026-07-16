# codex-deck

`codex-deck` is a keyboard-first terminal dashboard for background Codex
sessions. It runs the official Codex app-server behind a private Unix socket,
so tasks keep running after the dashboard exits.

## Requirements

- Codex CLI with `app-server` support
- macOS or another terminal supported by Crossterm

## Install

```bash
cargo install --path .
```

## Usage

Run from the directory new sessions should use:

```bash
codex-deck
```

Use another working directory:

```bash
codex-deck -C /path/to/project
```

By default, the deck is a managed lifecycle rather than a history browser. It
automatically adopts sessions started by the deck and currently active sessions
from other Codex clients. Completed sessions remain until you review and dismiss
them. Browse every unarchived local Codex session explicitly with:

```bash
codex-deck --all
```

Check daemon connectivity without opening the TUI:

```bash
codex-deck --check
```

## Keys

- `Up` / `Down`: select a session
- `Enter` / `Right`: attach the selected session in the native Codex TUI when
  the composer is empty
- `Tab`: switch the composer between a new task and a reply
- `Ctrl+N`: compose a new task
- `Ctrl+R`: reply to the selected session
- `Enter`: send
- `PageUp` / `PageDown`: scroll the shared thinking/final stream
- `Delete` / `Ctrl+D`: remove a completed/failed session from the deck after
  review; its Codex history is preserved
- `Ctrl+C`: close the dashboard; running tasks continue

While attached, use native Codex normally. Run `/exit` to return to the deck;
the dashboard reconnects to the same app-server and refreshes the transcript.
Sessions with large rollout files use a bounded 4 MiB tail preview instead of
requesting the full transcript, so one oversized history cannot disconnect the
dashboard.

When Codex requests approval, reply with `y` (once), `a` (session), or `n`.
When Codex asks several questions, separate answers with `|`.

## Scope

The lifecycle registry is stored in `~/.codex-deck/lifecycle.json`. It contains
only tracked thread IDs. Dismissing a session updates this registry without
deleting, archiving, or modifying the underlying Codex thread. A dismissed
thread is automatically adopted again if it becomes active later. Use `--all`
when you need the old full-history view.

The dashboard does not inspect terminal processes or SQLite. A bounded rollout
tail is used only for oversized transcript previews, never to infer runtime
status.

The detached app-server writes its PID, Unix socket, and log under
`~/.codex-deck/`. The dashboard only owns its short-lived WebSocket connection;
closing it does not stop the background server or active turns.
