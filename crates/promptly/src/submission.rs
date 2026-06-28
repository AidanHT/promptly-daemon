//! Local submission packaging (`19`), mirroring the server's intake validation
//! (`10`): gather the workspace files the manifest's `file_allowlist` permits,
//! confirm the file entry points exist, and enforce the payload cap — so a player
//! can validate a submission locally before the ranked upload (the cloud path is
//! `20`). The integrity/AST checks the server runs (`25`) stay server-side.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use thiserror::Error;

use promptlyd::manifest::Manifest;

/// Max submission payload, mirroring `MAX_PAYLOAD_BYTES` in `lib/judge0` (8 MiB).
pub const MAX_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;

/// Directories/files never part of a submission (generated artifacts + VCS +
/// harness config), pruned before allowlist matching.
const EXCLUDED_DIRS: [&str; 3] = [".promptly", ".git", ".claude"];

/// One file selected for submission.
#[derive(Debug, Clone)]
pub struct SubmissionFile {
    /// Forward-slash path relative to the workspace root.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// The validated set of files a submission would upload.
#[derive(Debug, Clone)]
pub struct SubmissionBundle {
    pub files: Vec<SubmissionFile>,
    pub total_bytes: u64,
}

/// Why a submission couldn't be packaged.
#[derive(Debug, Error)]
pub enum SubmitError {
    #[error("no files matched the allowlist {0:?} — did you edit the right files?")]
    NoFiles(Vec<String>),
    #[error("an allowlist glob is invalid ({0})")]
    BadGlob(String),
    #[error("missing entry point: {0} is not in the workspace")]
    MissingEntryPoint(String),
    #[error("submission is {0} bytes, over the {1}-byte limit")]
    TooLarge(u64, u64),
    #[error("reading the workspace failed: {0}")]
    Io(String),
}

/// Gather and validate the submittable files for `workspace` against `manifest`.
pub fn gather_submission(
    workspace: &Path,
    manifest: &Manifest,
) -> Result<SubmissionBundle, SubmitError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &manifest.file_allowlist {
        builder.add(Glob::new(pattern).map_err(|e| SubmitError::BadGlob(e.to_string()))?);
    }
    let allowlist = builder
        .build()
        .map_err(|e| SubmitError::BadGlob(e.to_string()))?;

    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    for (rel, abs) in walk(workspace).map_err(|e| SubmitError::Io(e.to_string()))? {
        if !allowlist.is_match(&rel) {
            continue;
        }
        let bytes = std::fs::read(&abs).map_err(|e| SubmitError::Io(e.to_string()))?;
        total_bytes += bytes.len() as u64;
        files.push(SubmissionFile { path: rel, bytes });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    if files.is_empty() {
        return Err(SubmitError::NoFiles(manifest.file_allowlist.clone()));
    }
    if total_bytes > MAX_PAYLOAD_BYTES {
        return Err(SubmitError::TooLarge(total_bytes, MAX_PAYLOAD_BYTES));
    }
    // File entry points must be present (symbol entry points like `Service.Start`
    // are skipped, mirroring `entryPointIsFile` in `lib/packaging`).
    for entry in &manifest.entry_points {
        if entry_point_is_file(entry) && !workspace.join(entry).is_file() {
            return Err(SubmitError::MissingEntryPoint(entry.clone()));
        }
    }
    Ok(SubmissionBundle { files, total_bytes })
}

/// Whether an entry point names a source file (vs a symbol), by extension —
/// mirrors the regex in `lib/packaging/validate.ts`.
fn entry_point_is_file(entry: &str) -> bool {
    const SOURCE_EXTS: [&str; 17] = [
        "go", "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "c", "h", "cc", "cpp", "cxx",
        "hpp", "hh", "hxx",
    ];
    entry
        .rsplit_once('.')
        .map(|(_, ext)| SOURCE_EXTS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Recursively list workspace files as (relative forward-slash path, absolute
/// path), pruning generated/VCS/harness dirs and the generated top-level README.
fn walk(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    walk_into(root, root, &mut out)?;
    Ok(out)
}

fn walk_into(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let abs = entry.path();
        let Ok(rel_path) = abs.strip_prefix(root) else {
            continue;
        };
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        if EXCLUDED_DIRS
            .iter()
            .any(|d| rel == *d || rel.starts_with(&format!("{d}/")))
        {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_into(root, &abs, out)?;
        } else if file_type.is_file() && rel != "README.md" {
            out.push((rel, abs));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptly-submit-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(ws: &Path, rel: &str, content: &str) {
        let p = ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn manifest(allowlist: &[&str], entry_points: &[&str]) -> Manifest {
        // Build via JSON so we exercise the real (defaulted) deserialization.
        let json = format!(
            r#"{{"schema_version":1,"level_id":"x","baseline_hash":"y",
                "file_allowlist":{},"entry_points":{}}}"#,
            serde_json::to_string(allowlist).unwrap(),
            serde_json::to_string(entry_points).unwrap(),
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn gathers_only_allowlisted_files_excluding_generated() {
        let ws = workspace("allowlist");
        write(&ws, "lru.go", "package main\n");
        write(&ws, "main.go", "package main\n");
        write(&ws, "README.md", "# generated\n");
        write(&ws, ".promptly/manifest.json", "{}");
        write(&ws, "notes.txt", "scratch\n");

        // Only `lru.go` is submittable.
        let bundle = gather_submission(&ws, &manifest(&["lru.go"], &["main.go"])).unwrap();
        let paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["lru.go"]);
        assert!(bundle.total_bytes > 0);

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn glob_allowlists_match_multiple_files() {
        let ws = workspace("glob");
        write(&ws, "a.go", "package main\n");
        write(&ws, "b.go", "package main\n");
        write(&ws, "c.txt", "no\n");
        let bundle = gather_submission(&ws, &manifest(&["*.go"], &[])).unwrap();
        assert_eq!(bundle.files.len(), 2, "both .go files, not the .txt");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn empty_match_is_an_error() {
        let ws = workspace("empty");
        write(&ws, "other.txt", "x\n");
        assert!(matches!(
            gather_submission(&ws, &manifest(&["*.go"], &[])),
            Err(SubmitError::NoFiles(_))
        ));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_missing_file_entry_point_is_rejected_but_a_symbol_is_not() {
        let ws = workspace("entry");
        write(&ws, "lru.go", "package main\n");
        // `main.go` entry point is absent → error.
        assert!(matches!(
            gather_submission(&ws, &manifest(&["lru.go"], &["main.go"])),
            Err(SubmitError::MissingEntryPoint(_))
        ));
        // A symbol entry point (no extension) is not existence-checked.
        assert!(gather_submission(&ws, &manifest(&["lru.go"], &["Service.Start"])).is_ok());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn entry_point_file_detection_matches_the_packaging_regex() {
        assert!(entry_point_is_file("main.go"));
        assert!(entry_point_is_file("app.tsx"));
        assert!(!entry_point_is_file("Service.Start"));
        assert!(!entry_point_is_file("NewBoundedQueue"));
    }
}
