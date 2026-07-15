//! Idle auto-shutdown: a daemon with nothing to capture shouldn't outlive its
//! usefulness. A detached `promptlyd` used to run until an explicit `promptly
//! down` (or a level switch relaunched it), quietly pinning system resources —
//! and, on Windows, whatever directory it happened to hold. The watchdog here
//! shuts the daemon down gracefully once it has been idle for the configured
//! window, through the same shutdown channel as `POST /shutdown` and Ctrl-C, so
//! every component drains and checkpoints exactly as a manual stop would.
//!
//! "Idle" means no ACTIVE capture session and no CLI control activity. An active
//! session is never idle — a player mid-think must not lose their capture, no
//! matter how quiet the harness is — so the watchdog re-stamps the tracker for
//! as long as one is bound. Passive reads (`GET /health`, the web HUD's
//! `/stream`) deliberately do NOT count as activity: a forgotten browser tab
//! must not keep the daemon alive forever.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::clock::now_ms;
use crate::engine::SharedState;
use crate::scoping::SessionMarker;
use crate::sources::{wait_for_shutdown, Shutdown};

/// Default idle window before the daemon shuts itself down.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 15 * 60;

/// How often the watchdog re-checks. Coarse on purpose: the shutdown lands
/// within a poll of the deadline, and an idle daemon does near-zero work.
const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// The last instant the daemon saw capture-relevant activity, shared between the
/// API handlers (which stamp it on CLI control requests) and the watchdog.
#[derive(Debug)]
pub struct IdleTracker(AtomicI64);

impl IdleTracker {
    pub fn new(now_ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(now_ms)))
    }

    /// Record activity. Monotonic (`fetch_max`), so a stale stamp never rewinds
    /// a newer one under concurrency.
    pub fn touch(&self, now_ms: i64) {
        self.0.fetch_max(now_ms, Ordering::Relaxed);
    }

    pub fn last_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Pure deadline test, so the policy is unit-testable without the async loop.
pub fn timed_out(last_activity_ms: i64, now_ms: i64, timeout: Duration) -> bool {
    now_ms.saturating_sub(last_activity_ms) >= timeout.as_millis() as i64
}

/// Run the idle watchdog until it either observes daemon shutdown or triggers
/// one. Spawned only when an idle timeout is configured.
pub async fn watchdog(
    shared: Arc<SharedState>,
    tracker: Arc<IdleTracker>,
    timeout: Duration,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    loop {
        // Check first, sleep second, so the deadline is honored on the next
        // poll after it passes (and tests with a zero window need no clock).
        let now = now_ms();
        if shared.binding().filter(SessionMarker::is_active).is_some() {
            tracker.touch(now);
        } else if timed_out(tracker.last_ms(), now, timeout) {
            tracing::info!(
                "idle for {} minutes with no active capture session; shutting down — `promptly play`/`promptly up` relaunches the daemon",
                timeout.as_secs() / 60,
            );
            let _ = shutdown_tx.send(true);
            return Ok(());
        }
        tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return Ok(()),
            () = tokio::time::sleep(CHECK_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_stamps_monotonically() {
        let tracker = IdleTracker::new(100);
        tracker.touch(500);
        tracker.touch(300); // stale stamp must not rewind
        assert_eq!(tracker.last_ms(), 500);
    }

    #[test]
    fn timed_out_only_at_or_past_the_deadline() {
        let timeout = Duration::from_secs(900);
        assert!(!timed_out(0, 899_999, timeout));
        assert!(timed_out(0, 900_000, timeout));
        assert!(timed_out(0, 900_001, timeout));
        // A clock that regressed below the stamp is not a timeout.
        assert!(!timed_out(1_000_000, 900_000, timeout));
    }

    /// End-to-end over the async loop with a zero window: an idle daemon flips
    /// the shutdown channel on the first check; the watchdog then returns.
    #[tokio::test]
    async fn watchdog_shuts_an_idle_daemon_down() {
        let shared = SharedState::new(None, Vec::new());
        let tracker = IdleTracker::new(now_ms());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(watchdog(
            Arc::clone(&shared),
            Arc::clone(&tracker),
            Duration::from_secs(0), // already past due at the first check
            shutdown_tx.clone(),
            shutdown_rx.clone(),
        ));
        handle.await.unwrap().unwrap();
        assert!(
            *shutdown_rx.borrow(),
            "the shutdown channel must be flipped"
        );
    }

    /// The watchdog exits quietly when something else shuts the daemon down.
    #[tokio::test]
    async fn watchdog_exits_on_external_shutdown() {
        let shared = SharedState::new(None, Vec::new());
        let tracker = IdleTracker::new(now_ms());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(watchdog(
            Arc::clone(&shared),
            Arc::clone(&tracker),
            Duration::from_secs(3600), // far away — only the external signal ends it
            shutdown_tx.clone(),
            shutdown_rx,
        ));
        shutdown_tx.send(true).unwrap();
        handle.await.unwrap().unwrap();
    }
}
