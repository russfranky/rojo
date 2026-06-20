# Server Resilience, Daemon Control & the MCP Server

This guide documents Rojo's server-resilience features, the daemon-control and
developer-workflow CLI commands, machine-readable output, and the optional
Model Context Protocol (MCP) server.

- [Connection resilience](#connection-resilience)
- [Server lifecycle commands](#server-lifecycle-commands)
- [`rojo test`](#rojo-test)
- [`rojo gen`](#rojo-gen)
- [Machine-readable output (`--json`)](#machine-readable-output---json)
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

## The MCP server (`rojo mcp`)

`rojo mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio, exposing Rojo's tooling to MCP clients (AI assistants and
editors). It is a thin wrapper that drives Rojo's own CLI, so its tools behave
exactly like the commands above.

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
| `status` | no | Whether a server is running, and its details. |
| `build` | yes | Build the project to a `.rbxl`/`.rbxlx`/`.rbxm`/`.rbxmx` file. |
| `gen_script` | yes | Scaffold a server/client/module script. |
| `stop` | yes | Stop the running server. |
| `restart` | yes | Restart the running server. |

### Read-only mode

`--read-only` exposes only the non-mutating tools (`sourcemap`, `status`); the
mutating tools are hidden and rejected. This is the safe default for untrusted
sessions.

### Safety

Tool file paths (`build` output, `gen_script` path) are confined to the project
directory — absolute paths and `..` are rejected. Tool failures are reported as
MCP errors rather than silently succeeding.

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
| `POST` | `/api/stop` | Gracefully shut down the server. Local-only and guarded by the current session id. |
