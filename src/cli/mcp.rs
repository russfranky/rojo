//! The `rojo mcp` subcommand: a Model Context Protocol server, exposing Rojo's
//! capabilities as tools for AI assistants over stdio.
//!
//! It is a thin wrapper that drives Rojo's own CLI (`rojo sourcemap`, `build`,
//! `status`, `gen`, `stop`, `restart`) as subprocesses and returns their JSON
//! output. Reusing the CLI keeps the tools consistent with what a developer runs
//! by hand, and inherits all of their behavior and tests.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context;
use clap::Parser;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};

use super::resolve_project_root;

/// Runs an MCP server over stdio that exposes Rojo's tooling to AI assistants.
#[derive(Debug, Parser)]
pub struct McpCommand {
    /// Path to the project the server operates on. Defaults to the current
    /// directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Only expose read-only tools (sourcemap, status). Mutating tools (build,
    /// gen, serve control) are refused. Safer for untrusted AI sessions.
    #[clap(long)]
    pub read_only: bool,
}

impl McpCommand {
    pub fn run(self) -> anyhow::Result<()> {
        // Resolve the project to a working directory we run Rojo subprocesses in,
        // so each subcommand's project/path defaults resolve correctly.
        let work_dir = resolve_project_root(&self.project)?;

        let server = RojoMcpServer::new(work_dir, self.read_only);

        let runtime = tokio::runtime::Runtime::new()
            .context("Failed to start the async runtime for the MCP server")?;

        runtime.block_on(async move {
            let service = server
                .serve(stdio())
                .await
                .context("Failed to start the MCP server over stdio")?;
            service
                .waiting()
                .await
                .context("MCP server stopped with an error")?;
            anyhow::Ok(())
        })?;

        Ok(())
    }
}

#[derive(Clone)]
struct RojoMcpServer {
    work_dir: PathBuf,
    read_only: bool,
    tool_router: ToolRouter<Self>,
}

/// Mutating tools, hidden and rejected in `--read-only` mode.
const MUTATING_TOOLS: [&str; 4] = ["build", "gen_script", "stop", "restart"];

impl RojoMcpServer {
    fn new(work_dir: PathBuf, read_only: bool) -> Self {
        let mut tool_router = Self::tool_router();

        // In read-only mode, hide the mutating tools entirely (rmcp also rejects
        // calls to disabled routes), so the model never sees or attempts them.
        if read_only {
            for name in MUTATING_TOOLS {
                tool_router.disable_route(name);
            }
        }

        Self {
            work_dir,
            read_only,
            tool_router,
        }
    }

    /// Runs `rojo <args>` in the project directory, returning stdout on success
    /// or an error message (which the tool surfaces as an `is_error` result).
    fn run_rojo(&self, args: &[&str]) -> Result<String, String> {
        run_rojo_command(&self.work_dir, args)
    }

    /// Backstop for read-only mode (the route is also disabled in `new`).
    fn ensure_writable(&self) -> Result<(), String> {
        if self.read_only {
            Err(
                "This tool is disabled because the MCP server is running in --read-only mode."
                    .to_string(),
            )
        } else {
            Ok(())
        }
    }

    /// Rejects a user-supplied path that could escape the project. Absolute
    /// paths and any `..` component are refused. CLI users aren't restricted;
    /// this guards the AI-facing MCP surface against arbitrary file writes.
    fn confine(&self, path: &str) -> Result<(), String> {
        let candidate = Path::new(path);

        if candidate.is_absolute() {
            return Err(format!(
                "Path '{path}' must be relative to the project, not absolute."
            ));
        }

        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("Path '{path}' must not contain '..'."));
        }

        Ok(())
    }

    /// Rejects a build output path whose extension isn't one Rojo can produce.
    /// `rojo build` enforces this too, but checking here gives the model a clear,
    /// fast error (and an explicit list of choices) instead of a subprocess
    /// failure, and avoids touching the filesystem on an obvious mistake.
    fn require_build_extension(&self, output: &str) -> Result<(), String> {
        const VALID_EXTENSIONS: [&str; 4] = ["rbxl", "rbxlx", "rbxm", "rbxmx"];

        let extension = Path::new(output)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);

        match extension {
            Some(ext) if VALID_EXTENSIONS.contains(&ext.as_str()) => Ok(()),
            _ => Err(format!(
                "Output '{output}' must end in one of: {}.",
                VALID_EXTENSIONS
                    .iter()
                    .map(|ext| format!(".{ext}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SourcemapArgs {
    /// Include non-script instances (folders, values, etc.), not just scripts.
    #[serde(default)]
    include_non_scripts: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BuildArgs {
    /// Output file path. Must end in .rbxl, .rbxlx, .rbxm, or .rbxmx.
    output: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GenScriptArgs {
    /// Name of the script to create, without extension.
    name: String,
    /// Kind of script: "server", "client", or "module" (default "module").
    #[serde(default)]
    kind: Option<String>,
    /// Directory to create the script in (defaults to the project's `src`).
    #[serde(default)]
    path: Option<String>,
}

#[tool_router]
impl RojoMcpServer {
    #[tool(
        description = "Return the project's instance tree as a Rojo sourcemap (JSON), mapping \
                       instances to their source files. The best tool for understanding and \
                       navigating the project's structure."
    )]
    fn sourcemap(&self, Parameters(args): Parameters<SourcemapArgs>) -> Result<String, String> {
        let mut rojo_args = vec!["sourcemap"];
        if args.include_non_scripts {
            rojo_args.push("--include-non-scripts");
        }
        self.run_rojo(&rojo_args)
    }

    #[tool(
        description = "Report whether a Rojo server is running for the project, with its \
                       address, port, uptime, and connected-client count (JSON)."
    )]
    fn status(&self) -> Result<String, String> {
        self.run_rojo(&["status", "--json"])
    }

    #[tool(
        description = "Build the project into a Roblox place or model file. Provide an output \
                       path, relative to the project, ending in .rbxl, .rbxlx, .rbxm, or .rbxmx."
    )]
    fn build(&self, Parameters(args): Parameters<BuildArgs>) -> Result<String, String> {
        self.ensure_writable()?;
        self.confine(&args.output)?;
        self.require_build_extension(&args.output)?;
        self.run_rojo(&["build", "--json", "--output", &args.output])
    }

    #[tool(
        description = "Scaffold a new Luau script in the project. kind is 'server', 'client', \
                       or 'module'."
    )]
    fn gen_script(&self, Parameters(args): Parameters<GenScriptArgs>) -> Result<String, String> {
        self.ensure_writable()?;
        if let Some(path) = args.path.as_deref() {
            self.confine(path)?;
        }

        let kind = args.kind.as_deref().unwrap_or("module");
        let mut rojo_args = vec![
            "gen",
            "script",
            args.name.as_str(),
            "--kind",
            kind,
            "--json",
        ];
        if let Some(path) = args.path.as_deref() {
            rojo_args.push("--path");
            rojo_args.push(path);
        }
        self.run_rojo(&rojo_args)
    }

    #[tool(description = "Stop the running Rojo server for the project.")]
    fn stop(&self) -> Result<String, String> {
        self.ensure_writable()?;
        self.run_rojo(&["stop", "--json"])
    }

    #[tool(
        description = "Restart the running Rojo server, preserving its session id so a connected \
                       Studio plugin reconnects seamlessly."
    )]
    fn restart(&self) -> Result<String, String> {
        self.ensure_writable()?;
        self.run_rojo(&["restart", "--json"])
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RojoMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("rojo", env!("CARGO_PKG_VERSION"));

        let tools = if self.read_only {
            "sourcemap (instance tree), status"
        } else {
            "sourcemap (instance tree), status, build, gen_script, stop, restart"
        };
        let mode = if self.read_only { " (read-only)" } else { "" };
        info.instructions = Some(format!(
            "Rojo MCP server{mode}. Tools: {tools}. Use sourcemap first to understand the project."
        ));

        info
    }
}

/// Runs `rojo <args>` in `work_dir`, capturing stdout. Returns the trimmed
/// stderr on failure.
fn run_rojo_command(work_dir: &Path, args: &[&str]) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;

    let output = Command::new(exe)
        .args(args)
        .current_dir(work_dir)
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`rojo {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> RojoMcpServer {
        RojoMcpServer::new(PathBuf::from("."), false)
    }

    #[test]
    fn build_extension_accepts_roblox_formats() {
        let server = server();
        for output in ["game.rbxl", "game.rbxlx", "model.rbxm", "model.rbxmx"] {
            assert!(
                server.require_build_extension(output).is_ok(),
                "{output} should be accepted"
            );
        }
    }

    #[test]
    fn build_extension_is_case_insensitive() {
        assert!(server().require_build_extension("Game.RBXL").is_ok());
    }

    #[test]
    fn build_extension_rejects_other_formats() {
        let server = server();
        for output in ["out.txt", "game.rbx", "noextension", "archive.zip"] {
            assert!(
                server.require_build_extension(output).is_err(),
                "{output} should be rejected"
            );
        }
    }
}
