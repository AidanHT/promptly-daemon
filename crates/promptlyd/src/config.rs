//! Runtime configuration: where the daemon binds, which workspace it scopes to,
//! and where its machine-local state lives. Built once at startup from CLI args
//! and the path defaults, then shared (read-only) across the async components.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use crate::paths;

/// Default localhost port for the embedded OTLP/HTTP receiver. 4318 is the OTLP
/// HTTP convention, so a harness pointed at the default endpoint just works.
pub const DEFAULT_OTLP_PORT: u16 = 4318;
/// Default localhost port for the daemon's own status/stream API.
pub const DEFAULT_API_PORT: u16 = 8765;

/// The canonical deployed Promptly web origin(s) the read/stream API's CORS
/// always allows, so the live HUD bridge (`22`) works against production with
/// **no per-machine configuration** — the daemon is a distributed binary and
/// can't read the web app's `PROMPTLY_SITE_URL`, so the stable origin is baked
/// in here (mirror of `siteOrigin()` in `lib/env.ts`; see
/// `docs/auth-production-setup.md`). Custom domains / preview deploys add origins
/// via `PROMPTLY_WEB_ORIGIN` or `--web-origin`; loopback dev origins are always
/// allowed separately (`api::is_loopback_origin`). It is deliberately an exact
/// origin, never a `*.vercel.app` wildcard — a wildcard would let any Vercel-
/// hosted site read a user's local telemetry, defeating the origin lock.
pub const DEFAULT_WEB_ORIGINS: &[&str] = &["https://trypromptly.vercel.app"];

/// Immutable daemon configuration. All network addresses are loopback by
/// construction — the daemon is never reachable off the machine.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// The workspace whose AI usage this daemon scopes capture to.
    pub workspace: PathBuf,
    /// Promptly's machine-local data dir (`~/.promptly`).
    pub data_dir: PathBuf,
    /// Claude Code's `projects/` log dir (`~/.claude/projects`).
    pub claude_projects_dir: PathBuf,
    /// Loopback address of the status/stream API.
    pub api_addr: SocketAddr,
    /// Loopback address of the embedded OTLP receiver.
    pub otlp_addr: SocketAddr,
    /// Deployed Promptly web origin(s) the read/stream API's CORS allows in
    /// addition to loopback dev origins (`22`). Empty means loopback-only.
    pub web_origins: Vec<String>,
}

impl DaemonConfig {
    /// Build a config from an explicit workspace, ports, and allowed web
    /// origins, filling the rest from the path defaults (which themselves honor
    /// the test env overrides).
    pub fn new(
        workspace: PathBuf,
        api_port: u16,
        otlp_port: u16,
        web_origins: Vec<String>,
    ) -> Self {
        Self {
            workspace,
            data_dir: paths::data_dir(),
            claude_projects_dir: paths::claude_projects_dir(),
            api_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, api_port)),
            otlp_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, otlp_port)),
            web_origins,
        }
    }

    /// Path of the versioned crash-recovery checkpoint.
    pub fn checkpoint_path(&self) -> PathBuf {
        self.data_dir.join("checkpoint.json")
    }

    /// Lock file guaranteeing a single `promptlyd` process per machine.
    pub fn process_lock_path(&self) -> PathBuf {
        self.data_dir.join("promptlyd.lock")
    }

    /// Lock file guaranteeing a single active capture session at a time.
    pub fn session_lock_path(&self) -> PathBuf {
        self.data_dir.join("session.lock")
    }
}

/// Resolve the deployed web origins the read/stream API's CORS allows: the
/// baked-in production default(s) ([`DEFAULT_WEB_ORIGINS`]), plus any from the
/// `PROMPTLY_WEB_ORIGIN` env var (comma- or whitespace-separated), plus the
/// `--web-origin` flags. Reads the env once at startup; the pure merge is
/// [`merge_web_origins`].
pub fn resolve_web_origins(flags: Vec<String>) -> Vec<String> {
    let env = std::env::var("PROMPTLY_WEB_ORIGIN").unwrap_or_default();
    merge_web_origins(DEFAULT_WEB_ORIGINS, &env, flags)
}

/// Merge the baked-in defaults, the `PROMPTLY_WEB_ORIGIN` env string, and the
/// `--web-origin` flags into one de-duplicated allowlist. Each origin is
/// trailing-slash-trimmed (a browser `Origin` header carries `scheme://host[:port]`
/// with no trailing slash, and the CORS predicate compares exact bytes) and
/// empties are dropped, so a stray separator or trailing `/` never silently
/// produces an origin that can't match.
fn merge_web_origins(defaults: &[&str], env: &str, flags: Vec<String>) -> Vec<String> {
    let from_env = env.split([',', ' ', '\t', '\n']).map(str::to_string);
    let mut out: Vec<String> = Vec::new();
    for origin in defaults
        .iter()
        .map(|o| o.to_string())
        .chain(from_env)
        .chain(flags)
    {
        let trimmed = origin.trim().trim_end_matches('/');
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_loopback_only() {
        let cfg = DaemonConfig::new(
            PathBuf::from("/ws"),
            DEFAULT_API_PORT,
            DEFAULT_OTLP_PORT,
            Vec::new(),
        );
        assert!(cfg.api_addr.ip().is_loopback());
        assert!(cfg.otlp_addr.ip().is_loopback());
        assert_eq!(cfg.api_addr.port(), DEFAULT_API_PORT);
        assert_eq!(cfg.otlp_addr.port(), DEFAULT_OTLP_PORT);
    }

    #[test]
    fn state_files_live_under_the_data_dir() {
        let cfg = DaemonConfig::new(PathBuf::from("/ws"), 1, 2, Vec::new());
        assert!(cfg.checkpoint_path().starts_with(&cfg.data_dir));
        assert!(cfg.process_lock_path().starts_with(&cfg.data_dir));
        assert!(cfg.session_lock_path().starts_with(&cfg.data_dir));
        assert_ne!(cfg.process_lock_path(), cfg.session_lock_path());
    }

    #[test]
    fn the_production_origin_is_always_allowed_with_no_config() {
        // The headline of the fix: a player on the deployed app needs zero
        // per-machine setup for the live bridge to be reachable.
        let origins = merge_web_origins(DEFAULT_WEB_ORIGINS, "", Vec::new());
        assert_eq!(origins, vec!["https://trypromptly.vercel.app".to_string()]);
    }

    #[test]
    fn env_and_flags_extend_the_defaults_and_dedupe() {
        let origins = merge_web_origins(
            &["https://trypromptly.vercel.app"],
            "https://preview.example.com, https://trypromptly.vercel.app",
            vec!["https://custom.example.org/".into()],
        );
        assert_eq!(
            origins,
            vec![
                "https://trypromptly.vercel.app".to_string(),
                "https://preview.example.com".to_string(),
                // The trailing slash is trimmed so it matches a browser Origin.
                "https://custom.example.org".to_string(),
            ],
            "defaults first, env then flags, de-duplicated, slash-trimmed",
        );
    }

    #[test]
    fn blank_separators_never_yield_an_empty_origin() {
        // A stray comma/space in PROMPTLY_WEB_ORIGIN must not inject "" (which
        // would never match a real Origin but pollutes the allowlist).
        let origins = merge_web_origins(&[], "  , ,https://a.test ", Vec::new());
        assert_eq!(origins, vec!["https://a.test".to_string()]);
    }
}
