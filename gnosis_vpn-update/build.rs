//! Compiles `assets/update-loader.applescript` into the zipped
//! `GnosisVPNUpdateLoader.app` applet bundle that `src/update/loader.rs`
//! embeds with `include_bytes!`. Runs only when targeting macOS: `osacompile`
//! and `ditto` are macOS system tools (`/usr/bin`), which is fine because the
//! updater itself is macOS-only and CI builds on macOS runners (Nix on darwin
//! runs with `sandbox = false`, so the system binaries are reachable inside
//! `nix build` too).

use std::path::{Path, PathBuf};
use std::process::Command;

const APPLESCRIPT: &str = "assets/update-loader.applescript";
const BUNDLE_NAME: &str = "GnosisVPNUpdateLoader.app";

fn main() {
    println!("cargo:rerun-if-changed={APPLESCRIPT}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let app = out_dir.join(BUNDLE_NAME);
    let zip = out_dir.join(format!("{BUNDLE_NAME}.zip"));

    // Start from a clean slate: osacompile merges into an existing bundle.
    let _ = std::fs::remove_dir_all(&app);
    let _ = std::fs::remove_file(&zip);

    run(Command::new("/usr/bin/osacompile").arg("-o").arg(&app).arg(APPLESCRIPT));

    // Pin all mtimes so the zip (and thus the embedding binary) does not
    // change with wall-clock time. A fixed date rather than SOURCE_DATE_EPOCH
    // keeps `cargo build` and `nix build` byte-identical; 1980 because ZIP
    // (DOS) timestamps cannot represent anything earlier.
    normalize_mtimes(&app);

    // Archive with `zip -X` rather than `ditto -c -k`: -X drops the
    // extended-timestamp/UID extra fields (whose atime component drifts
    // between builds and would break reproducibility) as well as the
    // AppleDouble `._*` xattr entries ditto would add. Permission bits — the
    // executable bit on Contents/MacOS/applet that loader.rs relies on — stay
    // in the central directory and survive the `ditto -x -k` re-extraction.
    run(Command::new("/usr/bin/zip")
        .current_dir(&out_dir)
        .args(["-q", "-r", "-X"])
        .arg(&zip)
        .arg(BUNDLE_NAME));
}

/// Recursively set the mtime of `root` and everything under it to a fixed
/// date (BSD `touch -d` with a trailing `Z` is interpreted as UTC).
fn normalize_mtimes(root: &Path) {
    // `find -exec touch` in one shot; simpler than walking in Rust.
    run(Command::new("/usr/bin/find").arg(root).args([
        "-exec",
        "/usr/bin/touch",
        "-m",
        "-d",
        "1980-01-01T00:00:00Z",
        "{}",
        "+",
    ]));
}

fn run(cmd: &mut Command) {
    let rendered = format!("{cmd:?}");
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {rendered}: {e}"));
    if !output.status.success() {
        panic!(
            "{rendered} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}
