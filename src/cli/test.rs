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
/// Rojo doesn't ship a Luau runtime, so this orchestrates an external runner:
///
/// * `run-in-roblox` (default): builds the project into a place and runs a
///   bootstrap script inside real Roblox Studio. Highest fidelity; needs Studio
///   and the `run-in-roblox` binary.
/// * `lune`: runs a Luau script with the standalone `lune` runtime. Fast and
///   CI-friendly, but only a subset of Roblox APIs are available.
/// * `custom`: runs an arbitrary command (given after `--`). The built place
///   path is provided to it via the `ROJO_TEST_PLACE` environment variable.
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
        // Build a place first for runners that consume one.
        let place_path = if self.runner.needs_place() {
            let path = match &self.place {
                Some(path) => path.clone(),
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
                .command(self.script.as_deref(), place_path.as_deref(), &self.args)?;
        if let Some(place) = &place_path {
            command.env("ROJO_TEST_PLACE", place);
        }

        let runner_name = self.runner.as_str().to_owned();

        // A failing test run is an expected outcome rather than an internal
        // error, so we exit with the runner's status code instead of returning
        // an error (which `main` would print with a noisy backtrace).
        if global.json {
            let out = command
                .output()
                .with_context(|| format!("Failed to run the '{runner_name}' test runner"))?;

            let result = TestResult {
                runner: runner_name,
                success: out.status.success(),
                exit_code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            };
            output::print_json(&result)?;

            if !out.status.success() {
                process::exit(out.status.code().unwrap_or(1));
            }
        } else {
            let status = command
                .status()
                .with_context(|| format!("Failed to run the '{runner_name}' test runner"))?;

            if !status.success() {
                eprintln!("Tests failed.");
                process::exit(status.code().unwrap_or(1));
            }

            println!("Tests passed.");
        }

        Ok(())
    }
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
