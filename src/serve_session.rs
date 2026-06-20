use std::{
    collections::HashSet,
    io,
    net::IpAddr,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use crossbeam_channel::Sender;
use memofs::Vfs;
use thiserror::Error;
use tokio::sync::Notify;

use crate::{
    change_processor::ChangeProcessor,
    message_queue::MessageQueue,
    project::{Project, ProjectError},
    session_id::SessionId,
    snapshot::{
        apply_patch_set, compute_patch_set, AppliedPatchSet, InstanceContext, InstanceSnapshot,
        PatchSet, RojoTree,
    },
    snapshot_middleware::snapshot_from_vfs,
};

/// Contains all of the state for a Rojo serve session. A serve session is used
/// when we need to build a Rojo tree and possibly rebuild it when input files
/// change.
///
/// Nothing here is specific to any Rojo interface. Though the primary way to
/// interact with a serve session is Rojo's HTTP right now, there's no reason
/// why Rojo couldn't expose an IPC or channels-based API for embedding in the
/// future. `ServeSession` would be roughly the right interface to expose for
/// those cases.
pub struct ServeSession {
    /// The object responsible for listening to changes from the in-memory
    /// filesystem, applying them, updating the Roblox instance tree, and
    /// routing messages through the session's message queue to any connected
    /// clients.
    ///
    /// SHOULD BE DROPPED FIRST! ServeSession and ChangeProcessor communicate
    /// with eachother via channels. If ServeSession hangs up those channels
    /// before dropping the ChangeProcessor, its thread will panic with a
    /// RecvError, causing the main thread to panic on drop.
    ///
    /// Allowed to be unused because it has side effects when dropped.
    #[allow(unused)]
    change_processor: ChangeProcessor,

    /// When the serve session was started. Used only for user-facing
    /// diagnostics.
    start_time: Instant,

    /// The root project for the serve session.
    ///
    /// This will be defined if a folder with a `default.project.json` file was
    /// used for starting the serve session, or if the user specified a full
    /// path to a `.project.json` file.
    root_project: Project,

    /// A randomly generated ID for this serve session. It's used to ensure that
    /// a client doesn't begin connecting to a different server part way through
    /// an operation that needs to be atomic.
    session_id: SessionId,

    /// The tree of Roblox instances associated with this session that will be
    /// updated in real-time. This is derived from the session's VFS and will
    /// eventually be mutable to connected clients.
    tree: Arc<Mutex<RojoTree>>,

    /// An in-memory filesystem containing all of the files relevant for this
    /// live session.
    ///
    /// The main use for accessing it from the session is for debugging issues
    /// with Rojo's live-sync protocol.
    vfs: Arc<Vfs>,

    /// A queue of changes that have been applied to `tree` that affect clients.
    ///
    /// Clients to the serve session will subscribe to this queue either
    /// directly or through the HTTP API to be notified of mutations that need
    /// to be applied.
    message_queue: Arc<MessageQueue<AppliedPatchSet>>,

    /// A channel to send mutation requests on. These will be handled by the
    /// ChangeProcessor and trigger changes in the tree.
    tree_mutation_sender: Sender<PatchSet>,

    /// The number of clients (e.g. Studio plugins) currently subscribed to this
    /// session's live message stream. Reported by the `/api/health` endpoint.
    connected_clients: Arc<AtomicUsize>,

    /// Notified when the session has been asked to shut down gracefully, e.g. via
    /// the `/api/stop` control endpoint. The web server awaits this to begin a
    /// graceful shutdown.
    shutdown: Arc<Notify>,
}

impl ServeSession {
    /// Start a new serve session from the given in-memory filesystem and start
    /// path.
    ///
    /// The project file is expected to be loaded out-of-band since it's
    /// currently loaded from the filesystem directly instead of through the
    /// in-memory filesystem layer.
    pub fn new<P: AsRef<Path>>(vfs: Vfs, start_path: P) -> Result<Self, ServeSessionError> {
        Self::new_with_session_id(vfs, start_path, None)
    }

    /// Like [`ServeSession::new`], but reuses the given [`SessionId`] when one is
    /// provided instead of generating a fresh one.
    ///
    /// `rojo serve` uses this to keep the same session id across server restarts
    /// (recorded in the project's serve-state file) so that a connected Studio
    /// plugin can reconnect seamlessly rather than tearing down on a session-id
    /// mismatch.
    pub fn new_with_session_id<P: AsRef<Path>>(
        vfs: Vfs,
        start_path: P,
        session_id: Option<SessionId>,
    ) -> Result<Self, ServeSessionError> {
        let start_path = start_path.as_ref();
        let start_time = Instant::now();

        log::trace!("Starting new ServeSession at path {}", start_path.display());

        let root_project = Project::load_initial_project(&vfs, start_path)?;

        let mut tree = RojoTree::new(InstanceSnapshot::new());

        let root_id = tree.get_root_id();

        let instance_context =
            InstanceContext::with_emit_legacy_scripts(root_project.emit_legacy_scripts);

        log::trace!("Generating snapshot of instances from VFS");
        let snapshot = snapshot_from_vfs(&instance_context, &vfs, start_path)?;

        log::trace!("Computing initial patch set");
        let patch_set = compute_patch_set(snapshot, &tree, root_id);

        log::trace!("Applying initial patch set");
        apply_patch_set(&mut tree, patch_set);

        let session_id = session_id.unwrap_or_else(SessionId::new);
        let message_queue = MessageQueue::new();

        let tree = Arc::new(Mutex::new(tree));
        let message_queue = Arc::new(message_queue);
        let vfs = Arc::new(vfs);

        let (tree_mutation_sender, tree_mutation_receiver) = crossbeam_channel::unbounded();

        log::trace!("Starting ChangeProcessor");
        let change_processor = ChangeProcessor::start(
            Arc::clone(&tree),
            Arc::clone(&vfs),
            Arc::clone(&message_queue),
            tree_mutation_receiver,
        );

        Ok(Self {
            change_processor,
            start_time,
            session_id,
            root_project,
            tree,
            message_queue,
            tree_mutation_sender,
            vfs,
            connected_clients: Arc::new(AtomicUsize::new(0)),
            shutdown: Arc::new(Notify::new()),
        })
    }

    pub fn tree_handle(&self) -> Arc<Mutex<RojoTree>> {
        Arc::clone(&self.tree)
    }

    pub fn tree(&self) -> MutexGuard<'_, RojoTree> {
        self.tree.lock().unwrap()
    }

    pub fn tree_mutation_sender(&self) -> Sender<PatchSet> {
        self.tree_mutation_sender.clone()
    }

    #[allow(unused)]
    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn message_queue(&self) -> &MessageQueue<AppliedPatchSet> {
        &self.message_queue
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn project_name(&self) -> &str {
        self.root_project
            .name
            .as_ref()
            .expect("all top-level projects must have their name set")
    }

    pub fn project_port(&self) -> Option<u16> {
        self.root_project.serve_port
    }

    pub fn place_id(&self) -> Option<u64> {
        self.root_project.place_id
    }

    pub fn game_id(&self) -> Option<u64> {
        self.root_project.game_id
    }

    pub fn start_time(&self) -> Instant {
        self.start_time
    }

    /// How long this session has been running.
    pub fn uptime(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// The number of clients currently subscribed to the live message stream.
    pub fn connected_clients(&self) -> usize {
        self.connected_clients.load(Ordering::SeqCst)
    }

    /// Returns a guard that counts a connected client for as long as it is held.
    /// The count is decremented automatically when the guard is dropped, so every
    /// exit path of a subscription is accounted for.
    pub fn track_connected_client(&self) -> ConnectedClientGuard {
        self.connected_clients.fetch_add(1, Ordering::SeqCst);
        ConnectedClientGuard {
            counter: Arc::clone(&self.connected_clients),
        }
    }

    /// A handle that can be awaited to learn when the session has been asked to
    /// shut down. Used by the web server to trigger a graceful shutdown.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Requests that the session shut down gracefully. Wakes anything awaiting
    /// the [`shutdown_handle`](Self::shutdown_handle).
    ///
    /// Uses `notify_one`, which stores a permit if the server is not yet
    /// awaiting, so a shutdown request can never be lost to a race.
    pub fn request_shutdown(&self) {
        log::debug!(
            "Graceful shutdown requested for session {}",
            self.session_id
        );
        self.shutdown.notify_one();
    }

    pub fn serve_place_ids(&self) -> Option<&HashSet<u64>> {
        self.root_project.serve_place_ids.as_ref()
    }

    pub fn blocked_place_ids(&self) -> Option<&HashSet<u64>> {
        self.root_project.blocked_place_ids.as_ref()
    }

    pub fn serve_address(&self) -> Option<IpAddr> {
        self.root_project.serve_address
    }

    pub fn serve_allowed_hosts(&self) -> &[String] {
        &self.root_project.serve_allowed_hosts
    }

    pub fn root_dir(&self) -> &Path {
        self.root_project.folder_location()
    }

    pub fn root_project(&self) -> &Project {
        &self.root_project
    }
}

/// Counts one connected client for as long as it is held; decrements the
/// session's client counter when dropped. See [`ServeSession::track_connected_client`].
pub struct ConnectedClientGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for ConnectedClientGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Error)]
pub enum ServeSessionError {
    #[error(transparent)]
    Io {
        #[from]
        source: io::Error,
    },

    #[error(transparent)]
    Project {
        #[from]
        source: ProjectError,
    },

    #[error(transparent)]
    Other {
        #[from]
        source: anyhow::Error,
    },
}
