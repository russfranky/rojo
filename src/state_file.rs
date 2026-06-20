//! Persistent, project-local record of a running `rojo serve` process.
//!
//! Written to `<project>/.rojo/serve-state.json` when a serve session binds, and
//! removed on graceful shutdown. This lets `rojo status`/`stop`/`restart`
//! discover a running server, and lets a restarted server reuse the same
//! [`SessionId`] so connected Studio plugins reconnect seamlessly rather than
//! tearing down on a "Server changed ID" mismatch.

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::session_id::SessionId;

/// Directory under the project root that holds Rojo's local state.
const STATE_DIR: &str = ".rojo";

/// File within [`STATE_DIR`] that records the running serve session.
const STATE_FILE: &str = "serve-state.json";

/// A snapshot of a running `rojo serve` process, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeState {
    pub session_id: SessionId,
    pub address: IpAddr,
    pub port: u16,
    pub pid: u32,
    pub project_name: String,
    pub project_file: PathBuf,
    /// Extra allowed hosts the server was started with, so `rojo restart` can
    /// reproduce the original invocation.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    pub started_unix: u64,
    pub server_version: String,
}

/// Returns the path to the serve-state file for a project root directory.
pub fn state_path(project_root: &Path) -> PathBuf {
    project_root.join(STATE_DIR).join(STATE_FILE)
}

/// Loads the serve-state file for the given project root.
///
/// Returns `None` when the file is missing, unreadable, or malformed. A
/// malformed file is logged and treated as absent so a stale or corrupt record
/// never blocks startup.
pub fn load(project_root: &Path) -> Option<ServeState> {
    let path = state_path(project_root);
    let contents = fs_err::read(&path).ok()?;

    match serde_json::from_slice(&contents) {
        Ok(state) => Some(state),
        Err(err) => {
            log::warn!(
                "Ignoring malformed Rojo serve-state file at {}: {}",
                path.display(),
                err
            );
            None
        }
    }
}

/// Writes the serve-state file for the given project root, creating the `.rojo`
/// directory if it does not yet exist.
pub fn write(project_root: &Path, state: &ServeState) -> anyhow::Result<()> {
    let dir = project_root.join(STATE_DIR);
    fs_err::create_dir_all(&dir)?;

    let contents = serde_json::to_vec_pretty(state)?;
    fs_err::write(dir.join(STATE_FILE), contents)?;

    Ok(())
}

/// Removes the serve-state file for the given project root. Succeeds even if the
/// file is already gone.
pub fn remove(project_root: &Path) {
    let path = state_path(project_root);

    match fs_err::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::warn!(
            "Failed to remove serve-state file at {}: {}",
            path.display(),
            err
        ),
    }
}

/// Returns the current Unix timestamp in seconds, or 0 if the clock is set
/// before the epoch.
pub fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
