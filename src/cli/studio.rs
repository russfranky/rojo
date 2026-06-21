//! The `rojo studio` subcommand group: OS-level control of the Roblox Studio
//! application on the machine running Rojo.
//!
//! Its one subcommand today, `rojo studio reset`, force-restarts Studio without
//! the native dialogs that otherwise need a human click — the "Don't Save"
//! prompt on close, the macOS "quit unexpectedly" crash dialog after a
//! force-kill, and the auto-recovery "restore" prompt on the next launch — so an
//! agent driving a serve/test loop can reboot Studio unattended.
//!
//! Studio is a native app, not something Rojo's Luau plugin can drive (the
//! recovery prompt even appears before any plugin loads), so this is done with
//! OS commands and is currently macOS only. The dialog-free recipe is:
//!
//! 1. Silence macOS's crash reporter (`com.apple.CrashReporter DialogType
//!    none`) for the duration. Force-killing Studio exits it abnormally, which
//!    otherwise raises the system "RobloxStudio quit unexpectedly" dialog; the
//!    previous setting is restored once the reboot is done.
//! 2. SIGKILL Studio — a killed process can't raise the "Don't Save" prompt.
//! 3. Delete the auto-recovery files — with nothing to restore, the restore
//!    prompt has nothing to offer. (There is no Studio setting that disables it.)
//! 4. Relaunch Studio; the Rojo plugin reconnects to the still-running
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

/// The macOS `defaults` domain controlling the system crash reporter.
const CRASH_REPORTER_DOMAIN: &str = "com.apple.CrashReporter";

/// The `defaults` key whose `none` value suppresses the "quit unexpectedly"
/// dialog. Set to `none` around the kill, then restored to its prior value.
const CRASH_REPORTER_KEY: &str = "DialogType";

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
    /// Force-restart Roblox Studio without the "Don't Save", crash, or
    /// auto-recovery dialogs, so it can be rebooted unattended during a
    /// serve/test loop.
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
/// Studio is force-killed (SIGKILL, so it can't raise a "Don't Save" prompt)
/// with macOS's crash reporter briefly silenced (so the system "quit
/// unexpectedly" dialog doesn't appear), its auto-recovery files are deleted (so
/// the next launch has nothing to offer restoring), and then it is relaunched. A
/// connected Rojo plugin reconnects to the running `rojo serve` on its own, since
/// the session id is preserved.
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

    /// Don't silence macOS's "quit unexpectedly" crash dialog during the kill.
    /// By default it is suppressed (and the prior setting restored afterward) so
    /// a force-killed Studio reboots without a system dialog to click.
    #[clap(long)]
    pub no_silence_crashes: bool,

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
    /// Whether to suppress the macOS crash dialog around the kill.
    silence_crashes: bool,
    restart_serve: bool,
}

/// The concrete steps a reset will run, computed up front so the decision logic
/// is unit-testable without touching the OS. Commands are stored as argv vectors
/// that the executor runs verbatim.
#[derive(Debug, PartialEq, Eq)]
struct ResetPlan {
    /// `defaults write` command (argv) that suppresses the macOS crash dialog,
    /// or `None` with `--no-silence-crashes`. The matching restore is computed at
    /// run time from the value read just before this runs.
    crash_suppress: Option<Vec<String>>,
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
    /// Whether the macOS crash dialog was suppressed for the kill.
    crash_dialog_suppressed: bool,
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
    let crash_suppress = if inputs.silence_crashes {
        Some(vec![
            "defaults".to_owned(),
            "write".to_owned(),
            CRASH_REPORTER_DOMAIN.to_owned(),
            CRASH_REPORTER_KEY.to_owned(),
            "none".to_owned(),
        ])
    } else {
        None
    };

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
        crash_suppress,
        kill,
        recovery_dir,
        restart_serve: inputs.restart_serve,
        launch,
    }
}

impl StudioResetCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        // This is OS automation specific to macOS (process names, `open`, the
        // recovery path, the crash-reporter default). Bail clearly elsewhere
        // rather than running commands that don't exist there. Using
        // `std::env::consts::OS` keeps it a runtime check, so the executor below
        // still compiles — and is unit-tested — on every platform in the CI
        // matrix.
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
            silence_crashes: !self.no_silence_crashes,
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
            if result.crash_dialog_suppressed {
                parts.push("silenced the macOS crash dialog".to_owned());
            }
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

/// Runs the planned steps in order: silence crash dialog → kill → clear recovery
/// → (restart serve) → relaunch → restore crash dialog. Killing and clearing are
/// best-effort (a missing Studio or recovery directory is fine); a failed
/// *relaunch* is fatal, since getting Studio back up is the point of the command.
fn execute_plan(plan: &ResetPlan, project: &Path) -> anyhow::Result<ResetResult> {
    // Silence macOS's crash reporter around the force-kill so Studio's abnormal
    // exit doesn't raise the system "quit unexpectedly" dialog. The guard
    // restores the prior setting when it drops — including on an early return
    // from a failed relaunch — so the window of suppression is just the reboot.
    let crash_guard = plan
        .crash_suppress
        .as_deref()
        .map(CrashReporterGuard::engage);
    let crash_dialog_suppressed = crash_guard.is_some();

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

    // Restore the crash-reporter setting now the kill-dialog window has passed.
    drop(crash_guard);

    Ok(ResetResult {
        killed,
        crash_dialog_suppressed,
        recovery_cleared_count,
        serve_restarted,
        relaunched,
    })
}

/// Suppresses macOS's "quit unexpectedly" dialog for the lifetime of the value,
/// restoring `com.apple.CrashReporter DialogType` to whatever it was before on
/// drop. Every step is best-effort: if `defaults` isn't cooperating we log and
/// carry on rather than abort the reboot.
struct CrashReporterGuard {
    /// The prior `DialogType` value to restore, or `None` if it was unset.
    prior: Option<String>,
}

impl CrashReporterGuard {
    /// Reads and stashes the current `DialogType`, then applies `write_argv`
    /// (the `defaults write … none` command from the plan).
    fn engage(write_argv: &[String]) -> Self {
        let prior = read_default(CRASH_REPORTER_DOMAIN, CRASH_REPORTER_KEY);
        if !run_best_effort(write_argv) {
            log::warn!(
                "Could not silence the macOS crash reporter; a \"quit unexpectedly\" \
                 dialog may appear when Studio is killed."
            );
        }
        CrashReporterGuard { prior }
    }
}

impl Drop for CrashReporterGuard {
    fn drop(&mut self) {
        let restore = match &self.prior {
            Some(value) => vec![
                "defaults".to_owned(),
                "write".to_owned(),
                CRASH_REPORTER_DOMAIN.to_owned(),
                CRASH_REPORTER_KEY.to_owned(),
                value.clone(),
            ],
            // It was unset before; remove our override so the OS default returns.
            None => vec![
                "defaults".to_owned(),
                "delete".to_owned(),
                CRASH_REPORTER_DOMAIN.to_owned(),
                CRASH_REPORTER_KEY.to_owned(),
            ],
        };
        if !run_best_effort(&restore) {
            log::warn!(
                "Could not restore the macOS crash-reporter setting ({} {}). Re-enable \
                 it with `defaults write {} {} crashreport` if needed.",
                CRASH_REPORTER_DOMAIN,
                CRASH_REPORTER_KEY,
                CRASH_REPORTER_DOMAIN,
                CRASH_REPORTER_KEY,
            );
        }
    }
}

/// Reads a `defaults` value, returning `None` if it is unset or unreadable.
fn read_default(domain: &str, key: &str) -> Option<String> {
    let output = Command::new("defaults")
        .args(["read", domain, key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Runs a command for its side effect, returning whether it exited successfully.
/// Used for the `defaults` calls, where a failure is logged but never fatal.
fn run_best_effort(argv: &[String]) -> bool {
    Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
            silence_crashes: true,
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
    fn plan_silences_crash_dialog_by_default() {
        let plan = plan_reset(&inputs(), &home());

        assert_eq!(
            plan.crash_suppress
                .as_ref()
                .map(|argv| argv.join(" "))
                .as_deref(),
            Some("defaults write com.apple.CrashReporter DialogType none")
        );
    }

    #[test]
    fn plan_no_silence_crashes_opts_out() {
        let mut i = inputs();
        i.silence_crashes = false;

        assert_eq!(plan_reset(&i, &home()).crash_suppress, None);
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
                assert!(!reset.no_silence_crashes);
                assert_eq!(reset.place, Some(PathBuf::from("game.rbxl")));
            }
        }
    }

    #[test]
    fn parses_no_silence_crashes_flag() {
        let cmd =
            StudioCommand::try_parse_from(["studio", "reset", "--no-silence-crashes"]).unwrap();

        match cmd.subcommand {
            StudioSubcommand::Reset(reset) => assert!(reset.no_silence_crashes),
        }
    }
}
