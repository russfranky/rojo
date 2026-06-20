# AGENTS.md

Orientation for automated coding agents working in this repository. Human
contributors should start with [`CONTRIBUTING.md`](CONTRIBUTING.md).

## What this is

Rojo builds Roblox projects from the filesystem. It has two parts:

- A **Rust CLI** — binary `rojo`, library `librojo` — in `src/`.
- A **Luau Studio plugin** in `plugin/` that connects to `rojo serve` over
  HTTP/WebSocket.

Rust edition 2021, minimum supported Rust 1.88.

## Build, test, lint

```bash
cargo build                  # build the CLI
cargo build --features mcp   # also build the `rojo mcp` server (needs newer Rust)
cargo test                   # unit + integration tests
cargo fmt -- --check         # Rust formatting
cargo clippy                 # Rust lints
stylua --check plugin/src    # Luau formatting (plugin)
selene plugin/src            # Luau lints (plugin)
```

Run one integration test: `cargo test --test end_to_end -- <name>`.

## Layout

- `src/cli/` — one file per subcommand; `mod.rs` defines `GlobalOptions`, the
  `Subcommand` enum, and dispatch.
- `src/web/` — the serve HTTP/WebSocket API (`api.rs` routes, `interface.rs`
  types, `mod.rs` server lifecycle).
- `src/serve_session.rs`, `src/change_processor.rs` — live session and file
  watching.
- `src/snapshot/`, `src/snapshot_middleware/` — file ↔ Roblox instance
  conversion.
- `src/state_file.rs` — the `.rojo/serve-state.json` running-server record.
- `src/cli/mcp.rs` — the optional MCP server (Cargo feature `mcp`).
- `plugin/src/` — the Luau plugin (`ServeSession.lua`, `ApiContext.lua`, `App/`,
  `Settings.lua`).
- `crates/` — workspace crates (`memofs`, `rojo-insta-ext`).
- `tests/` — integration tests (`rojo_test/` helpers, `tests/` cases);
  `rojo-test/` holds fixtures.
- `assets/` — `init` project templates and `gen` scaffolds.
- `docs/server-and-tooling.md` — connection resilience, daemon commands,
  `--json`, and the MCP server.

## Conventions

- **Add a subcommand:** create `src/cli/<name>.rs` (a `clap` struct plus a
  `run()` method), then register it in `src/cli/mod.rs` — module declaration,
  `pub use`, a `Subcommand` variant, and a dispatch arm.
- **Machine-readable output:** prefer the global `--json` flag and the helpers in
  `src/cli/output.rs`; when `--json` is set, keep human/progress text on stderr
  so stdout stays parseable.
- **Tests:** Rust unit tests live inline under `#[cfg(test)]`; integration tests
  live in `tests/` (some use `insta` snapshots); the plugin's Luau tests use
  TestEZ in `*.spec.lua` files.
- **Version/protocol coupling:** the CLI and plugin versions are kept in sync
  (`Cargo.toml` vs `plugin/Version.txt`), as are the protocol versions
  (`PROTOCOL_VERSION` in `src/web/interface.rs` vs `protocolVersion` in
  `plugin/src/Config.lua`). Bump both together when changing the protocol.
- Match the surrounding code's style. CI runs the build/test/lint commands above
  on Linux, macOS, and Windows.
