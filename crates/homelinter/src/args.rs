//! Command-line surface for the `homelinter` binary.
//!
//! Mirrors "Command-Line Interface Schema" in DESIGN.md: a plain scan is
//! the default and prints a review pointer; `--execute` and `--export`
//! select the non-interactive modes; `web` runs the standalone service.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

/// A validated CLI invocation.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Serve the standalone web/API process on an ephemeral loopback port.
    Web(WebOptions),
    /// Full-screen interactive terminal client on an ephemeral service.
    Tui(TuiOptions),
    /// Managed session: embed the service, drive it, then shut it down.
    Managed(ManagedOptions),
}

#[derive(Debug, Clone)]
pub struct ManagedOptions {
    pub config: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub hidden: bool,
    pub format: Option<OutputFormatArg>,
    pub target_paths: BTreeMap<String, String>,
    pub cache_clear: bool,
    pub action: ManagedAction,
    pub verbosity: i8,
}

#[derive(Debug, Clone)]
pub enum ManagedAction {
    Interactive,
    Execute,
    Export(PathBuf),
}

#[derive(Debug, Clone)]
pub struct WebOptions {
    pub config: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub hidden: bool,
    pub target_paths: BTreeMap<String, String>,
    pub cache_clear: bool,
    pub verbosity: i8,
    pub port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub config: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub hidden: bool,
    pub target_paths: BTreeMap<String, String>,
    pub cache_clear: bool,
    pub verbosity: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormatArg {
    Pretty,
    Json,
    Toml,
}

#[derive(Debug, Parser)]
#[command(
    name = "homelinter",
    version,
    about = "Find, organize, deduplicate, and archive files in your home directory."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Select the configuration file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Select the scan root for this invocation only (absolute or relative).
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,

    /// Execute all generated suggestions immediately, without review.
    #[arg(long, conflicts_with_all = ["export"])]
    execute: bool,

    /// Scan and write the executable plan script to this path.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["execute"])]
    export: Option<PathBuf>,

    /// Include hidden files and directories in primary discovery.
    #[arg(long)]
    hidden: bool,

    /// Override output.format for this invocation.
    #[arg(long, value_enum, value_name = "FORMAT")]
    format: Option<OutputFormatArg>,

    /// Override the destination format string for one Asset Kind; repeatable.
    #[arg(long = "target-path", value_name = "KIND=PATH", action = clap::ArgAction::Append)]
    target_path: Vec<String>,

    /// Clear the persistent metadata cache before scanning.
    #[arg(long)]
    cache_clear: bool,

    /// Increase structured logging verbosity (repeatable).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease structured logging verbosity (repeatable).
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Launch the standalone localhost service (SPA + API).
    Web(WebArgs),
    /// Launch the full-screen interactive terminal client.
    Tui(TuiArgs),
}

#[derive(Debug, ClapArgs)]
struct TuiArgs {
    /// Select the configuration file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Scan root for this session (defaults to the home directory).
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,
    /// Include hidden files and directories in primary discovery.
    #[arg(long)]
    hidden: bool,
    /// Override the destination format string for one Asset Kind; repeatable.
    #[arg(long = "target-path", value_name = "KIND=PATH", action = clap::ArgAction::Append)]
    target_path: Vec<String>,
    /// Clear the persistent metadata cache at startup.
    #[arg(long)]
    cache_clear: bool,
    /// Increase structured logging verbosity (repeatable).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
    /// Decrease structured logging verbosity (repeatable).
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,
}

#[derive(Debug, ClapArgs)]
struct WebArgs {
    /// Select the configuration file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Pre-seed the session's default scan root and the SPA scan dialog.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,
    /// Bind the loopback listener on this port instead of an ephemeral one.
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
    /// Pre-fill the SPA scan dialog with hidden files included.
    #[arg(long)]
    hidden: bool,
    /// Pre-fill one target-path override in the SPA scan dialog; repeatable.
    #[arg(long = "target-path", value_name = "KIND=PATH", action = clap::ArgAction::Append)]
    target_path: Vec<String>,
    /// Clear the persistent metadata cache at startup.
    #[arg(long)]
    cache_clear: bool,
    /// Increase structured logging verbosity (repeatable).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
    /// Decrease structured logging verbosity (repeatable).
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,
}

/// Parse command-line arguments into a validated `Mode`.
pub fn parse() -> Result<Mode, String> {
    let cli = Cli::parse();
    let verbosity = cli.verbose as i8 - cli.quiet as i8;
    let target_paths = parse_target_paths(&cli.target_path)?;

    if let Some(Commands::Web(web)) = &cli.command {
        if cli.config.is_some() || cli.root.is_some() || cli.hidden || cli.format.is_some() {
            return Err("--config, --root, --hidden, and --format are not accepted with `web`; pass them to the `web` subcommand instead".to_string());
        }
        return Ok(Mode::Web(WebOptions {
            config: web.config.clone(),
            root: web.root.clone(),
            hidden: web.hidden,
            target_paths: parse_target_paths(&web.target_path)?,
            cache_clear: web.cache_clear,
            verbosity: web.verbose as i8 - web.quiet as i8,
            port: web.port,
        }));
    }
    if let Some(Commands::Tui(tui)) = &cli.command {
        if cli.config.is_some() || cli.root.is_some() || cli.hidden || cli.format.is_some() {
            return Err("--config, --root, --hidden, and --format are not accepted with `tui`; pass them to the `tui` subcommand instead".to_string());
        }
        return Ok(Mode::Tui(TuiOptions {
            config: tui.config.clone(),
            root: tui.root.clone(),
            hidden: tui.hidden,
            target_paths: parse_target_paths(&tui.target_path)?,
            cache_clear: tui.cache_clear,
            verbosity: tui.verbose as i8 - tui.quiet as i8,
        }));
    }
    if cli.command.is_some() {
        return Err("internal: unhandled subcommand".to_string());
    }

    let action = if cli.execute {
        ManagedAction::Execute
    } else if let Some(path) = &cli.export {
        ManagedAction::Export(path.clone())
    } else {
        ManagedAction::Interactive
    };

    Ok(Mode::Managed(ManagedOptions {
        config: cli.config,
        root: cli.root,
        hidden: cli.hidden,
        format: cli.format,
        target_paths,
        cache_clear: cli.cache_clear,
        action,
        verbosity,
    }))
}

fn parse_target_paths(entries: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for entry in entries {
        let (kind, format) = entry
            .split_once('=')
            .ok_or_else(|| format!("--target-path expects KIND=PATH, got '{entry}'"))?;
        if map.contains_key(kind) {
            return Err(format!("asset kind '{kind}' supplied more than once"));
        }
        map.insert(kind.to_string(), format.to_string());
    }
    Ok(map)
}

impl OutputFormatArg {
    /// The model-level output format this argument selects.
    pub fn to_model(self) -> homelinter_model::config::OutputFormat {
        match self {
            OutputFormatArg::Pretty => homelinter_model::config::OutputFormat::Pretty,
            OutputFormatArg::Json => homelinter_model::config::OutputFormat::Json,
            OutputFormatArg::Toml => homelinter_model::config::OutputFormat::Toml,
        }
    }
}
