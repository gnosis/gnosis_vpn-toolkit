use clap::{Parser, Subcommand, ValueEnum};

pub use gnosis_vpn_toolkit_common::output::OutputFormat;

use crate::manifest::Channel;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ChannelArg {
    Stable,
    Snapshot,
    Experimental,
}

impl From<ChannelArg> for Channel {
    fn from(value: ChannelArg) -> Self {
        match value {
            ChannelArg::Stable => Channel::Stable,
            ChannelArg::Snapshot => Channel::Snapshot,
            ChannelArg::Experimental => Channel::Experimental,
        }
    }
}

/// Gnosis VPN toolkit — companion utilities for the Gnosis VPN client.
/// stdout carries the result (NDJSON with `--output json`); stderr is logs.
#[derive(Debug, Parser)]
#[command(name = "gnosis_vpn-update", version, about, long_about = None)]
pub struct Cli {
    /// Output format for everything written to stdout. Defaults to `plain`;
    /// machine consumers pass `json`.
    #[arg(short = 'o', long = "output", value_enum, global = true)]
    pub output: Option<OutputFormat>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download and install an update, streaming progress on stdout.
    ///
    /// Must be run with privileges sufficient to install system packages
    /// (root). The caller decides whether the VPN must be connected first.
    Update(UpdateArgs),

    /// Check whether an update is available; prints one result on stdout.
    CheckUpdate(CheckArgs),

    /// Print this toolkit's version and the installed package's. Human-readable
    /// unless `--output json` is passed.
    Version,
}

#[derive(Debug, clap::Args)]
pub struct UpdateArgs {
    /// Release channel to install from; defaults to the channel of the
    /// currently installed version
    #[arg(short = 'c', long, value_enum)]
    pub channel: Option<ChannelArg>,

    /// Permit installing an older release than the current one
    #[arg(long)]
    pub allow_downgrade: bool,

    /// Ignored: the caller decides about the VPN, older apps still pass it.
    #[arg(short = 'f', long, hide = true)]
    pub force: bool,
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Release channel to check; defaults to the channel of the currently
    /// installed version
    #[arg(short = 'c', long, value_enum)]
    pub channel: Option<ChannelArg>,

    /// Ignored: the caller decides about the VPN, older apps still pass it.
    #[arg(short = 'f', long, hide = true)]
    pub force: bool,
}

pub fn parse() -> Cli {
    Cli::parse()
}
