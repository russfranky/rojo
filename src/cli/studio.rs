//! The `rojo studio` subcommand group: OS-level control of the Roblox Studio
//! application on the machine running Rojo.
//!
//! Its one subcommand today, `rojo studio reset`, force-restarts Studio without
//! the two native dialogs that otherwise need a human click — the "Don't Save"
//! prompt on close and the auto-recovery "restore" prompt on the next launch —
//! so an agent driving a serve/test loop can reboot Studio unattended.
//!
//! Studio is a native app, not something Rojo's Luau plugin can drive (the
//! recovery prompt even appears before any plugin loads), so this is done with
//! OS commands and is currently macOS only. The dialog-free recipe is:
//!
//! 1. SIGKILL Studio — a killed process can't raise the "Don't Save" prompt.
//! 2. Delete the auto-recovery files — with nothing to restore, the restore
//!    prompt has nothing to offer. (There is no Studio setting that disables it.)
//! 3. Relaunch Studio; the Rojo plugin reconnects to the still-running
//!    `rojo serve` on its own, since the session id is preserved across the
//!    reboot.

use std::{
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Parser;
use serde::Serialize;

use super::{output, resolve_path, GlobalOptions};

/// The macOS process name matched when force-killing Studio. `pkill -f` matches
/// it as a substring of the full command line, so this also catches
/// `RobloxStudioBeta`.
const STUDIO_PROCESS_MATCH: &str = "RobloxStudio";

/// The macOS application name passed to `open -a` to (re)launch Studio. Launch
/// Services resolves it to `RobloxStudio.app` without needing its full path.
const STUDIO_APP_NAME: &str = "RobloxStudio";

/// How long to wait (polling) for Studio to actually exit after the kill signal
/// before clearing recovery files and relaunching.
const KILL_WAIT: Duration = Duration::from_secs(5);

/// Controls the Roblox Studio application (currently macOS only).
#[derive(Debug, Parser)]
pub struct StudioCommand {
    #[clap(subcommand)]
    subcommand: StudioSubcommand,
}

/// Subcommands of `rojo studio`.
#[derive(Debug, Parser)]
pub enum StudioSubcommand {
    /// Force-restart Roblox Studio without the "Don't Save" or auto-recovery
    /// dialogs, so it can be rebooted unattended during a serve/test loop.
    Reset(StudioResetCommand),
}

impl StudioCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        self.subcommand.run(global)
    }
}

impl StudioSubcommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        match self {
            StudioSubcommand::Reset(cmd) => cmd.run(global),
        }
    }
}

/// Force-restarts Roblox Studio with no native dialogs (macOS only).
///
/// Studio is force-killed (SIGKILL, so it can't raise a "Don't Save" prompt),
/// its auto-recovery files are deleted (so the next launch has nothing to offer
/// restoring), and then it is relaunched. A connected Rojo plugin reconnects to
/// the running `rojo serve` on its own, since the session id is preserved.
#[derive(Debug, Parser)]
pub struct StudioResetCommand {
    /// Path to the project, used only with `--restart-serve`. Defaults to the
    /// current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Open this place file in Studio after relaunching. Without it, Studio is
    /// launched to its start screen.
    #[clap(long)]
    pub place: Option<PathBuf>,

    /// Kill Studio (and clear recovery) but don't relaunch it.
    #[clap(long)]
    pub no_launch: bool,

    /// Don't delete auto-recovery files. The "restore" prompt may then appear on
    /// the next launch, but Studio's recovery backups are left intact.
    #[clap(long)]
    pub no_clear_recovery: bool,

    /// Override the auto-recovery directory to clear. Defaults to Studio's macOS
    /// AutoSaves folder under `~/Library/Application Support`.
    #[clap(long)]
    pub recovery_path: Option<PathBuf>,

    /// Also restart the project's `rojo serve` (best-effort) for a full reset.
    /// By default serve is left running and the plugin simply reconnects.
    #[clap(long)]
    pub restart_serve: bool,
}

/// Resolved inputs for [`plan_reset`], decoupled from clap and the current
/// directory so the planner is a pure, testable function.
struct ResetInputs {
    /// Absolute path to the place to open, if any.
    place: Option<PathBuf>,
    no_launch: bool,
    no_clear_recovery: bool,
    /// Explicit recovery-directory override (absolute), if any.
    recovery_path: Option<PathBuf>,
    restart_serve: bool,
}

/// The concrete steps a reset will run, computed up front so the decision logic
/// is unit-testable without touching the OS. Commands are stored as argv vectors
/// that the executor runs verbatim.
#[derive(Debug, PartialEq, Eq)]
struct ResetPlan {
    /// Force-kill command (argv). SIGKILL so Studio can't prompt to save.
    kill: Vec<String>,
    /// Auto-recovery directory to clear, or `None` with `--no-clear-recovery`.
    recovery_dir: Option<PathBuf>,
    /// Whether to restart the project's serve before relaunching.
    restart_serve: bool,
    /// Relaunch command (argv), or `None` with `--no-launch`.
    launch: Option<Vec<String>>,
}

/// Machine-readable result of `rojo studio reset`, emitted with `--json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResetResult {
    /// Whether a running Studio process was found and killed.
    killed: bool,
    /// Number of auto-recovery entries deleted.
    recovery_cleared_count: usize,
    /// Whether `rojo serve` was restarted (only with `--restart-serve`).
    serve_restarted: bool,
    /// Whether Studio was relaunched.
    relaunched: bool,
}

/// Studio's default macOS auto-recovery directory. There is no Studio setting to
/// disable the restore prompt, so deleting the files here is what suppresses it.
fn default_recovery_dir(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Roblox")
        .join("RobloxStudio")
        .join("AutoSaves")
}

/// Builds the [`ResetPlan`] from resolved inputs. Pure: no filesystem or process
/// access, so it can be exercised on any platform in unit tests.
fn plan_reset(inputs: &ResetInputs, home: &Path) -> ResetPlan {
    let kill = vec![
        "pkill".to_owned(),
        "-9".to_owned(),
        "-f".to_owned(),
        STUDIO_PROCESS_MATCH.to_owned(),
    ];

    let recovery_dir = if inputs.no_clear_recovery {
        None
    } else {
        Some(
            inputs
                .recovery_path
                .clone()
                .unwrap_or_else(|| default_recovery_dir(home)),
        )
    };

    let launch = if inputs.no_launch {
        None
    } else {
        let mut argv = vec![
            "open".to_owned(),
            "-a".to_owned(),
            STUDIO_APP_NAME.to_owned(),
        ];
        if let Some(place) = &inputs.place {
            argv.push(place.to_string_lossy().into_owned());
        }
        Some(argv)
    };

    ResetPlan {
        kill,
        recovery_dir,
        restart_serve: inputs.restart_serve,
        launch,
    }
}

impl StudioResetCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        // This is OS automation specific to macOS (process names, `open`, the
        // recovery path). Bail clearly elsewhere rather than running commands
        // that don't exist there. Using `std::env::consts::OS` keeps it a runtime
        // check, so the executor below still compiles — and is unit-tested — on
        // every platform in the CI matrix.
        if std::env::consts::OS != "macos" {
            anyhow::bail!(
                "`rojo studio reset` is currently only supported on macOS (detected: {}).",
                std::env::consts::OS
            );
        }

        let place = match self.place {
            Some(place) => {
                let resolved = resolve_path(&place)?.into_owned();
                if !resolved.exists() {
                    anyhow::bail!("Place file not found: {}", resolved.display());
                }
                Some(resolved)
            }
            None => None,
        };

        let recovery_path = match self.recovery_path {
            Some(path) => Some(resolve_path(&path)?.into_owned()),
            None => None,
        };

        let inputs = ResetInputs {
            place,
            no_launch: self.no_launch,
            no_clear_recovery: self.no_clear_recovery,
            recovery_path,
            restart_serve: self.restart_serve,
        };

        let home = home_dir()?;
        let plan = plan_reset(&inputs, &home);

        let result = execute_plan(&plan, &self.project)?;

        output::emit(&global, &result, || {
            let mut parts = vec![if result.killed {
                "killed running Studio".to_owned()
            } else {
                "no running Studio found".to_owned()
            }];
            if plan.recovery_dir.is_some() {
                parts.push(format!(
                    "cleared {} recovery file(s)",
                    result.recovery_cleared_count
                ));
            }
            if result.serve_restarted {
                parts.push("restarted serve".to_owned());
            }
            parts.push(if result.relaunched {
                "relaunched Studio".to_owned()
            } else {
                "did not relaunch".to_owned()
            });
            println!("Studio reset: {}.", parts.join("; "));
            Ok(())
        })
    }
}

/// Runs the planned steps in order: kill → clear recovery → (restart serve) →
/// relaunch. Killing and clearing are best-effort (a missing Studio or recovery
/// directory is fine); a failed *relaunch* is fatal, since getting Studio back up
/// is the point of the command.
fn execute_plan(plan: &ResetPlan, project: &Path) -> anyhow::Result<ResetResult> {
    let killed = run_kill(&plan.kill)?;

    // Give a killed Studio a moment to exit and release its files before we
    // delete the recovery data and relaunch.
    if killed {
        wait_for_exit(KILL_WAIT);
    }

    let recovery_cleared_count = match &plan.recovery_dir {
        Some(dir) => clear_recovery(dir),
        None => 0,
    };

    let serve_restarted = if plan.restart_serve {
        restart_serve(project)
    } else {
        false
    };

    let relaunched = match &plan.launch {
        Some(argv) => {
            run_launch(argv)?;
            true
        }
        None => false,
    };

    Ok(ResetResult {
        killed,
        recovery_cleared_count,
        serve_restarted,
        relaunched,
    })
}

/// Force-kills Studio. Returns whether at least one process was signaled; "no
/// matching process" (Studio wasn't running) is not an error.
fn run_kill(argv: &[String]) -> anyhow::Result<bool> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("Failed to run `{}` to stop Studio", argv.join(" ")))?;

    // pkill exits 0 when it signaled a process, 1 when none matched, >1 on error.
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!("`{}` failed with {}", argv.join(" "), status),
    }
}

/// Polls until no Studio process remains, up to `timeout`. SIGKILL is immediate,
/// but the OS needs a moment to reap the process and release its file handles
/// before we clear recovery data and relaunch.
fn wait_for_exit(timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !studio_running() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Whether any Studio process is currently running.
fn studio_running() -> bool {
    Command::new("pgrep")
        .args(["-f", STUDIO_PROCESS_MATCH])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Deletes the contents of the auto-recovery directory, returning how many
/// top-level entries were removed. Best-effort: a missing directory is zero, and
/// an entry that can't be deleted is logged and skipped rather than aborting the
/// whole reset.
fn clear_recovery(dir: &Path) -> usize {
    let entries = match fs_err::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Could not read recovery directory {}: {err}", dir.display());
            }
            return 0;
        }
    };

    let mut count = 0;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let result = if path.is_dir() {
            fs_err::remove_dir_all(&path)
        } else {
            fs_err::remove_file(&path)
        };
        match result {
            Ok(()) => count += 1,
            Err(err) => log::warn!("Could not delete recovery file {}: {err}", path.display()),
        }
    }
    count
}

/// Best-effort `rojo restart` for the project so the serve↔plugin connection is
/// fully reset (not just Studio). Failures (e.g. no server running) are logged
/// and reported as `false` rather than aborting the reset, since the plugin
/// reconnects to a still-running serve on its own.
fn restart_serve(project: &Path) -> bool {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            log::warn!("Could not locate the Rojo executable to restart serve: {err}");
            return false;
        }
    };

    let mut command = Command::new(exe);
    command.arg("restart").arg("--json");
    if !project.as_os_str().is_empty() {
        command.arg(project);
    }

    match command.output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!("`rojo restart` did not succeed: {}", stderr.trim());
            false
        }
        Err(err) => {
            log::warn!("Failed to run `rojo restart`: {err}");
            false
        }
    }
}

/// Relaunches Studio via `open`. `open` returns as soon as the launch is handed
/// to Launch Services, so Studio runs independently of this process. A failure
/// here is fatal: getting Studio back up is the purpose of the reset.
fn run_launch(argv: &[String]) -> anyhow::Result<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("Failed to run `{}` to launch Studio", argv.join(" ")))?;

    if !status.success() {
        anyhow::bail!(
            "`{}` failed with {} — is Roblox Studio installed?",
            argv.join(" "),
            status
        );
    }
    Ok(())
}

/// The user's home directory from `$HOME`. Reliable on macOS, where this command
/// runs.
fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .context("Could not determine the home directory: $HOME is not set.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> ResetInputs {
        ResetInputs {
            place: None,
            no_launch: false,
            no_clear_recovery: false,
            recovery_path: None,
            restart_serve: false,
        }
    }

    fn home() -> PathBuf {
        PathBuf::from("/Users/test")
    }

    #[test]
    fn plan_defaults_kill_clear_launch() {
        let plan = plan_reset(&inputs(), &home());

        assert_eq!(plan.kill.join(" "), "pkill -9 -f RobloxStudio");
        assert_eq!(
            plan.recovery_dir,
            Some(PathBuf::from(
                "/Users/test/Library/Application Support/Roblox/RobloxStudio/AutoSaves"
            ))
        );
        assert_eq!(
            plan.launch.as_ref().map(|argv| argv.join(" ")).as_deref(),
            Some("open -a RobloxStudio")
        );
        assert!(!plan.restart_serve);
    }

    #[test]
    fn plan_with_place_opens_it() {
        let mut i = inputs();
        i.place = Some(PathBuf::from("/tmp/game.rbxl"));

        let plan = plan_reset(&i, &home());

        assert_eq!(
            plan.launch.as_ref().map(|argv| argv.join(" ")).as_deref(),
            Some("open -a RobloxStudio /tmp/game.rbxl")
        );
    }

    #[test]
    fn plan_no_launch_skips_launch_but_still_kills_and_clears() {
        let mut i = inputs();
        i.no_launch = true;

        let plan = plan_reset(&i, &home());

        assert_eq!(plan.launch, None);
        assert!(!plan.kill.is_empty());
        assert!(plan.recovery_dir.is_some());
    }

    #[test]
    fn plan_no_clear_recovery_skips_recovery() {
        let mut i = inputs();
        i.no_clear_recovery = true;

        assert_eq!(plan_reset(&i, &home()).recovery_dir, None);
    }

    #[test]
    fn plan_recovery_path_override_wins() {
        let mut i = inputs();
        i.recovery_path = Some(PathBuf::from("/custom/recovery"));

        assert_eq!(
            plan_reset(&i, &home()).recovery_dir,
            Some(PathBuf::from("/custom/recovery"))
        );
    }

    #[test]
    fn plan_restart_serve_flag_propagates() {
        let mut i = inputs();
        i.restart_serve = true;

        assert!(plan_reset(&i, &home()).restart_serve);
    }

    #[test]
    fn parses_reset_subcommand() {
        let cmd = StudioCommand::try_parse_from([
            "studio",
            "reset",
            "--no-launch",
            "--place",
            "game.rbxl",
            "--restart-serve",
        ])
        .unwrap();

        match cmd.subcommand {
            StudioSubcommand::Reset(reset) => {
                assert!(reset.no_launch);
                assert!(reset.restart_serve);
                assert_eq!(reset.place, Some(PathBuf::from("game.rbxl")));
            }
        }
    }
}
