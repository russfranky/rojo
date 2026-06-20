//! Client-side helpers for talking to a running `rojo serve` over its local HTTP
//! control API (`/api/health`, `/api/stop`).
//!
//! These are used by `rojo serve` (to detect an already-running server for the
//! same project) and by `rojo status`/`stop`/`restart`. They request JSON via
//! the `Accept` header so the responses can be parsed directly with serde_json.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::Serialize;

use crate::{
    session_id::SessionId,
    state_file::{self, ServeState},
    web_api::HealthResponse,
};

/// Default timeout for a liveness probe. Short, because a live local server
/// answers almost instantly and we don't want to stall startup on a dead one.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Timeout for a stop request, which may take slightly longer as the server
/// drains in-flight connections.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Builds the base URL to reach a server bound to `address:port`. Unspecified
/// binds (`0.0.0.0` / `::`) are probed over loopback, which is reachable.
fn base_url(address: IpAddr, port: u16) -> String {
    let address = if address.is_unspecified() {
        match address {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        }
    } else {
        address
    };

    // SocketAddr's Display bracket-wraps IPv6 addresses correctly for URLs.
    format!("http://{}", SocketAddr::new(address, port))
}

/// Probes `address:port` for a live Rojo server, returning its health info if
/// one responds. Returns `None` on any error (no server, timeout, wrong
/// response), so callers can treat a missing or dead server uniformly.
pub fn probe(address: IpAddr, port: u16) -> Option<HealthResponse> {
    let client = reqwest::blocking::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;

    let url = format!("{}/api/health", base_url(address, port));
    let response = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    response.json::<HealthResponse>().ok()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StopBody {
    session_id: SessionId,
}

/// Asks the server at `address:port` to shut down gracefully. `session_id` must
/// match the running server's id (read from a probe or the serve-state file);
/// the server rejects mismatches.
pub fn request_stop(address: IpAddr, port: u16, session_id: SessionId) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(STOP_TIMEOUT)
        .build()?;

    let url = format!("{}/api/stop", base_url(address, port));
    let response = client
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&StopBody { session_id })
        .send()
        .context("Failed to contact the running Rojo server")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "The Rojo server refused the stop request (HTTP {})",
            response.status()
        );
    }

    Ok(())
}

/// Loads the serve-state for `root_dir` and probes it, returning both only if a
/// matching server is actually running. A stale state file (server gone, or a
/// different server now on that port) yields `None`.
pub fn discover_running(root_dir: &Path) -> Option<(ServeState, HealthResponse)> {
    let state = state_file::load(root_dir)?;
    let health = probe(state.address, state.port)?;

    if health.session_id != state.session_id {
        return None;
    }

    Some((state, health))
}

/// Polls until the server at `address:port` stops responding, up to `timeout`.
/// Returns whether it went offline in time.
pub fn wait_until_offline(address: IpAddr, port: u16, timeout: Duration) -> bool {
    let start = Instant::now();

    while start.elapsed() < timeout {
        if probe(address, port).is_none() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }

    false
}

/// Polls until the server at `address:port` starts responding, up to `timeout`.
/// Returns whether it came online in time.
pub fn wait_until_online(address: IpAddr, port: u16, timeout: Duration) -> bool {
    let start = Instant::now();

    while start.elapsed() < timeout {
        if probe(address, port).is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }

    false
}
