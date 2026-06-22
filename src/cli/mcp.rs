//! The `rojo mcp` subcommand: a Model Context Protocol server, exposing Rojo's
//! capabilities as tools for AI assistants over stdio.
//!
//! Read tools that get called repeatedly while an assistant explores a project
//! (`sourcemap`, `read_instance`) are answered from a single long-lived,
//! file-watching [`ServeSession`] held in memory, so they never pay to rebuild
//! the whole instance tree per call. The remaining tools (`build`, `gen_script`,
//! and the `status`/`stop`/`restart` controls for a separate `rojo serve`
//! daemon) drive Rojo's own CLI as subprocesses: that keeps them consistent with
//! what a developer runs by hand, and lets `build` always read fresh from disk.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::Context;
use clap::Parser;
use memofs::Vfs;
use rbx_dom_weak::types::{Ref, VariantType};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};

use crate::{serve_session::ServeSession, snapshot::RojoTree};

use super::{resolve_path, resolve_project_root};

/// Runs an MCP server over stdio that exposes Rojo's tooling to AI assistants.
#[derive(Debug, Parser)]
pub struct McpCommand {
    /// Path to the project the server operates on. Defaults to the current
    /// directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Only expose read-only tools (sourcemap, read_instance, status). Mutating
    /// tools (build, gen, serve control) are refused. Safer for untrusted AI
    /// sessions.
    #[clap(long)]
    pub read_only: bool,
}

impl McpCommand {
    pub fn run(self) -> anyhow::Result<()> {
        // Working directory for the subprocess-backed tools (build, gen, and the
        // serve-daemon controls), so each subcommand's project/path defaults
        // resolve correctly.
        let work_dir = resolve_project_root(&self.project)?;

        // One long-lived, file-watching session backs the read tools so they
        // answer from a live in-memory tree instead of rebuilding it per call.
        // `Vfs::new_default` enables watching, so the session's ChangeProcessor
        // keeps that tree current as files change on disk.
        let project_path = fs_err::canonicalize(resolve_path(&self.project)?)?;
        let vfs = Vfs::new_default()?;
        let session = Arc::new(
            ServeSession::new(vfs, &project_path)
                .context("Failed to open the Rojo project for the MCP server")?,
        );

        let server = RojoMcpServer::new(work_dir, session, self.read_only);

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
    /// Long-lived, file-watching session backing the in-memory read tools
    /// (`sourcemap`, `read_instance`).
    session: Arc<ServeSession>,
    tool_router: ToolRouter<Self>,
}

/// Mutating tools, hidden and rejected in `--read-only` mode.
const MUTATING_TOOLS: [&str; 5] = ["build", "gen_script", "stop", "restart", "reset_studio"];

impl RojoMcpServer {
    fn new(work_dir: PathBuf, session: Arc<ServeSession>, read_only: bool) -> Self {
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
            session,
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

    /// Core of the `read_instance` tool, factored out so it can be unit-tested
    /// without constructing the MCP `Parameters` wrapper. Reads the in-memory
    /// tree; the lock is held only for this synchronous call and never spans an
    /// await.
    fn read_instance_json(&self, path: &str) -> Result<String, String> {
        let tree = self.session.tree();

        let id = resolve_instance_path(&tree, path).ok_or_else(|| {
            format!("No instance found at path '{path}'. Run `sourcemap` to see valid paths.")
        })?;
        let instance = tree
            .get_instance(id)
            .expect("a resolved instance id must exist in the tree");

        // Serialize each property independently and skip any that can't be
        // represented as JSON (e.g. SharedString), so one exotic value can never
        // fail the whole read.
        let mut properties = serde_json::Map::new();
        for (key, value) in instance.properties() {
            if value.ty() == VariantType::SharedString {
                continue;
            }
            if let Ok(json) = serde_json::to_value(value) {
                properties.insert(key.to_string(), json);
            }
        }

        let children: Vec<ChildView> = instance
            .children()
            .iter()
            .filter_map(|&child_id| {
                tree.get_instance(child_id).map(|child| ChildView {
                    name: child.name().to_owned(),
                    class_name: child.class_name().to_string(),
                })
            })
            .collect();

        let view = InstanceView {
            name: instance.name().to_owned(),
            class_name: instance.class_name().to_string(),
            properties,
            children,
        };

        serde_json::to_string(&view).map_err(|err| err.to_string())
    }
}

/// Rejects a build output path whose extension isn't one Rojo can produce.
/// `rojo build` enforces this too, but checking here gives the model a clear,
/// fast error (and an explicit list of choices) instead of a subprocess failure,
/// and avoids touching the filesystem on an obvious mistake.
fn require_build_extension(output: &str) -> Result<(), String> {
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

/// Resolves a slash-separated instance path to a referent, walking the tree by
/// child name from the root. An empty path is the root itself; the root's own
/// name is accepted as an optional leading segment, so a path copied from a
/// `sourcemap` (which shows the root at the top) resolves too.
fn resolve_instance_path(tree: &RojoTree, path: &str) -> Option<Ref> {
    let root_id = tree.get_root_id();

    let segments: Vec<&str> = path
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.is_empty() {
        return Some(root_id);
    }

    let root = tree.get_instance(root_id)?;
    let start = usize::from(segments[0] == root.name());

    let mut current = root_id;
    for segment in &segments[start..] {
        let instance = tree.get_instance(current)?;
        current = instance.children().iter().copied().find(|&child_id| {
            tree.get_instance(child_id)
                .is_some_and(|child| child.name() == *segment)
        })?;
    }

    Some(current)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SourcemapArgs {
    /// Include non-script instances (folders, values, etc.), not just scripts.
    #[serde(default)]
    include_non_scripts: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadInstanceArgs {
    /// Slash-separated path to the instance from the project root, e.g.
    /// "ReplicatedStorage/Shared/MyModule". Omit it (or pass "") for the root.
    /// The root instance's own name may be included as the first segment.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BuildArgs {
    /// Output file path. Must end in .rbxl, .rbxlx, .rbxm, or .rbxmx.
    output: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadLogsArgs {
    /// Only return entries newer than this sequence number (the `tailSeq` from a
    /// previous call), so repeated polling doesn't see the same lines twice.
    #[serde(default)]
    since: Option<u64>,
    /// Minimum severity to include: "print", "info", "warning", or "error".
    #[serde(default)]
    level: Option<String>,
    /// Maximum number of entries to return (the newest are kept).
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ResetStudioArgs {
    /// Optional place file to open in Studio after relaunching, relative to the
    /// project. Omit it to launch Studio to its start screen.
    #[serde(default)]
    place: Option<String>,
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

/// A single instance's details, returned by the `read_instance` tool.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InstanceView {
    name: String,
    class_name: String,
    properties: serde_json::Map<String, serde_json::Value>,
    children: Vec<ChildView>,
}

/// A child entry in an [`InstanceView`]: enough for the model to navigate to it.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ChildView {
    name: String,
    class_name: String,
}

/// Connectivity summary returned by the `connection` tool: the connectivity
/// subset of `rojo status`, with an explicit `connected` boolean so the model
/// doesn't have to reason about `connectedClients > 0` itself.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionView {
    /// Whether a Rojo server is running for the project.
    running: bool,
    /// Whether at least one Studio plugin is attached (connectedClients > 0).
    connected: bool,
    connected_clients: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_seconds: Option<u64>,
}

/// Distills `rojo status --json` output into a [`ConnectionView`]. Pure, so it's
/// unit-tested without a running server.
fn summarize_connection(status_json: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(status_json).map_err(|err| err.to_string())?;

    let running = value["running"].as_bool().unwrap_or(false);
    let connected_clients = value["connectedClients"].as_u64().unwrap_or(0);

    let view = ConnectionView {
        running,
        connected: connected_clients > 0,
        connected_clients,
        session_id: value["sessionId"].as_str().map(str::to_owned),
        uptime_seconds: value["uptimeSeconds"].as_u64(),
    };

    serde_json::to_string(&view).map_err(|err| err.to_string())
}

#[tool_router]
impl RojoMcpServer {
    #[tool(
        description = "Return the project's instance tree as a Rojo sourcemap (JSON), mapping \
                       instances to their source files. The best tool for understanding and \
                       navigating the project's structure."
    )]
    fn sourcemap(&self, Parameters(args): Parameters<SourcemapArgs>) -> Result<String, String> {
        super::sourcemap::sourcemap_json(&self.session, args.include_non_scripts)
            .map_err(|err| err.to_string())
    }

    #[tool(
        description = "Inspect one instance in the project tree: its class name, property \
                       values, and the names and classes of its immediate children. Locate it \
                       with a slash-separated path from the root (e.g. \
                       \"ReplicatedStorage/Shared/MyModule\"); omit the path for the root. Run \
                       `sourcemap` first to discover the names."
    )]
    fn read_instance(
        &self,
        Parameters(args): Parameters<ReadInstanceArgs>,
    ) -> Result<String, String> {
        self.read_instance_json(&args.path.unwrap_or_default())
    }

    #[tool(
        description = "Report whether a Rojo server is running for the project, with its \
                       address, port, uptime, and whether a Studio plugin is connected \
                       (connectedClients > 0) (JSON)."
    )]
    fn status(&self) -> Result<String, String> {
        self.run_rojo(&["status", "--json"])
    }

    #[tool(
        description = "Read recent Output (prints, warnings, and errors with stack traces) \
                       captured from the connected Roblox Studio session — the way to observe what \
                       happened at runtime, e.g. after a playtest. Requires a running Rojo server \
                       with the Studio plugin connected. Filter with level ('print', 'info', \
                       'warning', or 'error') and limit; pass since=<tailSeq from a prior call> to \
                       poll for only new lines."
    )]
    fn read_logs(&self, Parameters(args): Parameters<ReadLogsArgs>) -> Result<String, String> {
        let since = args.since.map(|value| value.to_string());
        let limit = args.limit.map(|value| value.to_string());

        let mut rojo_args = vec!["logs", "--json"];
        if let Some(since) = since.as_deref() {
            rojo_args.push("--since");
            rojo_args.push(since);
        }
        if let Some(level) = args.level.as_deref() {
            rojo_args.push("--level");
            rojo_args.push(level);
        }
        if let Some(limit) = limit.as_deref() {
            rojo_args.push("--limit");
            rojo_args.push(limit);
        }
        self.run_rojo(&rojo_args)
    }

    #[tool(
        description = "Check whether a Roblox Studio plugin is currently connected to the running \
                       Rojo server (connected = connectedClients > 0). Returns running, connected, \
                       connectedClients, sessionId, and uptimeSeconds. Use this before expecting \
                       live sync or read_logs to reflect what's happening in Studio."
    )]
    fn connection(&self) -> Result<String, String> {
        let status = self.run_rojo(&["status", "--json"])?;
        summarize_connection(&status)
    }

    #[tool(
        description = "Build the project into a Roblox place or model file. Provide an output \
                       path, relative to the project, ending in .rbxl, .rbxlx, .rbxm, or .rbxmx."
    )]
    fn build(&self, Parameters(args): Parameters<BuildArgs>) -> Result<String, String> {
        self.ensure_writable()?;
        self.confine(&args.output)?;
        require_build_extension(&args.output)?;
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

    #[tool(
        description = "Force-restart Roblox Studio with no native dialogs (macOS only): kills \
                       Studio, deletes its auto-recovery files so no 'restore' prompt appears, \
                       and relaunches it. Optionally open a place file (relative to the project). \
                       Use this to reboot Studio unattended; the Rojo plugin reconnects to the \
                       running server on its own, and the result reports whether it reconnected \
                       (pluginReconnected)."
    )]
    fn reset_studio(
        &self,
        Parameters(args): Parameters<ResetStudioArgs>,
    ) -> Result<String, String> {
        self.ensure_writable()?;

        let mut rojo_args = vec!["studio", "reset", "--json"];
        if let Some(place) = args.place.as_deref() {
            self.confine(place)?;
            rojo_args.push("--place");
            rojo_args.push(place);
        }
        self.run_rojo(&rojo_args)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RojoMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("rojo", env!("CARGO_PKG_VERSION"));

        let tools = if self.read_only {
            "sourcemap (instance tree), read_instance, status, connection, read_logs"
        } else {
            "sourcemap (instance tree), read_instance, status, connection, read_logs, build, \
             gen_script, stop, restart, reset_studio"
        };
        let mode = if self.read_only { " (read-only)" } else { "" };
        info.instructions = Some(format!(
            "Rojo MCP server{mode}. Tools: {tools}. Use sourcemap first to understand the \
             project, then read_instance to inspect a specific instance's properties."
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

    /// Builds a server backed by a live session for the `attributes` test
    /// project (DataModel → Workspace → Folder, where Folder has an attribute).
    /// Watching is disabled so the test only sees the deterministic initial tree.
    fn attributes_server() -> RojoMcpServer {
        let project = fs_err::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-projects")
                .join("attributes"),
        )
        .unwrap();

        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let session = Arc::new(ServeSession::new(vfs, &project).unwrap());

        RojoMcpServer::new(project, session, false)
    }

    #[test]
    fn build_extension_accepts_roblox_formats() {
        for output in ["game.rbxl", "game.rbxlx", "model.rbxm", "model.rbxmx"] {
            assert!(
                require_build_extension(output).is_ok(),
                "{output} should be accepted"
            );
        }
    }

    #[test]
    fn build_extension_is_case_insensitive() {
        assert!(require_build_extension("Game.RBXL").is_ok());
    }

    #[test]
    fn build_extension_rejects_other_formats() {
        for output in ["out.txt", "game.rbx", "noextension", "archive.zip"] {
            assert!(
                require_build_extension(output).is_err(),
                "{output} should be rejected"
            );
        }
    }

    #[test]
    fn sourcemap_reads_embedded_tree() {
        let server = attributes_server();

        let json =
            super::super::sourcemap::sourcemap_json(&server.session, true).expect("sourcemap");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["name"], "attributes");
    }

    #[test]
    fn read_instance_resolves_paths() {
        let server = attributes_server();

        let root: serde_json::Value =
            serde_json::from_str(&server.read_instance_json("").unwrap()).unwrap();
        assert_eq!(root["name"], "attributes");

        // The nested instance resolves with and without the optional root-name
        // prefix.
        for path in ["Workspace/Folder", "attributes/Workspace/Folder"] {
            let folder: serde_json::Value =
                serde_json::from_str(&server.read_instance_json(path).unwrap()).unwrap();
            assert_eq!(folder["name"], "Folder", "for path {path}");
            assert_eq!(folder["className"], "Folder", "for path {path}");
        }
    }

    #[test]
    fn read_instance_rejects_unknown_path() {
        let server = attributes_server();

        assert!(server.read_instance_json("Workspace/DoesNotExist").is_err());
    }

    #[test]
    fn summarize_connection_reports_connected() {
        let json = r#"{"running":true,"connectedClients":2,"sessionId":"abc","uptimeSeconds":12}"#;
        let summary: serde_json::Value =
            serde_json::from_str(&summarize_connection(json).unwrap()).unwrap();

        assert_eq!(summary["running"], true);
        assert_eq!(summary["connected"], true);
        assert_eq!(summary["connectedClients"], 2);
        assert_eq!(summary["sessionId"], "abc");
        assert_eq!(summary["uptimeSeconds"], 12);
    }

    #[test]
    fn summarize_connection_handles_not_running() {
        let summary: serde_json::Value =
            serde_json::from_str(&summarize_connection(r#"{"running":false}"#).unwrap()).unwrap();

        assert_eq!(summary["running"], false);
        assert_eq!(summary["connected"], false);
        assert_eq!(summary["connectedClients"], 0);
        // Absent optional fields are omitted, not null.
        assert!(summary.get("sessionId").is_none());
    }

    #[test]
    fn read_logs_is_a_read_tool() {
        // `read_logs` must stay available in --read-only mode, so it must not be
        // in MUTATING_TOOLS (the list `new` uses to disable routes). Guards
        // against it being miscategorized as mutating.
        assert!(!MUTATING_TOOLS.contains(&"read_logs"));
        // Sanity: the genuinely mutating tools are still listed.
        assert!(MUTATING_TOOLS.contains(&"build"));
        assert!(MUTATING_TOOLS.contains(&"reset_studio"));
    }
}
