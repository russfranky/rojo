use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use super::{output, resolve_project_root, serve_control, GlobalOptions};

/// Reports whether a Rojo server is running for a project, and its details.
#[derive(Debug, Parser)]
pub struct StatusCommand {
    /// Path to the project to check. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,
}

/// Machine-readable result of `rojo status`, emitted with `--json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResult {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connected_clients: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

impl StatusCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let root_dir = resolve_project_root(&self.project)?;

        match serve_control::discover_running(&root_dir) {
            Some((state, health)) => {
                let result = StatusResult {
                    running: true,
                    project_name: Some(health.project_name.clone()),
                    address: Some(state.address.to_string()),
                    port: Some(state.port),
                    pid: Some(state.pid),
                    uptime_seconds: Some(health.uptime_seconds),
                    connected_clients: Some(health.connected_clients),
                    session_id: Some(health.session_id.to_string()),
                };

                output::emit(&global, &result, || {
                    println!("Rojo server is running:");
                    println!("  Project:           {}", health.project_name);
                    println!("  Address:           {}:{}", state.address, state.port);
                    println!("  PID:               {}", state.pid);
                    println!("  Uptime:            {}s", health.uptime_seconds);
                    println!("  Connected clients: {}", health.connected_clients);
                    println!("  Session ID:        {}", health.session_id);
                    Ok(())
                })
            }
            None => {
                let result = StatusResult {
                    running: false,
                    project_name: None,
                    address: None,
                    port: None,
                    pid: None,
                    uptime_seconds: None,
                    connected_clients: None,
                    session_id: None,
                };

                output::emit(&global, &result, || {
                    println!("No Rojo server is running for this project.");
                    Ok(())
                })
            }
        }
    }
}
