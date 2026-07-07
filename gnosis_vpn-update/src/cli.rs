use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::manifest::Channel;
use crate::vpn_status;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Newline-delimited JSON on stdout (default) — consumed by gnosis_vpn-app.
    #[default]
    Json,
    /// Human-readable status lines on stderr.
    Plain,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ChannelArg {
    Stable,
    Snapshot,
}

impl From<ChannelArg> for Channel {
    fn from(value: ChannelArg) -> Self {
        match value {
            ChannelArg::Stable => Channel::Stable,
            ChannelArg::Snapshot => Channel::Snapshot,
        }
    }
}

/// Gnosis VPN toolkit — companion utilities for the Gnosis VPN client.
///
/// Structured events are written to stdout (see --output); diagnostics go to
/// stderr. Designed to be spawned by gnosis_vpn-app and driven over stdout.
#[derive(Debug, Parser)]
#[command(name = "gnosis_vpn-update", version, about, long_about = None)]
pub struct Cli {
    /// Output format for events emitted on stdout
    #[arg(short = 'o', long = "output", value_enum, default_value_t = OutputFormat::Json, global = true)]
    pub output: OutputFormat,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download and install an update, streaming progress on stdout.
    ///
    /// Must be run with privileges sufficient to install system packages
    /// (root). Refuses to run unless the VPN is connected; pass --force to
    /// bypass that check.
    Update(UpdateArgs),

    /// Check whether an update is available; prints one result on stdout.
    CheckUpdate(CheckArgs),

    /// Print this toolkit's own version.
    Version,
}

#[derive(Debug, clap::Args)]
pub struct UpdateArgs {
    /// Release channel to install from
    #[arg(short = 'c', long, value_enum, default_value_t = ChannelArg::Stable)]
    pub channel: ChannelArg,

    /// Currently-installed client version (gates already-installed / downgrade)
    #[arg(long)]
    pub current_version: String,

    /// Permit installing an older release than the current one
    #[arg(long)]
    pub allow_downgrade: bool,

    /// Bypass the VPN-connected check (insecure)
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Path to the gnosis_vpn daemon socket (for the VPN-connected check)
    #[arg(short = 's', long, env = vpn_status::ENV_VAR, default_value = vpn_status::DEFAULT_SOCKET_PATH)]
    pub socket_path: PathBuf,
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Release channel to check
    #[arg(short = 'c', long, value_enum, default_value_t = ChannelArg::Stable)]
    pub channel: ChannelArg,

    /// Currently-installed client version (compared against the manifest)
    #[arg(long)]
    pub current_version: String,

    /// Bypass the VPN-connected check (insecure)
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Path to the gnosis_vpn daemon socket (for the VPN-connected check)
    #[arg(short = 's', long, env = vpn_status::ENV_VAR, default_value = vpn_status::DEFAULT_SOCKET_PATH)]
    pub socket_path: PathBuf,
}

pub fn parse() -> Cli {
    Cli::parse()
}
