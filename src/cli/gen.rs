use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::Parser;
use fs_err as fs;
use fs_err::OpenOptions;
use serde::Serialize;

use super::{output, GlobalOptions};

const SERVER_TEMPLATE: &str = include_str!("../../assets/scaffolds/server.server.luau");
const CLIENT_TEMPLATE: &str = include_str!("../../assets/scaffolds/client.client.luau");
const MODULE_TEMPLATE: &str = include_str!("../../assets/scaffolds/module.luau");

/// Scaffolds new source files into a Rojo project from templates.
#[derive(Debug, Parser)]
pub struct GenCommand {
    #[clap(subcommand)]
    pub subcommand: GenSubcommand,
}

#[derive(Debug, Parser)]
pub enum GenSubcommand {
    /// Generate a new script (server, client, or module).
    Script(GenScriptCommand),
}

impl GenCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        match self.subcommand {
            GenSubcommand::Script(command) => command.run(global),
        }
    }
}

/// Generates a new Luau script file.
#[derive(Debug, Parser)]
pub struct GenScriptCommand {
    /// Name of the script to create, without extension.
    pub name: String,

    /// The kind of script: 'server', 'client', or 'module'.
    #[clap(long, default_value = "module")]
    pub kind: ScriptKind,

    /// Directory to create the script in. Defaults to the project's `src`
    /// directory if it exists, otherwise the current directory.
    #[clap(long)]
    pub path: Option<PathBuf>,
}

/// Machine-readable result of `rojo gen`, emitted with `--json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenResult {
    created: Vec<String>,
    skipped: Vec<String>,
}

impl GenScriptCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        validate_name(&self.name)?;

        let dir = match &self.path {
            Some(path) => path.clone(),
            None => {
                let src = PathBuf::from("src");
                if src.is_dir() {
                    src
                } else {
                    PathBuf::from(".")
                }
            }
        };

        fs::create_dir_all(&dir)?;

        let (suffix, template) = self.kind.parts();
        let file_path = dir.join(format!("{}{}", self.name, suffix));
        let contents = render(template, &self.name);

        let mut created = Vec::new();
        let mut skipped = Vec::new();

        if write_if_not_exists(&file_path, &contents)? {
            created.push(file_path.display().to_string());
        } else {
            skipped.push(file_path.display().to_string());
        }

        let result = GenResult { created, skipped };

        output::emit(&global, &result, || {
            for path in &result.created {
                println!("Created {}", path);
            }
            for path in &result.skipped {
                println!("Skipped {} (already exists)", path);
            }
            Ok(())
        })
    }
}

/// The kinds of script `rojo gen script` can create.
#[derive(Debug, Clone, Copy)]
pub enum ScriptKind {
    Server,
    Client,
    Module,
}

impl ScriptKind {
    /// Returns the file-name suffix and template for this kind.
    fn parts(&self) -> (&'static str, &'static str) {
        match self {
            ScriptKind::Server => (".server.luau", SERVER_TEMPLATE),
            ScriptKind::Client => (".client.luau", CLIENT_TEMPLATE),
            ScriptKind::Module => (".luau", MODULE_TEMPLATE),
        }
    }
}

impl FromStr for ScriptKind {
    type Err = anyhow::Error;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        match source {
            "server" => Ok(ScriptKind::Server),
            "client" => Ok(ScriptKind::Client),
            "module" => Ok(ScriptKind::Module),
            _ => Err(anyhow::format_err!(
                "Invalid script kind '{}'. Valid kinds are: server, client, module",
                source
            )),
        }
    }
}

/// Validates that a script name is a single, safe filename component. The name
/// is interpolated into both a path and (for modules) Luau source, and is
/// reachable from the AI-facing MCP `gen_script` tool, so it must not be able to
/// escape the target directory or smuggle template placeholders.
fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("Script name cannot be empty.");
    }

    if name.contains(['/', '\\', ':', '{', '}']) || name.contains("..") {
        anyhow::bail!(
            "Invalid script name '{name}': names cannot contain path separators, '..', ':', or braces."
        );
    }

    if name.chars().any(char::is_control) {
        anyhow::bail!("Invalid script name '{name}': names cannot contain control characters.");
    }

    if name != name.trim() || name.starts_with('.') || name.ends_with('.') {
        anyhow::bail!(
            "Invalid script name '{name}': names cannot start or end with a dot or whitespace."
        );
    }

    Ok(())
}

/// Whether `name` is a valid Luau identifier (so it can be used as a `local`
/// binding). Names that aren't fall back to a generic identifier in the module
/// template so generated code always compiles.
fn is_valid_luau_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Render a template by substituting placeholders. `{name}` is substituted last
/// so a user-supplied name can never re-trigger another placeholder.
fn render(template: &str, name: &str) -> String {
    let identifier = if is_valid_luau_identifier(name) {
        name
    } else {
        "module"
    };

    template
        .replace("{identifier}", identifier)
        .replace("{rojo_version}", env!("CARGO_PKG_VERSION"))
        .replace("{name}", name)
}

/// Writes a file only if it does not already exist. Returns whether it was
/// created (false means it already existed and was left untouched).
fn write_if_not_exists(path: &Path, contents: &str) -> anyhow::Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err.into()),
    }
}
