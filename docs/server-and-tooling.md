# Server Resilience, Daemon Control & the MCP Server

This guide documents Rojo's server-resilience features, the daemon-control and
developer-workflow CLI commands, machine-readable output, and the optional
Model Context Protocol (MCP) server.

- [Connection resilience](#connection-resilience)
- [Server lifecycle commands](#server-lifecycle-commands)
- [`rojo test`](#rojo-test)
- [`rojo gen`](#rojo-gen)
- [Machine-readable output (`--json`)](#machine-readable-output---json)
- [Project hooks](#project-hooks)
- [The MCP server (`rojo mcp`)](#the-mcp-server-rojo-mcp)
- [The serve-state file](#the-serve-state-file)
- [HTTP API additions](#http-api-additions)

---

## Connection resilience

A live Rojo session is a long-running `rojo serve` process that the Studio
plugin connects to. Two changes make that connection survive interruptions that
previously required a manual reconnect.

### Stable session id across restarts

Every serve session has a `SessionId`. Historically it was random per process,
so restarting `rojo serve` produced a new id and the connected plugin tore down
with a *"Server changed ID"* error.

Now, when a server binds, it records its session in a project-local
`.rojo/serve-state.json` file. When you start `rojo serve` again for the same
project and no server is currently running, it **reuses the recorded session
id**, so a connected plugin reconnects seamlessly instead of erroring.

If a server *is* already running for the project, `rojo serve` refuses to start a
second one and points you at `rojo restart` / `rojo stop`.

### Automatic plugin reconnect

When the plugin's connection drops (server restart, a transient network blip),
the Studio plugin now reconnects automatically with exponential backoff and
jitter, re-running its initial sync each attempt. It will not silently sync a
*different* project that happens to appear on the same address, and it stops
retrying on fundamentally incompatible errors (protocol mismatch, wrong place
id, etc.).

Plugin settings (Rojo plugin → Settings):

| Setting | Default | Meaning |
| --- | --- | --- |
| `instantReconnect` | `true` | Reconnect automatically on an unexpected drop. |
| `instantReconnectMaxAttempts` | `8` | Attempts before falling back to slow discovery polling. `0` = unlimited. |
| `instantReconnectBaseDelay` | `1` | Base backoff delay in seconds (doubles each attempt, capped at 30s). |

---

## Server lifecycle commands

These commands discover a running server through the project's
`.rojo/serve-state.json` file and a health probe, so run them from the project
directory (or pass the project path).

### `rojo serve`

```
rojo serve [PROJECT] [--address <IP>] [--port <PORT>] [--allowed-hosts <HOST,...>]
```

Unchanged in everyday use; it now also writes the serve-state file on a
successful bind and reuses a prior session id as described above.

### `rojo status`

Reports whether a server is running for the project and its details.

```
rojo status [PROJECT] [--json]
```

Human output:

```
Rojo server is running:
  Project:           my-game
  Address:           127.0.0.1:34872
  PID:               40871
  Uptime:            312s
  Connected clients: 1
  Session ID:        66d0a760-2c72-4779-9612-7e80915d409b
```

With `--json` (not running):

```json
{ "running": false }
```

### `rojo stop`

Gracefully stops the running server.

```
rojo stop [PROJECT] [--json]
```

### `rojo restart`

Stops the running server and starts a fresh one, **preserving the session id**
so connected plugins reconnect seamlessly.

```
rojo restart [PROJECT] [--json] [--foreground]
```

By default the replacement server is detached into the background (its log is
written to `.rojo/serve.log`). Pass `--foreground` to run it attached in the
current terminal instead.

---

## Rebooting Studio unattended (`rojo studio reset`)

**macOS only.** Rebooting Roblox Studio normally needs a human to click through
up to three native dialogs: **"Don't Save"** when closing with unsaved changes,
the macOS **"RobloxStudio quit unexpectedly"** crash dialog after a force-kill,
and the auto-recovery **"restore"** prompt on the next launch. That stalls an
agent driving a serve/test loop. `rojo studio reset` force-restarts Studio
without any of them:

1. **Silences the macOS crash reporter** for the reboot
   (`defaults write com.apple.CrashReporter DialogType none`). A force-killed app
   exits abnormally, which otherwise raises the system "quit unexpectedly"
   dialog; the previous value is restored once the reboot is done.
2. **Force-kills** Studio (`SIGKILL`) — a killed process can't raise the "Don't
   Save" prompt.
3. **Deletes the auto-recovery files** — with nothing to restore, the restore
   prompt has nothing to offer. (There is no Studio setting that disables it, so
   removing the files is the only reliable suppression.)
4. **Relaunches** Studio. A connected Rojo plugin reconnects to the still-running
   `rojo serve` on its own, because the [session id](#stable-session-id-across-restarts)
   is preserved across the reboot.

This is OS-level automation (Rojo's Luau plugin can't dismiss native dialogs, and
the recovery prompt appears before any plugin even loads), which is why it is a
separate, platform-specific command. On other platforms it exits with a clear
"only supported on macOS" message.

```
rojo studio reset [PROJECT] [--place <FILE>] [--no-launch] [--no-clear-recovery]
                  [--recovery-path <DIR>] [--no-silence-crashes] [--restart-serve]
                  [--json]
```

| Flag | Effect |
| --- | --- |
| `--place <FILE>` | Open this place file after relaunching (otherwise Studio opens to its start screen). |
| `--no-launch` | Kill and clear recovery, but don't relaunch. |
| `--no-clear-recovery` | Leave the auto-recovery files in place (the restore prompt may then reappear). |
| `--recovery-path <DIR>` | Override the auto-recovery directory (see below). |
| `--no-silence-crashes` | Don't suppress the macOS "quit unexpectedly" crash dialog (it is silenced and restored by default). |
| `--restart-serve` | Also restart the project's `rojo serve` (best-effort), for a full reset of the connection. `PROJECT` selects which server. |

```json
{ "killed": true, "crashDialogSuppressed": true, "recoveryClearedCount": 2, "serveRestarted": false, "relaunched": true }
```

> **Clearing recovery is destructive by design.** It deletes Studio's
> auto-recovery backups so the prompt can't appear — intended for a test loop
> where Rojo's source files are the source of truth, not Studio. Use
> `--no-clear-recovery` to keep them.

**Recovery path & process name.** The default recovery directory is Studio's
macOS AutoSaves folder,
`~/Library/Application Support/Roblox/RobloxStudio/AutoSaves`, and Studio is
matched by the process name `RobloxStudio`. Both can change across Studio
versions; if the restore prompt still appears, find the real directory (look for
recently-modified files there after a crash) and pass it with `--recovery-path`.

**Silencing the crash dialog.** By default the reset toggles a **global** macOS
preference (`com.apple.CrashReporter DialogType`) to `none` for the duration of
the reboot and restores it afterward, so the "quit unexpectedly" dialog can't
block the relaunch. Pass `--no-silence-crashes` to leave the setting untouched.
To suppress it permanently yourself instead, run once
`defaults write com.apple.CrashReporter DialogType none` (undo with
`defaults write com.apple.CrashReporter DialogType crashreport`).

**Make sure your agent actually uses this.** The dialogs only stay gone if every
reboot goes through `rojo studio reset` (or the `reset_studio` MCP tool). If the
agent quits Studio any other way — Cmd-Q, the close button, `osascript … quit` —
Studio gets a *graceful* quit and the "Don't Save" prompt comes back. Two things
to check:

- The `rojo` the agent runs is a build that has this command
  (`rojo studio reset --help` should list it). Install it where the agent's
  `rojo` resolves (e.g. `cargo install --path .`), and rebuild the `rojo mcp`
  server too if the agent calls `reset_studio`.
- Most "reboot to see my changes" cycles are unnecessary: `rojo serve`
  **live-syncs** file changes into a running Studio already. Reserve reboots for
  playtests or plugin changes — fewer reboots, fewer dialogs.

---

## `rojo test`

Runs a project's Luau tests with a pluggable runner. Rojo does not ship a Luau
runtime, so this orchestrates an external one.

```
rojo test [PROJECT] [--runner <run-in-roblox|lune|custom>] [--script <PATH>]
          [--place <PATH>] [--json] [-- <ARGS>...]
```

| Runner | Needs | Notes |
| --- | --- | --- |
| `run-in-roblox` (default) | `run-in-roblox` binary + Studio | Highest fidelity: builds a place and runs your bootstrap script inside real Studio. |
| `lune` | `lune` binary | Fast and CI-friendly, but only a subset of Roblox APIs is available. |
| `custom` | your command (after `--`) | Runs an arbitrary command; the built place path is exposed via `ROJO_TEST_PLACE`. |

- `--script` is the test entry/bootstrap script (required for `run-in-roblox`
  and `lune`); it is resolved to an absolute path.
- For runners that need a place, one is built to `.rojo/test-place.rbxl` (or
  `--place`).
- The process exits with the runner's status code. With `--json`, the captured
  output and result are printed:

```json
{ "runner": "lune", "success": true, "exitCode": 0, "stdout": "..." }
```

Examples:

```bash
rojo test --runner lune --script tests/init.luau
rojo test --runner custom -- ./run-my-tests.sh   # $ROJO_TEST_PLACE is set
```

---

## `rojo gen`

Scaffolds new source files from templates, never overwriting an existing file.

```
rojo gen script <NAME> [--kind <server|client|module>] [--path <DIR>] [--json]
```

| Kind | Produces |
| --- | --- |
| `server` | `<NAME>.server.luau` |
| `client` | `<NAME>.client.luau` |
| `module` (default) | `<NAME>.luau` |

`<NAME>` must be a single safe filename component (no path separators, `..`, or
`:`); for modules, a name that isn't a valid Luau identifier still produces
compiling code. `--path` defaults to the project's `src/` directory if present,
otherwise the current directory.

```bash
rojo gen script Greeter --kind module
rojo gen script Main --kind server --path src/server
```

With `--json`:

```json
{ "created": ["src/Greeter.luau"], "skipped": [] }
```

---

## Machine-readable output (`--json`)

`--json` is a global flag. When set, supporting commands print a single JSON
document to stdout and route human/progress text to stderr, so stdout stays
parseable. Supported by `build`, `sourcemap`, `status`, `stop`, `restart`,
`test`, and `gen`.

```bash
rojo build --json -o game.rbxl
rojo status --json
```

---

## Project hooks

Hooks are commands Rojo runs at build and serve milestones, declared in the
project file. They're for automation and CI: code generation, asset processing,
linting, or notifications.

```jsonc
{
  "name": "my-game",
  "tree": { "$path": "src" },
  "hooks": {
    // Run before `rojo build` writes its output.
    "preBuild": ["wally install"],
    // Run after a successful `rojo build`.
    "postBuild": [
      "echo done",                       // a shell command line, or
      ["cp", "game.rbxl", "dist/game.rbxl"]  // an explicit program + args
    ],
    // Run once `rojo serve` has bound its port.
    "serve": ["echo serving"]
  }
}
```

Each event takes a list of commands run in order. A command is either a **string**
(run through the platform shell — `sh -c` on Unix, `cmd /C` on Windows, so pipes
and `&&` work) or an **array** of strings (a program and its arguments, run
directly without a shell to avoid quoting concerns). Commands run with the project
directory as their working directory.

- A failing `preBuild`/`postBuild` command fails the build (non-zero exit).
- A failing `serve` command is logged but does **not** stop an already-running
  server.
- In `rojo build --watch`, hooks run for the initial build only, not on each
  rebuild — so a hook that writes into the project can't cause a rebuild loop.
- With `--json`, hook output is routed to stderr so stdout stays parseable.

### Trust model

Hooks are arbitrary commands that run with your privileges, exactly like a
`Makefile` target or an npm `postinstall` script. **Only run hooks for projects
you trust.** The global `--no-hooks` flag disables all hooks for a single
invocation, which is the safe choice when building or serving a project from an
untrusted source:

```bash
rojo build --no-hooks -o game.rbxl
```

---

## The MCP server (`rojo mcp`)

`rojo mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio, exposing Rojo's tooling to MCP clients (AI assistants and
editors).

The read tools an assistant calls repeatedly while exploring a project
(`sourcemap`, `read_instance`) are answered from a single long-lived,
file-watching session held in memory, so they don't rebuild the instance tree on
every call and they reflect edits as soon as the file watcher picks them up. The
remaining tools (`build`, `gen_script`, the `status`/`stop`/`restart` server
controls, and `reset_studio`) drive Rojo's own CLI, so they behave exactly like
the commands above and `build` always reads fresh from disk.

It is **opt-in at build time** because its dependencies require a newer Rust
toolchain than Rojo's minimum supported version:

```bash
cargo build --release --features mcp
```

Run it against a project:

```bash
rojo mcp [PROJECT] [--read-only]
```

### Tools

| Tool | Mutating | Description |
| --- | --- | --- |
| `sourcemap` | no | The project's instance tree (best for navigation). |
| `read_instance` | no | One instance's class, property values, and immediate children, located by a slash-separated path from the root (e.g. `ReplicatedStorage/Shared/MyModule`). |
| `status` | no | Whether a server is running, and its details. |
| `build` | yes | Build the project to a `.rbxl`/`.rbxlx`/`.rbxm`/`.rbxmx` file. |
| `gen_script` | yes | Scaffold a server/client/module script. |
| `stop` | yes | Stop the running server. |
| `restart` | yes | Restart the running server. |
| `reset_studio` | yes | Force-restart Roblox Studio with no native dialogs (macOS only); optionally open a place. See [`rojo studio reset`](#rebooting-studio-unattended-rojo-studio-reset). |

### Read-only mode

`--read-only` exposes only the non-mutating tools (`sourcemap`, `read_instance`,
`status`); the mutating tools are hidden and rejected. This is the safe default
for untrusted sessions.

### Safety

Tool file paths (`build` output, `gen_script` path, `reset_studio` place) are
confined to the project directory — absolute paths and `..` are rejected. Tool
failures are reported as MCP errors rather than silently succeeding.

### Registering with a client

Point your MCP client at the `rojo mcp` command. For example, with the Claude
Code CLI:

```bash
claude mcp add rojo -- rojo mcp /absolute/path/to/your/project
```

For read-only access, append `--read-only`.

---

## The serve-state file

`rojo serve` writes `<project>/.rojo/serve-state.json` on a successful bind and
removes it on graceful shutdown. It records the session id, address, port, pid,
project name, and version, and is how `status`/`stop`/`restart` (and a restarted
`serve`) find the running server.

It is local state and should not be committed; `rojo init` adds `.rojo/` to the
generated `.gitignore`.

---

## HTTP API additions

These endpoints are served alongside the existing serve API. They respond with
MessagePack by default, or JSON when the request sends `Accept: application/json`.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/health`, `/api/status` | Session id, server/protocol version, project name, uptime, connected-client count. |
| `POST` | `/api/stop` | Gracefully shut down the server. Local-only; guarded by the current session id, and by the process id too when the caller provides one (so a stop aimed at a restarted server's predecessor can't hit its successor). |
