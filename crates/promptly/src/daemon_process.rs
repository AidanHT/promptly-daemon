//! Auto-management of the local `promptlyd` daemon from the CLI.
//!
//! A player should never have to launch the capture daemon by hand. The session
//! commands (`start`, `play`, `up`) call [`ensure_running`], which:
//!
//! - reuses a daemon that's already up and scoped to this level's folder,
//! - relaunches it when it's bound to a *different* folder (you switched levels),
//! - spawns a fresh background `promptlyd run` when none is up,
//!
//! and `down` stops it via the daemon's `POST /shutdown` route ([`stop_background`]).
//! The spawned daemon is detached from the terminal and redirects its output to
//! `~/.promptly/promptlyd.log`; every endpoint it binds is loopback.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use promptlyd::session::SessionGuard;

use crate::daemon_client::{DaemonApi, DaemonClient, DaemonError, Health};
use crate::style::Style;

/// How long to wait for a freshly-spawned daemon to answer `/health`.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a daemon we asked to stop to release its port.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How often to poll the loopback API while waiting on a state change.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// What [`ensure_running`] had to do, so the caller can word its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ensured {
    /// A daemon scoped to this workspace was already up — nothing to do.
    AlreadyRunning,
    /// No daemon was running; one was spawned in the background.
    Started,
    /// A daemon bound to a different workspace was stopped and relaunched here.
    Restarted,
}

/// Make sure a background daemon is running and scoped to `workspace`, spawning or
/// relaunching `promptlyd` as needed. Returns what it had to do.
pub fn ensure_running(api_port: u16, workspace: &Path, style: Style) -> anyhow::Result<Ensured> {
    let client = DaemonClient::new(api_port);
    match probe(client.health()) {
        PortState::Daemon(health) => {
            if same_workspace(&health.workspace, workspace) {
                Ok(Ensured::AlreadyRunning)
            } else {
                // A daemon is up but watching a different folder (you switched
                // levels) — stop it and relaunch scoped to this one.
                let was = if health.workspace.is_empty() {
                    "another workspace".to_string()
                } else {
                    health.workspace.clone()
                };
                eprintln!(
                    "{}",
                    style.dim(&format!(
                        "daemon was watching {was} — switching it to this level"
                    ))
                );
                // End its open capture session first (best-effort), so even an
                // older daemon reverts the previous workspace's harness settings
                // at switch time instead of leaving its OTEL bootstrap behind.
                if let Some(slug) = end_active_session(&client) {
                    eprintln!(
                        "{}",
                        style.dim(&format!("ended the open capture session for {slug}"))
                    );
                }
                stop_and_wait(&client)?;
                spawn_and_wait(api_port, workspace, style)?;
                Ok(Ensured::Restarted)
            }
        }
        PortState::Free => {
            spawn_and_wait(api_port, workspace, style)?;
            Ok(Ensured::Started)
        }
        // Something answers on the port but it isn't our daemon, so we can't bind
        // it — spawning would just time out. Fail fast, naming the clash and the
        // `--api-port` escape hatch, instead of the raw `HTTP 404` the probe saw.
        PortState::Foreign(err) => anyhow::bail!(
            "{} (its health check returned: {err})",
            foreign_port_message(api_port),
        ),
    }
}

/// What [`stop_background`] found on the daemon's control port and did about it,
/// so the caller can word its message.
#[derive(Debug)]
pub enum BackgroundStop {
    /// A running daemon was asked to stop and has fully exited. Carries the slug
    /// of the capture session that was still open and was ended first, if any —
    /// ending it reverts the workspace's harness settings, and leaving it active
    /// with no daemon would wedge every later `start`.
    Stopped { ended_session: Option<String> },
    /// Nothing was listening on the control port — no daemon to stop.
    NotRunning,
    /// The port is held by a process that isn't the Promptly daemon, so there was
    /// nothing of ours to stop. Carries a message describing the clash.
    ForeignPort(String),
}

/// Stop a running background daemon, if any — ending its open capture session
/// first, so the shutdown can't strand an active marker (and a bootstrapped
/// workspace) with no daemon left to stop it. A foreign process squatting the
/// port is reported (not an error): our daemon isn't running there, so there is
/// nothing to stop — and, crucially for `promptly update`, its binary is free to
/// replace.
pub fn stop_background(api_port: u16) -> anyhow::Result<BackgroundStop> {
    let client = DaemonClient::new(api_port);
    match probe(client.health()) {
        PortState::Daemon(_) => {
            let ended_session = end_active_session(&client);
            stop_and_wait(&client)?;
            Ok(BackgroundStop::Stopped { ended_session })
        }
        PortState::Free => Ok(BackgroundStop::NotRunning),
        PortState::Foreign(_) => Ok(BackgroundStop::ForeignPort(foreign_port_message(api_port))),
    }
}

/// Best-effort: end the daemon's active capture session — reverting its
/// workspace's harness settings — before the daemon is stopped or relaunched.
/// Returns the ended session's slug, or `None` when nothing was active or the
/// session couldn't be read/stopped (never an error: the shutdown proceeds, and
/// the daemon's own start-time supersede is the safety net).
fn end_active_session(client: &dyn DaemonApi) -> Option<String> {
    let snapshot = client.session().ok()?;
    let marker = snapshot.session?;
    if !marker.is_active() {
        return None;
    }
    match client.stop() {
        Ok(report) => Some(report.marker.map_or(marker.slug, |m| m.slug)),
        Err(_) => None,
    }
}

/// How the daemon's control port answered a `/health` probe.
enum PortState {
    /// A healthy Promptly daemon answered — only our daemon's `/health` JSON
    /// deserializes into a [`Health`], so an `Ok` here is unambiguous.
    Daemon(Box<Health>),
    /// Nothing is listening — the port is free (the connection was refused).
    Free,
    /// Something answered but it isn't a Promptly daemon (a foreign server on the
    /// port). Carries the probe error for the diagnostic message.
    Foreign(DaemonError),
}

/// Classify a `/health` probe: a refused connection is a free port, a valid
/// [`Health`] body is our daemon, and anything else (an HTTP error status, or a
/// body that isn't our health JSON) is a foreign process holding the port.
fn probe(health: Result<Health, DaemonError>) -> PortState {
    match health {
        Ok(health) => PortState::Daemon(Box::new(health)),
        Err(DaemonError::NotRunning(_)) => PortState::Free,
        Err(err) => PortState::Foreign(err),
    }
}

/// The message shown when the daemon's control port is held by another process.
/// Shared so the "can't start" error and the "nothing to stop" note describe the
/// clash the same way, and both name the `--api-port` escape hatch.
fn foreign_port_message(api_port: u16) -> String {
    format!(
        "port {api_port} is in use by another process that isn't the Promptly daemon — \
         stop that process, or pass `--api-port <port>` to use a different port"
    )
}

/// Ask the daemon to stop, then wait until it has *fully exited* — signalled by
/// its single-instance lock going free — so a relaunch can re-acquire that lock
/// without racing the old process, and `promptly down` only claims success once
/// the daemon is really gone. The lock outlives a refused `/health`: the API port
/// goes quiet first, while the background watchers/adapters drain on their own
/// poll ticks and only then is the lock released.
fn stop_and_wait(client: &DaemonClient) -> anyhow::Result<()> {
    client.shutdown().context("asking the daemon to stop")?;
    let lock = promptlyd::paths::process_lock_path();
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        thread::sleep(POLL_INTERVAL);
        if SessionGuard::is_free(&lock) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the running daemon didn't stop in time — try `promptly down`, then retry"
            );
        }
    }
}

/// Spawn `promptlyd run` in the background scoped to `workspace`, then poll until
/// it answers `/health` (or give up with a pointer to its log).
fn spawn_and_wait(api_port: u16, workspace: &Path, style: Style) -> anyhow::Result<()> {
    let exe = locate_promptlyd();
    let log_path = log_file_path();
    eprintln!(
        "{}",
        style.dim(&format!(
            "starting the capture daemon in the background (log: {})",
            log_path.display()
        ))
    );
    spawn_detached(&exe, workspace, api_port, &log_path)
        .with_context(|| format!("launching {}", exe.display()))?;

    let client = DaemonClient::new(api_port);
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        thread::sleep(POLL_INTERVAL);
        if client.health().is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the daemon didn't come up within {}s — see its log at {}",
                READY_TIMEOUT.as_secs(),
                log_path.display(),
            );
        }
    }
}

/// Locate the `promptlyd` binary: prefer the one installed next to this `promptly`
/// binary (the release archive and `cargo install` keep them together), else fall
/// back to the bare name so the OS resolves it on `PATH`.
fn locate_promptlyd() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(daemon_bin_name());
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("promptlyd")
}

/// The `promptlyd` binary's file name for the current platform.
fn daemon_bin_name() -> &'static str {
    if cfg!(windows) {
        "promptlyd.exe"
    } else {
        "promptlyd"
    }
}

/// Where the background daemon's stdout/stderr are redirected
/// (`~/.promptly/promptlyd.log`).
fn log_file_path() -> PathBuf {
    promptlyd::paths::data_dir().join("promptlyd.log")
}

/// Spawn the daemon detached so it outlives this CLI invocation, with its output
/// redirected to the log file (and no console window on Windows).
fn spawn_detached(
    exe: &Path,
    workspace: &Path,
    api_port: u16,
    log_path: &Path,
) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::process::Stdio;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening the daemon log at {}", log_path.display()))?;
    let err = out.try_clone()?;

    let mut cmd = Command::new(exe);
    cmd.arg("run")
        .arg("--workspace")
        .arg(workspace)
        .arg("--api-port")
        .arg(api_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    detach(&mut cmd);
    cmd.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: the background daemon runs with no console window popping
    // up; combined with redirected stdio it's detached from this terminal.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    // Rust spawns with bInheritHandles=TRUE on Windows, so without this the
    // long-lived daemon would also inherit *this CLI's own* stdout/stderr handles.
    // When our output is a pipe (`promptly start | tee`, a CI step, an editor task,
    // or any parent capturing it), the daemon holding that pipe's write end means
    // the reader never sees EOF — so `promptly start`/`play`/`up`/`watch` would
    // appear to hang forever even though the CLI already returned. Clearing the
    // inherit flag on our std handles confines the child to the log-file handles we
    // pass it explicitly. (No-op when stdout is a console; harmless when it's a
    // file.)
    dont_leak_std_handles();
}

/// Clear `HANDLE_FLAG_INHERIT` on this process's stdout/stderr so a detached child
/// spawned next can't inherit (and thus pin open) handles we own. Best-effort: any
/// failure just leaves the pre-fix behavior. `kernel32` is already linked by `std`.
#[cfg(windows)]
fn dont_leak_std_handles() {
    use std::os::windows::io::{AsRawHandle, RawHandle};

    extern "system" {
        fn SetHandleInformation(h: RawHandle, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

    let handles = [
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    // SAFETY: `SetHandleInformation` is a thread-safe kernel32 call; we only clear
    // one flag on our own std handles and skip null / INVALID_HANDLE_VALUE (-1).
    unsafe {
        for h in handles {
            if !h.is_null() && h as isize != -1 {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(not(windows))]
fn detach(_cmd: &mut Command) {
    // On Unix the spawned child keeps running after this CLI exits — we never wait
    // on it — and its stdio is already redirected to the log file.
}

/// Does the daemon's reported workspace refer to the same folder as `requested`?
/// Compares canonicalized paths so `.`/`..`/symlinks/trailing slashes don't cause
/// a spurious "different workspace" relaunch. Also used by read-only `watch` to
/// note when the attached session lives in a different folder than the cwd.
pub(crate) fn same_workspace(reported: &str, requested: &Path) -> bool {
    if reported.is_empty() {
        return false;
    }
    let reported = PathBuf::from(reported);
    match (
        std::fs::canonicalize(&reported),
        std::fs::canonicalize(requested),
    ) {
        (Ok(a), Ok(b)) => a == b,
        // If either path can't be canonicalized (e.g. it no longer exists), fall
        // back to a literal compare rather than guessing they're the same.
        _ => reported == requested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_client::{
        ResetReport, SessionMarker, SessionSnapshot, StartDecisions, StartOutcome, StartPlan,
        StopReport,
    };
    use std::cell::RefCell;

    /// A daemon fake exposing a session snapshot and recording `stop` calls.
    struct FakeSessionDaemon {
        marker: Option<SessionMarker>,
        stopped: RefCell<bool>,
    }

    impl FakeSessionDaemon {
        fn new(marker: Option<SessionMarker>) -> Self {
            Self {
                marker,
                stopped: RefCell::new(false),
            }
        }
    }

    impl DaemonApi for FakeSessionDaemon {
        fn health(&self) -> Result<Health, DaemonError> {
            unreachable!()
        }
        fn session(&self) -> Result<SessionSnapshot, DaemonError> {
            Ok(SessionSnapshot {
                session: self.marker.clone(),
                totals: Default::default(),
                turns: 0,
                signals: Vec::new(),
                captured: Vec::new(),
            })
        }
        fn preflight(&self) -> Result<StartPlan, DaemonError> {
            unreachable!()
        }
        fn start(&self, _decisions: StartDecisions) -> Result<StartOutcome, DaemonError> {
            unreachable!()
        }
        fn stop(&self) -> Result<StopReport, DaemonError> {
            *self.stopped.borrow_mut() = true;
            Ok(StopReport {
                marker: self.marker.clone().map(|mut m| {
                    m.stopped_at_ms = Some(2_000);
                    m
                }),
                reverted_bootstrap: true,
            })
        }
        fn reset(&self) -> Result<ResetReport, DaemonError> {
            unreachable!()
        }
    }

    fn session_marker(stopped_at_ms: Option<i64>) -> SessionMarker {
        SessionMarker {
            version: 1,
            session_id: "s1".into(),
            workspace: std::path::PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01-lru".into(),
            started_at_ms: 1_000,
            stopped_at_ms,
            attempt_nonce: "n".into(),
            nonce_origin: promptlyd::scoping::NonceOrigin::Local,
            file_allowlist: Vec::new(),
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    #[test]
    fn end_active_session_stops_an_open_session_and_names_it() {
        let fake = FakeSessionDaemon::new(Some(session_marker(None)));
        assert_eq!(
            end_active_session(&fake).as_deref(),
            Some("stage-1-01-lru"),
            "the ended session is named for the message"
        );
        assert!(*fake.stopped.borrow(), "stop was driven");
    }

    #[test]
    fn end_active_session_leaves_a_stopped_or_absent_session_alone() {
        // Already stopped: nothing to end (a second stop would be noise).
        let fake = FakeSessionDaemon::new(Some(session_marker(Some(1_500))));
        assert!(end_active_session(&fake).is_none());
        assert!(!*fake.stopped.borrow(), "stop was never driven");

        // Idle daemon: nothing to end.
        let fake = FakeSessionDaemon::new(None);
        assert!(end_active_session(&fake).is_none());
        assert!(!*fake.stopped.borrow());
    }

    fn health_fixture() -> Health {
        Health {
            status: "ok".into(),
            version: "0.1.8".into(),
            workspace: "/ws".into(),
            uptime_ms: 0,
            otlp_endpoint: String::new(),
            turns: 0,
            recent_errors: Vec::new(),
            adapters: Vec::new(),
        }
    }

    #[test]
    fn probe_classifies_a_daemon_a_free_port_and_a_foreign_server() {
        // Only our daemon's /health deserializes into `Health`, so an `Ok` is our
        // daemon unambiguously.
        assert!(matches!(probe(Ok(health_fixture())), PortState::Daemon(_)));
        // A refused connection (nothing listening) is a free port.
        assert!(matches!(
            probe(Err(DaemonError::NotRunning("addr".into()))),
            PortState::Free
        ));
        // The reported bug: an HTTP error status from a non-daemon server on the
        // port is `Foreign`, not `Free` — so the caller neither treats it as our
        // daemon nor as an empty port.
        assert!(matches!(
            probe(Err(DaemonError::Api("HTTP 404".into()))),
            PortState::Foreign(_)
        ));
        // A body that isn't our health JSON is equally foreign.
        assert!(matches!(
            probe(Err(DaemonError::Decode("expected value".into()))),
            PortState::Foreign(_)
        ));
    }

    #[test]
    fn foreign_port_message_names_the_port_and_the_escape_hatch() {
        let msg = foreign_port_message(8765);
        assert!(msg.contains("8765"), "{msg}");
        assert!(msg.contains("--api-port"), "{msg}");
    }

    #[test]
    fn daemon_bin_name_has_the_platform_extension() {
        if cfg!(windows) {
            assert_eq!(daemon_bin_name(), "promptlyd.exe");
        } else {
            assert_eq!(daemon_bin_name(), "promptlyd");
        }
    }

    #[test]
    fn an_empty_reported_workspace_is_never_a_match() {
        // An older daemon (or one that reported no workspace) must force a relaunch
        // rather than be mistaken for the current folder.
        assert!(!same_workspace("", Path::new(".")));
    }

    #[test]
    fn the_same_folder_matches_through_canonicalization() {
        // A real, existing dir compared against itself via two spellings (the dir,
        // and the dir joined with ".") is treated as the same workspace.
        let dir = std::env::temp_dir();
        let reported = dir.to_string_lossy().to_string();
        assert!(same_workspace(&reported, &dir.join(".")));
    }

    #[test]
    fn different_folders_do_not_match() {
        let a = std::env::temp_dir();
        let b = std::env::temp_dir().join("promptly-nonexistent-xyz-1234");
        assert!(!same_workspace(&a.to_string_lossy(), &b));
    }

    #[test]
    fn the_log_lives_under_the_promptly_data_dir() {
        let log = log_file_path();
        let data = promptlyd::paths::data_dir();
        assert!(log.ends_with("promptlyd.log"));
        assert_eq!(log.parent(), Some(data.as_path()));
    }
}
