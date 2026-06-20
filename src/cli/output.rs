//! Helpers for emitting machine-readable (JSON) output from CLI commands.
//!
//! Commands that support `--json` should print all human-readable, progress, or
//! diagnostic text to **stderr** (or gate it behind `!global.json`) so that
//! stdout carries nothing but the single JSON document produced by [`emit`].
//! This keeps Rojo's stdout reliably parseable by scripts and AI agents.

use std::io::{self, Write};

use serde::Serialize;

use super::GlobalOptions;

/// Emit the result of a command.
///
/// When the user passed `--json`, `value` is serialized as pretty JSON to
/// stdout. Otherwise, the `human` closure is run to produce normal
/// human-readable output.
pub fn emit<T, F>(global: &GlobalOptions, value: &T, human: F) -> anyhow::Result<()>
where
    T: Serialize,
    F: FnOnce() -> anyhow::Result<()>,
{
    if global.json {
        print_json(value)?;
        Ok(())
    } else {
        human()
    }
}

/// Write `value` to stdout as pretty JSON followed by a newline, regardless of
/// the `--json` flag. Useful for commands whose output is always structured.
pub fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)?;
    writeln!(handle)?;
    handle.flush()?;
    Ok(())
}
