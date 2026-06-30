//! "A newer version is available" notice for the CLI.
//!
//! After most commands the CLI prints a one-line nudge when a newer release
//! exists. To stay invisible and cheap it consults a once-a-day cache under the
//! data dir and only reaches the network when that cache is stale — and even
//! then only in an interactive terminal (the caller gates on the TTY). The
//! version math and the cache/staleness decisions are pure and unit-tested; the
//! single network call reuses [`crate::updater::fetch_latest_tag`].
//!
//! It never fails a command: every fallible step degrades to "no nudge".

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::style::Style;
use crate::updater::{self, Version};

/// How long a cached check stays fresh: one day. A new release doesn't need
/// minute-level latency, and this keeps us far under GitHub's anonymous rate
/// limit (one request per day per machine).
const CACHE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Bound on the background API call — short, since an interactive command pays it
/// at most once a day when the cache is stale.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// Env var that disables the update check entirely (CI, scripts, or privacy).
pub const OPT_OUT_ENV: &str = "PROMPTLY_NO_UPDATE_CHECK";

/// The on-disk cache: when we last checked, and the latest tag we saw.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cache {
    checked_at_ms: i64,
    latest: String,
}

fn cache_path() -> PathBuf {
    promptlyd::paths::data_dir().join("update-check.json")
}

/// The outcome of a check: the running version and the latest known (if any).
pub struct UpdateStatus {
    pub current: Version,
    pub latest: Option<Version>,
}

impl UpdateStatus {
    /// Whether a strictly-newer release is available.
    pub fn is_outdated(&self) -> bool {
        match self.latest {
            Some(latest) => latest > self.current,
            None => false,
        }
    }
}

/// Resolve the update status. Reads the day cache; if it's missing or stale and
/// `allow_network` is set, refreshes it from GitHub (bounded, errors swallowed).
/// Never fails — the worst case is `latest: None`, i.e. no nudge.
pub fn status(now_ms: i64, allow_network: bool) -> UpdateStatus {
    let current = Version::current();
    if std::env::var_os(OPT_OUT_ENV).is_some() {
        return UpdateStatus {
            current,
            latest: None,
        };
    }

    let cached = read_cache();
    let fresh = cached
        .as_ref()
        .map(|c| !is_stale(c.checked_at_ms, now_ms))
        .unwrap_or(false);

    // Fresh cache, or no permission to go online: use whatever we last saw.
    if fresh || !allow_network {
        let latest = cached.and_then(|c| Version::parse(&c.latest));
        return UpdateStatus { current, latest };
    }

    // Stale and allowed online: refresh, best-effort.
    match updater::fetch_latest_tag(NOTIFY_TIMEOUT) {
        Ok(tag) => {
            write_cache(&Cache {
                checked_at_ms: now_ms,
                latest: tag.clone(),
            });
            UpdateStatus {
                current,
                latest: Version::parse(&tag),
            }
        }
        Err(_) => {
            // Back off for the TTL even on failure so we don't retry on every
            // command, but keep showing the last known latest.
            if let Some(c) = &cached {
                write_cache(&Cache {
                    checked_at_ms: now_ms,
                    latest: c.latest.clone(),
                });
            }
            let latest = cached.and_then(|c| Version::parse(&c.latest));
            UpdateStatus { current, latest }
        }
    }
}

/// The one-line nudge, or `None` when up to date. Restrained styling: a dim
/// lead-in, the new version in accent, the action dim.
pub fn banner(status: &UpdateStatus, style: Style) -> Option<String> {
    if !status.is_outdated() {
        return None;
    }
    let latest = status.latest?;
    Some(format!(
        "{} {} {}",
        style.dim("→ a newer promptly is available:"),
        style.accent(&format!("v{latest}")),
        style.dim("· run `promptly update`"),
    ))
}

fn is_stale(checked_at_ms: i64, now_ms: i64) -> bool {
    now_ms.saturating_sub(checked_at_ms) >= CACHE_TTL_MS
}

fn read_cache() -> Option<Cache> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(cache: &Cache) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with(current: &str, latest: Option<&str>) -> UpdateStatus {
        UpdateStatus {
            current: Version::parse(current).unwrap(),
            latest: latest.and_then(Version::parse),
        }
    }

    #[test]
    fn outdated_only_when_latest_is_strictly_newer() {
        assert!(status_with("0.1.0", Some("0.2.0")).is_outdated());
        assert!(!status_with("0.2.0", Some("0.2.0")).is_outdated());
        assert!(!status_with("0.2.0", Some("0.1.0")).is_outdated());
        assert!(!status_with("0.1.0", None).is_outdated());
    }

    #[test]
    fn banner_appears_only_when_outdated_and_names_the_command() {
        assert!(banner(&status_with("0.2.0", Some("0.2.0")), Style::plain()).is_none());
        let line = banner(&status_with("0.1.0", Some("0.2.0")), Style::plain()).unwrap();
        assert!(line.contains("v0.2.0"));
        assert!(line.contains("promptly update"));
        // Plain style stays escape-free — it can land in a piped stderr.
        assert!(!line.contains('\x1b'));
    }

    #[test]
    fn staleness_respects_the_ttl() {
        assert!(!is_stale(1_000, 1_000)); // same instant: fresh
        assert!(!is_stale(1_000, 1_000 + CACHE_TTL_MS - 1)); // just under: fresh
        assert!(is_stale(1_000, 1_000 + CACHE_TTL_MS)); // at the TTL: stale
                                                        // Clock skew (now < checked) is treated as fresh, not stale.
        assert!(!is_stale(5_000, 1_000));
    }

    #[test]
    fn cache_round_trips_through_serde() {
        let json = serde_json::to_string(&Cache {
            checked_at_ms: 123,
            latest: "0.2.0".into(),
        })
        .unwrap();
        let back: Cache = serde_json::from_str(&json).unwrap();
        assert_eq!(back.checked_at_ms, 123);
        assert_eq!(back.latest, "0.2.0");
    }

    #[test]
    fn cache_path_sits_under_the_data_dir() {
        assert!(cache_path().ends_with("update-check.json"));
    }
}
