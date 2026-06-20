use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::Context;
use clap::Parser;
use serde::Serialize;

use crate::state_file;

use super::{output, resolve_project_root, serve_control, GlobalOptions, ServeCommand};

const STOP_WAIT: Duration = Duration::from_secs(10);
const START_WAIT: Duration = Duration::from_secs(10);

/// Restarts the Rojo server for a project.
///
/// The session id is preserved across the restart, so a connected Studio plugin
/// reconnects seamlessly rather than tearing down. By default the new server is
/// detached into the background; use `--foreground` to keep it attached.
#[derive(Debug, Parser)]
pub struct RestartCommand {
    /// Path to the project to restart. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Run the restarted server in the foreground (blocking) instead of
    /// detaching it into the background.
    #[clap(long)]
    pub foreground: bool,
}

/// Machine-readable result of `rojo restart`, emitted with `--json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RestartResult {
    restarted: bool,
    address: String,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
}

impl RestartCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let root_dir = resolve_project_root(&self.project)?;

        let Some(state) = state_file::load(&root_dir) else {
            anyhow::bail!(
                "No Rojo server state was found for this project. Use `rojo serve` to start one."
            );
        };

        // If a server matching our recorded session is genuinely running, ask it
        // to stop and wait for it to go away before starting a replacement.
        // `discover_running` validates the session id, so a stale state file
        // pointing at a *different* server now on that port won't make us stop a
        // stranger — we just treat it as not-running and start fresh.
        if serve_control::discover_running(&root_dir).is_some() {
            serve_control::request_stop(state.address, state.port, state.session_id, state.pid)?;
            if !serve_control::wait_until_offline(state.address, state.port, STOP_WAIT) {
                anyhow::bail!(
                    "Asked the existing Rojo server to stop, but it is still responding."
                );
            }
        }

        if self.foreground {
            // Run the replacement in-process, reusing the same session id.
            return ServeCommand {
                project: state.project_file,
                address: Some(state.address),
                port: Some(state.port),
                allowed_hosts: state.allowed_hosts,
                session_id: Some(state.session_id),
            }
            .run(global);
        }

        // Spawn a fresh, detached `rojo serve` reusing the session id so any
        // connected plugin reconnects without seeing the id change.
        let exe =
            std::env::current_exe().context("Could not determine the Rojo executable path")?;

        let mut command = Command::new(exe);
        command
            .arg("serve")
            .arg(&state.project_file)
            .arg("--address")
            .arg(state.address.to_string())
            .arg("--port")
            .arg(state.port.to_string())
            .arg("--session-id")
            .arg(state.session_id.to_string());

        if !state.allowed_hosts.is_empty() {
            command
                .arg("--allowed-hosts")
                .arg(state.allowed_hosts.join(","));
        }

        // Detach so the server outlives this short-lived process and doesn't
        // hold the terminal, and capture its stderr to a log so a failed
        // background start isn't invisible.
        let rojo_dir = root_dir.join(".rojo");
        let _ = fs_err::create_dir_all(&rojo_dir);
        let log_path = rojo_dir.join("serve.log");

        command.stdin(Stdio::null()).stdout(Stdio::null());
        match std::fs::File::create(&log_path) {
            Ok(log) => {
                command.stderr(Stdio::from(log));
            }
            Err(_) => {
                command.stderr(Stdio::null());
            }
        }
        detach(&mut command);

        let child = command
            .spawn()
            .context("Failed to start the new Rojo server")?;
        let pid = child.id();

        if !serve_control::wait_until_online(state.address, state.port, START_WAIT) {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            let log = log.trim();
            if log.is_empty() {
                anyhow::bail!(
                    "Started a new Rojo server (pid {pid}), but it did not come online in time."
                );
            }
            anyhow::bail!("The new Rojo server failed to start:\n{log}");
        }

        output::emit(
            &global,
            &RestartResult {
                restarted: true,
                address: state.address.to_string(),
                port: state.port,
                pid: Some(pid),
            },
            || {
                println!(
                    "Restarted the Rojo server for '{}' at {}:{} (pid {}).",
                    state.project_name, state.address, state.port, pid
                );
                Ok(())
            },
        )
    }
}

/// Puts the spawned server in its own process group / detached state so a
/// Ctrl-C or terminal-close aimed at this short-lived `restart` process in the
/// brief window before it exits can't also kill the new server.
fn detach(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    // Other platforms: no-op; a normally-exiting parent doesn't kill children.
    let _ = command;
}
