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

## Commands

```text
herdr-infinity run [--] [infinity arguments...]
herdr-infinity doctor
herdr-infinity install-shim [--dir <dir>] [--force]
herdr-infinity uninstall-shim [--dir <dir>]
herdr-infinity version
```

`doctor` prints the Herdr environment, the resolved Infinity binary, the daemon socket, and the sessions recorded in `~/.infinity/sessions.json`.

Set `HERDR_INFINITY_DEBUG=1` to append diagnostics to `$XDG_STATE_HOME/herdr-infinity/herdr-infinity.log` (default `~/.local/state/herdr-infinity/`). Nothing is ever written to the terminal while the TUI owns it.

## Requirements

- Linux or macOS (the integration uses Unix sockets and POSIX signals).
- Herdr 0.9.2 or newer for resume commands (state reporting works on older versions).
- Infinity Code with its daemon running (the normal `infinity` invocation starts it).

## Limitations

- This is an integration, not a Herdr built-in. Herdr will not auto-detect `infinity` unless the shim is on `PATH` or you launch through `herdr-infinity run`.
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
