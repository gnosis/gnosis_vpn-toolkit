use std::process;
use std::time::Duration;

use exitcode::{self, ExitCode};

use gnosis_vpn_update::cli::{self, Command, OutputFormat};
use gnosis_vpn_update::manifest::Channel;
#[cfg(target_os = "macos")]
use gnosis_vpn_update::update::EngineInput;
use gnosis_vpn_update::update::{self, CheckOutcome, CheckResult, UpdateStage, UpdateStatus};
use gnosis_vpn_update::{logging, output};

#[tokio::main]
async fn main() {
    logging::setup();
    let cli = cli::parse();
    let format = cli.output;

    let code = match cli.command {
        Command::Version => {
            print_version(format);
            exitcode::OK
        }
        Command::CheckUpdate(args) => run_check(format, args).await,
        Command::Update(args) => run_update(format, args).await,
    };

    process::exit(code);
}

/// Connect (TCP+TLS) deadline per attempt. A TLS 1.3 handshake over a 3 s-RTT
/// VPN link needs ~4 round trips; lost handshake packets are absorbed by the
/// download's retry loop, not by a longer timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Max idle gap between body reads — the stall detector for silently dead
/// connections. Deliberately NOT a total request deadline: a healthy-but-slow
/// multi-hundred-MB artifact download must be allowed to take arbitrarily
/// long (see `update::download`). Small fetches that want a total deadline
/// set one per request (see `manifest::REQUEST_TIMEOUT`).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn print_version(format: OutputFormat) {
    let version = env!("CARGO_PKG_VERSION");
    match format {
        OutputFormat::Json => {
            // A missing or empty version file is expected (client not
            // installed), so it reports as `null` rather than failing: the app
            // calls this to probe whether the toolkit is present at all.
            let package_version = installed_version()
                .inspect_err(|e| tracing::debug!(error = %e, "no installed client version"))
                .ok();
            output::emit(&serde_json::json!({
                "version": version,
                "package_version": package_version,
            }));
        }
        // Deliberately just the bare version: `self_update::finish` and the
        // app's `get_toolkit_version` both capture this stdout whole.
        OutputFormat::Plain => println!("{version}"),
    }
}

/// The installed client version, read from the version file the installer
/// writes (not caller-supplied — see `update::paths::installed_version_path`).
fn installed_version() -> Result<String, String> {
    update::read_installed_version(&update::paths::installed_version_path())
}

async fn run_check(format: OutputFormat, args: cli::CheckArgs) -> ExitCode {
    let result = match (installed_version(), build_client()) {
        (Ok(current_version), Ok(client)) => {
            // No --channel: stay on the channel the installed version came from.
            let channel = match args.channel {
                Some(c) => c.into(),
                None => update::channel_of_version(&current_version),
            };
            update::check(&client, channel, &current_version, &args.socket_path, args.force).await
        }
        // Without an installed version the channel cannot be inferred; fall
        // back to whatever was asked for. The outcome is an error either way.
        (Err(e), _) | (_, Err(e)) => CheckResult {
            channel: args.channel.map(Into::into).unwrap_or(Channel::Stable),
            outcome: CheckOutcome::Error(e),
            manifest: None,
        },
    };

    match format {
        OutputFormat::Json => output::emit(&result),
        OutputFormat::Plain => eprintln!("{result}"),
    }
    exit_for_check(&result)
}

/// No install engine off macOS: refuse before reading the version file, opening
/// the socket or fetching anything, and point at the apt commands the app's
/// "How to update" modal shows.
#[cfg(not(target_os = "macos"))]
async fn run_update(format: OutputFormat, _args: cli::UpdateArgs) -> ExitCode {
    let status = UpdateStatus::Failed {
        stage: UpdateStage::Install,
        error: update::MANUAL_UPDATE_HINT.to_string(),
    };
    emit_status(format, &status);
    exit_for_update(&status)
}

#[cfg(target_os = "macos")]
async fn run_update(format: OutputFormat, args: cli::UpdateArgs) -> ExitCode {
    let (current_app_version, client) = match installed_version().and_then(|v| build_client().map(|c| (v, c))) {
        Ok(pair) => pair,
        Err(e) => {
            let status = UpdateStatus::Failed {
                stage: UpdateStage::Check,
                error: e,
            };
            emit_status(format, &status);
            return exit_for_update(&status);
        }
    };

    // No --channel: stay on the channel the installed version came from.
    let channel = match args.channel {
        Some(c) => c.into(),
        None => update::channel_of_version(&current_app_version),
    };

    let input = EngineInput {
        client,
        channel,
        allow_downgrade: args.allow_downgrade,
        current_app_version,
        download_dir: update::paths::download_dir(),
        attempt_state_path: Some(update::paths::attempt_state_path()),
        audit_log_path: Some(update::paths::audit_log_path()),
        skip_vpn_check: args.force,
        socket_path: args.socket_path,
    };

    let mut rx = update::install_engine(input);
    let mut last: Option<UpdateStatus> = None;
    while let Some(status) = rx.recv().await {
        emit_status(format, &status);
        let terminal = status.is_terminal();
        last = Some(status);
        if terminal {
            break;
        }
    }

    match last {
        Some(status) => exit_for_update(&status),
        // The engine always ends with a terminal status; a closed channel with
        // nothing received means it died unexpectedly.
        None => exitcode::SOFTWARE,
    }
}

fn emit_status(format: OutputFormat, status: &UpdateStatus) {
    match format {
        OutputFormat::Json => output::emit(status),
        OutputFormat::Plain => eprintln!("{status}"),
    }
}

fn exit_for_check(result: &CheckResult) -> ExitCode {
    match &result.outcome {
        CheckOutcome::UpToDate { .. } | CheckOutcome::Available { .. } => exitcode::OK,
        CheckOutcome::NoReleaseForChannel(_) => exitcode::UNAVAILABLE,
        CheckOutcome::VpnNotConnected => exitcode::NOPERM,
        CheckOutcome::IntegrityError(_) => exitcode::SOFTWARE,
        CheckOutcome::Error(_) => exitcode::UNAVAILABLE,
    }
}

fn exit_for_update(status: &UpdateStatus) -> ExitCode {
    if status.is_failure() {
        exitcode::SOFTWARE
    } else {
        exitcode::OK
    }
}
