# gnosis_vpn-toolkit

Companion binary for the [Gnosis VPN client](https://github.com/gnosis/gnosis_vpn-client).
It performs auxiliary tasks — starting with **self-update** — on behalf of
`gnosis_vpn-app`, which spawns it as a subprocess and reads its **standard
output**.

Unlike the client, the toolkit runs **no socket server**: it communicates with
`gnosis_vpn-app` purely over stdout. It does open a small read-only client
connection to the running `gnosis_vpn` daemon socket to check whether the VPN
is connected before updating (see the `--force` flag to bypass).

## Output contract

- **stdout** carries the machine-readable protocol. With `--output json` (the
  default) each line is one JSON value (newline-delimited JSON / NDJSON):
  - `update` streams `UpdateStatus` events (`Checking`, `Available`,
    `Downloading`, `Verifying`, `Installing`, `RestartingService`,
    `Completed`, `Failed`) until a terminal status.
  - `check-update` prints a single result object (`UpToDate`, `Available`,
    `NoReleaseForChannel`, `VpnNotConnected`, `IntegrityError`, `Error`).
  - `version` prints `{"version": "…"}`.
  The JSON uses serde's externally-tagged enum encoding.
- **stderr** carries human logs / diagnostics (`RUST_LOG`, default `info`), and
  the human-readable output when `--output plain` is used.
- **exit codes** follow `exitcode` conventions (`OK`, `NOPERM` for
  VPN-not-connected, `SOFTWARE` for integrity/verify failures, `UNAVAILABLE`
  otherwise). The structured reason is always in the stdout payload.

## Usage

```console
# Check for an update on the stable channel (needs the VPN connected, or --force)
gnosis_vpn-toolkit check-update --channel stable --current-version 0.91.1

# Install an update (must run as root; streams progress as NDJSON)
sudo gnosis_vpn-toolkit update --channel stable --current-version 0.91.1

# Print the toolkit's own version
gnosis_vpn-toolkit version
```

Installing an update performs privileged work (`apt-get` on Linux,
`installer(8)` on macOS) and therefore must be launched with root privileges.
`gnosis_vpn-app` is responsible for elevating (pkexec/polkit on Linux,
Authorization Services on macOS). `--current-version` is the currently-installed
client version (the app sources it from the daemon's reported package version).

## Development

This repo uses Nix. With `direnv`, `cd` into the repo to enter the dev shell;
otherwise:

```console
nix develop            # dev shell with the rust toolchain + tooling
cargo build            # or: nix build .#binary-gnosis_vpn-toolkit
cargo test
nix flake check -L     # clippy + tests + audit + licenses
```

Cross-compiled static Linux binaries:

```console
nix build .#binary-gnosis_vpn-toolkit-x86_64-linux
nix build .#binary-gnosis_vpn-toolkit-aarch64-linux
```

## License

LGPL-3.0
