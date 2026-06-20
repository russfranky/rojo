use std::{
    io::{self, Write},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::Parser;
use memofs::Vfs;
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

use crate::{
    project::Project,
    serve_session::ServeSession,
    session_id::SessionId,
    state_file::{self, ServeState},
    web::LiveServer,
};

use super::{output, resolve_path, serve_control, GlobalOptions};

const DEFAULT_BIND_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const DEFAULT_PORT: u16 = 34872;

/// Expose a Rojo project to the Rojo Studio plugin.
#[derive(Debug, Parser)]
pub struct ServeCommand {
    /// Path to the project to serve. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// The IP address to listen on. Defaults to `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// The port to listen on. Defaults to the project's preference, or `34872` if
    /// it has none.
    #[clap(long)]
    pub port: Option<u16>,

    /// Extra `Host`/`Origin` values the server will accept, beyond localhost and
    /// the bind address (for example a hostname like `mypc.lan`). Repeat the
    /// option or comma-separate to allow several. When given, this overrides the
    /// project's `serveAllowedHosts`. Listing any host also turns on Host/Origin
    /// validation for binds where it is otherwise off (such as `0.0.0.0`).
    #[clap(long, value_delimiter = ',')]
    pub allowed_hosts: Vec<String>,

    /// Force a specific session id instead of generating or reusing one. Mainly
    /// used internally by `rojo restart` to preserve the session across a
    /// restart so connected clients reconnect without interruption.
    #[clap(long, hide = true)]
    pub session_id: Option<SessionId>,
}

impl ServeCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project)?;
        let project_ref: &Path = project_path.as_ref();

        let vfs = Vfs::new_default()?;

        // Load the project just to learn its root directory, so we can read any
        // prior serve-state and decide whether to reuse its session id before
        // building the (more expensive) ServeSession. Canonicalized so the
        // serve-state location matches what `status`/`stop`/`restart` compute.
        let root_dir = state_file::canonical_dir(
            Project::load_initial_project(&vfs, project_ref)?.folder_location(),
        );

        // Reuse the session id from a prior serve session (if one exited without
        // cleaning up) so connected plugins reconnect seamlessly across a
        // restart. Bails if a server is genuinely still running. An explicit
        // --session-id (e.g. from `rojo restart`) takes precedence.
        let session_id = resolve_session_id(&root_dir, self.session_id)?;

        let session = Arc::new(ServeSession::new_with_session_id(
            vfs,
            project_ref,
            session_id,
        )?);

        let ip = self
            .address
            .or_else(|| session.serve_address())
            .unwrap_or(DEFAULT_BIND_ADDRESS.into());

        let port = self
            .port
            .or_else(|| session.project_port())
            .unwrap_or(DEFAULT_PORT);

        // The CLI flag, when given, replaces the project's list rather than
        // merging with it, matching how --address and --port override theirs.
        let allowed_hosts = if self.allowed_hosts.is_empty() {
            session.serve_allowed_hosts().to_vec()
        } else {
            self.allowed_hosts.clone()
        };

        let serve_state = ServeState {
            session_id: session.session_id(),
            address: ip,
            port,
            pid: std::process::id(),
            project_name: session.project_name().to_owned(),
            project_file: project_path.to_path_buf(),
            allowed_hosts: allowed_hosts.clone(),
            started_unix: state_file::now_unix(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        };

        // Capture the serve hooks before the session is handed to the server, so
        // they can run once the port is bound. Disabled by `--no-hooks`.
        let serve_hooks = if global.no_hooks {
            Vec::new()
        } else {
            session
                .root_project()
                .hooks
                .as_ref()
                .map(|hooks| hooks.serve.clone())
                .unwrap_or_default()
        };

        let server = LiveServer::new(session);

        let on_listening = {
            let root_dir = root_dir.clone();
            move || {
                // Record the running server so `rojo status`/`stop`/`restart` can
                // find it. Written only after a successful bind.
                if let Err(err) = state_file::write(&root_dir, &serve_state) {
                    log::warn!("Failed to write Rojo serve-state file: {}", err);
                }

                if global.json {
                    let _ = output::print_json(&serve_state);
                } else {
                    let _ = show_start_message(ip, port, global.color.into());
                }

                // Serve hooks run after a successful bind. A failing serve hook is
                // logged but does not tear down an already-running server.
                if let Err(err) =
                    crate::hooks::run_event("serve", &serve_hooks, &root_dir, global.json)
                {
                    log::error!("serve hook failed: {err:#}");
                }
            }
        };

        server.start((ip, port).into(), allowed_hosts, on_listening)?;

        // The server only returns here on a graceful shutdown (e.g. via
        // `rojo stop`/`rojo restart`). Clean up our serve-state file, but only if
        // it still describes us — a `restart` successor may have already written
        // its own, and we must not delete that.
        state_file::remove_if_pid(&root_dir, std::process::id());

        Ok(())
    }
}

/// Decides which [`SessionId`] a starting server should use.
///
/// Bails if a server is already running for this project. Otherwise prefers an
/// explicit override (from `rojo restart`), then the id recorded by a previous
/// serve session (so a connected plugin reconnects seamlessly), and finally
/// `None` to mint a fresh id.
fn resolve_session_id(
    root_dir: &Path,
    explicit: Option<SessionId>,
) -> anyhow::Result<Option<SessionId>> {
    let prior = state_file::load(root_dir);

    if let Some(prior) = &prior {
        if let Some(health) = serve_control::probe(prior.address, prior.port) {
            if health.session_id == prior.session_id {
                anyhow::bail!(
                    "A Rojo server for this project is already running at {}:{} (pid {}).\n\
                     Use `rojo restart` to restart it, or `rojo stop` to stop it first.",
                    prior.address,
                    prior.port,
                    prior.pid
                );
            }
        }
    }

    if let Some(explicit) = explicit {
        return Ok(Some(explicit));
    }

    if let Some(prior) = prior {
        log::debug!(
            "Reusing session id {} from a previous serve session",
            prior.session_id
        );
        return Ok(Some(prior.session_id));
    }

    Ok(None)
}

fn show_start_message(bind_address: IpAddr, port: u16, color: ColorChoice) -> io::Result<()> {
    let mut green = ColorSpec::new();
    green.set_fg(Some(Color::Green)).set_bold(true);

    let writer = BufferWriter::stdout(color);
    let mut buffer = writer.buffer();

    let address_string = if bind_address.is_loopback() {
        "localhost".to_owned()
    } else {
        bind_address.to_string()
    };

    writeln!(&mut buffer, "Rojo server listening:")?;

    write!(&mut buffer, "  Address: ")?;
    buffer.set_color(&green)?;
    writeln!(&mut buffer, "{}", address_string)?;

    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, "  Port:    ")?;
    buffer.set_color(&green)?;
    writeln!(&mut buffer, "{}", port)?;

    writeln!(&mut buffer)?;

    if !bind_address.is_loopback() {
        let mut warning = ColorSpec::new();
        warning.set_fg(Some(Color::Yellow)).set_bold(true);

        buffer.set_color(&warning)?;
        writeln!(
            &mut buffer,
            "WARNING: This server is bound to {address_string}, which is reachable from the \
             network.\n\
             The serve API is unauthenticated, so anyone who can reach {address_string}:{port} \
             can read\n\
             and modify your project's source. Prefer binding to localhost and tunneling (e.g. \
             SSH,\n\
             Tailscale, or WireGuard) when you need remote access."
        )?;
        buffer.set_color(&ColorSpec::new())?;
        writeln!(&mut buffer)?;
    }

    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, "Visit ")?;

    buffer.set_color(&green)?;
    write!(&mut buffer, "http://{}:{}/", address_string, port)?;

    buffer.set_color(&ColorSpec::new())?;
    writeln!(&mut buffer, " in your browser for more information.")?;

    writer.print(&buffer)?;

    Ok(())
}
