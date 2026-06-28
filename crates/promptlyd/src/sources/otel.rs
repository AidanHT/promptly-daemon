//! The embedded OTLP receiver: a localhost-only HTTP server that ingests Claude
//! Code's native OpenTelemetry log export and feeds `api_request` events to the
//! engine. It speaks OTLP/HTTP+JSON (the bootstrap in `18` selects that protocol
//! via `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`). Binding is loopback-only.

use std::net::SocketAddr;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use super::{wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::otlp::turns_from_logs_json;

/// Runs the OTLP receiver on a loopback address owned by the daemon.
pub struct OtelSource {
    addr: SocketAddr,
}

impl OtelSource {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

fn router(sink: RawTurnSink) -> Router {
    Router::new()
        .route("/v1/logs", post(logs))
        .route("/v1/metrics", post(metrics))
        .with_state(sink)
}

/// `POST /v1/logs` — the turn-bearing endpoint. Parses the OTLP/JSON envelope and
/// forwards each `api_request` event to the engine.
async fn logs(
    State(sink): State<RawTurnSink>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
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
                if sink.send(turn).await.is_err() {
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

/// `POST /v1/metrics` — accepted so the exporter doesn't error/retry, but not
/// turned into turns. The per-request `api_request` log event is the turn unit,
/// which sidesteps metric delta/cumulative double-counting.
async fn metrics() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({})))
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
        axum::serve(listener, router(sink))
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

    fn post(uri: &str, content_type: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn posting_otlp_logs_emits_a_turn() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let resp = router(tx)
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
        let resp = router(tx)
            .oneshot(post("/v1/logs", "application/x-protobuf", ""))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let resp = router(tx)
            .oneshot(post("/v1/logs", "application/json", "{not json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
