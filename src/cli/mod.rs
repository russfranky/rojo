//! Defines Rojo's CLI through clap types.

mod build;
mod doc;
mod fmt_project;
mod gen;
mod init;
#[cfg(feature = "mcp")]
mod mcp;
mod output;
mod plugin;
mod restart;
mod serve;
mod serve_control;
mod sourcemap;
mod status;
mod stop;
mod studio;
mod syncback;
mod test;
mod upload;

use std::{borrow::Cow, env, path::Path, str::FromStr};

use anyhow::Context;
use clap::Parser;
use thiserror::Error;

pub use self::build::BuildCommand;
pub use self::doc::DocCommand;
pub use self::fmt_project::FmtProjectCommand;
pub use self::gen::GenCommand;
pub use self::init::{InitCommand, InitKind};
#[cfg(feature = "mcp")]
pub use self::mcp::McpCommand;
pub use self::plugin::{PluginCommand, PluginSubcommand};
pub use self::restart::RestartCommand;
pub use self::serve::ServeCommand;
pub use self::sourcemap::SourcemapCommand;
pub use self::status::StatusCommand;
pub use self::stop::StopCommand;
pub use self::studio::{StudioCommand, StudioSubcommand};
pub use self::syncback::SyncbackCommand;
pub use self::test::TestCommand;
pub use self::upload::UploadCommand;

/// Command line options that Rojo accepts, defined using the clap crate.
#[derive(Debug, Parser)]
#[clap(name = "Rojo", version, about)]
pub struct Options {
    #[clap(flatten)]
    pub global: GlobalOptions,

    /// Subcommand to run in this invocation.
    #[clap(subcommand)]
    pub subcommand: Subcommand,
}

impl Options {
    pub fn run(self) -> anyhow::Result<()> {
        match self.subcommand {
            Subcommand::Init(subcommand) => subcommand.run(),
            Subcommand::Serve(subcommand) => subcommand.run(self.global),
            Subcommand::Build(subcommand) => subcommand.run(self.global),
            Subcommand::Upload(subcommand) => subcommand.run(),
            Subcommand::Sourcemap(subcommand) => subcommand.run(self.global),
            Subcommand::FmtProject(subcommand) => subcommand.run(),
            Subcommand::Doc(subcommand) => subcommand.run(),
            Subcommand::Plugin(subcommand) => subcommand.run(),
            Subcommand::Syncback(subcommand) => subcommand.run(self.global),
            Subcommand::Status(subcommand) => subcommand.run(self.global),
            Subcommand::Stop(subcommand) => subcommand.run(self.global),
            Subcommand::Studio(subcommand) => subcommand.run(self.global),
            Subcommand::Restart(subcommand) => subcommand.run(self.global),
            Subcommand::Test(subcommand) => subcommand.run(self.global),
            Subcommand::Gen(subcommand) => subcommand.run(self.global),
            #[cfg(feature = "mcp")]
            Subcommand::Mcp(subcommand) => subcommand.run(),
        }
    }
}

#[derive(Debug, Parser)]
pub struct GlobalOptions {
    /// Sets verbosity level. Can be specified multiple times.
    #[clap(long("verbose"), short, global(true), parse(from_occurrences))]
    pub verbosity: u8,

    /// Set color behavior. Valid values are auto, always, and never.
    #[clap(long("color"), global(true), default_value("auto"))]
    pub color: ColorChoice,

    /// Emit machine-readable JSON on stdout instead of human-readable text.
    ///
    /// When set, progress and diagnostic messages are routed to stderr so that
    /// stdout contains only the command's JSON result. Useful for scripting and
    /// AI tooling.
    #[clap(long, global(true))]
    pub json: bool,

    /// Don't run any project lifecycle hooks (preBuild/postBuild/serve) for this
    /// invocation. Hooks are arbitrary commands defined in the project file; use
    /// this when working with a project you don't fully trust.
    #[clap(long, global(true))]
    pub no_hooks: bool,
}

impl Default for GlobalOptions {
    fn default() -> Self {
        GlobalOptions {
            verbosity: 0,
            color: ColorChoice::Auto,
            json: false,
            no_hooks: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl FromStr for ColorChoice {
    type Err = ColorChoiceParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        match source {
            "auto" => Ok(ColorChoice::Auto),
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            _ => Err(ColorChoiceParseError {
                attempted: source.to_owned(),
            }),
        }
    }
}

impl From<ColorChoice> for termcolor::ColorChoice {
    fn from(value: ColorChoice) -> Self {
        match value {
            ColorChoice::Auto => termcolor::ColorChoice::Auto,
            ColorChoice::Always => termcolor::ColorChoice::Always,
            ColorChoice::Never => termcolor::ColorChoice::Never,
        }
    }
}

impl From<ColorChoice> for env_logger::WriteStyle {
    fn from(value: ColorChoice) -> Self {
        match value {
            ColorChoice::Auto => env_logger::WriteStyle::Auto,
            ColorChoice::Always => env_logger::WriteStyle::Always,
            ColorChoice::Never => env_logger::WriteStyle::Never,
        }
    }
}

#[derive(Debug, Error)]
#[error("Invalid color choice '{attempted}'. Valid values are: auto, always, never")]
pub struct ColorChoiceParseError {
    attempted: String,
}

#[derive(Debug, Parser)]
pub enum Subcommand {
    Init(InitCommand),
    Serve(ServeCommand),
    Build(BuildCommand),
    Upload(UploadCommand),
    Sourcemap(SourcemapCommand),
    FmtProject(FmtProjectCommand),
    Doc(DocCommand),
    Plugin(PluginCommand),
    Syncback(SyncbackCommand),
    Status(StatusCommand),
    Stop(StopCommand),
    Studio(StudioCommand),
    Restart(RestartCommand),
    Test(TestCommand),
    Gen(GenCommand),
    #[cfg(feature = "mcp")]
    Mcp(McpCommand),
}

pub(super) fn resolve_path(path: &Path) -> anyhow::Result<Cow<'_, Path>> {
    if path.is_absolute() {
        Ok(Cow::Borrowed(path))
    } else {
        let current_dir = env::current_dir().context(
            "Could not determine the current working directory. \
             It may have been deleted, or Rojo may not have permission to access it.",
        )?;
        Ok(Cow::Owned(current_dir.join(path)))
    }
}

/// Resolves the project root directory (where Rojo keeps its `.rojo/` state) for
/// a project path argument. A path to a `.project.json` file resolves to its
/// parent directory; a directory resolves to itself. This mirrors how
/// `rojo serve` derives its root, so `status`/`stop`/`restart` find the same
/// state file.
pub(super) fn resolve_project_root(path: &Path) -> anyhow::Result<std::path::PathBuf> {
    let path = resolve_path(path)?;

    let dir = if path.is_file() {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };

    // Canonicalize so the serve-state location matches what `rojo serve` wrote,
    // regardless of symlinks, `.`/`..`, or trailing slashes.
    Ok(crate::state_file::canonical_dir(&dir))
}
