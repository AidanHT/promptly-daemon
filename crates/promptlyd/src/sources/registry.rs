//! The harness-adapter status registry (`21`).
//!
//! The reverse-engineered adapters (Cursor, Codex, Copilot) read undocumented,
//! version-fragile sources, so the plan requires that when one can't read its
//! source the daemon **reports** it rather than crashing or emitting garbage.
//! Each adapter publishes its latest detection state here; `GET /health` serves a
//! snapshot, and `promptly doctor` (`19`) renders one line per adapter.
//!
//! This is a small shared map (adapters write, the API reads) — the same
//! lock-behind-`Arc` shape as [`crate::diagnostics::Diagnostics`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Whether an adapter could locate and read its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdapterState {
    /// Source located and read — capture from this harness is live.
    Detected,
    /// Source not present: the harness isn't installed, or it has produced no
    /// session data for the bound workspace yet. Expected, not an error.
    NotFound,
    /// Source present but its schema/version isn't one this adapter understands;
    /// capture from it is paused (the editor likely updated its format).
    Unsupported,
}

/// One adapter's current status, as served on `/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterStatus {
    /// The harness this adapter captures (`cursor` / `codex` / `copilot`).
    pub name: String,
    pub state: AdapterState,
    /// Human-readable context (the path checked, or why it degraded).
    pub detail: String,
}

/// A shared map of adapter name → its latest [`AdapterStatus`]. Cloneable `Arc`;
/// adapters call [`set`](AdapterRegistry::set), the API reads [`snapshot`].
#[derive(Debug, Clone, Default)]
pub struct AdapterRegistry {
    inner: Arc<Mutex<BTreeMap<&'static str, AdapterStatus>>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish (or replace) an adapter's status.
    pub fn set(&self, name: &'static str, state: AdapterState, detail: impl Into<String>) {
        self.inner.lock().unwrap().insert(
            name,
            AdapterStatus {
                name: name.to_string(),
                state,
                detail: detail.into(),
            },
        );
    }

    /// Every adapter's current status, ordered by name (stable for rendering).
    pub fn snapshot(&self) -> Vec<AdapterStatus> {
        self.inner.lock().unwrap().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_and_snapshot_is_name_ordered() {
        let reg = AdapterRegistry::new();
        reg.set("cursor", AdapterState::Detected, "read 3 bubbles");
        reg.set("codex", AdapterState::NotFound, "no ~/.codex/sessions");
        // A later set for the same adapter replaces, not duplicates.
        reg.set("cursor", AdapterState::Unsupported, "cursorDiskKV missing");

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        // BTreeMap key order: "codex" before "cursor".
        assert_eq!(snap[0].name, "codex");
        assert_eq!(snap[0].state, AdapterState::NotFound);
        assert_eq!(snap[1].name, "cursor");
        assert_eq!(snap[1].state, AdapterState::Unsupported);
        assert_eq!(snap[1].detail, "cursorDiskKV missing");
    }

    #[test]
    fn state_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&AdapterState::Detected).unwrap(),
            "\"detected\""
        );
        // `lowercase` (not snake_case) → the variant collapses to one word.
        assert_eq!(
            serde_json::to_string(&AdapterState::NotFound).unwrap(),
            "\"notfound\""
        );
        assert_eq!(
            serde_json::to_string(&AdapterState::Unsupported).unwrap(),
            "\"unsupported\""
        );
    }
}
