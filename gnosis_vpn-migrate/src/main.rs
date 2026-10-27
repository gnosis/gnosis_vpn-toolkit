use std::path::{Path, PathBuf};
use std::process;

use clap::Parser;

use gnosis_vpn_migrate::cli::{Cli, Command, IdentityArgs, LayoutArgs, OutputFormat, StartCleanArgs, UploadLogArgs};
use gnosis_vpn_migrate::migrate::{self, Failure, IdentityRequest, MigrateResult, Outcome, Stage};
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
    let user_name = command_user(&cli.command);

    let outcome = match cli.command {
        Command::Version => {
            print_version(format);
            process::exit(exitcode::OK)
        }
        Command::Identity(args) => require_root().and_then(|()| run_identity(&args)),
        Command::StartClean(args) => require_root().and_then(|()| run_start_clean(&args)),
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
        print_paths_forward(&outcome, &layout::migrate_log(), user_name.as_deref());
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

fn command_user(command: &Command) -> Option<String> {
    match command {
        Command::Identity(args) => Some(args.user.clone()),
        Command::StartClean(args) => args.user.clone(),
        Command::Layout(args) => args.user.clone(),
        Command::UploadLog(_) | Command::Version => None,
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
    let network = network::from_config_link(&args.config_link).map_err(|e| Failure::new(Stage::Network, e))?;
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
        migrate_log_copy: logs.migrate_copy.filter(|p| p.exists()),
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
fn print_paths_forward(outcome: &Outcome, log_file: &Path, user: Option<&str>) {
    eprintln!("gnosis_vpn-migrate: {}", outcome.reason());
    eprintln!("log: {} | next: {}", log_file.display(), next_steps(outcome, user));
}

fn next_steps(outcome: &Outcome, user: Option<&str>) -> String {
    let user = user.unwrap_or("<user>");
    // On a conflict `--user` would also move the current identity aside, and the next start would generate a third one.
    if matches!(outcome, Outcome::Conflict { .. }) {
        return format!(
            "sudo gnosis_vpn-migrate identity --user {user} --replace (keep the legacy identity) | sudo gnosis_vpn-migrate start-clean (keep the current one)"
        );
    }
    format!(
        "sudo gnosis_vpn-migrate upload-log | sudo gnosis_vpn-migrate identity --user {user} [--replace] | sudo gnosis_vpn-migrate start-clean --user {user}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_offers_the_start_clean_that_keeps_the_current_identity() {
        let conflict = Outcome::Conflict {
            network: "jura".into(),
            user: "alice".into(),
            mismatches: vec![],
        };
        let next = next_steps(&conflict, Some("alice"));
        assert!(next.contains("identity --user alice --replace"), "{next}");
        assert!(next.contains("start-clean (keep the current one)"), "{next}");
        assert!(!next.contains("start-clean --user"), "{next}");

        let failed = Outcome::from_failure(Failure::new(Stage::Destination, "x"));
        assert!(next_steps(&failed, Some("alice")).contains("start-clean --user alice"));
    }
}
