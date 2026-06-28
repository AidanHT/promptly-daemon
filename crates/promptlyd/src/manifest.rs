//! The `.promptly/manifest.json` contract — the file that binds a workspace to a
//! level (`07`).
//!
//! `promptly start` reads it to learn which level the session is bound to, which
//! files the player may edit (the watcher scope), and the `baseline_hash` the
//! start-time integrity check verifies against. It is the single source of truth
//! for the binding; the daemon never guesses a level from the directory.
//!
//! This mirrors the shape authored in `lib/kits/types.ts` (the TypeScript
//! `Manifest`). Parsing is lenient where the daemon doesn't need a field (unknown
//! keys are ignored, optional fields default) and strict where it does — a
//! missing file, malformed JSON, an unsupported `schema_version`, or an empty
//! `level_id`/`baseline_hash` is a hard error so capture never begins against a
//! workspace it can't trust.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Highest `schema_version` this daemon understands. Bump only alongside a reader
/// change; mirrors `MANIFEST_SCHEMA_VERSION` in `lib/kits/types.ts`.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Directory (under the workspace root) holding the manifest and the daemon's
/// per-workspace state. Excluded from the baseline hash and the file watcher.
pub const PROMPTLY_DIR: &str = ".promptly";
/// The manifest file name within [`PROMPTLY_DIR`].
pub const MANIFEST_FILE: &str = "manifest.json";

/// Why a manifest could not be loaded into a trustworthy binding.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("no manifest at {0} — is this a Promptly workspace? run `promptly init <level>`")]
    Missing(PathBuf),
    #[error("manifest {path} is unreadable: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("manifest {path} is not valid JSON: {source}")]
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "manifest schema_version {found} is newer than this daemon supports ({SUPPORTED_SCHEMA_VERSION}) — update promptly"
    )]
    UnsupportedSchema { found: u32 },
    #[error("manifest is missing a required field: {0}")]
    MissingField(&'static str),
}

/// The fields of `.promptly/manifest.json` the daemon binds a session to. Extra
/// keys in the file are ignored so the manifest can carry data the daemon doesn't
/// consume (entry points, scoring overrides) without a reader change.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    /// Mirrors `levels.content_version`; the attempt records which kit it started
    /// from so the server validates against that version's canonical copy (`07`).
    #[serde(default)]
    pub kit_version: u32,
    /// Deterministic UUIDv5 of the slug — equals the seeded `levels.id` (`07`), so
    /// the binding needs no DB round-trip.
    pub level_id: String,
    /// The human-authored key binding row ↔ content ↔ kit.
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub stage_num: u32,
    #[serde(default)]
    pub level_num: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub language: String,
    /// Judge0 runtime pin, e.g. `go1.22`.
    #[serde(default)]
    pub runtime_version: String,
    #[serde(default)]
    pub execution_harness: String,
    /// Globs the player may submit; scopes the file watcher and edit-provenance.
    #[serde(default)]
    pub file_allowlist: Vec<String>,
    /// File(s)/symbol(s) the grader invokes; `submit` checks file entry points
    /// exist before packaging (`10`/`19`).
    #[serde(default)]
    pub entry_points: Vec<String>,
    /// Challenge type (`debugging`/`implementation`/`generation`) — selects the
    /// default token weights for local scoring parity (`13`/`19`).
    #[serde(default)]
    pub challenge_type: String,
    /// Per-level token-weight overrides (`token_weight_overrides`); `null`/absent
    /// means the challenge-type defaults. Keys are `W_in`/`W_out`/`W_think` (`13a`).
    #[serde(default)]
    pub token_weight_overrides: Option<HashMap<String, f64>>,
    /// SHA-256 (lowercase hex) over the canonical starter files — the integrity
    /// anchor the start-time check verifies against (`baseline.rs`).
    pub baseline_hash: String,
}

impl Manifest {
    /// The manifest path for a workspace root: `<workspace>/.promptly/manifest.json`.
    pub fn path_in(workspace: &Path) -> PathBuf {
        workspace.join(PROMPTLY_DIR).join(MANIFEST_FILE)
    }

    /// Load and validate the manifest for `workspace`. Refuses to produce a
    /// binding for a workspace that is missing the manifest, has malformed JSON,
    /// declares a schema this daemon can't read, or omits the fields the binding
    /// depends on.
    pub fn load(workspace: &Path) -> Result<Self, ManifestError> {
        let path = Self::path_in(workspace);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ManifestError::Missing(path));
            }
            Err(source) => return Err(ManifestError::Unreadable { path, source }),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|source| ManifestError::Malformed { path, source })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Check the invariants the daemon relies on once the JSON has parsed.
    fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version > SUPPORTED_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.level_id.trim().is_empty() {
            return Err(ManifestError::MissingField("level_id"));
        }
        if self.baseline_hash.trim().is_empty() {
            return Err(ManifestError::MissingField("baseline_hash"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptlyd-manifest-{}-{label}", std::process::id()));
        std::fs::create_dir_all(dir.join(PROMPTLY_DIR)).unwrap();
        dir
    }

    fn write_manifest(ws: &Path, body: &str) {
        std::fs::write(Manifest::path_in(ws), body).unwrap();
    }

    const VALID: &str = r#"{
        "schema_version": 1,
        "kit_version": 3,
        "level_id": "11111111-2222-5333-8444-555555555555",
        "slug": "stage-1-01-lru-eviction-debug",
        "stage_num": 1,
        "level_num": 1,
        "title": "LRU Eviction",
        "language": "Go",
        "runtime_version": "go1.22",
        "challenge_type": "debugging",
        "execution_harness": "stdin_stdout",
        "file_allowlist": ["lru.go"],
        "entry_points": ["main.go"],
        "baseline_hash": "bd2afddbffd4cfdaab55025226857a2ec307da310eb0b715ba6b98babcc57c57"
    }"#;

    #[test]
    fn loads_a_valid_manifest_and_keeps_the_binding_fields() {
        let ws = workspace("valid");
        write_manifest(&ws, VALID);
        let m = Manifest::load(&ws).expect("valid manifest loads");
        assert_eq!(m.level_id, "11111111-2222-5333-8444-555555555555");
        assert_eq!(m.slug, "stage-1-01-lru-eviction-debug");
        assert_eq!(m.kit_version, 3);
        assert_eq!(m.file_allowlist, vec!["lru.go".to_string()]);
        assert_eq!(m.runtime_version, "go1.22");
        // Scoring-parity fields (`19`): present in the contract, defaulted when absent.
        assert_eq!(m.challenge_type, "debugging");
        assert!(m.token_weight_overrides.is_none());
        assert_eq!(m.entry_points, vec!["main.go".to_string()]);
        assert!(m.baseline_hash.starts_with("bd2afdd"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn missing_manifest_is_a_clear_error() {
        let ws = workspace("missing");
        let err = Manifest::load(&ws).unwrap_err();
        assert!(matches!(err, ManifestError::Missing(_)));
        assert!(err.to_string().contains("promptly init"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn malformed_json_is_rejected() {
        let ws = workspace("malformed");
        write_manifest(&ws, "{ not json");
        assert!(matches!(
            Manifest::load(&ws).unwrap_err(),
            ManifestError::Malformed { .. }
        ));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let ws = workspace("schema");
        write_manifest(
            &ws,
            r#"{"schema_version": 999, "level_id": "x", "baseline_hash": "y"}"#,
        );
        assert!(matches!(
            Manifest::load(&ws).unwrap_err(),
            ManifestError::UnsupportedSchema { found: 999 }
        ));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn empty_level_id_or_baseline_hash_is_rejected() {
        let ws = workspace("fields");
        write_manifest(
            &ws,
            r#"{"schema_version": 1, "level_id": "  ", "baseline_hash": "y"}"#,
        );
        assert!(matches!(
            Manifest::load(&ws).unwrap_err(),
            ManifestError::MissingField("level_id")
        ));
        write_manifest(
            &ws,
            r#"{"schema_version": 1, "level_id": "x", "baseline_hash": ""}"#,
        );
        assert!(matches!(
            Manifest::load(&ws).unwrap_err(),
            ManifestError::MissingField("baseline_hash")
        ));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_missing_required_field_fails_to_parse() {
        let ws = workspace("required");
        // No `level_id` key at all -> serde rejects (the field is non-optional).
        write_manifest(&ws, r#"{"schema_version": 1, "baseline_hash": "y"}"#);
        assert!(matches!(
            Manifest::load(&ws).unwrap_err(),
            ManifestError::Malformed { .. }
        ));
        std::fs::remove_dir_all(&ws).ok();
    }
}
