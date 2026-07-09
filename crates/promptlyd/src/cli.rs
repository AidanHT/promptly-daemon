//! Command-line surface for `promptlyd`.
//!
//! - `run` — the foreground entrypoint the OS service manager invokes; it stands
//!   up the OTLP receiver, the JSONL watcher, the engine, the workspace watcher,
//!   and the status API, then captures until interrupted.
//! - `status` — query a running daemon (connected / capturing / idle).
//! - `install` / `uninstall` — register/remove the background OS service.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;

use crate::api::{self, ApiState};
use crate::checkpoint::{offsets_from_strings, Checkpoint};
use crate::clock::now_ms;
use crate::config::{resolve_web_origins, DaemonConfig, DEFAULT_API_PORT, DEFAULT_OTLP_PORT};
use crate::diagnostics::Diagnostics;
use crate::engine::{Engine, EngineInit};
use crate::model::RawTurn;
use crate::provenance::ProvenanceTracker;
use crate::scoping::{SessionMarker, SessionStore};
use crate::service;
use crate::session::SessionGuard;
use crate::sources::codex::CodexSource;
use crate::sources::copilot::CopilotSource;
use crate::sources::cursor::{self, CursorSource};
use crate::sources::jsonl::{JsonlSource, SharedOffsets};
use crate::sources::otel::{IngestAuth, OtelSource};
use crate::sources::registry::{AdapterRegistry, AdapterState};
use crate::sources::{wait_for_shutdown, TelemetrySource};
use crate::status;
use crate::watcher::{self, FileChange, Scope};

#[derive(Debug, Parser)]
#[command(name = "promptlyd", version, about = "Promptly local telemetry daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground (the service manager's entrypoint).
    Run(RunArgs),
    /// Report whether a running daemon is connected, capturing, or idle.
    Status(StatusArgs),
    /// Install the daemon as a managed background OS service.
    Install(InstallArgs),
    /// Remove the managed background OS service.
    Uninstall,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Workspace whose AI usage to capture (defaults to the current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Localhost port for the status/stream API.
    #[arg(long, default_value_t = DEFAULT_API_PORT)]
    api_port: u16,
    /// Localhost port for the embedded OTLP receiver.
    #[arg(long, default_value_t = DEFAULT_OTLP_PORT)]
    otlp_port: u16,
    /// An *additional* deployed Promptly web origin allowed to read the
    /// status/stream API (`22`), e.g. a preview deploy. Repeatable. The
    /// canonical production origin (`https://xpromptly.com`) and loopback dev
    /// origins are always allowed, so the live HUD bridge needs no flag for
    /// normal use; this (and `PROMPTLY_WEB_ORIGIN`) only extends the allowlist.
    #[arg(long = "web-origin", value_name = "ORIGIN")]
    web_origins: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct StatusArgs {
    /// Localhost port the daemon's API is on.
    #[arg(long, default_value_t = DEFAULT_API_PORT)]
    api_port: u16,
}

#[derive(Debug, clap::Args)]
struct InstallArgs {
    /// Workspace the installed service captures (defaults to the current
    /// directory, resolved to an absolute path so the background service scopes
    /// to your project rather than the service manager's cwd).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Status/stream API port the installed service binds (default 8765).
    #[arg(long)]
    api_port: Option<u16>,
    /// OTLP receiver port the installed service binds (default 4318).
    #[arg(long)]
    otlp_port: Option<u16>,
    /// An extra deployed web origin the installed service allows (repeatable).
    /// The canonical production origin and loopback dev origins are always
    /// allowed without this.
    #[arg(long = "web-origin", value_name = "ORIGIN")]
    web_origins: Vec<String>,
}

fn log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,promptlyd=info"))
}

/// Initialize structured logging to stderr. Idempotent: safe to call from tests
/// and never panics if a subscriber is already installed.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Like [`init_tracing`] but also mirrors WARN/ERROR events into `diagnostics`,
/// which the daemon surfaces on `GET /health` for `promptly doctor` (`19`).
fn init_tracing_with(diagnostics: &Diagnostics) {
    use tracing_subscriber::prelude::*;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);
    let _ = tracing_subscriber::registry()
        .with(log_filter())
        .with(fmt_layer)
        .with(diagnostics.layer())
        .try_init();
}

/// Parse args and dispatch. The single fallible boundary the binary reports
/// through its exit code.
pub fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run(args) => run_blocking(args),
        Command::Status(args) => status_command(args),
        Command::Install(args) => install_command(args),
        Command::Uninstall => service_command("uninstall", service::uninstall),
    }
}

fn run_blocking(args: RunArgs) -> ExitCode {
    let diagnostics = Diagnostics::new();
    init_tracing_with(&diagnostics);
    let workspace = match resolve_workspace(args.workspace) {
        Ok(ws) => ws,
        Err(err) => {
            tracing::error!(%err, "failed to resolve workspace");
            return ExitCode::FAILURE;
        }
    };
    let config = DaemonConfig::new(
        workspace,
        args.api_port,
        args.otlp_port,
        resolve_web_origins(args.web_origins),
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(%err, "failed to start async runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run_foreground(config, diagnostics)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "daemon exited with error");
            ExitCode::FAILURE
        }
    }
}

fn status_command(args: StatusArgs) -> ExitCode {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.api_port));
    let report = status::query(addr);
    println!("{}", status::render(&report));
    match report {
        status::DaemonStatus::NotRunning => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

fn service_command(verb: &str, action: fn() -> anyhow::Result<()>) -> ExitCode {
    init_tracing();
    match action() {
        Ok(()) => {
            println!("promptlyd {verb} complete");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("promptlyd {verb} failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Install the background service, capturing the install-time workspace (resolved
/// to an absolute path) and any port/origin overrides so the managed daemon
/// launches scoped to the player's project rather than the service manager's cwd.
fn install_command(args: InstallArgs) -> ExitCode {
    init_tracing();
    let workspace = match resolve_workspace(args.workspace) {
        Ok(ws) => ws,
        Err(err) => {
            eprintln!("promptlyd install failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    let service_args = service::ServiceArgs {
        workspace: Some(workspace),
        api_port: args.api_port,
        otlp_port: args.otlp_port,
        web_origins: args.web_origins,
    };
    match service::install(service_args) {
        Ok(()) => {
            println!("promptlyd install complete");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("promptlyd install failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the workspace to an absolute, canonical path; default to the cwd.
fn resolve_workspace(arg: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = match arg {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    // Canonicalize so downstream scope checks compare resolved paths, then drop
    // Windows' `\\?\` prefix so the path matches Claude Code's recorded cwd.
    let canonical = std::fs::canonicalize(&raw).unwrap_or(raw);
    Ok(crate::paths::strip_extended_prefix(canonical))
}

/// The foreground run loop: enforce single-instance + single-session, resume from
/// the checkpoint, then run every capture component until interrupted.
async fn run_foreground(config: DaemonConfig, diagnostics: Diagnostics) -> anyhow::Result<()> {
    // A single daemon instance per machine. The level-bound *capture session* is
    // the marker (`18`); the daemon may be idle (no session) until `start`.
    let _process_lock = SessionGuard::acquire(&config.process_lock_path())
        .map_err(|e| anyhow::anyhow!("promptlyd is already running: {e}"))?;
    let _session_lock = SessionGuard::acquire(&config.session_lock_path())
        .map_err(|e| anyhow::anyhow!("a capture session is already active: {e}"))?;

    // The session binding comes from the marker (`18`) — but only a marker bound
    // to THIS daemon's scoped workspace is adopted. A foreign-workspace marker is
    // never the live binding: an active one (a level switch or `down` that skipped
    // `stop`) is superseded — its bootstrap reverted, its marker archived — so a
    // wedged machine self-heals on the next daemon start. Captured turns are then
    // restored from the crash checkpoint only when it belongs to the adopted
    // session, so a refused marker's turns never resurrect into this scope.
    let store = SessionStore::new(config.data_dir.clone());
    let binding = crate::scoping::adopt_marker(&store, &config.workspace, now_ms());
    let (restored_turns, restored_seen, offsets_map) =
        restorable(&binding, Checkpoint::load(&config.checkpoint_path()));
    let jsonl_offsets: SharedOffsets = Arc::new(Mutex::new(offsets_map));

    let (raw_tx, raw_rx) = mpsc::channel(1024);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (engine, shared) = Engine::new(
        raw_rx,
        EngineInit {
            binding,
            checkpoint_path: config.checkpoint_path(),
            jsonl_offsets: Arc::clone(&jsonl_offsets),
            restored_turns,
            restored_seen,
        },
    );
    let session_desc = shared
        .binding()
        .map(|m| m.session_id)
        .unwrap_or_else(|| "idle".to_string());
    tracing::info!(
        session = %session_desc,
        workspace = %config.workspace.display(),
        api = %config.api_addr,
        otlp = %config.otlp_addr,
        "daemon ready",
    );

    // Shared status of the reverse-engineered adapters (`21`); the adapters write
    // it and `GET /health` reads it for `promptly doctor`.
    let adapters = AdapterRegistry::new();
    // Mint this daemon's loopback control token and persist it `0600` in the data
    // dir before the API binds, so only the `promptly` CLI (which can read the file)
    // can drive a control route. A fresh token per start means a stale file can
    // never authenticate to a new daemon.
    let control_token = crate::control_token::write(&config.data_dir, config.api_addr.port())
        .map_err(|e| anyhow::anyhow!("failed to write the control token: {e}"))?;
    // The OTLP receiver's ingest gate: closed until a consented session opens it to
    // its token. Seed it from the restored marker so a resumed active session keeps
    // accepting the harness's telemetry after a daemon restart, and an idle/stopped
    // one rejects every post.
    let ingest_auth = IngestAuth::closed();
    ingest_auth.set_from_marker(shared.binding().as_ref());
    let api_state = ApiState {
        shared: Arc::clone(&shared),
        started_at_ms: now_ms(),
        otlp_endpoint: format!("http://{}", config.otlp_addr),
        diagnostics,
        store: store.clone(),
        workspace: config.workspace.clone(),
        adapters: adapters.clone(),
        web_origins: config.web_origins.clone(),
        // Lets `POST /shutdown` stop the daemon without a signal (the `promptly
        // down` / level-switch path).
        shutdown: shutdown_tx.clone(),
        control_token,
        ingest_auth: ingest_auth.clone(),
    };

    let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();
    tasks.spawn(engine.run(shutdown_rx.clone()));
    tasks.spawn(
        Box::new(OtelSource::new(config.otlp_addr, ingest_auth.clone()))
            .run(raw_tx.clone(), shutdown_rx.clone()),
    );
    tasks.spawn(
        Box::new(JsonlSource::new(
            &config.workspace,
            &config.claude_projects_dir,
            Arc::clone(&jsonl_offsets),
        ))
        .run(raw_tx.clone(), shutdown_rx.clone()),
    );
    // The reverse-engineered local adapters (`21`): each runs behind the same
    // `TelemetrySource` trait, scoped to this workspace, and reports its detection
    // state into the shared registry.
    spawn_adapters(
        &mut tasks,
        &config.workspace,
        &adapters,
        &raw_tx,
        &shutdown_rx,
    );
    tasks.spawn(api::serve(config.api_addr, api_state, shutdown_rx.clone()));

    // Workspace edit-provenance watcher (`18`): while a session is active, track
    // how the allowlisted files evolve and flag a foreign bulk paste for `25`.
    match Scope::new(&config.workspace) {
        Ok(scope) => {
            let (fc_tx, mut fc_rx) = mpsc::channel::<FileChange>(256);
            tasks.spawn(watcher::watch(scope, fc_tx, shutdown_rx.clone()));
            let prov_shared = Arc::clone(&shared);
            tasks.spawn(async move {
                // The tracker is (re)built per session, scoped to its allowlist.
                let mut tracker: Option<(String, ProvenanceTracker)> = None;
                while let Some(change) = fc_rx.recv().await {
                    let Some(marker) = prov_shared.binding().filter(SessionMarker::is_active)
                    else {
                        tracker = None;
                        continue;
                    };
                    if tracker.as_ref().map(|(id, _)| id) != Some(&marker.session_id) {
                        tracker = Some((
                            marker.session_id.clone(),
                            ProvenanceTracker::new(&marker.workspace, &marker.file_allowlist),
                        ));
                    }
                    if let Some((_, t)) = tracker.as_mut() {
                        if let Some(signal) = t.observe(&change) {
                            tracing::info!(
                                path = %signal.path,
                                size = signal.size,
                                "edit-provenance signal: bulk replacement",
                            );
                            prov_shared.record_signal(signal);
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            });
        }
        Err(err) => tracing::warn!(%err, "workspace watcher disabled"),
    }

    // Drop our own sender so the engine's channel closes once all sources stop.
    drop(raw_tx);

    tracing::info!("promptlyd running; press Ctrl-C to stop");
    // Stop on either a Ctrl-C signal or an API-driven shutdown (`POST /shutdown`,
    // the `promptly down` / level-switch path), whichever comes first.
    let mut api_shutdown = shutdown_rx.clone();
    tokio::select! {
        res = tokio::signal::ctrl_c() => match res {
            Ok(()) => tracing::info!("shutdown requested (Ctrl-C); stopping"),
            Err(err) => tracing::error!(%err, "failed to listen for Ctrl-C; stopping"),
        },
        _ = wait_for_shutdown(&mut api_shutdown) => {
            tracing::info!("shutdown requested (API); stopping");
        }
    }
    let _ = shutdown_tx.send(true);

    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::error!(%err, "component error during shutdown"),
            Err(err) => tracing::error!(%err, "component task panicked"),
        }
    }
    tracing::info!("promptlyd stopped");
    Ok(())
}

/// The state to restore from the crash checkpoint: only when the checkpoint
/// belongs to the adopted session. With no adopted binding — including a marker
/// the startup guard refused because it was bound to another workspace — nothing
/// is restored, so a foreign session's turns can never resurrect into this scope.
fn restorable(
    binding: &Option<SessionMarker>,
    checkpoint: Option<Checkpoint>,
) -> (
    Vec<crate::model::NormalizedTurn>,
    Vec<String>,
    HashMap<PathBuf, u64>,
) {
    match (binding, checkpoint) {
        (Some(marker), Some(cp)) if cp.session_id == marker.session_id => {
            tracing::info!(
                turns = cp.turns.len(),
                session = %marker.session_id,
                "resuming session from checkpoint",
            );
            (cp.turns, cp.seen, offsets_from_strings(&cp.jsonl_offsets))
        }
        _ => (Vec::new(), Vec::new(), HashMap::new()),
    }
}

/// Spawn the `21` harness adapters (Cursor, Codex, Copilot) as capture sources,
/// each scoped to `workspace` and publishing into the shared `registry`. They are
/// best-effort: a missing or unreadable source reports a state rather than failing
/// the daemon, so they're spawned unconditionally (the source self-reports
/// `not found`). The one exception is Cursor with no resolvable OS config dir —
/// there's nowhere to look, so we record that directly.
fn spawn_adapters(
    tasks: &mut JoinSet<anyhow::Result<()>>,
    workspace: &Path,
    registry: &AdapterRegistry,
    raw_tx: &mpsc::Sender<RawTurn>,
    shutdown_rx: &watch::Receiver<bool>,
) {
    match crate::paths::cursor_user_dir() {
        Some(dir) => {
            tasks.spawn(
                Box::new(CursorSource::new(&dir, workspace, registry.clone()))
                    .run(raw_tx.clone(), shutdown_rx.clone()),
            );
        }
        None => registry.set(
            cursor::NAME,
            AdapterState::NotFound,
            "no OS config dir to locate Cursor storage",
        ),
    }

    tasks.spawn(
        Box::new(CodexSource::new(
            &crate::paths::codex_sessions_dir(),
            workspace,
            registry.clone(),
        ))
        .run(raw_tx.clone(), shutdown_rx.clone()),
    );

    // Copilot searches every VS Code-family install; an empty list self-reports.
    tasks.spawn(
        Box::new(CopilotSource::new(
            &crate::paths::vscode_user_dirs(),
            workspace,
            registry.clone(),
        ))
        .run(raw_tx.clone(), shutdown_rx.clone()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{sample_raw, Source};
    use crate::normalize::normalize;
    use crate::scoping::{NonceOrigin, SESSION_MARKER_VERSION};

    fn marker(session_id: &str) -> SessionMarker {
        SessionMarker {
            version: SESSION_MARKER_VERSION,
            session_id: session_id.into(),
            workspace: PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01".into(),
            started_at_ms: 1_000,
            stopped_at_ms: None,
            attempt_nonce: "n".into(),
            nonce_origin: NonceOrigin::Local,
            file_allowlist: Vec::new(),
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    fn checkpoint(session_id: &str) -> Checkpoint {
        Checkpoint {
            version: crate::checkpoint::CHECKPOINT_VERSION,
            session_id: session_id.into(),
            started_at_ms: 1_000,
            turns: vec![normalize(&sample_raw(
                Source::Otel,
                Some("claude-opus-4-8"),
                100,
                50,
            ))],
            jsonl_offsets: HashMap::new(),
            seen: vec!["seen-1".into()],
        }
    }

    #[test]
    fn a_checkpoint_for_the_adopted_session_is_restored() {
        let binding = Some(marker("s1"));
        let (turns, seen, _) = restorable(&binding, Some(checkpoint("s1")));
        assert_eq!(turns.len(), 1, "the session's own turns come back");
        assert_eq!(seen, vec!["seen-1".to_string()]);
    }

    #[test]
    fn no_adopted_binding_restores_nothing() {
        // The startup guard refused the marker (foreign workspace) — its
        // checkpoint turns must NOT resurrect into this daemon's scope.
        let (turns, seen, offsets) = restorable(&None, Some(checkpoint("s1")));
        assert!(turns.is_empty() && seen.is_empty() && offsets.is_empty());
    }

    #[test]
    fn a_checkpoint_for_another_session_is_ignored() {
        let binding = Some(marker("s2"));
        let (turns, seen, _) = restorable(&binding, Some(checkpoint("s1")));
        assert!(turns.is_empty() && seen.is_empty());
    }
}
