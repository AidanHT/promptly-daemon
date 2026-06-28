//! Edit-provenance tracking (`18`).
//!
//! While a session is active, the scoped file watcher (`17`) reports how the
//! allowlisted files evolve. This module turns that stream into *evidence that
//! the solution grew through editing inside the session* — and flags the classic
//! "paste the whole answer" move: a single change that drops a large blob into a
//! file that hadn't been built up incrementally. The signal is recorded and
//! carried to the server's anti-cheat checks (`25`); it never blocks locally
//! (legitimate large refactors happen), so the heuristic is deliberately
//! conservative and the size deltas — not file contents — are all it inspects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;

use crate::watcher::{ChangeKind, FileChange};

/// A change must add at least this many bytes in one event — and leave the file
/// at least this large — to count as a bulk paste rather than a normal edit.
const BULK_MIN_BYTES: u64 = 2_000;
/// A file edited at least this many times in-session before a big jump is treated
/// as legitimately grown, not pasted, so a late large refactor isn't flagged.
const BULK_MAX_PRIOR_EDITS: u32 = 1;

/// The running provenance of one allowlisted file across the session.
#[derive(Debug, Clone, Serialize)]
pub struct FileProvenance {
    pub path: String,
    /// In-session change events observed for this file.
    pub edits: u32,
    pub last_size: u64,
    pub max_size: u64,
    /// Whether a bulk-replacement signal has already fired for this file.
    pub flagged: bool,
}

/// Why a change looked like foreign bulk content rather than in-session editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceSignalKind {
    /// A large blob appeared in one event in a file not built up incrementally.
    BulkReplace,
}

/// A provenance signal surfaced for the server's integrity checks (`25`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvenanceSignal {
    pub kind: ProvenanceSignalKind,
    pub path: String,
    pub size: u64,
    pub previous_size: u64,
    /// In-session edits to this file before the flagged event.
    pub prior_edits: u32,
    pub timestamp_ms: i64,
}

/// Tracks the in-session evolution of the allowlisted files and emits provenance
/// signals. Pure and deterministic: it inspects only the change metadata the
/// watcher already collected (path, kind, size, time), never file contents.
pub struct ProvenanceTracker {
    /// Forms of the workspace root a change path might be expressed against.
    roots: Vec<PathBuf>,
    allowlist: GlobSet,
    files: HashMap<String, FileProvenance>,
}

impl ProvenanceTracker {
    /// Build a tracker scoped to `workspace_root` and the manifest `file_allowlist`
    /// globs. An empty or unparseable allowlist tracks nothing (no false signals).
    pub fn new(workspace_root: &Path, allowlist: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        for entry in allowlist {
            let normalized = entry.replace('\\', "/");
            if let Ok(glob) = Glob::new(&normalized) {
                builder.add(glob);
            }
        }
        let mut roots = vec![workspace_root.to_path_buf()];
        if let Ok(canonical) = std::fs::canonicalize(workspace_root) {
            let stripped = crate::paths::strip_extended_prefix(canonical);
            if !roots.contains(&stripped) {
                roots.push(stripped);
            }
        }
        Self {
            roots,
            allowlist: builder.build().unwrap_or_else(|_| GlobSet::empty()),
            files: HashMap::new(),
        }
    }

    /// The per-file provenance gathered so far (evidence the solution was edited
    /// in-session), sorted by path for stable output.
    pub fn summary(&self) -> Vec<FileProvenance> {
        let mut out: Vec<FileProvenance> = self.files.values().cloned().collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Record one workspace change, returning a signal if it looks like a foreign
    /// bulk paste. Changes outside the allowlist are ignored.
    pub fn observe(&mut self, change: &FileChange) -> Option<ProvenanceSignal> {
        let rel = self.relative(&change.path)?;
        if !self.allowlist.is_match(&rel) {
            return None;
        }

        let existing = self.files.get(&rel);
        let prior_edits = existing.map(|f| f.edits).unwrap_or(0);
        let previous_size = existing.map(|f| f.last_size).unwrap_or(0);
        let already_flagged = existing.map(|f| f.flagged).unwrap_or(false);

        let bulk = !already_flagged
            && change.kind != ChangeKind::Removed
            && change.size >= BULK_MIN_BYTES
            && change.size.saturating_sub(previous_size) >= BULK_MIN_BYTES
            && prior_edits <= BULK_MAX_PRIOR_EDITS;

        let entry = self
            .files
            .entry(rel.clone())
            .or_insert_with(|| FileProvenance {
                path: rel.clone(),
                edits: 0,
                last_size: 0,
                max_size: 0,
                flagged: false,
            });
        entry.edits += 1;
        entry.last_size = change.size;
        entry.max_size = entry.max_size.max(change.size);

        if bulk {
            entry.flagged = true;
            Some(ProvenanceSignal {
                kind: ProvenanceSignalKind::BulkReplace,
                path: rel,
                size: change.size,
                previous_size,
                prior_edits,
                timestamp_ms: change.timestamp_ms,
            })
        } else {
            None
        }
    }

    /// The change path relative to the workspace root, forward-slashed.
    fn relative(&self, path: &Path) -> Option<String> {
        for root in &self.roots {
            if let Ok(rel) = path.strip_prefix(root) {
                return Some(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\ws" } else { "/ws" })
    }

    fn change(rel: &str, kind: ChangeKind, size: u64, ts: i64) -> FileChange {
        FileChange {
            path: root().join(rel),
            kind,
            size,
            timestamp_ms: ts,
        }
    }

    #[test]
    fn incremental_editing_never_signals() {
        let mut t = ProvenanceTracker::new(&root(), &["lru.go".to_string()]);
        // The solution grows through many small saves.
        let mut size = 200;
        for i in 0..20 {
            size += 120;
            assert!(t
                .observe(&change("lru.go", ChangeKind::Modified, size, 1_000 + i))
                .is_none());
        }
        let summary = t.summary();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].edits, 20);
        assert_eq!(summary[0].last_size, size);
    }

    #[test]
    fn a_sudden_bulk_paste_is_flagged_once() {
        let mut t = ProvenanceTracker::new(&root(), &["*.go".to_string()]);
        // Baseline-sized starter, lightly touched, then a whole solution pasted in.
        assert!(t
            .observe(&change("lru.go", ChangeKind::Modified, 300, 1))
            .is_none());
        let signal = t
            .observe(&change("lru.go", ChangeKind::Modified, 9_000, 2))
            .expect("bulk paste is a provenance signal");
        assert_eq!(signal.kind, ProvenanceSignalKind::BulkReplace);
        assert_eq!(signal.path, "lru.go");
        assert_eq!(signal.size, 9_000);
        assert_eq!(signal.previous_size, 300);

        // It fires at most once per file, even on further large writes.
        assert!(t
            .observe(&change("lru.go", ChangeKind::Modified, 12_000, 3))
            .is_none());
        assert!(t.summary()[0].flagged);
    }

    #[test]
    fn changes_outside_the_allowlist_are_ignored() {
        let mut t = ProvenanceTracker::new(&root(), &["lru.go".to_string()]);
        // A huge write to a non-editable file is not in scope here (the baseline
        // check owns foreign files); provenance only tracks the editable set.
        assert!(t
            .observe(&change("main.go", ChangeKind::Modified, 50_000, 1))
            .is_none());
        assert!(t.summary().is_empty());
    }

    #[test]
    fn a_large_refactor_of_an_already_large_file_is_not_flagged() {
        let mut t = ProvenanceTracker::new(&root(), &["lru.go".to_string()]);
        // Build the file up incrementally past the prior-edits threshold...
        for (i, size) in [4_000u64, 4_500, 5_000].into_iter().enumerate() {
            t.observe(&change("lru.go", ChangeKind::Modified, size, i as i64));
        }
        // ...then a big rewrite: already grown in-session, so it's legitimate.
        assert!(t
            .observe(&change("lru.go", ChangeKind::Modified, 11_000, 9))
            .is_none());
    }

    #[test]
    fn a_removal_never_signals() {
        let mut t = ProvenanceTracker::new(&root(), &["lru.go".to_string()]);
        assert!(t
            .observe(&change("lru.go", ChangeKind::Removed, 0, 1))
            .is_none());
    }
}
