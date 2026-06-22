# Driving Rojo from an AI agent

This guide describes how to run an autonomous "edit → run → observe" loop on a
Roblox project with Rojo. The short version: **Rojo and the official [Roblox
Studio MCP server](https://create.roblox.com/docs/studio/mcp) are complementary,
and you use both.** Rojo owns files, the serve process, and observability; the
Studio MCP owns running code inside the live Studio.

## Division of labor

| Job | Use | Notes |
| --- | --- | --- |
| Sync files → Studio | `rojo serve` (live sync) | Source edits appear in Studio with no reboot. |
| Keep Studio open / reboot it unattended | `rojo studio reset` | Force-restarts Studio with **no native dialogs** (no "Don't Save", no "quit unexpectedly", no restore prompt). macOS. |
| Run / test code in the live Studio | **Studio MCP** `run_code` / `execute_luau` | Official, maintained. Runs Luau in the open Studio and returns the result. This is how you run tests (e.g. `IntegrationTests.runAll()`). |
| Observe Output (prints / warnings / errors) | `rojo logs` / MCP `read_logs` | The runtime-feedback loop — see what happened when the game ran. |
| Check connectivity | `rojo status` / MCP `connection` | Confirm the plugin is connected before expecting sync or logs. |

> **Do not use `run-in-roblox` to run tests.** Its only release is v0.3.0 (July
> 2020); driving a current Studio with it is a compatibility gamble. The
> maintained path is the Studio MCP's `run_code`/`execute_luau` against an open
> Studio, which you keep open with `rojo studio reset`.

## Studio must be open

Everything that *runs* code — the Studio MCP, playtests, tests — needs Roblox
Studio to be **open**. Nothing runs fully unattended if Studio is closed. The
fix is `rojo studio reset`: it launches (or relaunches) Studio without any of the
native dialogs that would otherwise wait for a human click, and the Rojo plugin
reconnects to the still-running `rojo serve` automatically.

## The loop

1. **Ensure Studio is open.** Check the Studio MCP's `get_studio_state` (and/or
   Rojo's `connection` tool — `connectedClients > 0`). If Studio is closed, run:

   ```bash
   rojo studio reset --place game.rbxl
   ```

   This relaunches Studio dialog-free and waits until the Rojo plugin has
   reconnected before returning.

2. **Edit files.** With `rojo serve` running, source changes **live-sync** into
   Studio — no reboot needed. Only playtests and *plugin* changes require a
   restart.

3. **Run / test.** Use the Studio MCP to execute Luau in the open Studio — e.g.
   `execute_luau` / `run_code` running your test entrypoint
   (`IntegrationTests.runAll()`, a TestEZ bootstrap, etc.). The call returns the
   result/errors directly.

4. **Observe.** Read accumulated Output with `rojo logs` (or the `read_logs` MCP
   tool) for the full trail of prints, warnings, and errors — including from
   playtests. Poll incrementally with `--since <tailSeq>`.

5. **Reboot only when needed.** After a plugin change, a crash, or to clear a
   wedged playtest, run `rojo studio reset` again. Source edits never need it.

## Rules of the road

- **Never** call `run-in-roblox`. (See above.)
- **Never** Cmd-Q or otherwise gracefully quit Studio — that triggers the "Don't
  Save" dialog and stalls waiting for a human. Always reboot with
  `rojo studio reset`, which force-kills cleanly.
- **Don't reboot for source edits.** They live-sync. Fewer reboots = fewer
  dialogs and a faster loop.
- Treat `rojo logs` / `read_logs` as your eyes: after running anything, read the
  logs to see what actually happened.

## A note you can paste into your agent's instructions

> To run or test code, make sure Roblox Studio is open: check `get_studio_state`,
> and if no instance is open run `rojo studio reset --place <place>`. Then run
> Luau via the Studio MCP's `execute_luau`/`run_code` (e.g.
> `IntegrationTests.runAll()`). Use `rojo serve` for live sync and `rojo logs` to
> read Studio's Output. Never use `run-in-roblox`, and never quit Studio any other
> way than `rojo studio reset`.

## Setup

- **Install Rojo where your agent's `rojo` resolves.** Build this branch and put
  it on the agent's `PATH` (e.g. `cargo install --path .` → `~/.cargo/bin/rojo`).
  Verify with `rojo studio reset --help` and `rojo logs --help`.
- **Install the matching Studio plugin** from this branch. The plugin and server
  negotiate a **protocol version** and refuse to talk if they differ, so both
  sides must be this build. Output capture is on by default.
- **Connect the official Roblox Studio MCP** to your agent — see the
  [Roblox docs](https://create.roblox.com/docs/studio/mcp). This is what provides
  `run_code`/`execute_luau`.
- **Optionally register Rojo's own MCP** (`rojo mcp`, built with `--features
  mcp`) for `sourcemap`, `read_instance`, `connection`, and `read_logs`. See
  [the MCP server section](./server-and-tooling.md#the-mcp-server-rojo-mcp).

## macOS specifics

`rojo studio reset` is currently macOS-only. It force-kills Studio (so there's no
save prompt), deletes the auto-recovery files (so there's no restore prompt), and
briefly sets the macOS crash-reporter dialog to `none` around the kill (so
there's no "Studio quit unexpectedly" dialog), then relaunches. See
[`rojo studio reset`](./server-and-tooling.md#rebooting-studio-unattended-rojo-studio-reset)
for all flags (`--no-launch`, `--no-clear-recovery`, `--restart-serve`,
`--no-wait-reconnect`, `--reconnect-timeout`, `--no-silence-crashes`).
