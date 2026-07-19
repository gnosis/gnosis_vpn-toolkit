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
# Check for an update on the installed version's channel (needs the VPN
# connected, or --force)
gnosis_vpn-update check-update

# Install an update (must run as root; streams progress as NDJSON)
sudo gnosis_vpn-update update

# Switch channels explicitly
sudo gnosis_vpn-update update --channel stable

# Print the binary's own version
gnosis_vpn-update version
```

Installing an update performs privileged work (`installer(8)`) and therefore
must be launched with root privileges. `gnosis_vpn-app` is responsible for
elevating (Authorization Services on macOS). The currently-installed client
version is read from `/etc/gnosisvpn/version.txt`, the file the client
installer writes; if it is missing or empty both commands fail. The installed
channel is inferred from that version string (a plain dotted-numeric version is
a stable release; anything carrying build/pr/commit metadata — with `+` or its
registry-slugged `-` form — is a snapshot-line build) and is the default when
`--channel` is omitted — a snapshot install stays on snapshot, a stable install
stays on stable. Requesting the *other*
channel explicitly is always offered/installed — switching stable ⇄ snapshot
skips the newer-version gate, which only applies within the same channel.

Installer choices made at original install time (HOPR network jura/rotsee, log
level) are preserved across updates: the updater detects the installed
selection (from the `/etc/gnosisvpn/config.toml` symlink target, falling back
to the choice files under `/Library/Logs/GnosisVPN/installer/`) and pins it via
`installer -applyChoiceChangesXML`, so a CLI-driven update never flips a rotsee
install back to the package default (jura).

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
