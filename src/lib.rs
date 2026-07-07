//! gnosis_vpn-toolkit library.
//!
//! All feature logic lives here so it is unit-testable (and so nix-lib's
//! `cargo test --lib` check has a target); `src/main.rs` is a thin binary that
//! parses the CLI and dispatches into these modules. Update is the first
//! feature; more will be added as sibling modules.

pub mod cli;
pub mod logging;
pub mod manifest;
pub mod output;
pub mod update;
pub mod vpn_status;
