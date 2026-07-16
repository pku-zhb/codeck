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

By default, the deck follows pagination and shows all unarchived sessions from
every Codex client. Limit the list to sessions created by the deck itself:

```bash
codex-deck --managed-only
```

Check daemon connectivity without opening the TUI:

```bash
codex-deck --check
```

## Keys

- `Up` / `Down`: select a session
- `Tab`: switch the composer between a new task and a reply
- `Ctrl+N`: compose a new task
- `Ctrl+R`: reply to the selected session
- `Enter`: send
- `PageUp` / `PageDown`: scroll the shared thinking/final stream
- `Ctrl+C`: close the dashboard; running tasks continue

When Codex requests approval, reply with `y` (once), `a` (session), or `n`.
When Codex asks several questions, separate answers with `|`.

## Scope

The dashboard shows sessions from all Codex clients by default and creates new
threads with the `codex-deck` thread source. Use `--managed-only` when you want
an isolated view. It does not inspect terminal processes, SQLite, or rollout
JSONL files to infer runtime status.

The detached app-server writes its PID, Unix socket, and log under
`~/.codex-deck/`. The dashboard only owns its short-lived WebSocket connection;
closing it does not stop the background server or active turns.
