//! Client for the Promptly web app's public routes.
//!
//! Two needs, both over plain HTTP(S): downloading a level's starter kit zip
//! (`GET /api/levels/{slug}/kit`, public — `07`) for `promptly init`, and probing
//! the execution backend (`GET /api/execution/health`, `08`) for `promptly
//! doctor`. Ranked submission and remote grading go through authenticated routes
//! owned by cloud pairing (`20`).

use std::io::Read;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Hard cap on a downloaded kit, so a hostile/huge response can't exhaust memory.
/// Kits are well under a megabyte; 64 MiB is comfortably generous.
const MAX_KIT_BYTES: u64 = 64 * 1024 * 1024;

/// Why a web-app request failed.
#[derive(Debug, Error)]
pub enum WebError {
    #[error(
        "couldn't reach the Promptly web app at {0} — is it running? Pass --api-url or set PROMPTLY_API_URL"
    )]
    NotReachable(String),
    #[error("level '{0}' was not found (unknown or inactive)")]
    NotFound(String),
    #[error("web app returned {0}")]
    Http(String),
    #[error("download failed: {0}")]
    Io(String),
}

/// The starter-kit zip source for `init` — a trait so the command is testable
/// against an in-memory kit without a server.
pub trait KitSource {
    fn download_kit(&self, slug: &str) -> Result<Vec<u8>, WebError>;
}

/// `GET /api/execution/health` (`08`): whether the Judge0 backend is reachable.
#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionHealth {
    pub healthy: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// A blocking HTTP client for one web-app origin.
pub struct WebClient {
    agent: ureq::Agent,
    base: String,
}

impl WebClient {
    pub fn new(api_url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(30))
            .build();
        Self {
            agent,
            base: api_url.trim_end_matches('/').to_string(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Probe the execution backend's health (`doctor`).
    pub fn execution_health(&self) -> Result<ExecutionHealth, WebError> {
        let url = format!("{}/api/execution/health", self.base);
        // The route returns 503 with a JSON body when unhealthy, so read the body
        // on both the ok and error paths rather than treating 503 as a failure.
        let body = match self.agent.get(&url).call() {
            Ok(resp) => resp
                .into_string()
                .map_err(|e| WebError::Io(e.to_string()))?,
            Err(ureq::Error::Status(_, resp)) => resp
                .into_string()
                .map_err(|e| WebError::Io(e.to_string()))?,
            Err(ureq::Error::Transport(_)) => {
                return Err(WebError::NotReachable(self.base.clone()))
            }
        };
        serde_json::from_str(&body).map_err(|e| WebError::Http(format!("bad health response: {e}")))
    }
}

impl KitSource for WebClient {
    fn download_kit(&self, slug: &str) -> Result<Vec<u8>, WebError> {
        let url = format!("{}/api/levels/{slug}/kit", self.base);
        match self.agent.get(&url).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader()
                    .take(MAX_KIT_BYTES)
                    .read_to_end(&mut buf)
                    .map_err(|e| WebError::Io(e.to_string()))?;
                Ok(buf)
            }
            Err(ureq::Error::Status(404, _)) => Err(WebError::NotFound(slug.to_string())),
            Err(ureq::Error::Status(code, resp)) => Err(WebError::Http(format!(
                "HTTP {code}: {}",
                resp.into_string().unwrap_or_default()
            ))),
            Err(ureq::Error::Transport(_)) => Err(WebError::NotReachable(self.base.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_web_app_reports_not_reachable() {
        // Port 1 on loopback has nothing listening.
        let client = WebClient::new("http://127.0.0.1:1");
        assert!(matches!(
            client.download_kit("stage-1-01-x"),
            Err(WebError::NotReachable(_))
        ));
    }

    #[test]
    fn execution_health_parses_the_documented_shape() {
        let json =
            r#"{"healthy":false,"reason":"not_configured","message":"no token","version":null}"#;
        let h: ExecutionHealth = serde_json::from_str(json).unwrap();
        assert!(!h.healthy);
        assert_eq!(h.reason, "not_configured");
        assert_eq!(h.message.as_deref(), Some("no token"));
    }
}
