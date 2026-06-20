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

        // If a server is genuinely running, ask it to stop and wait for it to go
        // away before we start a replacement on the same address.
        if serve_control::probe(state.address, state.port).is_some() {
            serve_control::request_stop(state.address, state.port, state.session_id)?;
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

        // Detach: the server should outlive this short-lived `restart` process
        // and not hold onto our terminal.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = command
            .spawn()
            .context("Failed to start the new Rojo server")?;
        let pid = child.id();

        if !serve_control::wait_until_online(state.address, state.port, START_WAIT) {
            anyhow::bail!("Started a new Rojo server, but it did not come online in time.");
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
