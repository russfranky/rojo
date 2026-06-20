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

impl RojoMcpServer {
    fn new(work_dir: PathBuf, read_only: bool) -> Self {
        Self {
            work_dir,
            read_only,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs `rojo <args>` in the project directory and returns stdout, or an
    /// error string suitable for returning to the AI as the tool result.
    fn run_rojo(&self, args: &[&str]) -> String {
        match run_rojo_command(&self.work_dir, args) {
            Ok(output) => output,
            Err(err) => format!("Error: {err}"),
        }
    }

    /// Returns a refusal message when running read-only, else `None`.
    fn read_only_refusal(&self) -> Option<String> {
        self.read_only.then(|| {
            "This tool is disabled because the MCP server is running in --read-only mode."
                .to_string()
        })
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
    fn sourcemap(&self, Parameters(args): Parameters<SourcemapArgs>) -> String {
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
    fn status(&self) -> String {
        self.run_rojo(&["status", "--json"])
    }

    #[tool(
        description = "Build the project into a Roblox place or model file. Provide an output \
                       path ending in .rbxl, .rbxlx, .rbxm, or .rbxmx."
    )]
    fn build(&self, Parameters(args): Parameters<BuildArgs>) -> String {
        if let Some(refusal) = self.read_only_refusal() {
            return refusal;
        }
        self.run_rojo(&["build", "--json", "--output", &args.output])
    }

    #[tool(
        description = "Scaffold a new Luau script in the project. kind is 'server', 'client', \
                       or 'module'."
    )]
    fn gen_script(&self, Parameters(args): Parameters<GenScriptArgs>) -> String {
        if let Some(refusal) = self.read_only_refusal() {
            return refusal;
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
    fn stop(&self) -> String {
        if let Some(refusal) = self.read_only_refusal() {
            return refusal;
        }
        self.run_rojo(&["stop", "--json"])
    }

    #[tool(
        description = "Restart the running Rojo server, preserving its session id so a connected \
                       Studio plugin reconnects seamlessly."
    )]
    fn restart(&self) -> String {
        if let Some(refusal) = self.read_only_refusal() {
            return refusal;
        }
        self.run_rojo(&["restart", "--json"])
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RojoMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("rojo", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Rojo MCP server. Tools: sourcemap (instance tree), status, build, gen_script, \
             stop, restart. Use sourcemap first to understand the project."
                .to_string(),
        );
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
