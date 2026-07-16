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
them. The list is grouped as `Pinned`, `Working`, and `Completed`; pinned sessions
stay in the first group regardless of their current runtime status. Historical
sessions outside the Deck are available from the built-in `Resume` menu tab, so
resuming old work does not require leaving the dashboard or adding CLI flags.

Check daemon connectivity without opening the TUI:

```bash
codex-deck --check
```

## Keys

- `Up` / `Down`: select a session
- `Left`, twice consecutively: open the menu on its default `Resume` tab when
  the composer is empty; use `Tab` to switch to `Settings`
- In `Resume`: type to filter historical sessions, choose with `Up` / `Down`, and
  press `Enter` to add one to the Deck; the composer switches to Reply so the
  next message resumes it in the background
- In `Settings`: choose preview verbosity with `Up` / `Down` and save with
  `Enter`
- `Right`, twice consecutively: attach the selected session in the native Codex
  TUI when the composer is empty; holding the key does not confirm attach
- `Tab`: switch the composer between a new task and a reply
- `$`: open the Codex skill picker; keep typing to filter, choose with `Up` /
  `Down`, and insert with `Enter` or `Tab`; confirmed skills become colored,
  atomic `$skill-name` tokens
- `Ctrl+N`: compose a new task
- `Ctrl+V` (or `Cmd+V` when the terminal forwards it): attach an image from the
  system clipboard; pasting one or more image file paths also attaches them
- `Ctrl+T`: pin or unpin the selected session
- `Ctrl+R`: rename the selected session
- `Ctrl+X`, twice consecutively: pause a working session; remove a
  completed/failed session from the deck while preserving its Codex history;
  holding the key does not confirm either action
- `Enter`: send
- `PageUp` / `PageDown`: scroll the shared thinking/final stream
- Mouse wheel: scroll only the conversation preview; it never changes the
  selected session in the picker
- `Ctrl+C`: close the dashboard; running tasks continue

Attached images are inserted at the current cursor as colored `[Image #N]`
tokens, so surrounding text stays visually continuous. The visible placeholder
is removed from prompt text and the image is sent as a native Codex `localImage`
input. Confirmed image and skill tokens are atomic: `Left` / `Right` jump across
the whole token, while adjacent `Backspace` or `Delete` removes it in one step.
Image-only prompts are supported.

Drafts are isolated by intent: `New` has one global draft, while `Reply` keeps a
separate in-memory draft for every session. Moving with `Up` / `Down` saves and
restores the corresponding reply text, cursor, and image attachments, so a
half-written reply cannot be sent to the newly selected session.

The skill picker uses Codex app-server's `skills/list` result for the active
working directory. Selected skills are sent as native structured `skill` input
items in addition to their visible `$skill-name` prompt marker. Local skill
changes invalidate the cached picker automatically.

While attached, use native Codex normally. Run `/exit` to return to the deck;
the dashboard reconnects to the same app-server and refreshes the transcript.
Sessions with large rollout files use a bounded 64 MiB tail preview instead of
requesting the full transcript, so one oversized history cannot disconnect the
dashboard.

Conversation content is rendered as terminal-native Markdown: headings,
emphasis, lists, quotes, inline/fenced code, task lists, tables, and links get
distinct ANSI styles. Absolute local paths plus `http`, `https`, `file`, and
`mailto` Markdown targets are emitted as OSC 8 hyperlinks without exposing the
target in the visible text.

The Settings tab offers three persistent preview levels: `🧠 Full` shows thinking,
progress, and final replies; `💬 Progress` hides thinking; `✅ Final` shows only
final replies. User prompts, questions, and system errors remain visible at
every level. Press `Left` twice again to close the menu without saving.

When Codex requests approval, reply with `y` (once), `a` (session), or `n`.
When Codex asks several questions, separate answers with `|`.

## Scope

The lifecycle registry is stored in `~/.codex-deck/lifecycle.json`. It contains
tracked and pinned thread IDs. Removing a session updates this registry without
deleting, archiving, or modifying the underlying Codex thread. A removed thread
is available immediately in the `Resume` tab and is automatically adopted again if
it becomes active elsewhere later.

The dashboard does not inspect terminal processes or SQLite. A bounded rollout
tail is used only for oversized transcript previews, never to infer runtime
status.

The detached app-server writes its PID, Unix socket, and log under
`~/.codex-deck/`. The dashboard only owns its short-lived WebSocket connection;
closing it does not stop the background server or active turns.
