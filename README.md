# gnosis_vpn-toolkit

A collection of companion binaries for the
[Gnosis VPN client](https://github.com/gnosis/gnosis_vpn-client) that perform
auxiliary tasks on behalf of `gnosis_vpn-app` or of the client installer, which
spawn them as subprocesses and read their **standard output** (and, for the
installer, their exit code).

This is a virtual Cargo workspace: each tool is its own `gnosis_vpn-*` member
crate, and what they share (stdout protocol, logging, package version) lives in
`gnosis_vpn-toolkit-common`. The tools:

- **`gnosis_vpn-update`** — the self-updater, spawned by `gnosis_vpn-app`
  ([below](#gnosis_vpn-update)).
- **`gnosis_vpn-migrate`** — moves the legacy system-wide identity into the
  installing user's per-network layout, spawned by the installer
  ([below](#gnosis_vpn-migrate)).

Future tools are added as sibling member crates. One release ships every tool
(`binary-toolkit-<arch>` builds them all), so the installer can only bump to a
toolkit version that carries both artifacts.

Unlike the client, these tools run **no socket server**: they communicate
purely over stdout. `gnosis_vpn-update` does open a small read-only client
connection to the running `gnosis_vpn` daemon socket to check whether the VPN
is connected before updating (see the `--force` flag to bypass).

## gnosis_vpn-update

### Output contract

- **stdout** carries the result of whatever was asked for. Every subcommand
  renders it human-readably by default; `--output json` switches the whole
  stream to one JSON value per line (newline-delimited JSON / NDJSON), which is
  how `gnosis_vpn-app` drives this binary:
  - `update` streams `UpdateStatus` events (`Checking`, `Downloading`,
    `Installing`, then a terminal `Completed` or `Failed`).
  - `check-update` prints a single result object
    `{"channel": …, "outcome": …, "end_of_life": …, "manifest": …}`. `outcome`
    is the gated decision (`UpToDate`, `Available`, `NoReleaseForChannel`,
    `VpnNotConnected`, `IntegrityError`, `Error`) and `channel` is the one that
    was checked — the `--channel` value, or the channel inferred from the
    installed version. `UpToDate`, `Available` and `NoReleaseForChannel` name
    the installed version they were reached for as `current` (older toolkits
    sent `NoReleaseForChannel` as the bare channel name). `manifest` is the
    fetched update manifest, carrying
    **every** channel entry — `channels.stable`, `channels.snapshot` and
    `channels.experimental` — but not their raw `end_of_life` lists (see
    below), so one invocation yields the whole release picture as well as the
    decision. It is omitted when no manifest was fetched (`VpnNotConnected`,
    `IntegrityError`, and an `Error` raised before or during the fetch), where
    a consumer should keep its last known one; the `Error` for an app too old
    to update directly comes after the fetch and still carries it.
  - `end_of_life` is `{"version": …, "ends_at": …, "reason": …}`: the
    manifest's end-of-life entry that covers the installed version (same
    version scheme, installed `<=` `version`; the earliest `ends_at` when
    several do). It is omitted when no entry covers the installed version or
    no manifest was fetched. The manifest's raw per-channel `end_of_life`
    lists are never re-emitted, so nothing in the output describes an end of
    life that does not apply.
  - `version` prints `{"version": "…", "package_version": "…"}`, where
    `package_version` is the installed client version from
    `/etc/gnosisvpn/version.txt` (`null` when the client is not installed).
    The JSON uses serde's externally-tagged enum encoding.
- **stderr** carries only logs / diagnostics (`RUST_LOG`, default `info`).
  Nothing a caller is meant to parse goes there, in either format.

  `check-update` prints the chosen channel's changelog only when the release
  carries one (today only stable does), and `End of life` / `Reason` lines
  only when the installed version is covered.

  ```console
  $ gnosis_vpn-update version
  Updater version: 0.5.0
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

### Usage

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

#### Where manifests come from

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

The manifest and its `.asc` are each fetched up to 5 times, retrying network
errors and any HTTP error status with a backoff starting at 1 s. Both fetches
together get one 60 s budget, which keeps `check-update` inside the app's 75 s
timeout; exhausting it aborts the check with a timeout error, and the causes of
the individual attempts are in the stderr log. Retries are logged to stderr
only; stdout still carries one result.

Installer choices made at original install time (HOPR network jura/rotsee, log
level) are preserved across updates: the updater detects the installed
selection (from the `/etc/gnosisvpn/config.toml` symlink target, falling back
to the choice files under `/Library/Logs/GnosisVPN/installer/`) and pins it via
`installer -applyChoiceChangesXML`, so a CLI-driven update never flips a rotsee
install back to the package default (jura).

### Platform support

`check-update` and `version` behave identically on macOS and Linux. `update`
has an install engine only on macOS; on Linux it refuses immediately — before
the VPN check, the manifest fetch or any download — and prints the apt commands
that update a Gnosis VPN install:

```console
sudo apt-get update
sudo apt-get install -y gnosisvpn
```

## gnosis_vpn-migrate

A root-run, non-interactive tool the client installer runs between "old
service stopped" and "new service started" on an upgrade from a single-user
install. It moves the one legacy identity (`/var/lib/gnosisvpn/.config/` on
Linux, `/Library/Application Support/GnosisVPN/.config/` on macOS, plus the
Curvy PIX db next to it) into the installing user's per-network layout, so the
upgraded install keeps its node address. It never deletes anything: the legacy
files are renamed aside, every destination is written to a staging dir and
verified (sha256) before it is renamed into place, and a half-written
`networks/<network>/` never exists.

The network is the installer's choice, read offline from the
`/etc/gnosisvpn/config.toml` symlink target (`config-<network>.toml` on Linux,
`<network>.toml` on macOS). The per-user layout (duplicated from the client in
`gnosis_vpn-migrate/src/layout.rs`, contract in GNO-810):

|       | Linux                                                                   | macOS                                                          |
| ----- | ----------------------------------------------------------------------- | -------------------------------------------------------------- |
| data  | `~/.local/share/gnosisvpn/networks/<network>/hopr.{id,pass}` and `pix/` | `~/Library/Application Support/GnosisVPN/networks/<network>/…` |
| cache | `~/.cache/gnosisvpn/networks/<network>/hopr.safe`                       | `~/Library/Caches/GnosisVPN/networks/<network>/hopr.safe`      |

Directories are `0700`, files `0600`, everything created is owned by the user.

### Usage

```console
gnosis_vpn-migrate [-o json|plain] <command>

# Move the legacy identity into <user>'s layout (what the installer runs)
sudo gnosis_vpn-migrate identity --user <user> [--dry-run] [--replace]
                                 [--legacy-home <dir>] [--config-link <path>]

# Move the legacy identity aside and, with --user, that user's
# networks/<network>/ dirs too, so the next start generates a fresh identity
sudo gnosis_vpn-migrate start-clean [--user <user>]
                                    [--legacy-home <dir>] [--config-link <path>]

# Bundle the migration log (plus installer/updater logs where present),
# upload it and print the reference to quote
sudo gnosis_vpn-migrate upload-log [--description <text>]

# Print this tool's version and the installed package's (no root needed)
gnosis_vpn-migrate version
```

`identity` outcomes: `Migrated`, `AlreadyMigrated` (the destination already
holds byte-identical copies of every legacy file), `NothingToMigrate` (no
legacy `gnosisvpn-hopr.id`, e.g. a fresh install or after `--reset-identity`),
`WouldMigrate` (`--dry-run`), `Conflict` (the user already has a _different_
identity on that network: re-run with `--replace` to keep the legacy one — the
current one moves to `networks/<network>.replaced-<ts>/` — or run `start-clean`
to keep the current one). `--user root` is refused: the client never runs a
worker as uid 0.

### Output contract

- **stdout**: with `--output json`, one line `{"outcome": {"type": …, …}}`
  where `type` is one of `Migrated`, `WouldMigrate`, `AlreadyMigrated`,
  `NothingToMigrate`, `Conflict`, `StartedClean`, `NothingToDo`, `Uploaded`,
  `Failed` (the last carries `stage` and `error`). Plain mode prints the same
  outcome in one or more human-readable lines. `version` prints
  `{"version": …, "package_version": …}` like the updater.
- **stderr**: logs (`RUST_LOG`, default `info`), teed into
  `/var/log/gnosisvpn/migrate.log` (Linux) or
  `/Library/Logs/GnosisVPN/installer/migrate.log` (macOS) when that file can be
  opened; otherwise stderr-only with one warning (`version` and the not-root
  path never need the file). On every non-OK exit two extra stderr lines name
  the reason, the log file and the three ways forward:
  `gnosis_vpn-migrate upload-log`,
  `gnosis_vpn-migrate identity --user <user> [--replace]`,
  `gnosis_vpn-migrate start-clean --user <user>`.
- **exit codes** are the installer's contract (its scripts are bash without
  `jq`), so they carry the meaning on their own. This table is
  `gnosis_vpn-migrate`'s own — note `NOPERM` means "not root" here, unlike the
  updater's "VPN not connected":

| exit | `exitcode` name | meaning                                                                                                       |
| ---- | --------------- | ------------------------------------------------------------------------------------------------------------- |
| 0    | `OK`            | `Migrated`, `AlreadyMigrated`, `NothingToMigrate`, `WouldMigrate`, `StartedClean`, `NothingToDo`, `Uploaded`  |
| 64   | `USAGE`         | bad arguments, unknown user, `--user root`, home missing or equal to the legacy dir                           |
| 65   | `DATAERR`       | config link unusable (not a symlink, or its target is not a network name) while there is something to migrate |
| 66   | `NOINPUT`       | legacy files unreadable or cannot be moved aside                                                              |
| 70   | `SOFTWARE`      | `upload-log` failed, or anything unexpected                                                                   |
| 73   | `CANTCREAT`     | destination write, chown or verify failure (staging dirs are removed; a re-run starts over)                   |
| 75   | `TEMPFAIL`      | `Conflict`                                                                                                    |
| 77   | `NOPERM`        | not running as root                                                                                           |

### Log upload

`upload-log` reuses `gnosis_vpn-app`'s protocol against the same endpoint
(`https://log-uploader.gnosisvpn.com/api/upload`, override with
`GNOSISVPN_LOG_UPLOAD_API_URL` or `--api-url`): one zstd frame of plain text,
`===== <title> =====` sections (system info with the toolkit and installed
package versions, the optional description capped at 2000 characters, the
migrate log — on Linux also `/var/lib/gnosisvpn/migrate.log`, where the
installer copies it because an rpm upgrade's old `%postun` deletes
`/var/log/gnosisvpn` — then the macOS installer logs and `updates.log` when
present), a multipart POST of the first 4 MiB plus the size, then a PUT of the
whole bundle to the signed URL. It prints the returned reference to quote when
reporting the problem.

## Development

The crate builds on macOS (Apple Silicon) and Linux (x86_64 and aarch64), but
the install engine is macOS-only — exercise it there. This repo uses Nix. With
`direnv`, `cd` into the repo to enter the dev shell; otherwise:

```console
nix develop            # dev shell with the rust toolchain + tooling
cargo build            # builds the whole workspace; or: nix build .#binary-gnosis_vpn-update / .#binary-gnosis_vpn-migrate
cargo test             # runs the workspace test suite (unprivileged; `just test-root` exercises the real chown path)
nix flake check -L     # clippy + docs + tests + licenses
nix run .#audit        # cargo-audit against the live RUSTSEC advisory DB
```

The release packages are per target and carry every binary in `result/bin/` —
statically linked against musl on Linux. CI builds exactly these, so one run
publishes `gnosis_vpn-update-<arch>` and `gnosis_vpn-migrate-<arch>` together:

```console
nix build .#binary-toolkit-aarch64-darwin   # on a macOS host; or: just build-toolkit-darwin
nix build .#binary-toolkit-x86_64-linux     # or: just build-toolkit-x86_64
nix build .#binary-toolkit-aarch64-linux    # or: just build-toolkit-arm64
```

Single-binary per-target builds of the updater remain available as
`binary-gnosis_vpn-update-<arch>` (`just build-x86_64`, `just build-arm64`).

## License

LGPL-3.0
