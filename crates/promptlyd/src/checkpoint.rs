//! The crash-recovery checkpoint.
//!
//! A simple versioned JSON file under the data dir (`~/.promptly/checkpoint.json`)
//! that lets a restart resume without losing or double-counting turns. It records
//! the session, the captured turns so far, the per-file JSONL offsets (so tailing
//! resumes mid-file), and the set of already-seen raw-turn ids (so a re-read line
//! or resent event isn't counted twice). Machine-local; never synced.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::NormalizedTurn;

/// Bump when the on-disk shape changes; an older/mismatched file is discarded.
pub const CHECKPOINT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub session_id: String,
    pub started_at_ms: i64,
    pub turns: Vec<NormalizedTurn>,
    /// JSONL byte offsets keyed by path string (so it round-trips as JSON).
    pub jsonl_offsets: HashMap<String, u64>,
    /// Already-seen raw-turn content ids, the de-duplication set.
    pub seen: Vec<String>,
}

impl Checkpoint {
    /// Load a checkpoint, returning `None` (and logging) if it is absent, corrupt,
    /// or a version we don't understand — all of which mean "start fresh".
    pub fn load(path: &Path) -> Option<Checkpoint> {
        let bytes = std::fs::read(path).ok()?;
        match serde_json::from_slice::<Checkpoint>(&bytes) {
            Ok(cp) if cp.version == CHECKPOINT_VERSION => Some(cp),
            Ok(cp) => {
                tracing::warn!(
                    found = cp.version,
                    expected = CHECKPOINT_VERSION,
                    "checkpoint version mismatch; starting fresh"
                );
                None
            }
            Err(err) => {
                tracing::warn!(%err, "corrupt checkpoint; starting fresh");
                None
            }
        }
    }

    /// Persist atomically: write a temp file then rename over the target, so a
    /// crash mid-write never leaves a half-written checkpoint.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Convert live `PathBuf`-keyed offsets to the JSON-friendly string map.
pub fn offsets_to_strings(offsets: &HashMap<PathBuf, u64>) -> HashMap<String, u64> {
    offsets
        .iter()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), *v))
        .collect()
}

/// Convert restored string-keyed offsets back to a `PathBuf`-keyed map.
pub fn offsets_from_strings(offsets: &HashMap<String, u64>) -> HashMap<PathBuf, u64> {
    offsets
        .iter()
        .map(|(k, v)| (PathBuf::from(k), *v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("promptlyd-cp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{label}.json"))
    }

    fn checkpoint() -> Checkpoint {
        Checkpoint {
            version: CHECKPOINT_VERSION,
            session_id: "sess-1".into(),
            started_at_ms: 1_000,
            turns: Vec::new(),
            jsonl_offsets: HashMap::from([("/x/s.jsonl".to_string(), 42)]),
            seen: vec!["abc".into()],
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let path = tmp_path("roundtrip");
        checkpoint().save(&path).unwrap();
        let loaded = Checkpoint::load(&path).expect("loads back");
        assert_eq!(loaded.session_id, "sess-1");
        assert_eq!(loaded.jsonl_offsets.get("/x/s.jsonl").copied(), Some(42));
        assert_eq!(loaded.seen, vec!["abc".to_string()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_version_mismatch_and_corruption() {
        let path = tmp_path("badversion");
        let mut cp = checkpoint();
        cp.version = 999;
        cp.save(&path).unwrap();
        assert!(
            Checkpoint::load(&path).is_none(),
            "version mismatch -> fresh"
        );

        std::fs::write(&path, "{ not json").unwrap();
        assert!(Checkpoint::load(&path).is_none(), "corrupt -> fresh");

        assert!(
            Checkpoint::load(&tmp_path("missing")).is_none(),
            "absent -> fresh"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn offset_key_conversions_are_inverse() {
        let live = HashMap::from([(PathBuf::from("/a/b.jsonl"), 7u64)]);
        let strings = offsets_to_strings(&live);
        assert_eq!(offsets_from_strings(&strings), live);
    }
}
