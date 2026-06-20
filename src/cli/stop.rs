use std::{path::PathBuf, time::Duration};

use clap::Parser;
use serde::Serialize;

use super::{output, resolve_project_root, serve_control, GlobalOptions};

const STOP_WAIT: Duration = Duration::from_secs(10);

/// Stops a running Rojo server for a project.
#[derive(Debug, Parser)]
pub struct StopCommand {
    /// Path to the project whose server should be stopped. Defaults to the
    /// current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,
}

/// Machine-readable result of `rojo stop`, emitted with `--json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StopResult {
    stopped: bool,
}

impl StopCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let root_dir = resolve_project_root(&self.project)?;

        let Some((state, _health)) = serve_control::discover_running(&root_dir) else {
            return output::emit(&global, &StopResult { stopped: false }, || {
                println!("No Rojo server is running for this project.");
                Ok(())
            });
        };

        serve_control::request_stop(state.address, state.port, state.session_id)?;

        if !serve_control::wait_until_offline(state.address, state.port, STOP_WAIT) {
            anyhow::bail!("Asked the Rojo server to stop, but it is still responding.");
        }

        output::emit(&global, &StopResult { stopped: true }, || {
            println!("Stopped the Rojo server for '{}'.", state.project_name);
            Ok(())
        })
    }
}
