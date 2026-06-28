//! Session scoping — what makes captured telemetry trustworthy (`18`).
//!
//! Without scoping the daemon would count every Claude Code turn on the machine
//! toward a Promptly attempt. This module binds a capture session to a specific
//! level and workspace, issues the attempt nonce telemetry is stamped with, and
//! guards the start with the baseline integrity check, so the clock and capture
//! always begin from the genuine starter.
//!
//! The lifecycle is `preflight` → `start` → `stop` (and `reset`). `preflight`
//! computes — without side effects — what a start would do, so the CLI (`19`) can
//! show the bound level, surface a baseline mismatch, and name the settings the
//! bootstrap would change before asking for consent. `start` then executes with
//! the player's decisions. The authoritative session state lives in a **session
//! marker** the engine reads for attribution and a restart reloads.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::baseline::{
    self, BaselineStatus, CachedStarter, CanonicalStarter, ResetError, ResetReport,
};
use crate::bootstrap::{self, BootstrapState};
use crate::manifest::{Manifest, ManifestError};
use crate::sources::jsonl::{cwd_matches, normalize_for_compare};

/// Bump when the marker's on-disk shape changes; an older file is ignored.
pub const SESSION_MARKER_VERSION: u32 = 1;
/// The session marker file under the daemon's data dir.
pub const MARKER_FILE: &str = "session.json";
/// Sub-dir of the data dir caching canonical starters for offline resets.
pub const CACHE_DIR: &str = "cache";

/// Where the attempt nonce came from. A local nonce can't be server-verified, so
/// an offline-started attempt caps at `unverified` integrity; cloud issuance
/// (`20`) is what makes a `verified` capture possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NonceOrigin {
    Local,
    Server,
}

/// The authoritative state of the current capture session: its identity, the
/// level/workspace it's bound to, the attempt nonce, and the bootstrap state
/// needed to revert. Persisted as `session.json`; the engine reads it to attribute
/// turns and a restart reloads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMarker {
    pub version: u32,
    pub session_id: String,
    pub workspace: PathBuf,
    pub level_id: String,
    pub slug: String,
    pub started_at_ms: i64,
    /// `None` while the session is active; set when stopped.
    #[serde(default)]
    pub stopped_at_ms: Option<i64>,
    /// Stamped into every attributed turn and reconciled at submit (`20`/`12`).
    pub attempt_nonce: String,
    pub nonce_origin: NonceOrigin,
    /// The manifest's editable-file globs — the scope for edit-provenance (`18`).
    #[serde(default)]
    pub file_allowlist: Vec<String>,
    /// Times the workspace was reset to the canonical starter (reconciled to the
    /// attempt's `code_reset_count` at submit).
    #[serde(default)]
    pub code_reset_count: u32,
    /// Recorded harness-bootstrap state for revert; `None` = JSONL-only capture
    /// (consent declined), which the daemon flags as lower confidence.
    #[serde(default)]
    pub bootstrap: Option<BootstrapState>,
}

impl SessionMarker {
    /// Is the capture window currently open?
    pub fn is_active(&self) -> bool {
        self.stopped_at_ms.is_none()
    }

    /// The best integrity status this attempt can reach (see [`NonceOrigin`]).
    pub fn integrity_cap(&self) -> &'static str {
        match self.nonce_origin {
            NonceOrigin::Local => "unverified",
            NonceOrigin::Server => "verified",
        }
    }

    /// Should a turn observed at `turn_ts` from `turn_workspace` be attributed to
    /// this session? Window-based (robust to processing lag): the turn must fall
    /// within `[started, stopped]` and originate from the bound workspace. A turn
    /// carrying no cwd is accepted — loopback binding plus project-scoped bootstrap
    /// already constrain the source — matching the JSONL watcher's rule.
    pub fn attributes(&self, turn_ts: i64, turn_workspace: Option<&str>) -> bool {
        if turn_ts < self.started_at_ms {
            return false;
        }
        if let Some(stopped) = self.stopped_at_ms {
            if turn_ts > stopped {
                return false;
            }
        }
        match turn_workspace {
            Some(cwd) => cwd_matches(
                cwd,
                &normalize_for_compare(&self.workspace.to_string_lossy()),
            ),
            None => true,
        }
    }
}

/// Reads and writes the session marker and resolves the canonical-starter cache,
/// rooted at the daemon's data dir.
#[derive(Debug, Clone)]
pub struct SessionStore {
    data_dir: PathBuf,
}

impl SessionStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub fn marker_path(&self) -> PathBuf {
        self.data_dir.join(MARKER_FILE)
    }

    /// Where a level's pristine starter is cached for offline resets.
    pub fn cache_dir(&self, level_id: &str, kit_version: u32) -> PathBuf {
        self.data_dir
            .join(CACHE_DIR)
            .join(level_id)
            .join(format!("v{kit_version}"))
    }

    /// Load the current marker, or `None` if absent/corrupt/from another version.
    pub fn load_marker(&self) -> Option<SessionMarker> {
        let bytes = std::fs::read(self.marker_path()).ok()?;
        match serde_json::from_slice::<SessionMarker>(&bytes) {
            Ok(marker) if marker.version == SESSION_MARKER_VERSION => Some(marker),
            Ok(_) => {
                tracing::warn!("session marker version mismatch; ignoring");
                None
            }
            Err(err) => {
                tracing::warn!(%err, "corrupt session marker; ignoring");
                None
            }
        }
    }

    /// Persist the marker atomically (temp file + rename).
    pub fn save_marker(&self, marker: &SessionMarker) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let path = self.marker_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(marker).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &path)
    }
}

/// The level a session is bound to, surfaced to the CLI/`/session`.
#[derive(Debug, Clone, Serialize)]
pub struct LevelBinding {
    pub level_id: String,
    pub slug: String,
    pub title: String,
    pub language: String,
    pub runtime_version: String,
    pub execution_harness: String,
}

impl LevelBinding {
    fn from_manifest(m: &Manifest) -> Self {
        Self {
            level_id: m.level_id.clone(),
            slug: m.slug.clone(),
            title: m.title.clone(),
            language: m.language.clone(),
            runtime_version: m.runtime_version.clone(),
            execution_harness: m.execution_harness.clone(),
        }
    }
}

/// Whether a start begins a new attempt or resumes the bound one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StartKind {
    Fresh,
    Resume,
}

/// The player's answers to the prompts a fresh start may raise, plus the optional
/// server-issued nonce that lifts the attempt's integrity ceiling (`20`).
#[derive(Debug, Clone, Default)]
pub struct StartDecisions {
    /// Proceed with backup + reset when the workspace doesn't match the baseline.
    pub confirm_reset: bool,
    /// Inject the OTEL env into the project settings (else JSONL-only capture).
    pub consent_bootstrap: bool,
    /// A server-issued attempt nonce (`20`). When present on a *fresh* start the
    /// attempt binds to it with [`NonceOrigin::Server`], so the capture can reach
    /// `verified`; absent, a local nonce is generated and the attempt caps at
    /// `unverified`. Ignored on resume — the bound attempt keeps its own nonce.
    pub server_nonce: Option<String>,
}

/// A baseline mismatch the player must resolve before a fresh start proceeds.
#[derive(Debug, Clone, Serialize)]
pub struct BaselineMismatch {
    pub expected: String,
    pub computed: String,
    /// Whether the workspace can be reset offline (a cached starter is available).
    pub can_reset: bool,
}

/// A non-destructive preview of what `start` would do — the bound level, the
/// baseline status (fresh only), and the settings keys the bootstrap would write.
#[derive(Debug, Clone, Serialize)]
pub struct StartPlan {
    pub level: LevelBinding,
    pub kind: StartKind,
    /// `None` for a resume (no baseline check on an in-progress attempt).
    pub baseline: Option<BaselineStatus>,
    pub can_reset: bool,
    pub bootstrap_keys: Vec<&'static str>,
    pub bootstrap_already_applied: bool,
    pub integrity_cap: &'static str,
}

/// The result of a successful start.
#[derive(Debug, Clone, Serialize)]
pub struct StartedSession {
    pub marker: SessionMarker,
    pub kind: StartKind,
    pub level: LevelBinding,
    /// Set when a baseline mismatch was reset before starting.
    pub reset: Option<ResetReport>,
    pub bootstrap_applied: bool,
    /// True when capture is JSONL-only (consent declined) — lower confidence.
    pub jsonl_only: bool,
    pub integrity_cap: &'static str,
}

/// The outcome of `start`: either the session began, or it needs the player to
/// confirm a baseline reset first (returned without side effects).
#[derive(Debug)]
pub enum StartOutcome {
    Started(Box<StartedSession>),
    NeedsResetConfirmation(BaselineMismatch),
}

/// A hard failure that aborts a start (as opposed to a decision the player owns).
#[derive(Debug, Error)]
pub enum StartError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("a capture session is already active for {0} — stop it before starting another")]
    SessionActiveElsewhere(PathBuf),
    #[error("cannot reset the workspace: {0}")]
    CannotReset(#[from] ResetError),
    #[error("session I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// The result of `stop`.
#[derive(Debug, Clone, Serialize)]
pub struct StopOutcome {
    /// The stopped session, or `None` if nothing was active.
    pub marker: Option<SessionMarker>,
    pub reverted_bootstrap: bool,
}

fn same_workspace(a: &Path, b: &Path) -> bool {
    normalize_for_compare(&a.to_string_lossy()) == normalize_for_compare(&b.to_string_lossy())
}

/// Does the current marker bind this same workspace + level (so a start resumes
/// rather than re-checking the baseline)?
fn is_resume(marker: &SessionMarker, workspace: &Path, level_id: &str) -> bool {
    same_workspace(&marker.workspace, workspace) && marker.level_id == level_id
}

/// Preview what a start would do, without touching anything.
pub fn preflight(
    workspace: &Path,
    otlp_endpoint: &str,
    store: &SessionStore,
) -> Result<StartPlan, StartError> {
    let manifest = Manifest::load(workspace)?;
    let level = LevelBinding::from_manifest(&manifest);
    let existing = store.load_marker();
    let bootstrap_plan = bootstrap::plan(workspace, otlp_endpoint)?;

    if let Some(marker) = existing {
        if is_resume(&marker, workspace, &manifest.level_id) {
            return Ok(StartPlan {
                level,
                kind: StartKind::Resume,
                baseline: None,
                can_reset: false,
                bootstrap_keys: bootstrap_plan.keys,
                bootstrap_already_applied: bootstrap_plan.already_applied,
                integrity_cap: marker.integrity_cap(),
            });
        }
    }

    let baseline = baseline::verify_workspace(workspace, &manifest.baseline_hash)?;
    let cache = CachedStarter::new(store.cache_dir(&manifest.level_id, manifest.kit_version));
    Ok(StartPlan {
        level,
        kind: StartKind::Fresh,
        can_reset: cache.is_available(),
        baseline: Some(baseline),
        bootstrap_keys: bootstrap_plan.keys,
        bootstrap_already_applied: bootstrap_plan.already_applied,
        integrity_cap: "unverified",
    })
}

/// Begin (or resume) a capture session bound to the workspace's level.
pub fn start(
    workspace: &Path,
    otlp_endpoint: &str,
    store: &SessionStore,
    decisions: StartDecisions,
    now_ms: i64,
) -> Result<StartOutcome, StartError> {
    let manifest = Manifest::load(workspace)?;
    let level = LevelBinding::from_manifest(&manifest);

    if let Some(mut marker) = store.load_marker() {
        if is_resume(&marker, workspace, &manifest.level_id) {
            // Resume the bound attempt: reopen the window, re-assert the bootstrap,
            // never re-run the baseline check (the player's edits are their own).
            marker.stopped_at_ms = None;
            if marker.bootstrap.is_some() {
                bootstrap::reapply(workspace, otlp_endpoint)?;
            }
            store.save_marker(&marker)?;
            let jsonl_only = marker.bootstrap.is_none();
            let integrity_cap = marker.integrity_cap();
            return Ok(StartOutcome::Started(Box::new(StartedSession {
                marker,
                kind: StartKind::Resume,
                level,
                reset: None,
                bootstrap_applied: !jsonl_only,
                jsonl_only,
                integrity_cap,
            })));
        }
        // A live session bound elsewhere must be stopped first (single session).
        if marker.is_active() {
            return Err(StartError::SessionActiveElsewhere(marker.workspace));
        }
        // A stopped session for another workspace is simply superseded below.
    }

    // Fresh start: verify the baseline before capturing anything.
    let cache_dir = store.cache_dir(&manifest.level_id, manifest.kit_version);
    let starter = CachedStarter::new(cache_dir.clone());
    let mut code_reset_count = 0;
    let mut reset = None;
    match baseline::verify_workspace(workspace, &manifest.baseline_hash)? {
        BaselineStatus::Match => {
            // Cache the genuine starter so a future tampered start can be reset.
            if let Err(err) = baseline::cache_canonical(workspace, &cache_dir) {
                tracing::warn!(%err, "failed to cache canonical starter");
            }
        }
        BaselineStatus::Mismatch { computed } => {
            if !decisions.confirm_reset {
                return Ok(StartOutcome::NeedsResetConfirmation(BaselineMismatch {
                    expected: manifest.baseline_hash.clone(),
                    computed,
                    can_reset: starter.is_available(),
                }));
            }
            reset = Some(baseline::reset_workspace(workspace, &starter, now_ms)?);
            code_reset_count = 1;
        }
    }

    // Bootstrap the harness only with consent; otherwise capture is JSONL-only.
    let bootstrap = if decisions.consent_bootstrap {
        Some(bootstrap::apply(workspace, otlp_endpoint)?)
    } else {
        None
    };
    let jsonl_only = bootstrap.is_none();

    // A server-issued nonce binds the attempt to a cloud session and is what makes
    // a `verified` capture possible; without one the attempt is locally seeded and
    // caps at `unverified` (the offline path).
    let (attempt_nonce, nonce_origin) = match decisions.server_nonce {
        Some(nonce) => (nonce, NonceOrigin::Server),
        None => (uuid::Uuid::new_v4().to_string(), NonceOrigin::Local),
    };

    let marker = SessionMarker {
        version: SESSION_MARKER_VERSION,
        session_id: uuid::Uuid::new_v4().to_string(),
        workspace: workspace.to_path_buf(),
        level_id: manifest.level_id.clone(),
        slug: manifest.slug.clone(),
        started_at_ms: now_ms,
        stopped_at_ms: None,
        attempt_nonce,
        nonce_origin,
        file_allowlist: manifest.file_allowlist.clone(),
        code_reset_count,
        bootstrap,
    };
    store.save_marker(&marker)?;
    let integrity_cap = marker.integrity_cap();
    Ok(StartOutcome::Started(Box::new(StartedSession {
        marker,
        kind: StartKind::Fresh,
        level,
        reset,
        bootstrap_applied: !jsonl_only,
        jsonl_only,
        integrity_cap,
    })))
}

/// End the active session: restore the harness settings and close the window. The
/// marker is retained (stopped) so a later `start` resumes the attempt rather than
/// resetting it.
pub fn stop(store: &SessionStore, now_ms: i64) -> std::io::Result<StopOutcome> {
    let Some(mut marker) = store.load_marker() else {
        return Ok(StopOutcome {
            marker: None,
            reverted_bootstrap: false,
        });
    };
    let reverted = match &marker.bootstrap {
        Some(state) => {
            bootstrap::revert(&marker.workspace, state)?;
            true
        }
        None => false,
    };
    if marker.stopped_at_ms.is_none() {
        marker.stopped_at_ms = Some(now_ms);
    }
    store.save_marker(&marker)?;
    Ok(StopOutcome {
        marker: Some(marker),
        reverted_bootstrap: reverted,
    })
}

/// Explicitly reset the workspace to the canonical starter (the `promptly reset`
/// path) — back up, restore, and bump `code_reset_count` on the bound attempt.
pub fn reset(
    workspace: &Path,
    store: &SessionStore,
    now_ms: i64,
) -> Result<ResetReport, StartError> {
    let manifest = Manifest::load(workspace)?;
    let starter = CachedStarter::new(store.cache_dir(&manifest.level_id, manifest.kit_version));
    let report = baseline::reset_workspace(workspace, &starter, now_ms)?;
    if let Some(mut marker) = store.load_marker() {
        if same_workspace(&marker.workspace, workspace) {
            marker.code_reset_count += 1;
            store.save_marker(&marker)?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT: &str = "http://127.0.0.1:4318";
    const NOW: i64 = 1_700_000_000_000;

    struct Fixture {
        workspace: PathBuf,
        store: SessionStore,
        data_dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.workspace).ok();
            std::fs::remove_dir_all(&self.data_dir).ok();
        }
    }

    /// A workspace whose canonical files reproduce the manifest's `baseline_hash`,
    /// plus a fresh data dir for the marker/cache.
    fn fixture(label: &str) -> Fixture {
        let base =
            std::env::temp_dir().join(format!("promptlyd-scoping-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let workspace = base.join("ws");
        let data_dir = base.join("data");
        std::fs::create_dir_all(workspace.join(".promptly")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        std::fs::write(workspace.join("main.go"), "package main\n").unwrap();
        std::fs::write(workspace.join("lru.go"), "package main // TODO\n").unwrap();
        let baseline_hash = baseline::hash_workspace(&workspace).unwrap();
        let manifest = format!(
            r#"{{"schema_version":1,"kit_version":1,"level_id":"lvl-1","slug":"stage-1-01","title":"LRU","language":"Go","runtime_version":"go1.22","execution_harness":"stdin_stdout","file_allowlist":["lru.go"],"baseline_hash":"{baseline_hash}"}}"#
        );
        std::fs::write(workspace.join(".promptly/manifest.json"), manifest).unwrap();

        Fixture {
            workspace,
            store: SessionStore::new(data_dir.clone()),
            data_dir,
        }
    }

    fn started(outcome: StartOutcome) -> StartedSession {
        match outcome {
            StartOutcome::Started(s) => *s,
            StartOutcome::NeedsResetConfirmation(_) => panic!("expected a started session"),
        }
    }

    #[test]
    fn fresh_start_binds_the_level_issues_a_nonce_and_bootstraps() {
        let f = fixture("fresh");
        let decisions = StartDecisions {
            confirm_reset: false,
            consent_bootstrap: true,
            server_nonce: None,
        };
        let session = started(start(&f.workspace, ENDPOINT, &f.store, decisions, NOW).unwrap());

        assert_eq!(session.kind, StartKind::Fresh);
        assert_eq!(session.level.level_id, "lvl-1");
        assert_eq!(session.level.slug, "stage-1-01");
        assert!(!session.marker.attempt_nonce.is_empty());
        assert_eq!(session.marker.nonce_origin, NonceOrigin::Local);
        // A locally-issued nonce can never reach `verified`.
        assert_eq!(session.integrity_cap, "unverified");
        assert!(session.bootstrap_applied && !session.jsonl_only);
        // The OTEL env landed in the project settings, and the marker persisted.
        assert!(bootstrap::settings_path(&f.workspace).exists());
        assert!(f.store.load_marker().unwrap().is_active());
    }

    #[test]
    fn a_server_nonce_binds_the_attempt_and_reaches_verified() {
        let f = fixture("server-nonce");
        let decisions = StartDecisions {
            confirm_reset: false,
            consent_bootstrap: true,
            server_nonce: Some("srv-nonce-123".into()),
        };
        let session = started(start(&f.workspace, ENDPOINT, &f.store, decisions, NOW).unwrap());

        // The attempt binds to the exact server-issued nonce, not a local uuid.
        assert_eq!(session.marker.attempt_nonce, "srv-nonce-123");
        assert_eq!(session.marker.nonce_origin, NonceOrigin::Server);
        // A server nonce is what unlocks a `verified` capture.
        assert_eq!(session.integrity_cap, "verified");
        // And it persisted to the marker the engine reloads.
        let reloaded = f.store.load_marker().unwrap();
        assert_eq!(reloaded.attempt_nonce, "srv-nonce-123");
        assert_eq!(reloaded.nonce_origin, NonceOrigin::Server);
    }

    #[test]
    fn declining_consent_falls_back_to_jsonl_only() {
        let f = fixture("jsonl");
        let session = started(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions::default(),
                NOW,
            )
            .unwrap(),
        );
        assert!(session.jsonl_only && !session.bootstrap_applied);
        assert!(!bootstrap::settings_path(&f.workspace).exists());
        assert!(f.store.load_marker().unwrap().bootstrap.is_none());
    }

    #[test]
    fn a_missing_manifest_fails_to_start() {
        let f = fixture("nomanifest");
        std::fs::remove_file(f.workspace.join(".promptly/manifest.json")).unwrap();
        assert!(matches!(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions::default(),
                NOW
            ),
            Err(StartError::Manifest(_))
        ));
    }

    #[test]
    fn a_pre_modified_workspace_needs_confirmation_then_resets() {
        let f = fixture("tampered");
        // A prior clean run cached the genuine starter (what `init` would do).
        let manifest = Manifest::load(&f.workspace).unwrap();
        baseline::cache_canonical(
            &f.workspace,
            &f.store.cache_dir(&manifest.level_id, manifest.kit_version),
        )
        .unwrap();
        // Now the workspace is pre-loaded with a foreign solution.
        std::fs::write(
            f.workspace.join("lru.go"),
            "package main // PASTED SOLUTION\n",
        )
        .unwrap();
        std::fs::write(f.workspace.join("stolen.py"), "print('x')\n").unwrap();

        // Without confirmation, start is a no-op that reports the mismatch.
        match start(
            &f.workspace,
            ENDPOINT,
            &f.store,
            StartDecisions::default(),
            NOW,
        )
        .unwrap()
        {
            StartOutcome::NeedsResetConfirmation(m) => assert!(m.can_reset),
            StartOutcome::Started(_) => panic!("must not start a tampered workspace silently"),
        }
        assert!(f.store.load_marker().is_none(), "no session was begun");

        // With confirmation, it backs up, resets, and records the reset.
        let session = started(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions {
                    confirm_reset: true,
                    consent_bootstrap: false,
                    server_nonce: None,
                },
                NOW,
            )
            .unwrap(),
        );
        assert!(session.reset.is_some());
        assert_eq!(session.marker.code_reset_count, 1);
        // The foreign file is gone and the workspace matches the baseline again.
        assert!(!f.workspace.join("stolen.py").exists());
        assert!(
            baseline::verify_workspace(&f.workspace, &manifest.baseline_hash)
                .unwrap()
                .is_match()
        );
    }

    #[test]
    fn explicit_reset_restores_the_starter_and_counts_it() {
        let f = fixture("explicit-reset");
        // A clean start caches the genuine starter and records the attempt.
        started(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions::default(),
                NOW,
            )
            .unwrap(),
        );
        let baseline = Manifest::load(&f.workspace).unwrap().baseline_hash;

        // The player makes a mess, then runs `promptly reset`.
        std::fs::write(f.workspace.join("lru.go"), "package main // mess\n").unwrap();
        let report = reset(&f.workspace, &f.store, NOW + 5).unwrap();

        assert_eq!(report.restored_hash, baseline);
        assert!(baseline::verify_workspace(&f.workspace, &baseline)
            .unwrap()
            .is_match());
        assert!(report.backup_dir.exists());
        // The reset is recorded on the bound attempt.
        assert_eq!(f.store.load_marker().unwrap().code_reset_count, 1);
    }

    #[test]
    fn confirming_a_reset_without_a_cached_starter_is_an_error() {
        let f = fixture("nocache");
        std::fs::write(f.workspace.join("lru.go"), "package main // changed\n").unwrap();
        assert!(matches!(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions {
                    confirm_reset: true,
                    consent_bootstrap: false,
                    server_nonce: None,
                },
                NOW,
            ),
            Err(StartError::CannotReset(ResetError::NoCanonicalSource))
        ));
    }

    #[test]
    fn stop_then_start_resumes_the_same_attempt_without_resetting() {
        let f = fixture("resume");
        let first = started(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions {
                    confirm_reset: false,
                    consent_bootstrap: true,
                    server_nonce: None,
                },
                NOW,
            )
            .unwrap(),
        );
        let nonce = first.marker.attempt_nonce.clone();

        // Stop restores the harness settings and closes the window.
        let stopped = stop(&f.store, NOW + 1).unwrap();
        assert!(stopped.reverted_bootstrap);
        assert!(!bootstrap::settings_path(&f.workspace).exists());
        assert!(!f.store.load_marker().unwrap().is_active());

        // The player edits their solution, then resumes — no reset, same nonce.
        std::fs::write(f.workspace.join("lru.go"), "package main // my work\n").unwrap();
        let resumed = started(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions {
                    confirm_reset: false,
                    consent_bootstrap: true,
                    // A server nonce on a resume must be ignored — a resume rebinds
                    // the original attempt, never re-seeds it (anti-replay).
                    server_nonce: Some("must-be-ignored-on-resume".into()),
                },
                NOW + 2,
            )
            .unwrap(),
        );
        assert_eq!(resumed.kind, StartKind::Resume);
        assert_eq!(
            resumed.marker.attempt_nonce, nonce,
            "same attempt, nonce unchanged"
        );
        assert!(resumed.reset.is_none());
        // The bootstrap was re-asserted on resume.
        assert!(bootstrap::settings_path(&f.workspace).exists());
    }

    #[test]
    fn attribution_is_scoped_to_the_window_and_workspace() {
        let f = fixture("attr");
        let session = started(
            start(
                &f.workspace,
                ENDPOINT,
                &f.store,
                StartDecisions::default(),
                NOW,
            )
            .unwrap(),
        );
        let marker = session.marker;
        let ws = marker.workspace.to_string_lossy().to_string();

        // In-window, same workspace -> attributed.
        assert!(marker.attributes(NOW + 10, Some(&ws)));
        // No cwd reported -> accepted (loopback + project bootstrap scope it).
        assert!(marker.attributes(NOW + 10, None));
        // Before the session began -> not attributed.
        assert!(!marker.attributes(NOW - 10, Some(&ws)));
        // A different directory during the window -> not attributed.
        assert!(!marker.attributes(NOW + 10, Some("/somewhere/else")));

        // After stop, turns past the close are not attributed; in-window ones still are.
        let stopped = stop(&f.store, NOW + 100).unwrap().marker.unwrap();
        assert!(stopped.attributes(NOW + 50, Some(&ws)));
        assert!(!stopped.attributes(NOW + 500, Some(&ws)));
    }
}
