# herdr-infinity

Native [Herdr](https://herdr.dev) integration for [Infinity Code](https://infinity.hydro.run/docs/infinity-code/overview), the AWS/Hydro agent runtime.

Infinity agents spend most of their life suspended: a turn ends when the model calls a tool, and the agent is just a conversation history until the tool result arrives. That makes Infinity a natural fit for a terminal workspace manager, but Herdr cannot see `idle`, `working`, or `blocked` unless the agent tells it. This project adds that reporting without patching Infinity.

```
herdr pane                      Infinity daemon                 Herdr
┌───────────────────┐          ┌──────────────────┐          ┌─────────────────┐
│ infinity (TUI)    │  client  │ sessions.json    │  passive │ pane            │
│  └ herdr-infinity │ ───────► │ daemon.sock      │ ───────► │  report-agent   │
│     observes      │          │ SessionsUpdated  │  client  │  idle/working/  │
└───────────────────┘          └──────────────────┘          │  blocked        │
                                                              └─────────────────┘
```

## What it does

- Shows Infinity Code as a first-class agent in Herdr's sidebar and `herdr agent list`.
- Maps Infinity session states to Herdr states:

  | Infinity `SessionStatus` | Herdr state |
  | --- | --- |
  | `Running`, `Migrating` | `working` |
  | `Idle`, `Stopped` | `idle` |
  | `WaitingForChoice` | `blocked` |
  | `Archived` | released |

- Reports a resume command (`herdr-infinity run -- infinity -s <session>`) so a Herdr server restart reopens the same session.
- Releases the pane when Infinity exits.
- Falls back to `exec`'ing the real `infinity` binary outside Herdr, so the shim is safe to keep on `PATH`.

## Install

```sh
cargo install --git https://github.com/yijunyu/herdr-infinity
```

Install Infinity itself if you have not already:

```sh
cargo install infinity-agent-cli
infinity provider install bedrock --crate infinity-provider-bedrock
infinity rap install --user --crate sandbox-local
```

Then either launch Infinity through the wrapper:

```sh
herdr-infinity run -- infinity
```

or install the optional `infinity` shim so ordinary `infinity` invocations are routed automatically inside Herdr:

```sh
herdr-infinity install-shim
export PATH="$HOME/.local/share/herdr-infinity/bin:$PATH"
```

Outside a Herdr pane the shim is a transparent pass-through.

## How it works

Infinity's daemon listens on `~/.infinity/daemon.sock` and broadcasts a `SessionsUpdated` message whenever a session changes state. `herdr-infinity` connects to that socket as a **passive client** and never sends a message, so it cannot create, resume, stop, or modify a session.

The wire format is a 4-byte big-endian length prefix followed by JSON (Infinity's `DaemonMessage`). Only `Welcome` and `SessionsUpdated` are read.

### Session attribution

A passive observer is not told which client owns which session, so the wrapper picks one:

1. `--session` / `-s` passed to Infinity, resolved as a unique prefix.
2. A session created after the wrapper started whose recorded `cwd` matches the pane.
3. The most recently updated live session whose recorded `cwd` matches the pane.
4. A single new `Running` session, if the daemon has not flushed `sessions.json` yet.

The chosen session is kept until it is archived or removed. Run one Infinity instance per pane; two panes in the same directory can only be distinguished when each is started with `-s` or creates a new session.

### Headless mode

`infinity -H "task"` hands the task to the daemon and exits. With the wrapper, the pane stays `working` until the task's session goes idle, then releases. The reported resume command attaches to the session (`-s`), it does not re-run the task.

### `--local`

`infinity --local` runs the agent in the TUI process with no daemon, so there is no state to observe. The wrapper still runs it, but does not report state.

## Session management (Herdr plugin)

State reporting answers "what is this pane doing?". The plugin answers the
complementary question: "what sessions exist, and how do I open one?".

```sh
herdr plugin link /path/to/herdr-infinity
```

That registers three workspace actions and a session-picker pane:

| Surface | Command |
| --- | --- |
| Action — Infinity: new session | `herdr-infinity new` |
| Action — Infinity: list sessions | `herdr-infinity sessions --all` |
| Action — Infinity: diagnose integration | `herdr-infinity doctor` |
| Pane — Infinity sessions (popup) | `herdr-infinity sessions --all` |

Session management is deliberately *not* implemented as state reporting.
A plugin action is a one-shot process, so it cannot hold the daemon
subscription that `idle`/`working`/`blocked` needs; that stays in
`herdr-infinity run`. The two halves are independent — the plugin is useful
without the shim, and the shim without the plugin.

Opening a session goes through Herdr (`pane split`, then `pane run`) rather
than spawning Infinity directly, so the new pane is an ordinary Herdr pane
that reports its own state. `open` refuses an archived session and refuses an
ambiguous prefix rather than guessing which conversation you meant.

### The launcher

The manifest invokes `plugin/run.sh`, not `herdr-infinity` directly. Herdr
runs plugin commands with the **server's** environment, which is frequently
just the system `PATH` — a server started from a desktop session sees neither
`~/.cargo/bin` nor `~/.local/bin`, so a bare `herdr-infinity` command fails
with `No such file or directory`. The launcher resolves the binary explicitly:
`$HERDR_INFINITY_BIN`, then the plugin root's own `target/release` and
`target/debug` (so a linked checkout works without installing), then
`$CARGO_HOME/bin`, `~/.local/bin`, `/usr/local/bin`, `/usr/bin`, and finally
`PATH`. Set `HERDR_INFINITY_BIN` if your installation is somewhere else.

## Memory: bridging Letta's MemFS and Claude Code

Letta Code stores agent memory as **git-tracked Markdown** under
`$MEMORY_DIR/system/`, one fact per file, with YAML frontmatter whose
`description` decides when the fact is relevant. Claude Code's memory
directory uses the same shape, plus a `name`, a `metadata.type`, and a
`MEMORY.md` index. Because both substrates are plain text, memory can move
between them:

```sh
herdr-infinity memfs status                      # what each side has
herdr-infinity memfs import                      # MemFS      -> Claude Code
herdr-infinity memfs export                      # Claude Code -> MemFS
herdr-infinity memfs import --dry-run            # show the plan only
```

MemFS uses two layouts, and the bridge detects which one a directory is
using rather than assuming: `<root>/system/*.md`, as the bundled authoring
instructions describe, and `<root>/*.md`, which is what Letta's local backend
actually creates under
`~/.letta/lc-local-backend/memfs/<agent>/memory/`. A sync writes back into
whichever layout is already in use, so it never reorganises an agent's memory;
an empty root falls back to `system/`.

`--memory-dir` defaults to `$MEMORY_DIR` (Letta sets it for an agent session);
`--claude-dir` defaults to the directory Claude Code derives from the current
project path. Syncing is always explicit — memory steers an agent's behaviour
for a long time, so nothing is written on a timer or behind your back, and
`--dry-run` shows the plan first.

Round trips settle: `import` then `export` leaves both sides reporting
"nothing to do", and `letta memory tokens --memory-dir <dir>` parses what the
bridge writes. Only `description` and the body are compared, since
`metadata.type` has no MemFS equivalent and would otherwise show a spurious
change on every file. `MEMORY.md` is rebuilt from the files that exist rather
than merged, so a stale entry cannot survive.

This is the part of the memory question that is actually tractable. A
memory-native model such as MemT-1.0 keeps memory as internal layer tensors,
strictly isolated per history with no text or embedding export, so its memory
can never be read by another agent. Letta's is files, so it can.

### Running it entirely locally

Letta Code and its App Server are Apache-2.0, so the whole stack can be
self-hosted with no account:

```sh
letta backend local        # agents and state under ~/.letta/lc-local-backend/
letta connect ollama       # auto-detects http://localhost:11434/v1
LLAMA_CPP_BASE_URL=http://127.0.0.1:18081/v1 letta connect llama-cpp
```

Neither local provider needs an API key. `letta connect ollama` auto-detects
the default port; `llama-cpp` defaults to `localhost:8080/v1`, so a server on
another port needs `LLAMA_CPP_BASE_URL`. Confirm what Letta can see with
`letta model list`.

**The model must support tool calls, and carry a large context.** Letta Code is
an agent harness — memory edits, file operations, and shell commands all go
through tools — so a model without tool support produces an empty turn rather
than an error. `llama3` and `phi3` report `does not support tools` through
Ollama's OpenAI endpoint and cannot drive it; `qwen2.5-coder`, `qwen3`, and
`llama3.1` and newer can. A completion-only `llama-server` model is likewise
unusable as an agent, however good it is at text.

Tool support alone is not enough. Letta's system prompt and tool definitions
need roughly 28k tokens, while Ollama serves 4,096 by default, so it refuses to
send the prompt rather than let the engine truncate memory and persona
silently. A derived model raises the window without restarting a shared daemon:

```sh
printf 'FROM qwen2.5-coder:7b\nPARAMETER num_ctx 32768\n' > qwen32k.modelfile
ollama create qwen2.5-coder-32k -f qwen32k.modelfile
```

On the CPU-only machine this was developed on, that got past the context check
but still did not produce a completed turn, and a 64k variant would not stay
resident. **Driving Letta from a local model is therefore not proven here** —
treat it as needing a GPU or a larger served context. To diagnose a silent
turn, read
`~/.letta/lc-local-backend/conversations/<id>/messages.jsonl`: a user message
with no assistant entry means the model never produced one.

None of this affects the memory bridge, which reads and writes files and does
not care which model — or whether any model — produced them.

## Commands

```text
herdr-infinity run [--] [infinity arguments...]
herdr-infinity sessions [--all] [--here]
herdr-infinity memfs <import|export|status> [--memory-dir <dir>]
                                           [--claude-dir <dir>] [--dry-run]
herdr-infinity open <session-prefix> [--focus]
herdr-infinity new [--focus]
herdr-infinity doctor
herdr-infinity install-shim [--dir <dir>] [--force]
herdr-infinity uninstall-shim [--dir <dir>]
herdr-infinity version
```

`sessions` lists live sessions (`--all` includes archived, `--here` restricts
to the current directory) by joining `~/.infinity/sessions.json` with the
daemon's live view. With no daemon running it degrades to the store alone
rather than failing. `open` and `new` create a pane and so require a Herdr
session; `sessions` and `doctor` work anywhere.

`doctor` prints the Herdr environment, the resolved Infinity binary, the daemon socket, and the sessions recorded in `~/.infinity/sessions.json`.

Set `HERDR_INFINITY_DEBUG=1` to append diagnostics to `$XDG_STATE_HOME/herdr-infinity/herdr-infinity.log` (default `~/.local/state/herdr-infinity/`). Nothing is ever written to the terminal while the TUI owns it.

## Requirements

- Linux or macOS (the integration uses Unix sockets and POSIX signals).
- Herdr 0.9.2 or newer for resume commands (state reporting works on older versions).
- Infinity Code with its daemon running (the normal `infinity` invocation starts it).

## Limitations

- This is an integration, not a Herdr built-in. `infinity` is not among Herdr 0.9.1's built-in agent kinds, so `herdr agent start --kind infinity` does not exist; Herdr will not auto-detect `infinity` unless the shim is on `PATH` or you launch through `herdr-infinity run`.
- The plugin's session picker is a listing, not an interactive menu. Herdr plugin panes run a command; they do not provide a selection widget. Read the id from the picker and run `herdr-infinity open <id>`.
- Session switching inside the TUI (`/load`) is not followed once a session is selected; start a new pane or pass `-s` if you need a different session tracked.
- Infinity's daemon protocol is not a stable public API. The reader is deliberately tolerant: unknown messages are ignored, unknown statuses leave the last reported state unchanged, and a malformed `sessions.json` is treated as empty. If the framing changes, the bridge disconnects and retries rather than crashing the agent.
- `--local` and multiple Infinity panes in one directory have the caveats described above.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The end-to-end test starts a fake Infinity daemon (a Unix socket speaking the real framing), a fake `herdr` binary that records its arguments, and a fake `infinity` binary. It asserts the wrapper reports `working` → `idle` and then releases the pane.

## License

Apache-2.0. See [LICENSE](LICENSE).
