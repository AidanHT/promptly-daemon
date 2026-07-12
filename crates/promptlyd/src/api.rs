//! The localhost-only status, live-stream, and session-control HTTP API.
//!
//! Read endpoints (the web bridge `22` consumes these):
//! - `GET /health` — liveness, version, uptime, and the OTLP endpoint.
//! - `GET /session` — the active session binding, totals, turns, and signals.
//! - `GET /stream` — Server-Sent Events, one event per captured normalized turn.
//! - `GET /session/preflight` — what a `start` would do (no side effects).
//!
//! Control endpoints (the `promptly` CLI `19` drives these) begin/end the scoped
//! session (`18`):
//! - `POST /session/start` `{confirm_reset, consent_bootstrap}`
//! - `POST /session/stop`
//! - `POST /session/reset`
//!
//! Binding is loopback-only and CORS only allows GET, and only from loopback dev
//! origins plus the configured Promptly web origin(s) (`22`) — so a browser can't
//! cross-origin-POST a control endpoint and an arbitrary site can't read a user's
//! local telemetry. The mutating routes additionally require the CLI's per-process
//! **capability token** as the value of the `X-Promptly-Control` header (minted at
//! startup, stored `0600` in the data dir; see [`crate::control_token`]) — so a
//! local non-browser process can't drive them either, not just a cross-origin
//! browser. Every route also rejects a non-loopback `Host` ([`host_guard`]),
//! closing DNS rebinding as a third layer over the loopback bind and origin lock. A
//! public-origin Promptly page talking to `127.0.0.1` triggers Chrome's Private
//! Network Access preflight, so the CORS layer also answers it
//! (`Access-Control-Allow-Private-Network: true`).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::clock::now_ms;
use crate::control_token;
use crate::diagnostics::Diagnostics;
use crate::engine::SharedState;
use crate::scoping::{
    self, SessionMarker, SessionStore, StartDecisions, StartError, StartKind, StartOutcome,
};
use crate::sources::otel::IngestAuth;
use crate::sources::registry::AdapterRegistry;
use crate::sources::{wait_for_shutdown, Shutdown};

/// Header the CLI sets on control requests. A browser can't set a custom header on
/// a cross-origin request without a preflight, which the GET-only CORS denies — so
/// requiring it blocks CSRF against the mutating endpoints.
const CONTROL_HEADER: &str = "x-promptly-control";

/// Everything the API handlers read, cloned per request.
#[derive(Clone)]
pub struct ApiState {
    pub shared: Arc<SharedState>,
    pub started_at_ms: i64,
    pub otlp_endpoint: String,
    pub diagnostics: Diagnostics,
    /// Session marker/cache store, for the control endpoints (`18`).
    pub store: SessionStore,
    /// The workspace the daemon is scoped to; the session binds to its manifest.
    pub workspace: PathBuf,
    /// Latest detection status of the `21` harness adapters (Cursor/Codex/Copilot),
    /// surfaced on `/health` for `promptly doctor` (`19`).
    pub adapters: AdapterRegistry,
    /// Non-loopback web origins allowed to read the status/stream API (`22`): the
    /// deployed Promptly origin(s). Loopback dev origins are always allowed; any
    /// other origin is rejected so an arbitrary site can't read local telemetry.
    pub web_origins: Vec<String>,
    /// Shutdown trigger for the `POST /shutdown` control route (the `promptly
    /// down` / level-switch path). Flipping it stops every component and the run
    /// loop, exactly like a Ctrl-C — so a background daemon can be stopped without
    /// a signal.
    pub shutdown: tokio::sync::watch::Sender<bool>,
    /// This daemon's per-process control capability token ([`crate::control_token`]).
    /// A control request must echo it in the `X-Promptly-Control` header; the CLI
    /// reads it from the `0600` data-dir file, a browser/other-user process can't.
    pub control_token: String,
    /// The OTLP receiver's ingest gate. Start opens it to the session's token; stop
    /// closes it — so only the consented session's harness telemetry is accepted.
    pub ingest_auth: IngestAuth,
}

/// Build the API router with origin-locked, GET-only CORS.
pub fn router(state: ApiState) -> Router {
    let cors = web_cors(state.web_origins.clone());
    Router::new()
        .route("/health", get(health))
        .route("/session", get(session))
        .route("/session/preflight", get(session_preflight))
        .route("/session/start", post(session_start))
        .route("/session/stop", post(session_stop))
        .route("/session/reset", post(session_reset))
        .route("/shutdown", post(shutdown_daemon))
        .route("/stream", get(stream))
        .layer(cors)
        // Reject a non-loopback `Host` on every route (anti DNS-rebinding), as
        // defense-in-depth over the loopback bind and the CORS origin lock. Added
        // last so it is the outermost layer and runs before CORS.
        .layer(middleware::from_fn(host_guard))
        .with_state(state)
}

/// Is `origin` a loopback dev origin — `http://` with host *exactly* `localhost`
/// or `127.0.0.1` and an optional numeric port, and nothing more? Local dev
/// serves the web app from one of these, so they are always allowed.
///
/// The host must match exactly. A prefix test (`starts_with("http://localhost")`)
/// would let a public attacker origin such as `http://localhost.evil.com` or
/// `http://127.0.0.1.evil.com` satisfy it and — since `allow_private_network`
/// answers Chrome's PNA preflight for the same predicate — read the user's local
/// telemetry cross-origin, defeating the whole point of the origin lock (`22`).
fn is_loopback_origin(origin: &[u8]) -> bool {
    // Origins are ASCII and carry no path/userinfo (a browser sends just
    // `scheme://host[:port]`); anything non-UTF-8 or non-`http` isn't loopback dev.
    let Ok(origin) = std::str::from_utf8(origin) else {
        return false;
    };
    let Some(authority) = origin.strip_prefix("http://") else {
        return false;
    };
    // The host is the authority minus an optional `:port`. (IPv6 loopback `[::1]`
    // splits to a `[`-prefixed host here and is rejected — dev never uses it.)
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if !matches!(host, "localhost" | "127.0.0.1") {
        return false;
    }
    // A real browser Origin's port is numeric or absent; rejecting anything else
    // stops a crafted authority (`localhost:1.evil.com`) smuggling a foreign host.
    match port {
        None => true,
        Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// CORS for the read/stream API: GET only, from loopback dev origins or one of the
/// configured deployed Promptly origins — so a browser can read the status/stream
/// but never cross-origin-POST a control endpoint, and an arbitrary site can't
/// read a user's local telemetry. `allow_private_network` answers Chrome's PNA
/// preflight (`Access-Control-Allow-Private-Network: true`) for those origins, so
/// a public HTTPS Promptly page can reach `127.0.0.1` (`22`).
fn web_cors(web_origins: Vec<String>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            is_loopback_origin(origin.as_bytes())
                || web_origins
                    .iter()
                    .any(|o| o.as_bytes() == origin.as_bytes())
        }))
        .allow_methods([axum::http::Method::GET])
        .allow_private_network(true)
}

async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        // The folder this daemon is scoped to. The CLI reads it to tell whether a
        // running daemon is already watching the level you're starting, or needs
        // to be relaunched for a different one.
        "workspace": state.workspace.display().to_string(),
        "uptime_ms": (now_ms() - state.started_at_ms).max(0),
        "capturing": true,
        "otlp_endpoint": state.otlp_endpoint,
        "turns": state.shared.turn_count(),
        // Recent warnings/errors for `promptly doctor` (`19`).
        "recent_errors": state.diagnostics.recent(),
        // Per-harness adapter detection status (`21`), one entry per adapter.
        "adapters": state.adapters.snapshot(),
    }))
}

async fn session(State(state): State<ApiState>) -> impl IntoResponse {
    Json(json!({
        // The active session binding (`18`): the bound level, attempt nonce, and
        // window — `null` when the daemon is idle. Served WITHOUT the marker's
        // per-session secrets (see `sanitized_marker`).
        "session": state.shared.binding().map(|m| sanitized_marker(&m)),
        "totals": state.shared.totals(),
        "turns": state.shared.turn_count(),
        // Edit-provenance signals raised this session, for the server's checks (`25`).
        "signals": state.shared.signals(),
        // The full captured set, so the web bridge (`22`) has the initial state
        // before subscribing to `/stream`.
        "captured": state.shared.snapshot(),
    }))
}

/// The session marker as `GET /session` serves it: the persisted marker minus
/// its per-session secrets. `otlp_token` is the credential the OTLP receiver
/// requires on every ingest post, and `bootstrap` records the harness-settings
/// state it was written into — no API consumer reads either (the web bridge and
/// the CLI use the level/nonce/window fields), and `/session` is readable by
/// the allowed browser origins, so serving the token would hand any script on
/// those pages the key to injecting fabricated telemetry. Stripped here at the
/// API boundary — deliberately NOT `#[serde(skip)]` on the struct, because the
/// same serialization persists `session.json`, where the token must survive for
/// a resume to re-authorize ingest ([`crate::scoping`]).
fn sanitized_marker(marker: &SessionMarker) -> serde_json::Value {
    let mut value = serde_json::to_value(marker).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("otlp_token");
        map.remove("bootstrap");
    }
    value
}

async fn stream(State(state): State<ApiState>) -> impl IntoResponse {
    let events = BroadcastStream::new(state.shared.subscribe()).filter_map(|res| {
        // A lagged receiver yields an error; skip it rather than tear down.
        let turn = res.ok()?;
        let event = Event::default().json_data(turn).ok()?;
        Some(Ok::<Event, Infallible>(event))
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Preview what a `start` would do for the daemon's workspace (no side effects).
async fn session_preflight(State(state): State<ApiState>) -> Response {
    match scoping::preflight(&state.workspace, &state.otlp_endpoint, &state.store) {
        Ok(plan) => Json(plan).into_response(),
        Err(err) => start_error_response(err),
    }
}

/// `POST /session/start` — begin (or resume) the bound capture session.
async fn session_start(State(state): State<ApiState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(resp) = control_guard(&headers, &state.control_token) {
        return resp;
    }
    // Lenient body: empty or unparseable defaults to "no" on both decisions.
    let body: StartBody = if body.is_empty() {
        StartBody::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    let decisions = StartDecisions {
        confirm_reset: body.confirm_reset,
        consent_bootstrap: body.consent_bootstrap,
        // A server nonce (from the CLI's `POST /api/cli/attempts`) lifts a fresh
        // attempt's integrity ceiling to `verified`; absent, capture is local-only.
        server_nonce: body.server_nonce,
        // The server's authoritative kit baseline (from the same call): attested
        // against the local manifest before a fresh start proceeds.
        expected_baseline: body.expected_baseline,
    };
    match scoping::start(
        &state.workspace,
        &state.otlp_endpoint,
        &state.store,
        decisions,
        now_ms(),
    ) {
        Ok(StartOutcome::Started(session)) => {
            // A fresh attempt starts with zero turns; a resume keeps the ones
            // already restored for the bound attempt.
            match session.kind {
                StartKind::Fresh => state.shared.begin_session(session.marker.clone()),
                StartKind::Resume => state.shared.set_binding(Some(session.marker.clone())),
            }
            // Open the receiver to this session's ingest token (or close it for a
            // JSONL-only start, whose marker carries none) so only the consented
            // harness's telemetry is accepted.
            state.ingest_auth.set_from_marker(Some(&session.marker));
            Json(json!({ "status": "started", "session": *session })).into_response()
        }
        Ok(StartOutcome::NeedsResetConfirmation(mismatch)) => (
            StatusCode::CONFLICT,
            Json(json!({ "status": "needs_reset_confirmation", "baseline": mismatch })),
        )
            .into_response(),
        Err(err) => start_error_response(err),
    }
}

/// `POST /session/stop` — end the active session and restore the harness settings.
async fn session_stop(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(resp) = control_guard(&headers, &state.control_token) {
        return resp;
    }
    match scoping::stop(&state.store, now_ms()) {
        Ok(outcome) => {
            // Keep the in-memory binding in step (a stopped marker still attributes
            // late in-window turns; `None` only when nothing was active).
            state.shared.set_binding(outcome.marker.clone());
            // Close the receiver: the harness's OTEL export is reverted on stop, so
            // any post now is unauthenticated (a stale token or another process).
            state.ingest_auth.close();
            // No counterpart can arrive anymore — flush the correlation buffer
            // immediately so a stop→submit right after sees complete totals
            // instead of racing the pairing horizon.
            state.shared.request_flush();
            Json(json!({ "status": "stopped", "stop": outcome })).into_response()
        }
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `POST /session/reset` — explicitly restore the workspace to the canonical
/// starter (the `promptly reset` path).
async fn session_reset(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(resp) = control_guard(&headers, &state.control_token) {
        return resp;
    }
    match scoping::reset(&state.workspace, &state.store, now_ms()) {
        Ok(report) => {
            // Refresh the binding so its `code_reset_count` reflects the reset.
            state.shared.set_binding(state.store.load_marker());
            Json(json!({ "status": "reset", "reset": report })).into_response()
        }
        Err(err) => start_error_response(err),
    }
}

/// `POST /shutdown` — stop the daemon gracefully (the `promptly down` and
/// level-switch path). Control-header guarded like the session routes, so a
/// browser can't cross-origin-POST it. Flips the shared shutdown flag; the run
/// loop and every component observe it and stop, the same as a Ctrl-C.
async fn shutdown_daemon(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(resp) = control_guard(&headers, &state.control_token) {
        return resp;
    }
    // Best-effort: a closed receiver just means we're already on the way down.
    let _ = state.shutdown.send(true);
    Json(json!({ "status": "stopping" })).into_response()
}

/// Request body for `POST /session/start`.
#[derive(Debug, Default, serde::Deserialize)]
struct StartBody {
    #[serde(default)]
    confirm_reset: bool,
    #[serde(default)]
    consent_bootstrap: bool,
    /// The CLI's server-issued attempt nonce (`20`); absent for an offline start.
    #[serde(default)]
    server_nonce: Option<String>,
    /// The server's authoritative kit `baseline_hash` for this level (`20`); absent
    /// offline. A fresh start refuses when it disagrees with the local manifest.
    #[serde(default)]
    expected_baseline: Option<String>,
}

/// Reject a control request that doesn't carry the CLI's capability token in the
/// control header; returns the rejection response, or `None` when the request may
/// proceed. The token (minted per daemon start, stored `0600` in the data dir) is
/// compared in constant time, so presence of the header is no longer enough — only
/// a process that can read the owning user's token file (the `promptly` CLI) can
/// drive a mutation. A missing or wrong token is rejected identically.
fn control_guard(headers: &HeaderMap, expected: &str) -> Option<Response> {
    let provided = headers
        .get(CONTROL_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if control_token::token_matches(provided.as_bytes(), expected.as_bytes()) {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "control endpoints require the promptly CLI" })),
            )
                .into_response(),
        )
    }
}

/// Reject a request whose `Host` header names a non-loopback authority — the
/// fingerprint of DNS rebinding (a public page whose hostname was rebound to
/// `127.0.0.1` to reach the daemon). The loopback bind already refuses off-machine
/// peers and the CORS origin-lock blocks cross-origin reads; this is the third
/// layer. A request with no `Host` (a non-browser tool, HTTP/1.0) isn't a
/// rebinding attempt — that attack needs a *foreign* Host — so it passes, and the
/// capability token still gates every mutation regardless.
///
/// Shared with the OTLP receiver ([`crate::sources::otel`]), where the same
/// rebinding trick would otherwise let a rebound page inject fabricated telemetry
/// (a same-origin POST after rebinding sidesteps the receiver's no-CORS posture).
pub(crate) async fn host_guard(req: Request, next: Next) -> Response {
    if let Some(host) = req.headers().get(axum::http::header::HOST) {
        let allowed = host.to_str().map(is_loopback_host).unwrap_or(false);
        if !allowed {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "host not allowed" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Is `host` (a `Host` header value, `host[:port]`) a loopback authority —
/// `localhost`, `127.0.0.1`, or the IPv6 literal `[::1]`, with an optional numeric
/// port and nothing else? The scheme-less sibling of [`is_loopback_origin`].
fn is_loopback_host(host: &str) -> bool {
    // Split a trailing `:port` only when the right side is all digits, so the inner
    // colons of a bracketed IPv6 literal (`[::1]`) aren't mistaken for the port.
    let host = match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Map a [`StartError`] to a status code and JSON body.
fn start_error_response(err: StartError) -> Response {
    let status = match err {
        StartError::Manifest(_) => StatusCode::BAD_REQUEST,
        StartError::CannotReset(_) => StatusCode::UNPROCESSABLE_ENTITY,
        StartError::ManifestOutOfDate => StatusCode::UNPROCESSABLE_ENTITY,
        StartError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({ "error": err.to_string() }))).into_response()
}

fn internal_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// Bind the API on `addr` (loopback) and serve until shutdown.
pub async fn serve(
    addr: SocketAddr,
    state: ApiState,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("status API failed to bind {addr}: {e}"))?;
    tracing::info!(addr = %addr, "status API listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { wait_for_shutdown(&mut shutdown).await })
        .await?;
    tracing::info!("status API stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{sample_raw, Source};
    use crate::normalize::normalize;
    use crate::scoping::{NonceOrigin, SessionMarker, SESSION_MARKER_VERSION};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::path::PathBuf;
    use tower::ServiceExt;

    /// The control token the test `ApiState`s mint; control requests must echo it.
    const TEST_TOKEN: &str = "test-control-token";

    fn bound_marker() -> SessionMarker {
        SessionMarker {
            version: SESSION_MARKER_VERSION,
            session_id: "sess-1".into(),
            workspace: PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01".into(),
            started_at_ms: 1_000,
            stopped_at_ms: None,
            attempt_nonce: "nonce-xyz".into(),
            nonce_origin: NonceOrigin::Local,
            file_allowlist: vec!["lru.go".into()],
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    fn state_with_one_turn() -> ApiState {
        let turn = normalize(&sample_raw(Source::Otel, Some("claude-opus-4-8"), 100, 50));
        let adapters = AdapterRegistry::new();
        adapters.set(
            crate::sources::cursor::NAME,
            crate::sources::registry::AdapterState::Detected,
            "read 2 turns",
        );
        ApiState {
            shared: SharedState::new(Some(bound_marker()), vec![turn]),
            started_at_ms: 0,
            otlp_endpoint: "http://127.0.0.1:4318".into(),
            diagnostics: crate::diagnostics::Diagnostics::new(),
            store: SessionStore::new(std::env::temp_dir().join("promptlyd-api-noop")),
            workspace: PathBuf::from("/ws"),
            adapters,
            web_origins: Vec::new(),
            shutdown: tokio::sync::watch::channel(false).0,
            control_token: TEST_TOKEN.into(),
            ingest_auth: IngestAuth::closed(),
        }
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn get_with_origin(uri: &str, origin: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("origin", origin)
            .body(Body::empty())
            .unwrap()
    }

    const ACAO: &str = "access-control-allow-origin";

    #[tokio::test]
    async fn health_reports_ok_and_turn_count() {
        let resp = router(state_with_one_turn())
            .oneshot(get("/health"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["turns"], 1);
        // The scoped workspace is surfaced so the CLI can auto-manage the daemon.
        assert_eq!(body["workspace"], "/ws");
        assert_eq!(body["otlp_endpoint"], "http://127.0.0.1:4318");
        // The adapter registry is surfaced for `promptly doctor` (`21`).
        assert_eq!(body["adapters"][0]["name"], "cursor");
        assert_eq!(body["adapters"][0]["state"], "detected");
        assert_eq!(body["adapters"][0]["detail"], "read 2 turns");
    }

    #[tokio::test]
    async fn session_reports_the_bound_session_and_totals() {
        let resp = router(state_with_one_turn())
            .oneshot(get("/session"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["session"]["session_id"], "sess-1");
        // The bound level the session is attributing turns to (`18`).
        assert_eq!(body["session"]["level_id"], "lvl-1");
        assert_eq!(body["totals"]["tokens_input"], 100);
        assert_eq!(body["totals"]["turns"], 1);
        assert!(body["signals"].is_array());
        assert_eq!(body["captured"].as_array().unwrap().len(), 1);
        assert_eq!(body["captured"][0]["confidence"], "otel");
    }

    #[tokio::test]
    async fn session_never_serves_the_otlp_token_or_bootstrap_state() {
        // A consented session's marker carries the per-session secrets: the OTLP
        // ingest token and the harness-settings bootstrap record.
        let mut marker = bound_marker();
        marker.otlp_token = Some("secret-ingest-token".into());
        marker.bootstrap = Some(crate::bootstrap::BootstrapState {
            file_existed: false,
            dir_existed: false,
            env_existed: false,
            prior: Vec::new(),
        });
        let mut state = state_with_one_turn();
        state.shared = SharedState::new(Some(marker), Vec::new());

        let resp = router(state).oneshot(get("/session")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        // The browser-readable response must not carry the secrets — not even
        // as null keys — while the binding fields the bridge reads still serve.
        let session = body["session"].as_object().expect("session is bound");
        assert!(!session.contains_key("otlp_token"), "{session:?}");
        assert!(!session.contains_key("bootstrap"), "{session:?}");
        assert_eq!(session["slug"], "stage-1-01");
        assert_eq!(session["attempt_nonce"], "nonce-xyz");
        assert!(!serde_json::to_string(&body)
            .unwrap()
            .contains("secret-ingest-token"));
    }

    #[tokio::test]
    async fn stream_is_server_sent_events() {
        let resp = router(state_with_one_turn())
            .oneshot(get("/stream"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("text/event-stream"),
            "got {content_type}"
        );
    }

    #[tokio::test]
    async fn cors_allows_loopback_and_the_configured_web_origin_only() {
        let mut state = state_with_one_turn();
        state.web_origins = vec!["https://xpromptly.com".into()];
        let app = router(state);

        // The configured deployed origin is allowed (ACAO echoes it back).
        let resp = app
            .clone()
            .oneshot(get_with_origin("/stream", "https://xpromptly.com"))
            .await
            .unwrap();
        assert_eq!(resp.headers().get(ACAO).unwrap(), "https://xpromptly.com");

        // A loopback dev origin (local `next dev`) is always allowed.
        let resp = app
            .clone()
            .oneshot(get_with_origin("/health", "http://localhost:3000"))
            .await
            .unwrap();
        assert!(resp.headers().contains_key(ACAO));

        // Any other site is rejected: no ACAO header, so the browser blocks the
        // cross-origin read of the user's local telemetry.
        let resp = app
            .clone()
            .oneshot(get_with_origin("/stream", "https://evil.example"))
            .await
            .unwrap();
        assert!(!resp.headers().contains_key(ACAO));

        // A public origin whose host merely *starts with* the loopback string
        // (`http://localhost.evil.com`) must NOT be treated as loopback — a prefix
        // match would echo ACAO and leak local telemetry cross-origin.
        let resp = app
            .clone()
            .oneshot(get_with_origin("/stream", "http://localhost.evil.com"))
            .await
            .unwrap();
        assert!(!resp.headers().contains_key(ACAO));
    }

    #[test]
    fn loopback_origin_requires_an_exact_host() {
        // Genuine loopback dev origins (where local `next dev` is served) pass.
        assert!(is_loopback_origin(b"http://localhost"));
        assert!(is_loopback_origin(b"http://localhost:3000"));
        assert!(is_loopback_origin(b"http://127.0.0.1"));
        assert!(is_loopback_origin(b"http://127.0.0.1:8765"));

        // A host that merely *starts with* the loopback literal is a foreign,
        // attacker-controlled origin and must be rejected (the prefix-match bug).
        assert!(!is_loopback_origin(b"http://localhost.evil.com"));
        assert!(!is_loopback_origin(b"http://localhost.evil.com:80"));
        assert!(!is_loopback_origin(b"http://127.0.0.1.evil.com"));
        assert!(!is_loopback_origin(b"http://localhost-evil.com"));
        // A non-numeric "port" can't smuggle a foreign host past the split.
        assert!(!is_loopback_origin(b"http://localhost:1.evil.com"));
        assert!(!is_loopback_origin(b"http://127.0.0.1:8765.evil.com"));
        // Wrong scheme / junk is never a loopback dev origin.
        assert!(!is_loopback_origin(b"https://localhost"));
        assert!(!is_loopback_origin(b"http://evil.example"));
        assert!(!is_loopback_origin(b"localhost"));
        assert!(!is_loopback_origin(b"null"));
        assert!(!is_loopback_origin(b""));
    }

    #[tokio::test]
    async fn cors_answers_the_private_network_preflight_for_an_allowed_origin() {
        let mut state = state_with_one_turn();
        state.web_origins = vec!["https://xpromptly.com".into()];
        let app = router(state);

        // A public-origin page hitting `127.0.0.1` triggers Chrome's PNA preflight;
        // the daemon must answer it or the browser blocks the stream.
        let preflight = Request::builder()
            .method("OPTIONS")
            .uri("/stream")
            .header("origin", "https://xpromptly.com")
            .header("access-control-request-method", "GET")
            .header("access-control-request-private-network", "true")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(preflight).await.unwrap();
        assert_eq!(
            resp.headers()
                .get("access-control-allow-private-network")
                .unwrap(),
            "true"
        );
    }

    /// A real workspace (manifest + canonical files) and a fresh data dir, wired
    /// into an idle `ApiState` so the control endpoints can drive a session.
    fn control_state(label: &str) -> (ApiState, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("promptlyd-api-ctl-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let workspace = base.join("ws");
        let data_dir = base.join("data");
        std::fs::create_dir_all(workspace.join(".promptly")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(workspace.join("lru.go"), "package main // TODO\n").unwrap();
        let baseline = crate::baseline::hash_workspace(&workspace).unwrap();
        std::fs::write(
            workspace.join(".promptly/manifest.json"),
            format!(
                r#"{{"schema_version":1,"kit_version":1,"level_id":"lvl-9","slug":"stage-1-09","title":"X","language":"Go","runtime_version":"go1.22","execution_harness":"stdin_stdout","file_allowlist":["lru.go"],"baseline_hash":"{baseline}"}}"#
            ),
        )
        .unwrap();
        let state = ApiState {
            shared: SharedState::new(None, Vec::new()),
            started_at_ms: 0,
            otlp_endpoint: "http://127.0.0.1:4318".into(),
            diagnostics: crate::diagnostics::Diagnostics::new(),
            store: SessionStore::new(data_dir),
            workspace,
            adapters: AdapterRegistry::new(),
            web_origins: Vec::new(),
            shutdown: tokio::sync::watch::channel(false).0,
            control_token: TEST_TOKEN.into(),
            ingest_auth: IngestAuth::closed(),
        };
        (state, base)
    }

    fn control_post(uri: &str, with_header: bool) -> Request<Body> {
        let mut builder = Request::builder().method("POST").uri(uri);
        if with_header {
            builder = builder.header(CONTROL_HEADER, TEST_TOKEN);
        }
        builder.body(Body::empty()).unwrap()
    }

    fn control_post_json(uri: &str, json: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(CONTROL_HEADER, TEST_TOKEN)
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn start_then_stop_drives_the_session_binding() {
        let (state, base) = control_state("lifecycle");
        let app = router(state.clone());

        // Idle until started.
        assert!(state.shared.binding().is_none());

        // A control request without the header is rejected (CSRF guard).
        let forbidden = app
            .clone()
            .oneshot(control_post("/session/start", false))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        // Start binds the level and the daemon begins attributing.
        let resp = app
            .clone()
            .oneshot(control_post("/session/start", true))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "started");
        assert_eq!(body["session"]["level"]["level_id"], "lvl-9");
        let marker = state.shared.binding().expect("session is bound");
        assert!(marker.is_active());
        assert!(!marker.attempt_nonce.is_empty());
        // Consent wasn't given in the empty body -> JSONL-only.
        assert!(marker.bootstrap.is_none());

        // /session now reports the bound level.
        let session = body_json(app.clone().oneshot(get("/session")).await.unwrap()).await;
        assert_eq!(session["session"]["slug"], "stage-1-09");

        // Stop closes the window.
        let resp = app
            .clone()
            .oneshot(control_post("/session/stop", true))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!state.shared.binding().unwrap().is_active());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn a_consented_start_opens_the_ingest_gate_and_stop_closes_it() {
        let (state, base) = control_state("ingest-gate");
        let app = router(state.clone());

        // The receiver is closed while idle — no post is authorized.
        assert!(!state.ingest_auth.authorized(b"anything"));

        // A consented start mints an ingest token and opens the receiver to it.
        let resp = app
            .clone()
            .oneshot(control_post_json(
                "/session/start",
                r#"{"consent_bootstrap":true}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let token = state
            .shared
            .binding()
            .and_then(|m| m.otlp_token)
            .expect("a consented start mints an ingest token");
        assert!(state.ingest_auth.authorized(token.as_bytes()));
        assert!(!state.ingest_auth.authorized(b"a-different-token"));

        // Stop closes it again — a stale token no longer authenticates.
        app.clone()
            .oneshot(control_post("/session/stop", true))
            .await
            .unwrap();
        assert!(!state.ingest_auth.authorized(token.as_bytes()));

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn a_server_nonce_in_the_start_body_binds_and_reaches_verified() {
        let (state, base) = control_state("server-nonce");
        let app = router(state.clone());

        // A fresh start carrying a server-issued nonce binds the attempt to it.
        let resp = app
            .clone()
            .oneshot(control_post_json(
                "/session/start",
                r#"{"consent_bootstrap":false,"server_nonce":"srv-1"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "started");
        // The server nonce is what lifts the integrity ceiling to `verified`.
        assert_eq!(body["session"]["integrity_cap"], "verified");

        let marker = state.shared.binding().expect("session is bound");
        assert_eq!(marker.attempt_nonce, "srv-1");
        assert_eq!(marker.nonce_origin, NonceOrigin::Server);

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn a_start_supersedes_an_active_session_bound_elsewhere() {
        let (state, base) = control_state("supersede");
        // An active marker bound to a *different* workspace sits in the store —
        // the level-switch wedge (`stop` was skipped before the daemon moved).
        let mut stale = bound_marker();
        stale.session_id = "stale-1".into();
        stale.workspace = base.join("old-ws");
        state.store.save_marker(&stale).unwrap();

        // This used to answer 409 ("a capture session is already active…") and
        // wedge every start. The daemon now supersedes the stale session itself.
        let resp = router(state.clone())
            .oneshot(control_post("/session/start", true))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "started");

        // The stale session was archived (never adopted as the live binding).
        assert!(state.store.archive_dir().join("stale-1.json").exists());
        assert_ne!(state.shared.binding().unwrap().session_id, "stale-1");

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn starting_a_tampered_workspace_asks_for_confirmation() {
        let (state, base) = control_state("tampered");
        // Pre-modify the workspace so it no longer matches the baseline.
        std::fs::write(state.workspace.join("lru.go"), "package main // PASTED\n").unwrap();
        let resp = router(state.clone())
            .oneshot(control_post("/session/start", true))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "needs_reset_confirmation");
        assert!(state.shared.binding().is_none(), "no session was begun");
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn shutdown_requires_the_control_header_and_flips_the_signal() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let mut state = state_with_one_turn();
        state.shutdown = tx;
        let app = router(state);

        // Without the control header it's rejected (CSRF guard) and nothing flips.
        let forbidden = app
            .clone()
            .oneshot(control_post("/shutdown", false))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        assert!(
            !*rx.borrow_and_update(),
            "a rejected request never stops the daemon"
        );

        // With it, the daemon acknowledges and the shared shutdown flag goes true,
        // which is what stops the run loop and every capture component.
        let resp = app.oneshot(control_post("/shutdown", true)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["status"], "stopping");
        assert!(*rx.borrow_and_update(), "the shutdown signal flipped");
    }

    #[tokio::test]
    async fn control_rejects_a_wrong_or_missing_token() {
        let (state, base) = control_state("wrong-token");
        let app = router(state.clone());

        // No header at all -> rejected.
        let missing = app
            .clone()
            .oneshot(control_post("/session/start", false))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);

        // Header present but the wrong value -> still rejected. Presence alone (the
        // old guard) is no longer enough; the caller must hold the minted token.
        let wrong = Request::builder()
            .method("POST")
            .uri("/session/start")
            .header(CONTROL_HEADER, "not-the-real-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(wrong).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Neither rejected request bound a session (no side effects).
        assert!(state.shared.binding().is_none());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn host_guard_rejects_a_foreign_host_and_allows_loopback() {
        let app = router(state_with_one_turn());

        // A foreign `Host` (the DNS-rebinding fingerprint) is rejected even on a
        // read route.
        let foreign = Request::builder()
            .uri("/health")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(foreign).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // A loopback `Host` passes through to the handler.
        let loopback = Request::builder()
            .uri("/health")
            .header("host", "127.0.0.1:8765")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(loopback).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn loopback_host_accepts_only_loopback_authorities() {
        for ok in [
            "localhost",
            "localhost:3000",
            "127.0.0.1",
            "127.0.0.1:8765",
            "[::1]",
            "[::1]:8765",
        ] {
            assert!(is_loopback_host(ok), "{ok} should be loopback");
        }
        for bad in [
            "evil.example",
            "evil.example:80",
            "localhost.evil.com",
            "127.0.0.1.evil.com",
            "10.0.0.5",
            "",
        ] {
            assert!(!is_loopback_host(bad), "{bad} should not be loopback");
        }
    }
}
