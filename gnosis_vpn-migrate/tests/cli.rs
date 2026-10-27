//! The binary's exit codes and stderr contract, as the installer scripts see them.

use std::process::Command;

fn migrate() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gnosis_vpn-migrate"))
}

fn unprivileged() -> bool {
    uzers::get_effective_uid() != 0
}

#[test]
fn version_runs_unprivileged_on_both_formats() {
    let out = migrate().arg("version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("Migrate version: "), "{stdout}");
    assert!(stdout.contains("Package version: "), "{stdout}");

    let out = migrate().args(["--output", "json", "version"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let line: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(line["version"], env!("CARGO_PKG_VERSION"));
    assert!(line.get("package_version").is_some());
}

#[test]
fn bad_usage_exits_64_and_help_exits_0() {
    let out = migrate().arg("identity").output().unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--user"));
    let out = migrate().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("start-clean"));
}

#[test]
fn privileged_commands_refuse_without_root_and_print_the_paths_forward() {
    if !unprivileged() {
        eprintln!("skipped: running as root");
        return;
    }
    for args in [
        vec!["identity", "--user", "nobody"],
        vec!["start-clean", "--user", "nobody"],
        vec!["upload-log"],
    ] {
        let out = migrate().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(77), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("must run as root"), "{stderr}");
        assert!(stderr.contains("gnosis_vpn-migrate upload-log"), "{stderr}");
        assert!(stderr.contains("start-clean --user"), "{stderr}");
        assert!(stderr.contains("logging to stderr only"), "{stderr}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("Failed (privileges)"), "{stdout}");
    }
    let out = migrate()
        .args(["--output", "json", "identity", "--user", "nobody"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(77));
    let line: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(line["outcome"]["type"], "Failed");
    assert_eq!(line["outcome"]["stage"]["type"], "Privileges");
}
