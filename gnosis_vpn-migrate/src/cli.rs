use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub use gnosis_vpn_toolkit_common::output::OutputFormat;

use crate::layout;
use crate::upload;

/// Moves the legacy Gnosis VPN identity into the installing user's layout (root). stdout is the result, stderr is logs.
#[derive(Debug, Parser)]
#[command(name = "gnosis_vpn-migrate", version, about, long_about = None)]
pub struct Cli {
    /// Output format for stdout; machine consumers pass `json`
    #[arg(short = 'o', long = "output", value_enum, global = true)]
    pub output: Option<OutputFormat>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Move the legacy identity into the user's per-network layout (root only).
    Identity(IdentityArgs),

    /// Move the legacy identity aside, and with --user that user's per-network dirs, for a fresh identity (root only).
    StartClean(StartCleanArgs),

    /// Upload the migration log (plus installer and updater logs where present); prints the reference to quote (root only).
    UploadLog(UploadLogArgs),

    /// Print this tool's version and the installed package's.
    Version,
}

#[derive(Debug, clap::Args)]
pub struct IdentityArgs {
    /// Account the client will run as; receives the identity
    #[arg(long)]
    pub user: String,

    /// Legacy data dir holding `.config/gnosisvpn-hopr.*`
    #[arg(long, default_value_os_t = layout::legacy_home())]
    pub legacy_home: PathBuf,

    /// Symlink naming the chosen network's config
    #[arg(long, default_value_os_t = layout::config_link())]
    pub config_link: PathBuf,

    /// Print what would be moved without touching anything
    #[arg(long)]
    pub dry_run: bool,

    /// On a conflict, move the user's current identity aside and migrate the legacy one
    #[arg(long)]
    pub replace: bool,
}

#[derive(Debug, clap::Args)]
pub struct StartCleanArgs {
    /// Account whose per-network dirs are moved aside as well
    #[arg(long)]
    pub user: Option<String>,

    /// Legacy data dir holding `.config/gnosisvpn-hopr.*`
    #[arg(long, default_value_os_t = layout::legacy_home())]
    pub legacy_home: PathBuf,

    /// Symlink naming the chosen network's config
    #[arg(long, default_value_os_t = layout::config_link())]
    pub config_link: PathBuf,
}

#[derive(Debug, clap::Args)]
pub struct UploadLogArgs {
    /// What went wrong, in your words (first 2000 characters are kept)
    #[arg(long)]
    pub description: Option<String>,

    /// Endpoint log bundles are POSTed to
    #[arg(long, env = upload::API_URL_ENV, default_value = upload::DEFAULT_API_URL)]
    pub api_url: String,
}
