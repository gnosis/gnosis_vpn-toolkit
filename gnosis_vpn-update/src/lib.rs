//! gnosis_vpn-update library.
//!
//! All feature logic lives here so it is unit-testable (and so nix-lib's
//! `cargo test --lib` check has a target); `src/main.rs` is a thin binary that
//! parses the CLI and dispatches into these modules. This is the first binary
//! of the toolkit; future tools are separate `gnosis_vpn-*` member crates.

pub mod cli;
pub mod logging;
pub mod manifest;
pub mod output;
pub mod update;
pub mod vpn_status;
