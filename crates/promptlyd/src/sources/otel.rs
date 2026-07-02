//! The embedded OTLP receiver: a localhost-only HTTP server that ingests Claude
//! Code's native OpenTelemetry log export and feeds `api_request` events to the
//! engine. It speaks OTLP/HTTP+JSON (the bootstrap in `18` selects that protocol
//! via `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`). Binding is loopback-only.
//!
//! Loopback alone doesn't identify the *poster*: any process on the machine (or a
//! DNS-rebound page) could POST fabricated `api_request` events and inflate — or
//! forge outright — a verified attempt. So a consented session mints a fresh ingest
//! token (`18`), writes it into the harness settings as a request header, and binds
//! it here via [`IngestAuth`]: the receiver accepts a post only when it carries the
//! active session's token, and rejects everything while idle / JSONL-only. The
//! token is checked **before the body is parsed**, so an unauthenticated caller
//! never reaches the turn pipeline.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{middleware, Json, Router};
use serde_json::json;

use super::{wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::otlp::turns_from_logs_json;
use crate::scoping::SessionMarker;

/// The request header (set by the bootstrap's `OTEL_EXPORTER_OTLP_HEADERS`) that
/// carries a session's ingest token; the receiver checks it constant-time. Lower
/// case because HTTP header names compare case-insensitively and axum normalizes.
const OTLP_TOKEN_REQUEST_HEADER: &str = "x-promptly-otlp-token";

/// Who may POST telemetry to the OTLP receiver right now.
#[derive(Debug, Clone)]
pub enum IngestPolicy {
    /// Reject all ingest — no consented OTEL session is active (idle, stopped, or
    /// JSONL-only). Closes the "decline consent, then inject fabricated OTEL" hole
    /// and stops any other loopback process posting into an otherwise-idle receiver.
    Closed,
    /// Accept only posts carrying this session's token in the ingest header.
    Token(String),
}

/// A shared, runtime-updatable handle to the receiver's [`IngestPolicy`]. The
/// control layer ([`crate::api`]) swaps it on session start/stop; the receiver
/// reads it per request. Cheap to clone (an `Arc`).
#[derive(Debug, Clone)]
pub struct IngestAuth(Arc<Mutex<IngestPolicy>>);

impl IngestAuth {
    /// A receiver that rejects all ingest until a consented session opens it.
    pub fn closed() -> Self {
        Self(Arc::new(Mutex::new(IngestPolicy::Closed)))
    }

    /// Open ingest to exactly `token` (a consented session's minted token).
    pub fn open(&self, token: String) {
        *self.0.lock().unwrap() = IngestPolicy::Token(token);
    }

    /// Close ingest (stop / idle / JSONL-only) — reject everything.
    pub fn close(&self) {
        *self.0.lock().unwrap() = IngestPolicy::Closed;
    }

    /// Reconcile the policy to a session marker: open to its token when the session
    /// is active and consented (has a token), else close. A JSONL-only or legacy
    /// (token-less) marker, or a stopped/absent one, closes the receiver — the
    /// bootstrap that made the harness export is reverted in those states anyway.
    pub fn set_from_marker(&self, marker: Option<&SessionMarker>) {
        match marker {
            Some(m) if m.is_active() => match &m.otlp_token {
                Some(token) => self.open(token.clone()),
                None => self.close(),
            },
            _ => self.close(),
        }
    }

    /// Whether a request bearing `provided` (the ingest-token header value, or empty
    /// when absent) is authorized. `Closed` rejects everything, even a correct token.
    pub fn authorized(&self, provided: &[u8]) -> bool {
        match &*self.0.lock().unwrap() {
            IngestPolicy::Closed => false,
            IngestPolicy::Token(expected) => {
                crate::control_token::token_matches(provided, expected.as_bytes())
            }
        }
    }
}

/// The receiver's shared state: the engine sink plus the ingest-auth handle.
#[derive(Clone)]
struct OtelState {
    sink: RawTurnSink,
    auth: IngestAuth,
}

/// Runs the OTLP receiver on a loopback address owned by the daemon.
pub struct OtelSource {
    addr: SocketAddr,
    auth: IngestAuth,
}

impl OtelSource {
    pub fn new(addr: SocketAddr, auth: IngestAuth) -> Self {
        Self { addr, auth }
    }
}

fn router(state: OtelState) -> Router {
    Router::new()
        .route("/v1/logs", post(logs))
        .route("/v1/metrics", post(metrics))
        // Reject a non-loopback `Host` so a DNS-rebound page can't inject fabricated
        // telemetry into the capture stream (a same-origin POST after rebinding
        // sidesteps the receiver's no-CORS posture). Shared with the status API.
        .layer(middleware::from_fn(crate::api::host_guard))
        .with_state(state)
}

/// Reject an OTLP post that isn't authorized under the current ingest policy —
/// before the body is parsed, so an unauthenticated caller can never forward a
/// forged turn into the engine. Returns the 401 response, or `None` to proceed.
fn reject_unauthorized(auth: &IngestAuth, headers: &HeaderMap) -> Option<Response> {
    let provided = headers
        .get(OTLP_TOKEN_REQUEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth.authorized(provided.as_bytes()) {
        None
    } else {
        Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "OTLP ingest requires this session's promptly token"
                })),
            )
                .into_response(),
        )
    }
}

/// `POST /v1/logs` — the turn-bearing endpoint. Authenticates the poster, then
/// parses the OTLP/JSON envelope and forwards each `api_request` event to the
/// engine.
async fn logs(
    State(state): State<OtelState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(resp) = reject_unauthorized(&state.auth, &headers) {
        return resp;
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.contains("protobuf") {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": "promptlyd accepts OTLP/HTTP JSON; set OTEL_EXPORTER_OTLP_PROTOCOL=http/json"
            })),
        )
            .into_response();
    }

    match turns_from_logs_json(&body, now_ms()) {
        Ok(turns) => {
            for turn in turns {
                if state.sink.send(turn).await.is_err() {
                    tracing::warn!("dropping OTEL turn: engine channel closed");
                    break;
                }
            }
            // OTLP success response (`ExportLogsServiceResponse`).
            (StatusCode::OK, Json(json!({}))).into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "rejecting malformed OTLP logs payload");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid OTLP/JSON logs payload" })),
            )
                .into_response()
        }
    }
}

/// `POST /v1/metrics` — authenticated like `/v1/logs` (so a closed/idle receiver
/// refuses it too), accepted so the exporter doesn't error/retry, but not turned
/// into turns. The per-request `api_request` log event is the turn unit, which
/// sidesteps metric delta/cumulative double-counting.
async fn metrics(State(state): State<OtelState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(resp) = reject_unauthorized(&state.auth, &headers) {
        return resp;
    }
    (StatusCode::OK, Json(json!({}))).into_response()
}

#[async_trait]
impl TelemetrySource for OtelSource {
    fn name(&self) -> &'static str {
        "otel"
    }

    async fn run(self: Box<Self>, sink: RawTurnSink, mut shutdown: Shutdown) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .map_err(|e| anyhow::anyhow!("OTLP receiver failed to bind {}: {e}", self.addr))?;
        tracing::info!(addr = %self.addr, "OTLP receiver listening");
        let state = OtelState {
            sink,
            auth: self.auth.clone(),
        };
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async move { wait_for_shutdown(&mut shutdown).await })
            .await?;
        tracing::info!("OTLP receiver stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const API_REQUEST: &str = r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[
      {"body":{"stringValue":"api_request"},"attributes":[
        {"key":"model","value":{"stringValue":"claude-opus-4-8"}},
        {"key":"input_tokens","value":{"intValue":"10"}},
        {"key":"output_tokens","value":{"intValue":"20"}}]}]}]}]}"#;

    const TOKEN: &str = "test-token";

    /// A receiver state whose ingest policy accepts [`TOKEN`].
    fn open_state(sink: RawTurnSink) -> OtelState {
        let auth = IngestAuth::closed();
        auth.open(TOKEN.to_string());
        OtelState { sink, auth }
    }

    /// A minimal active session marker carrying `otlp_token`, for policy tests.
    fn marker_with_token(token: Option<&str>) -> SessionMarker {
        SessionMarker {
            version: crate::scoping::SESSION_MARKER_VERSION,
            session_id: "sess-1".into(),
            workspace: std::path::PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01".into(),
            started_at_ms: 1_000,
            stopped_at_ms: None,
            attempt_nonce: "nonce-1".into(),
            nonce_origin: crate::scoping::NonceOrigin::Server,
            file_allowlist: vec![],
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: token.map(str::to_string),
            baseline_attested: token.is_some(),
        }
    }

    /// A `POST` carrying the ingest token (the authenticated happy path).
    fn post(uri: &str, content_type: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", content_type)
            .header(OTLP_TOKEN_REQUEST_HEADER, TOKEN)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn posting_otlp_logs_emits_a_turn() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let resp = router(open_state(tx))
            .oneshot(post("/v1/logs", "application/json", API_REQUEST))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let turn = rx.try_recv().expect("a turn was forwarded");
        assert_eq!(turn.source, crate::model::Source::Otel);
        assert_eq!(turn.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(turn.tokens_output, 20);
    }

    #[tokio::test]
    async fn protobuf_content_type_is_unsupported() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let resp = router(open_state(tx))
            .oneshot(post("/v1/logs", "application/x-protobuf", ""))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let resp = router(open_state(tx))
            .oneshot(post("/v1/logs", "application/json", "{not json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ingest_without_the_session_token_is_rejected_before_parsing() {
        // No token header: the receiver 401s and never forwards a (would-be valid)
        // turn — the fabricated-telemetry injection this layer closes.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header("content-type", "application/json")
            .body(Body::from(API_REQUEST))
            .unwrap();
        let resp = router(open_state(tx)).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err(), "no turn was forwarded");
    }

    #[tokio::test]
    async fn ingest_with_the_wrong_token_is_rejected() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header("content-type", "application/json")
            .header(OTLP_TOKEN_REQUEST_HEADER, "not-the-token")
            .body(Body::from(API_REQUEST))
            .unwrap();
        let resp = router(open_state(tx)).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_closed_receiver_rejects_even_a_correct_looking_token() {
        // Idle / JSONL-only: the policy is Closed, so every post is refused.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let state = OtelState {
            sink: tx,
            auth: IngestAuth::closed(),
        };
        let resp = router(state)
            .oneshot(post("/v1/logs", "application/json", API_REQUEST))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn metrics_requires_the_token_too() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Authorized metrics post → 200.
        let ok = router(open_state(tx.clone()))
            .oneshot(post("/v1/metrics", "application/json", "{}"))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        // Unauthenticated metrics post → 401.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/metrics")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let denied = router(open_state(tx)).oneshot(req).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn set_from_marker_opens_for_an_active_token_and_closes_otherwise() {
        let auth = IngestAuth::closed();
        assert!(!auth.authorized(TOKEN.as_bytes()));
        // An active, consented marker opens ingest to its token.
        let mut marker = marker_with_token(Some(TOKEN));
        auth.set_from_marker(Some(&marker));
        assert!(auth.authorized(TOKEN.as_bytes()));
        assert!(!auth.authorized(b"other"));
        // A JSONL-only (token-less) active marker closes it.
        auth.set_from_marker(Some(&marker_with_token(None)));
        assert!(!auth.authorized(TOKEN.as_bytes()));
        // Re-open, then a stopped marker closes it again.
        auth.set_from_marker(Some(&marker_with_token(Some(TOKEN))));
        assert!(auth.authorized(TOKEN.as_bytes()));
        marker.stopped_at_ms = Some(marker.started_at_ms + 1);
        auth.set_from_marker(Some(&marker));
        assert!(!auth.authorized(TOKEN.as_bytes()));
    }

    #[tokio::test]
    async fn a_foreign_host_is_rejected() {
        // DNS-rebinding fingerprint: a foreign `Host` is refused before the payload
        // is parsed, so a rebound page can't inject fabricated telemetry.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header("content-type", "application/json")
            .header(OTLP_TOKEN_REQUEST_HEADER, TOKEN)
            .header("host", "evil.example")
            .body(Body::from(API_REQUEST))
            .unwrap();
        let resp = router(open_state(tx)).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(rx.try_recv().is_err(), "no turn was forwarded");
    }
}
