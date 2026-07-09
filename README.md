# gnosis_vpn-toolkit

A collection of companion binaries for the
[Gnosis VPN client](https://github.com/gnosis/gnosis_vpn-client) that perform
auxiliary tasks on behalf of `gnosis_vpn-app`, which spawns them as subprocesses
and reads their **standard output**.

This is a virtual Cargo workspace: each tool is its own `gnosis_vpn-*` member
crate. The first is **`gnosis_vpn-update`** — the self-updater — documented
below. Future tools are added as sibling member crates.

Unlike the client, these tools run **no socket server**: they communicate with
`gnosis_vpn-app` purely over stdout. `gnosis_vpn-update` does open a small
read-only client connection to the running `gnosis_vpn` daemon socket to check
whether the VPN is connected before updating (see the `--force` flag to bypass).

## Output contract

- **stdout** carries the machine-readable protocol. With `--output json` (the
  default) each line is one JSON value (newline-delimited JSON / NDJSON):
  - `update` streams `UpdateStatus` events (`Checking`, `Downloading`,
    `Installing`, then a terminal `Completed` or `Failed`).
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
gnosis_vpn-update check-update --channel stable --current-version 0.91.1

# Install an update (must run as root; streams progress as NDJSON)
sudo gnosis_vpn-update update --channel stable --current-version 0.91.1

# Print the binary's own version
gnosis_vpn-update version
```

Installing an update performs privileged work (`installer(8)`) and therefore
must be launched with root privileges. `gnosis_vpn-app` is responsible for
elevating (Authorization Services on macOS). `--current-version` is the
currently-installed client version (the app sources it from the daemon's
reported package version).

## Development

The toolkit targets **macOS** (Apple Silicon); build and test on macOS. This
repo uses Nix. With `direnv`, `cd` into the repo to enter the dev shell;
otherwise:

```console
nix develop            # dev shell with the rust toolchain + tooling
cargo build            # builds the whole workspace; or: nix build .#binary-gnosis_vpn-update
cargo test             # runs the workspace test suite
nix flake check -L     # clippy + tests + audit + licenses
```

The signed release binary is built with:

```console
nix build .#binary-gnosis_vpn-update-aarch64-darwin
```

## License

LGPL-3.0
