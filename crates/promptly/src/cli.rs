//! The `promptly` command-line surface (clap) and the top-level dispatcher.
//!
//! Each subcommand maps to a handler in [`crate::commands`] that returns the
//! process exit code, so diagnostics (`doctor`) and test runs can signal failure
//! without panicking. Errors bubble up as `anyhow::Error` and are rendered once,
//! here, in the resolved style.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::cloud::HttpCloud;
use crate::commands;
use crate::config;
use crate::credentials::FileCredentialStore;
use crate::daemon_client::DaemonClient;
use crate::daemon_process;
use crate::prompt::StdinAsk;
use crate::style::Style;
use crate::web_client::WebClient;

/// Default loopback port of the daemon's status/control API (`config::DEFAULT_API_PORT`).
const DEFAULT_API_PORT: u16 = promptlyd::config::DEFAULT_API_PORT;

#[derive(Debug, Parser)]
#[command(
    name = "promptly",
    version,
    about = "Promptly — the competitive prompt-engineering arena CLI",
    long_about = "Fetch a level workspace, run a scored attempt with session boundaries, \
                  watch live token burn, score locally with parity to the server, and \
                  diagnose your setup."
)]
pub struct Cli {
    /// Disable colored output (also honored: the NO_COLOR env var, and any
    /// non-TTY output stream).
    #[arg(long, global = true)]
    no_color: bool,

    /// Loopback port of the daemon's status/control API.
    #[arg(long, global = true, default_value_t = DEFAULT_API_PORT)]
    api_port: u16,

    /// Promptly web-app base URL (else `PROMPTLY_API_URL`, else localhost).
    #[arg(long, global = true)]
    api_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download and unpack a level's starter workspace, starting the solve clock.
    Init(commands::init::InitArgs),
    /// Fetch a level and start capturing in one step — fetch, launch the daemon,
    /// and open a scored session. The fastest path from nothing to solving.
    Play(commands::play::PlayArgs),
    /// Report whether the daemon is running and capturing a bound session.
    Status,
    /// Begin a scored capture session bound to this workspace's level (`18`).
    Start(commands::session::StartArgs),
    /// End the active capture session and revert the harness settings.
    Stop,
    /// Restore the workspace's starter files to the canonical starter (after a backup).
    Reset(commands::session::ResetArgs),
    /// Run the level's public tests locally (falls back to remote when needed).
    Test,
    /// Stream live per-turn token burn and a running projected score (`17` feed).
    Watch,
    /// Start the background capture daemon for this folder (without a session).
    Up,
    /// Stop the background capture daemon.
    Down,
    /// Compute the projected score for an attempt, with parity to the server (`13`).
    Score(commands::score::ScoreArgs),
    /// Diagnose the setup: daemon, OTEL config, manifest, runtime, and Judge0.
    Doctor,
    /// Package the solution and submit it for ranked grading (cloud path: `20`).
    Submit,
    /// Pair this device with your Promptly account (`20`).
    Pair,
}

/// Parse arguments and dispatch. The single fallible boundary the binary reports
/// through its exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let style = Style::resolve(cli.no_color);

    let result = match cli.command {
        Command::Init(args) => {
            let web = WebClient::new(&config::resolve_api_url(cli.api_url.as_deref()));
            commands::init::run(&web, args, now_ms(), style)
        }
        Command::Play(args) => {
            let web = WebClient::new(&config::resolve_api_url(cli.api_url.as_deref()));
            let cloud = cloud(cli.api_url.as_deref());
            let mut asker = StdinAsk::new();
            commands::play::run(
                &web,
                &cloud,
                &mut asker,
                cli.api_port,
                args,
                now_ms(),
                style,
            )
        }
        Command::Status => {
            let client = DaemonClient::new(cli.api_port);
            commands::status::run(&client, style)
        }
        Command::Start(args) => {
            let workspace = current_dir();
            // Auto-launch the background daemon scoped to this folder, so a player
            // never has to run `promptlyd` in a second terminal.
            daemon_process::ensure_running(cli.api_port, &workspace, style).and_then(|_| {
                let client = DaemonClient::new(cli.api_port);
                let cloud = cloud(cli.api_url.as_deref());
                let mut asker = StdinAsk::new();
                // A paired device claims a server-issued attempt nonce (so the
                // capture can reach `verified`); unpaired falls back to a local
                // nonce (offline play, capped at `unverified`).
                commands::session::run_start(&client, &cloud, &mut asker, args, style)
            })
        }
        Command::Stop => {
            let client = DaemonClient::new(cli.api_port);
            commands::session::run_stop(&client, style)
        }
        Command::Reset(args) => {
            let client = DaemonClient::new(cli.api_port);
            let mut asker = StdinAsk::new();
            commands::session::run_reset(&client, &mut asker, args, style)
        }
        Command::Test => {
            let web = WebClient::new(&config::resolve_api_url(cli.api_url.as_deref()));
            let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
            commands::test::run(&workspace, &web, style)
        }
        Command::Watch => {
            let workspace = current_dir();
            daemon_process::ensure_running(cli.api_port, &workspace, style).and_then(|_| {
                let client = DaemonClient::new(cli.api_port);
                commands::watch::run(&client, cwd_manifest().as_ref(), style)
            })
        }
        Command::Up => {
            let workspace = current_dir();
            commands::daemon::run_up(cli.api_port, &workspace, style)
        }
        Command::Down => commands::daemon::run_down(cli.api_port, style),
        Command::Score(args) => {
            let client = DaemonClient::new(cli.api_port);
            commands::score::run(&client, cwd_manifest().as_ref(), args, style)
        }
        Command::Doctor => {
            let client = DaemonClient::new(cli.api_port);
            let web = WebClient::new(&config::resolve_api_url(cli.api_url.as_deref()));
            let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
            commands::doctor::run(&client, &web, &workspace, style)
        }
        Command::Submit => {
            let client = DaemonClient::new(cli.api_port);
            let cloud = cloud(cli.api_url.as_deref());
            let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
            commands::submit::run_submit(
                &workspace,
                cwd_manifest().as_ref(),
                &client,
                &cloud,
                style,
            )
        }
        Command::Pair => commands::submit::run_pair(&cloud(cli.api_url.as_deref()), style),
    };

    match result {
        Ok(exit) => exit.into_code(),
        Err(err) => {
            // `{:#}` prints the full anyhow context chain on one line.
            eprintln!("{} {err:#}", style.red("error:"));
            ExitCode::FAILURE
        }
    }
}

/// Build the authenticated cloud client: the resolved web-app URL plus the
/// on-disk device credentials (`~/.promptly/credentials.json`). Unpaired is fine —
/// the client treats a missing credential as offline play (`20`).
fn cloud(api_url: Option<&str>) -> HttpCloud {
    HttpCloud::new(
        &config::resolve_api_url(api_url),
        Box::new(FileCredentialStore::default_store()),
    )
}

/// Load the workspace manifest from the current directory, if present. `watch`
/// and `score` read its `challenge_type`/`token_weight_overrides` so the local
/// projection uses the level's real token weights.
fn cwd_manifest() -> Option<promptlyd::manifest::Manifest> {
    let cwd = std::env::current_dir().ok()?;
    promptlyd::manifest::Manifest::load(&cwd).ok()
}

/// The current working directory — the workspace the session commands scope the
/// daemon to. Falls back to `.` if the cwd can't be read.
fn current_dir() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| ".".into())
}

/// Current wall-clock time in epoch millis, for the one-shot commands that stamp
/// a timestamp (`init` acquisition). Unlike the daemon's timer-free core, the CLI
/// reads the clock directly — it never needs deterministic replay.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
