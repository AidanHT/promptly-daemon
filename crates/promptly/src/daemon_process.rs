//! Auto-management of the local `promptlyd` daemon from the CLI.
//!
//! A player should never have to launch the capture daemon by hand. The session
//! commands (`start`, `watch`, `play`, `up`) call [`ensure_running`], which:
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

use crate::daemon_client::{DaemonApi, DaemonClient, DaemonError};
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
    match client.health() {
        Ok(health) => {
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
                stop_and_wait(&client)?;
                spawn_and_wait(api_port, workspace, style)?;
                Ok(Ensured::Restarted)
            }
        }
        Err(DaemonError::NotRunning(_)) => {
            spawn_and_wait(api_port, workspace, style)?;
            Ok(Ensured::Started)
        }
        Err(err) => Err(err).context("checking whether the daemon is running"),
    }
}

/// Stop a running background daemon, if any. Returns whether one was stopped, so
/// `down` can say "stopped" vs "nothing was running".
pub fn stop_background(api_port: u16) -> anyhow::Result<bool> {
    let client = DaemonClient::new(api_port);
    match client.health() {
        Err(DaemonError::NotRunning(_)) => Ok(false),
        Err(err) => Err(err).context("checking whether the daemon is running"),
        Ok(_) => {
            stop_and_wait(&client)?;
            Ok(true)
        }
    }
}

/// Ask the daemon to stop, then wait until its loopback port goes quiet — so a
/// relaunch can re-acquire the single-instance lock without racing the old one.
fn stop_and_wait(client: &DaemonClient) -> anyhow::Result<()> {
    client.shutdown().context("asking the daemon to stop")?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        thread::sleep(POLL_INTERVAL);
        if matches!(client.health(), Err(DaemonError::NotRunning(_))) {
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
    // up; combined with redirected stdio it's fully detached from this terminal.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn detach(_cmd: &mut Command) {
    // On Unix the spawned child keeps running after this CLI exits — we never wait
    // on it — and its stdio is already redirected to the log file.
}

/// Does the daemon's reported workspace refer to the same folder as `requested`?
/// Compares canonicalized paths so `.`/`..`/symlinks/trailing slashes don't cause
/// a spurious "different workspace" relaunch.
fn same_workspace(reported: &str, requested: &Path) -> bool {
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
