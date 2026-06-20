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

/// Candidate addresses to reach a server bound to `address`. Unspecified binds
/// (`0.0.0.0` / `::`) are reached over loopback; a `::` bind is tried on both
/// IPv6 and IPv4 loopback because whether IPv4 is mapped into a `::` socket is
/// platform-dependent (`bindv6only`).
fn candidate_addresses(address: IpAddr) -> Vec<IpAddr> {
    match address {
        IpAddr::V4(a) if a.is_unspecified() => vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        IpAddr::V6(a) if a.is_unspecified() => {
            vec![
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ]
        }
        other => vec![other],
    }
}

/// Builds a URL for `path` against a candidate address. `SocketAddr`'s Display
/// bracket-wraps IPv6 addresses correctly for URLs.
fn url_for(address: IpAddr, port: u16, path: &str) -> String {
    format!("http://{}{}", SocketAddr::new(address, port), path)
}

/// Probes `address:port` for a live Rojo server, returning its health info if
/// one responds. Returns `None` on any error (no server, timeout, wrong
/// response), so callers can treat a missing or dead server uniformly.
pub fn probe(address: IpAddr, port: u16) -> Option<HealthResponse> {
    let client = reqwest::blocking::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;

    for addr in candidate_addresses(address) {
        let url = url_for(addr, port, "/api/health");
        let Ok(response) = client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
        else {
            continue;
        };

        if !response.status().is_success() {
            continue;
        }

        if let Ok(health) = response.json::<HealthResponse>() {
            return Some(health);
        }
    }

    None
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StopBody {
    session_id: SessionId,
    pid: u32,
}

/// Asks the server at `address:port` to shut down gracefully. `session_id` and
/// `pid` must match the running server (both read from a probe or the
/// serve-state file); the server rejects mismatches. Sending the `pid` keeps a
/// stop aimed at a particular process from hitting a successor that reused the
/// session id (e.g. after `rojo restart`).
pub fn request_stop(
    address: IpAddr,
    port: u16,
    session_id: SessionId,
    pid: u32,
) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(STOP_TIMEOUT)
        .build()?;

    let mut last_err = None;
    for addr in candidate_addresses(address) {
        let url = url_for(addr, port, "/api/stop");
        match client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&StopBody { session_id, pid })
            .send()
        {
            Ok(response) => {
                if response.status().is_success() {
                    return Ok(());
                }
                // We reached a server but it refused (e.g. wrong session id);
                // that's authoritative, so don't try other candidates.
                anyhow::bail!(
                    "The Rojo server refused the stop request (HTTP {})",
                    response.status()
                );
            }
            Err(err) => last_err = Some(err),
        }
    }

    match last_err {
        Some(err) => {
            Err(anyhow::Error::new(err).context("Failed to contact the running Rojo server"))
        }
        None => anyhow::bail!("Failed to contact the running Rojo server"),
    }
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
