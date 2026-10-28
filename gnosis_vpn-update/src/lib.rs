//! gnosis_vpn-update library: all feature logic, unit-testable; `src/main.rs` only dispatches into it.

pub mod cli;
pub mod manifest;
#[cfg(test)]
mod test_server;
pub mod update;
pub mod vpn_status;
