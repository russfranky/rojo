//! Project lifecycle hooks: commands declared in a project file that Rojo runs
//! at build and serve milestones, for automation and CI (code generation, asset
//! processing, linting, notifications, and so on).
//!
//! Hooks are arbitrary commands that run with the developer's own privileges,
//! exactly like a `Makefile` target or an npm `postinstall` script. Only run
//! hooks for projects you trust; the global `--no-hooks` flag disables them for a
//! single invocation.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// A single hook command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookCommand {
    /// A command line run through the platform shell (`sh -c` on Unix, `cmd /C`
    /// on Windows), so shell features like pipes and `&&` work.
    Shell(String),
    /// An explicit program and its arguments, run directly without a shell. Use
    /// this to avoid shell quoting and portability concerns.
    Program(Vec<String>),
}

/// Lifecycle hooks declared in a project's `hooks` field.
///
/// Each field is a list of commands run in order at that milestone.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Hooks {
    /// Commands run before a `rojo build` writes its output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_build: Vec<HookCommand>,

    /// Commands run after a `rojo build` writes its output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_build: Vec<HookCommand>,

    /// Commands run once `rojo serve` has successfully bound its port. These
    /// should be quick; background a long-running companion process yourself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serve: Vec<HookCommand>,
}

/// Runs each command for the named `event`, in order, with `dir` as the working
/// directory. Bails on the first command that fails to start or exits non-zero.
///
/// Hook output is shown to the user. When `json` is set, it is captured and
/// re-emitted on stderr so the command's own stdout (the machine-readable JSON
/// result) stays clean; otherwise it is inherited and streamed live.
pub fn run_event(
    event: &str,
    commands: &[HookCommand],
    dir: &Path,
    json: bool,
) -> anyhow::Result<()> {
    for command in commands {
        let (mut process, description) = build_command(command, dir)?;

        log::info!("Running {event} hook: {description}");

        let status = if json {
            let output = process
                .output()
                .with_context(|| format!("Failed to run {event} hook: {description}"))?;

            // Keep our own stdout clean for machine consumers by routing the
            // hook's output to stderr.
            let mut stderr = std::io::stderr();
            let _ = stderr.write_all(&output.stdout);
            let _ = stderr.write_all(&output.stderr);

            output.status
        } else {
            process
                .status()
                .with_context(|| format!("Failed to run {event} hook: {description}"))?
        };

        if !status.success() {
            bail!("{event} hook failed: `{description}` exited with {status}");
        }
    }

    Ok(())
}

/// Builds the [`Command`] for a hook, returning it alongside a human-readable
/// description for logging and error messages.
fn build_command(command: &HookCommand, dir: &Path) -> anyhow::Result<(Command, String)> {
    let (mut process, description) = match command {
        HookCommand::Shell(line) => {
            if line.trim().is_empty() {
                bail!("Hook command is an empty string");
            }

            let mut process = if cfg!(windows) {
                let mut process = Command::new("cmd");
                process.arg("/C").arg(line);
                process
            } else {
                let mut process = Command::new("sh");
                process.arg("-c").arg(line);
                process
            };

            // Hooks read their input from the project, not the terminal.
            process.stdin(Stdio::null());
            (process, line.clone())
        }
        HookCommand::Program(parts) => {
            let (program, args) = parts
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("Hook command list is empty"))?;

            let mut process = Command::new(program);
            process.args(args).stdin(Stdio::null());
            (process, parts.join(" "))
        }
    };

    process.current_dir(dir);
    Ok((process, description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_deserializes_from_string() {
        let command: HookCommand = serde_json::from_str(r#""wally install""#).unwrap();
        assert_eq!(command, HookCommand::Shell("wally install".to_owned()));
    }

    #[test]
    fn program_command_deserializes_from_array() {
        let command: HookCommand = serde_json::from_str(r#"["wally", "install"]"#).unwrap();
        assert_eq!(
            command,
            HookCommand::Program(vec!["wally".to_owned(), "install".to_owned()])
        );
    }

    #[test]
    fn hooks_round_trip_through_json() {
        let hooks = Hooks {
            pre_build: vec![HookCommand::Shell("codegen".to_owned())],
            post_build: vec![HookCommand::Program(vec![
                "cp".to_owned(),
                "out.rbxl".to_owned(),
                "dist/".to_owned(),
            ])],
            serve: vec![],
        };

        let json = serde_json::to_string(&hooks).unwrap();
        let parsed: Hooks = serde_json::from_str(&json).unwrap();
        assert_eq!(hooks, parsed);

        // Empty lists are omitted, so `serve` should not appear.
        assert!(!json.contains("serve"));
    }

    #[test]
    fn unknown_hook_field_is_rejected() {
        let result: Result<Hooks, _> = serde_json::from_str(r#"{ "onChange": ["x"] }"#);
        assert!(result.is_err());
    }
}
