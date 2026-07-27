# gnosis_vpn-update

macOS-only self-updater for the Gnosis VPN client. Spawned as root by
`gnosis_vpn-app` (`sudo -n /usr/local/bin/gnosis_vpn-update update ...`); emits
NDJSON statuses (`Checking | Downloading | Installing | Completed | Failed`)
on stdout. See the module docs in `src/` for the engine details.

## assets/

### `update-loader.applescript`

While `installer(8)` runs, the pkg preinstall has already killed the
"Gnosis VPN.app" UI and the postinstall only relaunches it at the very end, so
the updater shows a small loader window for the duration of the install
(`src/update/loader.rs`). At compile time `build.rs` runs `osacompile` on this
script and zips the resulting `GnosisVPNUpdateLoader.app` applet bundle with
`zip -X` (into `OUT_DIR`, with pinned mtimes so builds stay reproducible); the
zip is embedded into the binary with `include_bytes!`, extracted to a temp dir
at install time, and launched into the console user's GUI session. The loader
polls `/Library/Logs/GnosisVPN/installer/update_status` and quits when it
contains `done` (or after a ~120 s self-timeout, so it can never linger).

Because `osacompile` and `zip` are macOS system tools (`/usr/bin`), the
crate builds only on macOS hosts — which the toolkit requires anyway. Inside
`nix build` this works because Nix on darwin runs with `sandbox = false`.

Constraints when touching the loader:

- Neither the bundle name nor any path it runs from may contain the substring
  `Gnosis VPN` (with the space): the pkg preinstall runs
  `pkill -f "Gnosis VPN"` and would kill the loader mid-install.
- The bundle stays unsigned; it must only ever be written by this (root)
  process, never downloaded, so it carries no quarantine attribute and
  Gatekeeper does not assess it.

### `gnosisvpn-public-key.asc`

PGP key for manifest verification (currently stubbed, same gap as the client).

## Nix

All files under `assets/` that the build reads (including
`update-loader.applescript`, consumed by `build.rs`) must be listed in
`nix/toolkit.nix` `sources.*.extraFiles`, and must be git-tracked — the flake
sandbox silently omits untracked files.
