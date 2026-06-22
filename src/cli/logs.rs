use std::path::PathBuf;

use clap::Parser;

use super::{output, resolve_project_root, serve_control, GlobalOptions};

/// Prints recent Output (prints, warnings, and errors) captured from the
/// connected Roblox Studio session.
///
/// This is the read side of Rojo's runtime-feedback loop: the Studio plugin
/// streams its Output to the running `rojo serve`, which buffers it; this command
/// (and the `read_logs` MCP tool) read it back. Requires a running server for the
/// project, and the plugin connected with output capture enabled.
#[derive(Debug, Parser)]
pub struct LogsCommand {
    /// Path to the project. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Only return entries newer than this sequence number (the `tailSeq` from a
    /// previous call), so a poller doesn't see the same lines twice.
    #[clap(long)]
    pub since: Option<u64>,

    /// Minimum severity to include: `print`, `info`, `warning`, or `error`.
    #[clap(long)]
    pub level: Option<String>,

    /// Maximum number of entries to return (the newest are kept).
    #[clap(long)]
    pub limit: Option<usize>,
}

impl LogsCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let root_dir = resolve_project_root(&self.project)?;

        let Some((state, _health)) = serve_control::discover_running(&root_dir) else {
            anyhow::bail!(
                "No Rojo server is running for this project. Start one with `rojo serve`."
            );
        };

        let logs = serve_control::fetch_logs(
            state.address,
            state.port,
            self.since,
            self.level.as_deref(),
            self.limit,
        )
        .ok_or_else(|| anyhow::anyhow!("Failed to fetch logs from the running Rojo server."))?;

        output::emit(&global, &logs, || {
            if logs.entries.is_empty() {
                println!(
                    "No logs captured yet. Is the Studio plugin connected with output capture on?"
                );
            } else {
                for entry in &logs.entries {
                    println!("[{:<7} {}] {}", entry.level, entry.run_mode, entry.message);
                }
                if logs.dropped > 0 {
                    println!(
                        "({} earlier entries were dropped from the buffer)",
                        logs.dropped
                    );
                }
            }
            Ok(())
        })
    }
}
