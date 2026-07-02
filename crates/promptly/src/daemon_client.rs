//! Client for the daemon's localhost control + status API (`17`/`18`).
//!
//! The CLI ↔ daemon seam is the daemon's loopback HTTP API: read endpoints
//! (`GET /health`, `/session`) and the CLI-only control endpoints
//! (`POST /session/start|stop|reset`, `GET /session/preflight`), the mutating ones
//! guarded by the daemon's per-process capability token in the `X-Promptly-Control`
//! header (`18`; read from the `0600` data-dir file the running daemon wrote). This
//! module is the typed client for that seam.
//!
//! The turn schema (`NormalizedTurn`), the session marker, and `Totals` are
//! reused from the daemon crate (the real contract). The control responses
//! (`StartPlan`, `StartedSession`, …) use `&'static str` fields server-side and
//! so can't be deserialized into the daemon's own types; the DTOs here mirror
//! their JSON exactly and stay in lockstep with `crate::scoping`/`crate::baseline`.

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use promptlyd::engine::Totals;
pub use promptlyd::model::NormalizedTurn;
pub use promptlyd::scoping::SessionMarker;
pub use promptlyd::sources::registry::{AdapterState, AdapterStatus};

/// Why a daemon request failed.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(
        "the Promptly daemon isn't running at {0} — `promptly start` launches it for you (or run `promptly up`)"
    )]
    NotRunning(String),
    #[error("daemon error: {0}")]
    Api(String),
    #[error("daemon response wasn't understood: {0}")]
    Decode(String),
}

/// `GET /health` — liveness, version, uptime, and recent diagnostics.
#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    /// The workspace the daemon is scoped to (empty on older daemons that predate
    /// the field). The CLI's daemon auto-management compares it to the level folder
    /// you're starting in, to decide whether to reuse or relaunch the daemon.
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub uptime_ms: i64,
    #[serde(default)]
    pub otlp_endpoint: String,
    #[serde(default)]
    pub turns: u64,
    #[serde(default)]
    pub recent_errors: Vec<DiagEvent>,
    /// Per-harness capture-adapter detection status (`21`); empty on older daemons.
    #[serde(default)]
    pub adapters: Vec<AdapterStatus>,
}

/// One recent warning/error from the daemon's diagnostics ring (`diagnostics.rs`).
#[derive(Debug, Clone, Deserialize)]
pub struct DiagEvent {
    #[serde(default)]
    pub timestamp_ms: i64,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub message: String,
}

/// `GET /session` — the active binding, totals, and captured turns.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionSnapshot {
    /// The bound session marker, or `None` when the daemon is idle.
    pub session: Option<SessionMarker>,
    #[serde(default)]
    pub totals: Totals,
    #[serde(default)]
    pub turns: u64,
    #[serde(default)]
    pub signals: Vec<serde_json::Value>,
    #[serde(default)]
    pub captured: Vec<NormalizedTurn>,
}

/// The level a session is (or would be) bound to (mirrors `scoping::LevelBinding`).
#[derive(Debug, Clone, Deserialize)]
pub struct LevelBinding {
    pub level_id: String,
    pub slug: String,
    pub title: String,
    pub language: String,
    pub runtime_version: String,
    pub execution_harness: String,
}

/// Baseline-check outcome for a fresh start (mirrors `baseline::BaselineStatus`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BaselineStatus {
    Match,
    Mismatch { computed: String },
}

/// `GET /session/preflight` — what a `start` would do (mirrors `scoping::StartPlan`).
#[derive(Debug, Clone, Deserialize)]
pub struct StartPlan {
    pub level: LevelBinding,
    /// `"fresh"` or `"resume"`.
    pub kind: String,
    /// `None` for a resume (no baseline check on an in-progress attempt).
    pub baseline: Option<BaselineStatus>,
    pub can_reset: bool,
    pub bootstrap_keys: Vec<String>,
    pub bootstrap_already_applied: bool,
    pub integrity_cap: String,
}

/// A reset's report (mirrors `baseline::ResetReport`).
#[derive(Debug, Clone, Deserialize)]
pub struct ResetReport {
    pub backup_dir: String,
    pub restored_hash: String,
}

/// A successful start (mirrors `scoping::StartedSession`).
#[derive(Debug, Clone, Deserialize)]
pub struct StartedSession {
    pub marker: SessionMarker,
    pub kind: String,
    pub level: LevelBinding,
    pub reset: Option<ResetReport>,
    pub bootstrap_applied: bool,
    pub jsonl_only: bool,
    pub integrity_cap: String,
}

/// A baseline mismatch needing confirmation (mirrors `scoping::BaselineMismatch`).
#[derive(Debug, Clone, Deserialize)]
pub struct BaselineMismatch {
    pub expected: String,
    pub computed: String,
    pub can_reset: bool,
}

/// A stop's outcome (mirrors `scoping::StopOutcome`).
#[derive(Debug, Clone, Deserialize)]
pub struct StopReport {
    pub marker: Option<SessionMarker>,
    pub reverted_bootstrap: bool,
}

/// The player's answers to the prompts a fresh start may raise — the
/// `POST /session/start` body. `server_nonce` carries the CLI's server-issued
/// attempt nonce (`20`) when paired; omitted from the wire when absent.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartDecisions {
    pub confirm_reset: bool,
    pub consent_bootstrap: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_nonce: Option<String>,
    /// The server's authoritative kit `baseline_hash` (`20`/`07`), returned with the
    /// attempt nonce; the daemon attests the local manifest against it before a fresh
    /// start. Omitted from the wire when absent (offline).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_baseline: Option<String>,
}

/// The outcome of a `start` call: the session began, or a baseline reset must be
/// confirmed first (the daemon's `409`).
#[derive(Debug, Clone)]
pub enum StartOutcome {
    Started(Box<StartedSession>),
    NeedsReset(BaselineMismatch),
}

/// The control + status operations the CLI drives. A trait so commands can be
/// tested against an in-memory fake without a live daemon.
pub trait DaemonApi {
    fn health(&self) -> Result<Health, DaemonError>;
    fn session(&self) -> Result<SessionSnapshot, DaemonError>;
    fn preflight(&self) -> Result<StartPlan, DaemonError>;
    fn start(&self, decisions: StartDecisions) -> Result<StartOutcome, DaemonError>;
    fn stop(&self) -> Result<StopReport, DaemonError>;
    fn reset(&self) -> Result<ResetReport, DaemonError>;
}

/// The control-request header carrying the daemon's per-process capability token.
/// Its value is the secret the running daemon wrote `0600` to the data dir
/// ([`promptlyd::control_token`]); presenting it proves this process can read the
/// owning user's token file, which a browser and another user's process cannot
/// (`18`). The GET-only CORS still blocks a browser from setting it cross-origin.
const CONTROL_HEADER: &str = "X-Promptly-Control";

/// A blocking HTTP client for one daemon, bound to its loopback API address.
pub struct DaemonClient {
    agent: ureq::Agent,
    base: String,
    /// The running daemon's control token, read from the data dir at construction.
    /// `None` when no daemon has started (then a control call transport-fails to
    /// `NotRunning` before the token is even needed).
    control_token: Option<String>,
}

impl DaemonClient {
    /// Build a client for the daemon's API on `127.0.0.1:<port>`.
    pub fn new(api_port: u16) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .timeout_read(Duration::from_secs(10))
            .build();
        // Load the running daemon's capability token (minted at its startup, stored
        // `0600` in the shared data dir). Best-effort: a missing or unreadable file
        // yields `None`, and control requests then fail closed at the daemon.
        let control_token = promptlyd::control_token::read(&promptlyd::paths::data_dir())
            .ok()
            .flatten()
            .map(|auth| auth.token);
        Self {
            agent,
            base: format!("http://127.0.0.1:{api_port}"),
            control_token,
        }
    }

    /// The daemon's API base URL (for error/diagnostic messages).
    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, DaemonError> {
        let url = format!("{}{path}", self.base);
        match self.agent.get(&url).call() {
            Ok(resp) => {
                let body = resp
                    .into_string()
                    .map_err(|e| DaemonError::Decode(e.to_string()))?;
                serde_json::from_str(&body).map_err(|e| DaemonError::Decode(e.to_string()))
            }
            Err(ureq::Error::Status(code, resp)) => Err(api_error(code, resp)),
            Err(ureq::Error::Transport(_)) => Err(DaemonError::NotRunning(self.base.clone())),
        }
    }

    /// POST a control request, returning the status code and body. Transport
    /// failures map to `NotRunning`; HTTP error statuses return their body so the
    /// caller can parse a structured envelope (e.g. the `409` baseline mismatch).
    fn control(&self, path: &str, body: &str) -> Result<(u16, String), DaemonError> {
        let url = format!("{}{path}", self.base);
        let result = self
            .agent
            .post(&url)
            .set(CONTROL_HEADER, self.control_token.as_deref().unwrap_or(""))
            .set("Content-Type", "application/json")
            .send_string(body);
        match result {
            Ok(resp) => Ok((
                resp.status(),
                resp.into_string()
                    .map_err(|e| DaemonError::Decode(e.to_string()))?,
            )),
            Err(ureq::Error::Status(code, resp)) => {
                Ok((code, resp.into_string().unwrap_or_default()))
            }
            Err(ureq::Error::Transport(_)) => Err(DaemonError::NotRunning(self.base.clone())),
        }
    }

    /// Open the live turn stream (`GET /stream`, Server-Sent Events). Uses a
    /// dedicated agent with no read timeout so an idle stream stays open between
    /// turns; each yielded item is one captured [`NormalizedTurn`].
    pub fn stream(&self) -> Result<TurnStream, DaemonError> {
        let url = format!("{}/stream", self.base);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .build();
        match agent.get(&url).call() {
            Ok(resp) => Ok(TurnStream {
                reader: BufReader::new(resp.into_reader()),
            }),
            Err(ureq::Error::Status(code, resp)) => Err(api_error(code, resp)),
            Err(ureq::Error::Transport(_)) => Err(DaemonError::NotRunning(self.base.clone())),
        }
    }

    /// Ask the daemon to stop (`POST /shutdown`). Returns `Ok` once it has
    /// acknowledged; the caller then polls [`health`](DaemonApi::health) until the
    /// loopback port goes quiet. A transport error means it was already gone.
    pub fn shutdown(&self) -> Result<(), DaemonError> {
        match self.control("/shutdown", "") {
            Ok(_) => Ok(()),
            Err(DaemonError::NotRunning(_)) => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// An iterator over the daemon's live SSE turn stream. Each `Some(Ok(_))` is one
/// captured turn; keep-alive comments and non-`data` lines are skipped, and the
/// iterator ends when the stream closes.
pub struct TurnStream {
    reader: BufReader<Box<dyn Read + Send + Sync + 'static>>,
}

impl Iterator for TurnStream {
    type Item = Result<NormalizedTurn, DaemonError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut data = String::new();
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None, // stream closed
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        // Event boundary: emit the buffered turn, or keep waiting
                        // through a bare keep-alive separator.
                        if !data.is_empty() {
                            return Some(
                                serde_json::from_str(&data)
                                    .map_err(|e| DaemonError::Decode(e.to_string())),
                            );
                        }
                    } else if let Some(payload) = trimmed.strip_prefix("data:") {
                        data.push_str(payload.trim_start());
                    }
                    // Other SSE fields (`event:`, `:comment` keep-alives) are ignored.
                }
                Err(e) => return Some(Err(DaemonError::Decode(e.to_string()))),
            }
        }
    }
}

#[cfg(test)]
impl TurnStream {
    /// Wrap an arbitrary reader for testing the SSE parsing without a socket.
    fn from_reader<R: Read + Send + Sync + 'static>(reader: R) -> Self {
        Self {
            reader: BufReader::new(Box::new(reader)),
        }
    }
}

/// Build an `Api` error from a non-2xx response, preferring the daemon's
/// structured `{ "error": "…" }` message over a bare status code.
fn api_error(code: u16, resp: ureq::Response) -> DaemonError {
    let body = resp.into_string().unwrap_or_default();
    DaemonError::Api(error_message(code, &body))
}

fn error_message(code: u16, body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {code}"))
}

/// The `start` response envelope: `{ "status": "started", "session": … }` or
/// `{ "status": "needs_reset_confirmation", "baseline": … }`.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum StartEnvelope {
    // Boxed: the started payload dwarfs the mismatch one, so an unboxed enum
    // would carry the larger size everywhere (clippy::large_enum_variant).
    Started { session: Box<StartedSession> },
    NeedsResetConfirmation { baseline: BaselineMismatch },
}

#[derive(Debug, Deserialize)]
struct StopEnvelope {
    stop: StopReport,
}

#[derive(Debug, Deserialize)]
struct ResetEnvelope {
    reset: ResetReport,
}

impl DaemonApi for DaemonClient {
    fn health(&self) -> Result<Health, DaemonError> {
        self.get_json("/health")
    }

    fn session(&self) -> Result<SessionSnapshot, DaemonError> {
        self.get_json("/session")
    }

    fn preflight(&self) -> Result<StartPlan, DaemonError> {
        self.get_json("/session/preflight")
    }

    fn start(&self, decisions: StartDecisions) -> Result<StartOutcome, DaemonError> {
        let body = serde_json::to_string(&decisions).expect("StartDecisions always serializes");
        let (code, body) = self.control("/session/start", &body)?;
        match code {
            200 | 409 => match serde_json::from_str::<StartEnvelope>(&body) {
                Ok(StartEnvelope::Started { session }) => Ok(StartOutcome::Started(session)),
                Ok(StartEnvelope::NeedsResetConfirmation { baseline }) => {
                    Ok(StartOutcome::NeedsReset(baseline))
                }
                Err(e) => Err(DaemonError::Decode(e.to_string())),
            },
            _ => Err(DaemonError::Api(error_message(code, &body))),
        }
    }

    fn stop(&self) -> Result<StopReport, DaemonError> {
        let (code, body) = self.control("/session/stop", "")?;
        if code == 200 {
            serde_json::from_str::<StopEnvelope>(&body)
                .map(|e| e.stop)
                .map_err(|e| DaemonError::Decode(e.to_string()))
        } else {
            Err(DaemonError::Api(error_message(code, &body)))
        }
    }

    fn reset(&self) -> Result<ResetReport, DaemonError> {
        let (code, body) = self.control("/session/reset", "")?;
        if code == 200 {
            serde_json::from_str::<ResetEnvelope>(&body)
                .map(|e| e.reset)
                .map_err(|e| DaemonError::Decode(e.to_string()))
        } else {
            Err(DaemonError::Api(error_message(code, &body)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_preflight_plan_with_a_baseline_mismatch() {
        // The exact shape `scoping::StartPlan` serializes to.
        let json = r#"{
            "level": {"level_id":"lvl-9","slug":"stage-1-09","title":"X",
                      "language":"Go","runtime_version":"go1.22","execution_harness":"stdin_stdout"},
            "kind": "fresh",
            "baseline": {"status":"mismatch","computed":"deadbeef"},
            "can_reset": true,
            "bootstrap_keys": ["CLAUDE_CODE_ENABLE_TELEMETRY","OTEL_EXPORTER_OTLP_ENDPOINT"],
            "bootstrap_already_applied": false,
            "integrity_cap": "unverified"
        }"#;
        let plan: StartPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.kind, "fresh");
        assert_eq!(plan.level.slug, "stage-1-09");
        assert_eq!(plan.bootstrap_keys.len(), 2);
        match plan.baseline {
            Some(BaselineStatus::Mismatch { computed }) => assert_eq!(computed, "deadbeef"),
            other => panic!("expected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_started_envelope_and_a_needs_reset_envelope() {
        let started = r#"{"status":"started","session":{
            "marker":{"version":1,"session_id":"s1","workspace":"/ws","level_id":"lvl-1",
                      "slug":"stage-1-01","started_at_ms":1000,"attempt_nonce":"n","nonce_origin":"local",
                      "file_allowlist":["lru.go"],"code_reset_count":0},
            "kind":"fresh",
            "level":{"level_id":"lvl-1","slug":"stage-1-01","title":"LRU","language":"Go",
                     "runtime_version":"go1.22","execution_harness":"stdin_stdout"},
            "reset":null,"bootstrap_applied":true,"jsonl_only":false,"integrity_cap":"unverified"}}"#;
        match serde_json::from_str::<StartEnvelope>(started).unwrap() {
            StartEnvelope::Started { session } => {
                assert_eq!(session.level.slug, "stage-1-01");
                assert!(session.bootstrap_applied && !session.jsonl_only);
                assert_eq!(session.marker.attempt_nonce, "n");
            }
            other => panic!("expected started, got {other:?}"),
        }

        let needs = r#"{"status":"needs_reset_confirmation",
            "baseline":{"expected":"aaa","computed":"bbb","can_reset":true}}"#;
        match serde_json::from_str::<StartEnvelope>(needs).unwrap() {
            StartEnvelope::NeedsResetConfirmation { baseline } => {
                assert!(baseline.can_reset);
                assert_eq!(baseline.expected, "aaa");
            }
            other => panic!("expected needs-reset, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_session_snapshot_and_health() {
        let session = r#"{"session":null,"totals":{"turns":0,"tokens_input":0,"tokens_output":0,
            "tokens_thinking":0,"tokens_cache":0},"turns":0,"signals":[],"captured":[]}"#;
        let snap: SessionSnapshot = serde_json::from_str(session).unwrap();
        assert!(snap.session.is_none());
        assert_eq!(snap.totals.turns, 0);

        let health = r#"{"status":"ok","version":"0.1.0","pid":1,"uptime_ms":42,"capturing":true,
            "otlp_endpoint":"http://127.0.0.1:4318","turns":3,
            "recent_errors":[{"timestamp_ms":1,"level":"WARN","message":"disk full"}],
            "adapters":[{"name":"cursor","state":"detected","detail":"3 turns"},
                        {"name":"codex","state":"notfound","detail":"no sessions"}]}"#;
        let h: Health = serde_json::from_str(health).unwrap();
        assert_eq!(h.version, "0.1.0");
        assert_eq!(h.turns, 3);
        assert_eq!(h.recent_errors.len(), 1);
        assert_eq!(h.recent_errors[0].message, "disk full");
        assert_eq!(h.adapters.len(), 2);
        assert_eq!(h.adapters[0].name, "cursor");
        assert_eq!(h.adapters[0].state, AdapterState::Detected);
        assert_eq!(h.adapters[1].state, AdapterState::NotFound);
    }

    #[test]
    fn error_message_prefers_the_structured_error() {
        assert_eq!(
            error_message(400, r#"{"error":"bad manifest"}"#),
            "bad manifest"
        );
        assert_eq!(error_message(500, "not json"), "HTTP 500");
    }

    #[test]
    fn unreachable_daemon_reports_not_running() {
        // Port 1 on loopback has nothing listening.
        let client = DaemonClient::new(1);
        assert!(matches!(client.health(), Err(DaemonError::NotRunning(_))));
    }

    #[test]
    fn sse_stream_parses_turns_and_skips_keepalives() {
        use promptlyd::model::{Agreement, Confidence, Plausibility, Source};
        let turn = NormalizedTurn {
            schema_version: 1,
            turn_id: "t1".into(),
            model: "claude-opus-4-8".into(),
            harness: "claude_code_cli".into(),
            tokens_input: 10,
            tokens_output: 20,
            tokens_thinking: 0,
            tokens_cache: 0,
            prompt_id: Some("p1".into()),
            timestamp_ms: 1,
            confidence: Confidence::Otel,
            cost_usd: None,
            duration_ms: None,
            sources: vec![Source::Otel],
            session_id: None,
            attempt_nonce: Some("n".into()),
            workspace: None,
            agreement: Agreement::Single,
            plausibility: Plausibility::Plausible,
        };
        let json = serde_json::to_string(&turn).unwrap();
        // Two data events around a keep-alive comment and blank lines.
        let body = format!("data: {json}\n\n:\n\ndata: {json}\n\n");
        let mut stream = TurnStream::from_reader(std::io::Cursor::new(body));

        let first = stream.next().expect("first event").expect("parses");
        assert_eq!(first.turn_id, "t1");
        assert_eq!(first.tokens_output, 20);
        let second = stream.next().expect("second event").expect("parses");
        assert_eq!(second.model, "claude-opus-4-8");
        assert!(
            stream.next().is_none(),
            "stream ends when the reader closes"
        );
    }
}
