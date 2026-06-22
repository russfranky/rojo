use std::{
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    str::FromStr,
};

use anyhow::Context;
use clap::Parser;
use fs_err as fs;
use serde::Serialize;

use super::{output, resolve_path, resolve_project_root, GlobalOptions};

/// Runs a project's Luau tests with a pluggable test runner.
///
/// Rojo doesn't ship a Luau runtime, so this orchestrates an external one. Pick
/// the runner that matches where your tests need to run:
///
/// * `lune`: runs a Luau script with the standalone `lune` runtime. Fast and
///   CI-friendly and the best choice for headless logic tests — but it exposes
///   only a subset of Roblox APIs and can't boot a real `DataModel`, so it can't
///   test anything that needs the engine running.
/// * `custom`: runs an arbitrary command (given after `--`). The built place
///   path is provided to it via the `ROJO_TEST_PLACE` environment variable. Use
///   this to plug in your own boot harness.
/// * `run-in-roblox` (default, legacy): builds the project into a place and runs
///   a bootstrap script inside real Roblox Studio via the `run-in-roblox`
///   binary. NOTE: `run-in-roblox`'s only release is v0.3.0 (July 2020), so
///   driving a current Studio with it is a real compatibility gamble. For
///   tests that need a live Studio, prefer running your test entrypoint through
///   the official Roblox Studio MCP server (`run_code`/`execute_luau`) against
///   an open Studio — keep one running with `rojo studio reset`. See
///   `docs/agent-workflow.md`.
#[derive(Debug, Parser)]
pub struct TestCommand {
    /// Path to the project to test. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Which test runner to use: 'run-in-roblox', 'lune', or 'custom'.
    #[clap(long, default_value = "run-in-roblox")]
    pub runner: TestRunner,

    /// The test entry/bootstrap script (required for 'run-in-roblox' and 'lune').
    #[clap(long)]
    pub script: Option<PathBuf>,

    /// Where to build the test place. Defaults to `.rojo/test-place.rbxl` in the
    /// project. Ignored by the 'lune' runner, which doesn't use a place.
    #[clap(long)]
    pub place: Option<PathBuf>,

    /// Extra arguments passed to the runner. For the 'custom' runner, this is the
    /// command to run (the first value is the program). Provide after `--`.
    #[clap(last = true)]
    pub args: Vec<String>,
}

/// Machine-readable result of `rojo test`, emitted with `--json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TestResult {
    runner: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    stdout: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    stderr: String,
}

impl TestCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        // Resolve the script (and an explicit --place) to absolute paths so they
        // don't depend on the runner's working directory.
        let script = self
            .script
            .as_deref()
            .map(resolve_path)
            .transpose()?
            .map(|path| path.into_owned());

        // Build a place first for runners that consume one.
        let place_path = if self.runner.needs_place() {
            let path = match &self.place {
                Some(path) => resolve_path(path)?.into_owned(),
                None => resolve_project_root(&self.project)?
                    .join(".rojo")
                    .join("test-place.rbxl"),
            };
            build_place(&self.project, &path)?;
            Some(path)
        } else {
            None
        };

        let mut command =
            self.runner
                .command(script.as_deref(), place_path.as_deref(), &self.args)?;
        if let Some(place) = &place_path {
            command.env("ROJO_TEST_PLACE", place);
        }

        let runner_name = self.runner.as_str().to_owned();
        let program = command.get_program().to_string_lossy().into_owned();

        // A failing test run is an expected outcome rather than an internal
        // error, so we exit with the runner's status code instead of returning
        // an error (which `main` would print with a noisy backtrace).
        if global.json {
            let out = command.output().map_err(|err| spawn_error(&program, err))?;

            let result = TestResult {
                runner: runner_name,
                success: out.status.success(),
                exit_code: out.status.code(),
                stdout: cap(String::from_utf8_lossy(&out.stdout).into_owned()),
                stderr: cap(String::from_utf8_lossy(&out.stderr).into_owned()),
            };
            output::print_json(&result)?;

            if !out.status.success() {
                process::exit(out.status.code().unwrap_or(1));
            }
        } else {
            let status = command.status().map_err(|err| spawn_error(&program, err))?;

            if !status.success() {
                eprintln!("Tests failed.");
                process::exit(status.code().unwrap_or(1));
            }

            println!("Tests passed.");
        }

        Ok(())
    }
}

/// Maps a runner spawn failure to a helpful error, calling out a missing binary
/// (the most common failure) specifically.
fn spawn_error(program: &str, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!("Could not find `{program}` on your PATH. Is it installed?")
    } else {
        anyhow::Error::new(err).context(format!("Failed to run `{program}`"))
    }
}

/// Caps captured runner output so a chatty runner can't balloon memory or blow
/// an MCP client's token budget in `--json` mode.
fn cap(mut output: String) -> String {
    const MAX: usize = 64 * 1024;

    if output.len() > MAX {
        let mut end = MAX;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str("\n…[output truncated]");
    }

    output
}

/// Builds the project into a place file by invoking `rojo build` on this same
/// executable. Keeps our own stdout clean so `--json` output stays parseable.
fn build_place(project: &Path, place: &Path) -> anyhow::Result<()> {
    if let Some(parent) = place.parent() {
        fs::create_dir_all(parent)?;
    }

    let project = resolve_path(project)?;
    let exe = std::env::current_exe().context("Could not determine the Rojo executable path")?;

    let status = Command::new(exe)
        .arg("build")
        .arg(project.as_ref())
        .arg("--output")
        .arg(place)
        .stdout(Stdio::null())
        .status()
        .context("Failed to build the test place")?;

    if !status.success() {
        anyhow::bail!("Building the test place failed");
    }

    Ok(())
}

/// The supported test runners.
#[derive(Debug, Clone, Copy)]
pub enum TestRunner {
    RunInRoblox,
    Lune,
    Custom,
}

impl TestRunner {
    fn as_str(&self) -> &'static str {
        match self {
            TestRunner::RunInRoblox => "run-in-roblox",
            TestRunner::Lune => "lune",
            TestRunner::Custom => "custom",
        }
    }

    /// Whether this runner needs a built place file.
    fn needs_place(&self) -> bool {
        matches!(self, TestRunner::RunInRoblox | TestRunner::Custom)
    }

    /// Builds the command to invoke this runner.
    fn command(
        &self,
        script: Option<&Path>,
        place: Option<&Path>,
        args: &[String],
    ) -> anyhow::Result<Command> {
        match self {
            TestRunner::RunInRoblox => {
                let script = script.context(
                    "The 'run-in-roblox' runner requires a bootstrap script; pass --script <PATH>.",
                )?;
                let place = place.context("The 'run-in-roblox' runner requires a built place.")?;
                let mut command = Command::new("run-in-roblox");
                command
                    .arg("--place")
                    .arg(place)
                    .arg("--script")
                    .arg(script)
                    .args(args);
                Ok(command)
            }
            TestRunner::Lune => {
                let script = script
                    .context("The 'lune' runner requires a script to run; pass --script <PATH>.")?;
                let mut command = Command::new("lune");
                command.arg("run").arg(script).args(args);
                Ok(command)
            }
            TestRunner::Custom => {
                let (program, rest) = args.split_first().context(
                    "The 'custom' runner requires a command; provide it after `--`, \
                     e.g. `rojo test --runner custom -- my-test-tool`.",
                )?;
                let mut command = Command::new(program);
                command.args(rest);
                Ok(command)
            }
        }
    }
}

impl FromStr for TestRunner {
    type Err = anyhow::Error;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        match source {
            "run-in-roblox" => Ok(TestRunner::RunInRoblox),
            "lune" => Ok(TestRunner::Lune),
            "custom" => Ok(TestRunner::Custom),
            _ => Err(anyhow::format_err!(
                "Invalid test runner '{}'. Valid runners are: run-in-roblox, lune, custom",
                source
            )),
        }
    }
}
