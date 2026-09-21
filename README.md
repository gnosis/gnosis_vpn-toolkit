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

- **stdout** carries the machine-readable protocol. With `--output json` — the
  default for `update`, and opt-in for `version` and `check-update` — each
  line is one JSON value
  (newline-delimited JSON / NDJSON):
  - `update` streams `UpdateStatus` events (`Checking`, `Downloading`,
    `Installing`, then a terminal `Completed` or `Failed`).
  - `check-update` prints a single result object
    `{"channel": …, "outcome": …, "manifest": …}`. `outcome` is the gated
    decision (`UpToDate`, `Available`, `NoReleaseForChannel`,
    `VpnNotConnected`, `IntegrityError`, `Error`) and `channel` is the one that
    was checked — the `--channel` value, or the channel inferred from the
    installed version. `manifest` is the update manifest exactly as fetched,
    carrying **every** channel entry — `channels.stable`,
    `channels.snapshot` and `channels.experimental` — so one
    invocation yields the whole release picture as well as the decision; it is
    omitted on the three outcomes that never got a manifest
    (`VpnNotConnected`, `IntegrityError`, `Error`), where a consumer should
    keep its last known one.
  - `version` prints `{"version": "…", "package_version": "…"}`, where
    `package_version` is the installed client version from
    `/etc/gnosisvpn/version.txt` (`null` when the client is not installed).
    The JSON uses serde's externally-tagged enum encoding.
- **stderr** carries human logs / diagnostics (`RUST_LOG`, default `info`), and
  the human-readable output when `--output plain` is used. `version` and
  `check-update` are the exceptions: both default to plain and write those
  lines to stdout, because there the result *is* the output rather than a
  diagnostic. `check-update` prints the chosen channel's changelog only when
  the release carries one (today only stable does).

  ```console
  $ gnosis_vpn-update version
  Updater version: 0.4.0
  Package version: 2026.06.06+build.000005

  $ gnosis_vpn-update check-update
  Update needed to 2026.09.20+build.144124.experimental
  Current installed version: 2026.06.06+build.000005
  Channel: Experimental
  Changelog: …
  ```

- **exit codes** follow `exitcode` conventions (`OK`, `NOPERM` for
  VPN-not-connected, `SOFTWARE` for integrity/verify failures, `UNAVAILABLE`
  otherwise). The structured reason is always in the stdout payload.

## Usage

```console
# Check for an update on the installed version's channel (needs the VPN
# connected, or --force)
gnosis_vpn-update check-update

# Install an update (macOS only; must run as root; streams progress as NDJSON)
sudo gnosis_vpn-update update

# Switch channels explicitly
sudo gnosis_vpn-update update --channel stable

# Print this binary's version and the installed client's
gnosis_vpn-update version
```

Installing an update performs privileged work (`installer(8)`) and therefore
must be launched with root privileges. `gnosis_vpn-app` is responsible for
elevating (Authorization Services on macOS). The currently-installed client
version is read from `/etc/gnosisvpn/version.txt`, the file the client
installer writes; if it is missing or empty `update` and `check-update` fail
(`version` reports `package_version: null` and still exits 0). The installed
channel is inferred from that version string (a plain dotted-numeric version is
a stable release; one carrying an `experimental` segment is an experimental
build; anything else carrying build/pr/commit metadata — with `+` or its
registry-slugged `-` form — is a snapshot-line build) and is the default when
`--channel` is omitted, so an install stays on the channel it came from.
Requesting a _different_ channel explicitly is always offered/installed —
switching channels skips the newer-version gate, which only applies within the
same channel.

### Where manifests come from

The **stable** manifest is read from the ENS/IPFS gateway at
`https://download.vpn.gnosis.eth.limo/manifests/`, so a production update check
does not depend on a single centrally-hosted origin. **Snapshot** reads
`https://download.gnosisvpn.io/manifests/` directly — the IPFS mirror lags the
origin by hours (and days on experimental), and nightly builds need what was
published minutes ago. **Experimental** reads the origin for the same reason.

Either way the plain `<platform>.json` is used, never the `.ipfs.json` variant
(that one is stable-only and its `download_url`s are IPFS paths), so **artifacts
always download from `download.gnosisvpn.io`** regardless of which host served
the manifest. Two consequences worth knowing:

- The gateway rejects requests without a `User-Agent` (403) and answers more
  slowly than the origin, occasionally 504-ing on a cold cache.
- A stable check returns the gateway's copy of the _whole_ manifest, so the
  `snapshot` and `experimental` entries it reports can lag. A check on either
  of those channels reports the current ones.
- `channels.experimental` is absent until that channel has published once, so
  consumers must treat it as optional rather than required.

Installer choices made at original install time (HOPR network jura/rotsee, log
level) are preserved across updates: the updater detects the installed
selection (from the `/etc/gnosisvpn/config.toml` symlink target, falling back
to the choice files under `/Library/Logs/GnosisVPN/installer/`) and pins it via
`installer -applyChoiceChangesXML`, so a CLI-driven update never flips a rotsee
install back to the package default (jura).

## Platform support

`check-update` and `version` behave identically on macOS and Linux. `update`
has an install engine only on macOS; on Linux it refuses immediately — before
the VPN check, the manifest fetch or any download — and prints the apt commands
that update a Gnosis VPN install:

```console
sudo apt-get update
sudo apt-get install -y gnosisvpn
```

## Development

The crate builds on macOS (Apple Silicon) and Linux (x86_64 and aarch64), but
the install engine is macOS-only — exercise it there. This repo uses Nix. With
`direnv`, `cd` into the repo to enter the dev shell; otherwise:

```console
nix develop            # dev shell with the rust toolchain + tooling
cargo build            # builds the whole workspace; or: nix build .#binary-gnosis_vpn-update
cargo test             # runs the workspace test suite
nix flake check -L     # clippy + tests + audit + licenses
```

The release binaries are per target — statically linked against musl on Linux:

```console
nix build .#binary-gnosis_vpn-update-aarch64-darwin   # on a macOS host
nix build .#binary-gnosis_vpn-update-x86_64-linux     # or: just build-x86_64
nix build .#binary-gnosis_vpn-update-aarch64-linux    # or: just build-arm64
```

## License

LGPL-3.0
