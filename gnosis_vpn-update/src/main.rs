use std::process;
use std::time::Duration;

use exitcode::{self, ExitCode};

use gnosis_vpn_update::cli::{self, Command, OutputFormat};
use gnosis_vpn_update::update::{self, CheckOutcome, EngineInput, UpdateStage, UpdateStatus};
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

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

fn print_version(format: OutputFormat) {
    let version = env!("CARGO_PKG_VERSION");
    match format {
        OutputFormat::Json => output::emit(&serde_json::json!({ "version": version })),
        OutputFormat::Plain => println!("{version}"),
    }
}

async fn run_check(format: OutputFormat, args: cli::CheckArgs) -> ExitCode {
    let outcome = match build_client() {
        Ok(client) => {
            update::check(
                &client,
                args.channel.into(),
                &args.current_version,
                &args.socket_path,
                args.force,
            )
            .await
        }
        Err(e) => CheckOutcome::Error(e),
    };

    match format {
        OutputFormat::Json => output::emit(&outcome),
        OutputFormat::Plain => eprintln!("{}", check_summary(&outcome)),
    }
    exit_for_check(&outcome)
}

async fn run_update(format: OutputFormat, args: cli::UpdateArgs) -> ExitCode {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            let status = UpdateStatus::Failed {
                stage: UpdateStage::Check,
                error: e,
            };
            emit_status(format, &status);
            return exit_for_update(&status);
        }
    };

    let input = EngineInput {
        client,
        channel: args.channel.into(),
        allow_downgrade: args.allow_downgrade,
        current_app_version: args.current_version,
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

fn check_summary(outcome: &CheckOutcome) -> String {
    match outcome {
        CheckOutcome::UpToDate { current } => format!("Up to date (current {current})"),
        CheckOutcome::Available { current, release } => {
            format!("Update available: {} (current {current})", release.version)
        }
        CheckOutcome::NoReleaseForChannel(channel) => format!("No release for channel {channel}"),
        CheckOutcome::VpnNotConnected => "VPN not connected — pass --force to bypass".to_string(),
        CheckOutcome::IntegrityError(e) => format!("Integrity error: {e}"),
        CheckOutcome::Error(e) => format!("Error: {e}"),
    }
}

fn exit_for_check(outcome: &CheckOutcome) -> ExitCode {
    match outcome {
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
