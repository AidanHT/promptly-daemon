//! The crash-recovery checkpoint.
//!
//! A simple versioned JSON file under the data dir (`~/.promptly/checkpoint.json`)
//! that lets a restart resume without losing or double-counting turns. It records
//! the session, the captured turns so far, the per-file JSONL offsets (so tailing
//! resumes mid-file), and the set of already-seen raw-turn ids (so a re-read line
//! or resent event isn't counted twice). Machine-local; never synced.
//!
//! It is also **sealed**: each save stamps the file with the [`crate::ledger`]
//! head over its turns, and a load recomputes that head and discards the
//! checkpoint if it no longer matches. So an offline edit of the persisted
//! capture (lowering a turn's tokens, deleting a turn, flipping an integrity
//! signal) makes the daemon start fresh rather than resume the doctored turns —
//! the tampered data is denied, not trusted.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ledger;
use crate::model::NormalizedTurn;

/// Bump when the on-disk shape changes; an older/mismatched file is discarded.
/// v2 added the ledger seal (`ledger` field).
pub const CHECKPOINT_VERSION: u32 = 2;

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
    /// a version we don't understand, or its integrity seal doesn't verify — all of
    /// which mean "start fresh".
    pub fn load(path: &Path) -> Option<Checkpoint> {
        let bytes = std::fs::read(path).ok()?;
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(%err, "corrupt checkpoint; starting fresh");
                return None;
            }
        };
        // The seal travels alongside the checkpoint fields (not inside `Checkpoint`
        // itself), so pull it out before deserializing the rest.
        let sealed: Option<ledger::LedgerHead> = value
            .get("ledger")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let cp: Checkpoint = match serde_json::from_value(value) {
            Ok(cp) => cp,
            Err(err) => {
                tracing::warn!(%err, "corrupt checkpoint; starting fresh");
                return None;
            }
        };
        if cp.version != CHECKPOINT_VERSION {
            tracing::warn!(
                found = cp.version,
                expected = CHECKPOINT_VERSION,
                "checkpoint version mismatch; starting fresh"
            );
            return None;
        }
        // Verify the seal: recompute the ledger head over the loaded turns and
        // require it to match what was stored. A mismatch (or a missing seal) means
        // the persisted capture was edited out-of-band — start fresh so doctored
        // turns are never resumed and counted toward an attempt.
        let recomputed = ledger::compute_head(&cp.session_id, cp.started_at_ms, &cp.turns);
        match sealed {
            Some(head) if head == recomputed => Some(cp),
            Some(_) => {
                tracing::warn!("checkpoint integrity seal mismatch; starting fresh");
                None
            }
            None => {
                tracing::warn!("checkpoint missing its integrity seal; starting fresh");
                None
            }
        }
    }

    /// Persist atomically with an integrity seal: stamp the file with the ledger
    /// head over the current turns, then write a temp file and rename over the
    /// target, so a crash mid-write never leaves a half-written checkpoint.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let head = ledger::compute_head(&self.session_id, self.started_at_ms, &self.turns);
        let mut value = serde_json::to_value(self).map_err(io::Error::other)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "ledger".to_string(),
                serde_json::to_value(&head).map_err(io::Error::other)?,
            );
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
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
    fn tampering_with_turns_on_disk_fails_the_seal() {
        use crate::model::{sample_raw, Source};
        use crate::normalize::normalize;

        let path = tmp_path("tamper");
        let mut cp = checkpoint();
        cp.turns = vec![normalize(&sample_raw(
            Source::Otel,
            Some("claude-opus-4-8"),
            100,
            50,
        ))];
        cp.save(&path).unwrap();
        // A clean load works: the seal matches the turns just written.
        assert!(Checkpoint::load(&path).is_some(), "freshly sealed -> loads");

        // Tamper: lower the captured output tokens on disk, leaving the seal intact.
        let raw = std::fs::read_to_string(&path).unwrap();
        let edited = raw.replace("\"tokens_output\": 50", "\"tokens_output\": 5");
        assert_ne!(edited, raw, "the edit applied");
        std::fs::write(&path, edited).unwrap();

        assert!(
            Checkpoint::load(&path).is_none(),
            "an out-of-band edit breaks the seal -> start fresh, doctored turns denied",
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
