use std::path::{Path, PathBuf};
use std::process;

use clap::Parser;

use gnosis_vpn_migrate::cli::{
    Cli, Command, IdentityArgs, LayoutArgs, OutputFormat, StartCleanArgs, SwitchNetworkArgs, UploadLogArgs,
};
use gnosis_vpn_migrate::migrate::{self, Failure, IdentityRequest, MigrateResult, Outcome, Stage};
use gnosis_vpn_migrate::switch::{self, SwitchRequest};
use gnosis_vpn_migrate::upload::{self, BundleSources};
use gnosis_vpn_migrate::user::Account;
use gnosis_vpn_migrate::{layout, network, relocate, user};
use gnosis_vpn_toolkit_common::{logging, output, package_version};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[tokio::main]
async fn main() {
    let cli = parse_or_exit();
    let format = cli.output.unwrap_or(OutputFormat::Plain);
    // `version` runs unprivileged and must not try the root-owned log file.
    let log_file = (!matches!(cli.command, Command::Version)).then(layout::migrate_log);
    logging::setup(log_file.as_deref());
    tracing::info!(version = VERSION, command = ?cli.command, "gnosis_vpn-migrate start");
    let retry = Retry::of(&cli.command);

    let outcome = match cli.command {
        Command::Version => {
            print_version(format);
            process::exit(exitcode::OK)
        }
        Command::Identity(args) => require_root().and_then(|()| run_identity(&args)),
        Command::StartClean(args) => require_root().and_then(|()| run_start_clean(&args)),
        Command::SwitchNetwork(args) => require_root().and_then(|()| run_switch_network(&args)),
        Command::Layout(args) => require_root().and_then(|()| run_layout(&args)),
        Command::UploadLog(args) => match require_root() {
            Ok(()) => run_upload(&args).await,
            Err(failure) => Err(failure),
        },
    }
    .unwrap_or_else(Outcome::from_failure);

    let code = outcome.exit_code();
    tracing::info!(exit_code = code, outcome = %outcome.reason(), "gnosis_vpn-migrate end");
    match format {
        OutputFormat::Json => output::emit(&MigrateResult { outcome: &outcome }),
        OutputFormat::Plain => println!("{outcome}"),
    }
    if code != exitcode::OK {
        print_paths_forward(&outcome, &layout::migrate_log(), &retry);
    }
    process::exit(code);
}

/// clap exits 2 on bad usage; the installer contract says USAGE (64). `--help` is not an error.
fn parse_or_exit() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            let code = if e.use_stderr() { exitcode::USAGE } else { exitcode::OK };
            process::exit(code)
        }
    }
}

/// What the ways forward repeat so a retry acts on the same user, network and dirs as this run, shell-quoted.
struct Retry {
    user: String,
    /// `identity --network`: the legacy identity belongs to this network, not the one the config link names.
    network: Option<String>,
    /// `--legacy-home` and `--config-link` where they differ from the defaults.
    paths: String,
    switch_from: Option<String>,
}

impl Retry {
    fn of(command: &Command) -> Self {
        let none = Retry {
            user: "<user>".to_string(),
            network: None,
            paths: String::new(),
            switch_from: None,
        };
        match command {
            Command::Identity(a) => Retry {
                user: shell_quote(&a.user),
                network: a.network.as_deref().map(shell_quote),
                paths: retry_paths(&a.legacy_home, Some(&a.config_link)),
                ..none
            },
            Command::StartClean(a) => Retry {
                user: a.user.as_deref().map_or(none.user.clone(), shell_quote),
                paths: retry_paths(&a.legacy_home, Some(&a.config_link)),
                ..none
            },
            Command::SwitchNetwork(a) => Retry {
                paths: retry_paths(&a.legacy_home, Some(&a.config_link)),
                switch_from: Some(shell_quote(&a.from)),
                ..none
            },
            Command::Layout(a) => Retry {
                user: a.user.as_deref().map_or(none.user.clone(), shell_quote),
                paths: retry_paths(&a.legacy_home, None),
                ..none
            },
            Command::UploadLog(_) | Command::Version => none,
        }
    }
}

/// `--legacy-home` and `--config-link` where they differ from the defaults.
fn retry_paths(legacy_home: &Path, config_link: Option<&PathBuf>) -> String {
    let mut paths = String::new();
    if legacy_home != layout::legacy_home() {
        paths.push_str(&format!(
            " --legacy-home {}",
            shell_quote(&legacy_home.to_string_lossy())
        ));
    }
    if let Some(link) = config_link.filter(|link| **link != layout::config_link()) {
        paths.push_str(&format!(" --config-link {}", shell_quote(&link.to_string_lossy())));
    }
    paths
}

/// Single-quotes a value unless it is plainly safe to paste into a shell.
fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=".contains(c));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

fn require_root() -> Result<(), Failure> {
    if user::is_root() {
        return Ok(());
    }
    Err(Failure::new(Stage::Privileges, "must run as root"))
}

/// A fresh install has no legacy identity and may have no config link yet; that is OK, not DATAERR.
fn run_identity(args: &IdentityArgs) -> Result<Outcome, Failure> {
    if !migrate::legacy_identity_exists(&args.legacy_home)? {
        return Ok(Outcome::NothingToMigrate {
            legacy_home: args.legacy_home.clone(),
        });
    }
    let account = user::resolve(&args.user, &args.legacy_home).map_err(|e| Failure::new(Stage::User, e))?;
    let network = match &args.network {
        Some(name) => network::validate(name),
        None => network::from_config_link(&args.config_link),
    }
    .map_err(|e| Failure::new(Stage::Network, e))?;
    migrate::migrate_identity(&IdentityRequest {
        account: &account,
        network: &network,
        legacy_home: &args.legacy_home,
        dry_run: args.dry_run,
        replace: args.replace,
        toolkit_version: VERSION,
    })
}

fn run_start_clean(args: &StartCleanArgs) -> Result<Outcome, Failure> {
    let Some(user_name) = &args.user else {
        return migrate::start_clean(&args.legacy_home, None);
    };
    let account = user::resolve(user_name, &args.legacy_home).map_err(|e| Failure::new(Stage::User, e))?;
    // No usable link means no chosen network, so there is no per-user dir to reset; the legacy part still runs.
    let network = match network::from_config_link(&args.config_link) {
        Ok(network) => network,
        Err(e) => {
            tracing::warn!(error = %e, "no network configured, resetting only the legacy identity");
            return migrate::start_clean(&args.legacy_home, None);
        }
    };
    migrate::start_clean(&args.legacy_home, Some((&account, &network)))
}

/// Every local account, so users other than the installing one keep their identity too.
fn run_switch_network(args: &SwitchNetworkArgs) -> Result<Outcome, Failure> {
    let from = network::validate(&args.from).map_err(|e| Failure::new(Stage::Network, e))?;
    let to = network::from_config_link(&args.config_link).map_err(|e| Failure::new(Stage::Network, e))?;
    if from == to {
        return Err(Failure::new(
            Stage::Network,
            format!(
                "{} already names {from}; nothing to switch from",
                args.config_link.display()
            ),
        ));
    }
    let (accounts, uninspected) = user::local_accounts(&layout::home_roots(), &args.legacy_home);
    tracing::info!(%from, %to, accounts = accounts.len(), "switching network");
    let mut report = switch::switch_network(&SwitchRequest {
        from: &from,
        to: &to,
        legacy_home: &args.legacy_home,
        accounts: &accounts,
        toolkit_version: VERSION,
    });
    // A home that could not be inspected may hold an identity that stayed behind.
    report.failed.extend(uninspected);
    Ok(Outcome::Switched(report))
}

/// System files first, then the user's; a failed step is reported in the outcome and never a failure.
fn run_layout(args: &LayoutArgs) -> Result<Outcome, Failure> {
    let root_account = Account {
        name: "root".to_string(),
        uid: 0,
        gid: 0,
        home: PathBuf::from("/"),
    };
    let mut report = relocate::run(Path::new("/"), &relocate::system_steps(), &root_account)?;
    if let Some(user_name) = &args.user {
        let account = user::resolve(user_name, &args.legacy_home).map_err(|e| Failure::new(Stage::User, e))?;
        report.extend(relocate::run(&account.home, &relocate::user_steps(), &account)?);
    }
    Ok(Outcome::Relocated(report))
}

async fn run_upload(args: &UploadLogArgs) -> Result<Outcome, Failure> {
    let fail = |e: String| Failure::new(Stage::Upload, e);
    let logs = layout::log_sources();
    let sources = BundleSources {
        toolkit_version: VERSION.to_string(),
        package_version: package_version::read_installed_version(&package_version::installed_version_path()).ok(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        migrate_log: logs.migrate,
        extra_logs: logs.extra.into_iter().filter(|p| p.exists()).collect(),
    };
    let bundle = upload::write_bundle(args.description.as_deref(), &sources).map_err(fail)?;
    tracing::info!(bytes = bundle.len(), url = %args.api_url, "log bundle compressed, uploading");
    let client = upload::build_client(USER_AGENT).map_err(fail)?;
    let file_name = upload::bundle_file_name(chrono::Utc::now());
    let uuid = upload::send(&client, &args.api_url, file_name, bundle)
        .await
        .map_err(fail)?;
    tracing::info!(reference = %uuid, "log bundle uploaded");
    Ok(Outcome::Uploaded { uuid })
}

fn print_version(format: OutputFormat) {
    let package_version = package_version::read_installed_version(&package_version::installed_version_path())
        .inspect_err(|e| tracing::debug!(error = %e, "no installed client version"))
        .ok();
    match format {
        OutputFormat::Json => output::emit(&serde_json::json!({
            "version": VERSION,
            "package_version": package_version,
        })),
        OutputFormat::Plain => {
            println!("Migrate version: {VERSION}");
            println!(
                "Package version: {}",
                package_version.as_deref().unwrap_or("not installed")
            );
        }
    }
}

/// Two stderr lines the installer prints verbatim: the reason, then the log and the ways forward.
fn print_paths_forward(outcome: &Outcome, log_file: &Path, retry: &Retry) {
    eprintln!("gnosis_vpn-migrate: {}", outcome.reason());
    eprintln!("log: {} | next: {}", log_file.display(), next_steps(outcome, retry));
}

fn next_steps(outcome: &Outcome, retry: &Retry) -> String {
    const MIGRATE: &str = "sudo gnosis_vpn-migrate";
    let Retry {
        user,
        network,
        paths,
        switch_from,
    } = retry;
    if let Some(from) = switch_from {
        return format!("{MIGRATE} upload-log | {MIGRATE} switch-network --from {from}{paths}");
    }
    let identity = match network {
        Some(network) => format!("{MIGRATE} identity --user {user} --network {network}{paths}"),
        None => format!("{MIGRATE} identity --user {user}{paths}"),
    };
    let legacy_only = format!("{MIGRATE} start-clean{paths}");
    // On a conflict `--user` would also move the current identity aside, and the next start would generate a third one.
    if matches!(outcome, Outcome::Conflict { .. }) {
        return format!("{identity} --replace (keep the legacy identity) | {legacy_only} (keep the current one)");
    }
    // `start-clean --user` resets the network the config link names; with `--network` that is not the one being migrated.
    let fresh = match network {
        Some(_) => legacy_only,
        None => format!("{MIGRATE} start-clean --user {user}{paths}"),
    };
    format!("{MIGRATE} upload-log | {identity} [--replace] | {fresh}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry(args: &[&str]) -> Retry {
        let cli = Cli::try_parse_from(std::iter::once("gnosis_vpn-migrate").chain(args.iter().copied())).unwrap();
        Retry::of(&cli.command)
    }

    fn failed() -> Outcome {
        Outcome::from_failure(Failure::new(Stage::Destination, "x"))
    }

    fn conflict() -> Outcome {
        Outcome::Conflict {
            network: "jura".into(),
            user: "alice".into(),
            mismatches: vec![],
        }
    }

    #[test]
    fn a_conflict_offers_the_start_clean_that_keeps_the_current_identity() {
        let alice = retry(&["identity", "--user", "alice"]);
        let next = next_steps(&conflict(), &alice);
        assert!(next.contains("identity --user alice --replace"), "{next}");
        assert!(next.contains("start-clean (keep the current one)"), "{next}");
        assert!(!next.contains("start-clean --user"), "{next}");
        assert!(next_steps(&failed(), &alice).contains("start-clean --user alice"));
    }

    #[test]
    fn a_named_network_is_repeated_and_never_reset_through_the_config_link() {
        let alice = retry(&["identity", "--user", "alice", "--network", "jura-staging"]);
        let next = next_steps(&failed(), &alice);
        assert!(
            next.contains("identity --user alice --network jura-staging [--replace]"),
            "{next}"
        );
        assert!(next.ends_with("| sudo gnosis_vpn-migrate start-clean"), "{next}");
        let next = next_steps(&conflict(), &alice);
        assert!(next.contains("--network jura-staging --replace"), "{next}");
    }

    #[test]
    fn custom_dirs_are_repeated_and_quoted_defaults_left_out() {
        let custom = retry(&[
            "identity",
            "--user",
            "alice",
            "--legacy-home",
            "/srv/my legacy",
            "--config-link",
            "/etc/x.toml",
        ]);
        let next = next_steps(&failed(), &custom);
        assert!(
            next.contains("identity --user alice --legacy-home '/srv/my legacy' --config-link /etc/x.toml [--replace]"),
            "{next}"
        );
        assert!(next.ends_with("start-clean --user alice --legacy-home '/srv/my legacy' --config-link /etc/x.toml"));
        assert!(!next_steps(&failed(), &retry(&["identity", "--user", "alice"])).contains("--legacy-home"));
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn a_failed_switch_offers_its_own_retry() {
        let switch = retry(&["switch-network", "--from", "jura-dev", "--config-link", "/tmp/c.toml"]);
        let next = next_steps(&failed(), &switch);
        assert!(
            next.ends_with("switch-network --from jura-dev --config-link /tmp/c.toml"),
            "{next}"
        );
        assert!(!next.contains("identity"), "{next}");
    }
}
