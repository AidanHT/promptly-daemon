//! The start-time baseline integrity check and workspace reset (`18`).
//!
//! Prompt-efficiency scoring only means something if the player builds the
//! solution **from the genuine starter**, not by pasting in code produced
//! elsewhere. So `start` recomputes a content hash over the workspace's canonical
//! starter files and compares it to the manifest's pinned `baseline_hash`. A
//! match proves the workspace began from the real starter; a mismatch means it
//! was altered before the session (e.g. pre-loaded with a copied solution), and
//! the daemon backs the player's files up and resets the workspace to the
//! canonical starter before capturing anything.
//!
//! # Pinned hash spec (must match `lib/kits/baseline-hash.ts` bit-for-bit)
//! SHA-256, lowercase hex, over the concatenation — for each canonical starter
//! file, **sorted by normalized path** — of `path + "\0" + normalized_content`,
//! where content normalization strips a leading UTF-8 BOM and converts CRLF/CR to
//! LF. The canonical set is every workspace file **except** the generated
//! artifacts (`.promptly/**` and the top-level `README.md`). The daemon
//! additionally never walks `.git/**` or `.claude/**` — VCS metadata and the
//! harness config the bootstrap (`18`) writes are never kit content — but, unlike
//! the watcher, it deliberately does **not** honor `.promptlyignore`: the
//! integrity anchor is broader than the editable set precisely so foreign code
//! can't be hidden from it.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::manifest::PROMPTLY_DIR;

/// Backups live under `.promptly/backup/<timestamp>/` — inside the workspace but
/// excluded from the hash and the watcher, so a reset never destroys WIP and the
/// backup itself never perturbs the next baseline check.
pub const BACKUP_DIR: &str = "backup";

/// One file considered for the baseline hash. `content` is the raw bytes read
/// from disk; normalization happens inside [`compute_baseline_hash`].
#[derive(Debug, Clone)]
pub struct CanonicalFile {
    /// Path relative to the workspace root (any separator; normalized internally).
    pub path: String,
    pub content: Vec<u8>,
}

/// The outcome of comparing a workspace against a manifest's `baseline_hash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BaselineStatus {
    /// The workspace's canonical files reproduce the pinned hash.
    Match,
    /// They do not — the workspace was altered before the session began.
    Mismatch { computed: String },
}

impl BaselineStatus {
    pub fn is_match(&self) -> bool {
        matches!(self, BaselineStatus::Match)
    }
}

/// Forward-slash path with no leading `./` or `/` (mirrors `normalizePath`).
pub fn normalize_path(path: &str) -> String {
    let slashed = path.replace('\\', "/");
    let trimmed = slashed.trim_start_matches("./").trim_start_matches('/');
    trimmed.to_string()
}

/// Generated artifacts excluded from the hash: the manifest tree and the
/// generated top-level README (mirrors `isGeneratedArtifact`).
pub fn is_generated_artifact(path: &str) -> bool {
    let p = normalize_path(path);
    p == "README.md" || p == PROMPTLY_DIR || p.starts_with(&format!("{PROMPTLY_DIR}/"))
}

/// Strip a leading UTF-8 BOM and convert CRLF/lone-CR to LF, on raw bytes — the
/// byte-level equivalent of `normalizeContent`, so genuine (text) kits hash
/// identically regardless of the platform that checked them out.
pub fn normalize_content(raw: &[u8]) -> Vec<u8> {
    let body = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\r' {
            out.push(b'\n');
            if body.get(i + 1) == Some(&b'\n') {
                i += 1; // consume the LF of a CRLF pair
            }
        } else {
            out.push(body[i]);
        }
        i += 1;
    }
    out
}

/// Compute `baseline_hash` over a set of files per the pinned spec. Input order
/// is irrelevant — files are sorted by normalized path; generated artifacts are
/// dropped. Returns the full 64-char lowercase-hex digest.
pub fn compute_baseline_hash(files: &[CanonicalFile]) -> String {
    let mut canonical: Vec<(String, &[u8])> = files
        .iter()
        .map(|f| (normalize_path(&f.path), f.content.as_slice()))
        .filter(|(p, _)| !is_generated_artifact(p))
        .collect();
    canonical.sort_by(|a, b| a.0.cmp(&b.0));
    // The pinned TS spec (lib/kits/baseline-hash.ts) rejects duplicate normalized
    // paths. Every real caller — the workspace walk and the submission bundle —
    // passes a unique set, so a duplicate is a programmer error; assert it in
    // debug/test/CI builds (parity with the TS throw) rather than make this hot,
    // infallible path return a Result for input that can't occur in production.
    debug_assert!(
        canonical.windows(2).all(|w| w[0].0 != w[1].0),
        "duplicate normalized path in baseline hash input",
    );

    let mut hasher = Sha256::new();
    for (path, content) in &canonical {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(normalize_content(content));
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Recursively list the canonical starter files under `root` (relative,
/// forward-slash paths), pruning `.promptly/`, `.git/`, and the top-level
/// `README.md`. Entries are returned in sorted order for a deterministic walk.
fn walk_canonical(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    walk_into(root, root, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Directories never part of a kit: the daemon's own dir, VCS metadata, and the
/// harness config the bootstrap writes. Pruned at any depth.
fn is_pruned_dir(rel: &str) -> bool {
    for dir in [PROMPTLY_DIR, ".git", ".claude"] {
        if rel == dir || rel.ends_with(&format!("/{dir}")) {
            return true;
        }
    }
    false
}

fn walk_into(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let abs = entry.path();
        let Ok(rel_path) = abs.strip_prefix(root) else {
            continue;
        };
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        // Never descend into the daemon's own dir, VCS metadata, or the harness
        // config the bootstrap writes, and skip the generated README. The pruned
        // dirs are excluded at any depth.
        if is_pruned_dir(&rel) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            // A symlink's target isn't walked, so silently skipping it would let a
            // player hide pasted code behind a link without changing the baseline
            // hash — exactly what the anchor exists to prevent — and a link could
            // also point outside the workspace. Kits never contain symlinks, so
            // refuse one rather than ignore it.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink is not allowed in a workspace: {rel}"),
            ));
        }
        if file_type.is_dir() {
            walk_into(root, &abs, out)?;
        } else if file_type.is_file() && rel != "README.md" {
            out.push((rel, abs));
        }
    }
    Ok(())
}

/// Read the workspace's canonical starter files from disk.
pub fn collect_canonical_files(workspace: &Path) -> io::Result<Vec<CanonicalFile>> {
    let mut files = Vec::new();
    for (rel, abs) in walk_canonical(workspace)? {
        files.push(CanonicalFile {
            path: rel,
            content: std::fs::read(&abs)?,
        });
    }
    Ok(files)
}

/// Compute the workspace's current baseline hash.
pub fn hash_workspace(workspace: &Path) -> io::Result<String> {
    Ok(compute_baseline_hash(&collect_canonical_files(workspace)?))
}

/// Verify a workspace against an expected `baseline_hash`.
pub fn verify_workspace(workspace: &Path, expected: &str) -> io::Result<BaselineStatus> {
    let computed = hash_workspace(workspace)?;
    Ok(if computed == expected.trim() {
        BaselineStatus::Match
    } else {
        BaselineStatus::Mismatch { computed }
    })
}

/// Why a reset could not restore the canonical starter.
#[derive(Debug, Error)]
pub enum ResetError {
    #[error(
        "no cached canonical starter to reset from — run `promptly init <level>` or connect to fetch it"
    )]
    NoCanonicalSource,
    #[error("reset I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// What a reset did, for the CLI to report and the attempt to record.
#[derive(Debug, Clone, Serialize)]
pub struct ResetReport {
    /// Where the player's pre-reset files were backed up.
    pub backup_dir: PathBuf,
    /// The workspace's hash after restoring the canonical starter.
    pub restored_hash: String,
}

/// A source of the canonical starter tree for a reset. The cloud re-fetch is
/// `20`; the local implementation restores from a cached pristine copy that
/// `promptly init` (or a prior verified start) populated.
pub trait CanonicalStarter {
    /// Whether a reset can be performed offline right now.
    fn is_available(&self) -> bool;
    /// Overwrite the workspace's starter files with the canonical tree, so the
    /// post-restore workspace reproduces `baseline_hash`.
    fn restore_into(&self, workspace: &Path) -> Result<(), ResetError>;
}

/// Restores from a pristine copy cached on disk (under the daemon's data dir).
#[derive(Debug, Clone)]
pub struct CachedStarter {
    cache_dir: PathBuf,
}

impl CachedStarter {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }
}

impl CanonicalStarter for CachedStarter {
    fn is_available(&self) -> bool {
        walk_canonical(&self.cache_dir)
            .map(|files| !files.is_empty())
            .unwrap_or(false)
    }

    fn restore_into(&self, workspace: &Path) -> Result<(), ResetError> {
        if !self.is_available() {
            return Err(ResetError::NoCanonicalSource);
        }
        copy_tree(&self.cache_dir, workspace)?;
        Ok(())
    }
}

/// Cache the workspace's current canonical starter into `cache_dir` (called on a
/// verified-clean start) so a future tampered start can be reset offline.
pub fn cache_canonical(workspace: &Path, cache_dir: &Path) -> io::Result<()> {
    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir)?;
    }
    copy_tree(workspace, cache_dir)
}

/// Copy every canonical starter file from `src` into `dest`, preserving relative
/// paths and creating parent directories as needed.
fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    for (rel, abs) in walk_canonical(src)? {
        let target = dest.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&abs, &target)?;
    }
    Ok(())
}

/// Back the player's current canonical files up under `.promptly/backup/<now_ms>/`
/// so an honest work-in-progress is never destroyed by a reset.
pub fn backup_workspace(workspace: &Path, now_ms: i64) -> io::Result<PathBuf> {
    let backup_dir = workspace
        .join(PROMPTLY_DIR)
        .join(BACKUP_DIR)
        .join(now_ms.to_string());
    copy_tree(workspace, &backup_dir)?;
    Ok(backup_dir)
}

/// Reset the workspace's starter files to the canonical starter: back the current
/// files up, remove them, restore the canonical tree, and report the result. The
/// `.promptly/` tree (manifest, backups) and `.git/` are never touched.
pub fn reset_workspace(
    workspace: &Path,
    starter: &dyn CanonicalStarter,
    now_ms: i64,
) -> Result<ResetReport, ResetError> {
    if !starter.is_available() {
        return Err(ResetError::NoCanonicalSource);
    }
    let backup_dir = backup_workspace(workspace, now_ms)?;
    for (_, abs) in walk_canonical(workspace)? {
        std::fs::remove_file(&abs)?;
    }
    starter.restore_into(workspace)?;
    Ok(ResetReport {
        backup_dir,
        restored_hash: hash_workspace(workspace)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &[u8]) -> CanonicalFile {
        CanonicalFile {
            path: path.to_string(),
            content: content.to_vec(),
        }
    }

    #[test]
    fn walk_refuses_a_symlink_so_foreign_content_cannot_hide() {
        let ws =
            std::env::temp_dir().join(format!("promptlyd-baseline-symlink-{}", std::process::id()));
        std::fs::remove_dir_all(&ws).ok();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("main.go"), b"package main\n").unwrap();
        // A symlink the player might use to smuggle in (or point out at) content
        // the canonical walk would otherwise never see.
        let link = ws.join("sneaky.go");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink("main.go", &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file("main.go", &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let made = false;
        if !made {
            // Creating a symlink needs privilege on Windows; skip if unavailable
            // (the contract is still exercised on the Linux CI lane).
            std::fs::remove_dir_all(&ws).ok();
            return;
        }
        let err = hash_workspace(&ws).expect_err("a workspace with a symlink must not hash");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(&ws).ok();
    }

    // Ground-truth vectors generated by the canonical TS algorithm
    // (lib/kits/baseline-hash.ts) — these pin bit-for-bit parity.
    #[test]
    fn hash_matches_the_canonical_typescript_vector() {
        // Two starter files plus the two generated artifacts that must be dropped.
        let files = vec![
            file("main.go", b"package main\n"),
            file("tests/public/cases.json", b"{}\n"),
            file("README.md", b"ignored\n"),
            file(".promptly/manifest.json", b"{}\n"),
        ];
        assert_eq!(
            compute_baseline_hash(&files),
            "027c1ca4a32c2591eb3cb144331898b727403f0ed425dec18878a46614d18271",
        );
    }

    #[test]
    fn hash_normalizes_bom_crlf_backslashes_and_sorts() {
        let mut a = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        a.extend_from_slice(b"hello\n");
        let files = vec![
            file("b\\two.txt", b"x\r\ny\r"), // CRLF + lone CR, backslash path
            file("a.txt", &a),
        ];
        assert_eq!(
            compute_baseline_hash(&files),
            "d58a4772860eaeb16b2516a0873837d5d6019830c30febd83687e20cd629c310",
        );
    }

    #[test]
    fn normalize_content_handles_all_eol_forms() {
        assert_eq!(normalize_content(b"a\r\nb\rc\nd"), b"a\nb\nc\nd");
        assert_eq!(normalize_content(&[0xEF, 0xBB, 0xBF, b'x']), b"x");
    }

    fn temp_ws(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptlyd-baseline-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(ws: &Path, rel: &str, content: &str) {
        let p = ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn pristine(ws: &Path) {
        write(ws, "main.go", "package main\n");
        write(ws, "tests/public/cases.json", "{}\n");
        write(ws, "README.md", "# generated\n");
        write(ws, ".promptly/manifest.json", "{\"x\":1}\n");
    }

    #[test]
    fn workspace_hash_excludes_generated_and_vcs_and_matches_files() {
        let ws = temp_ws("collect");
        pristine(&ws);
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git/HEAD"), "ref: x").unwrap();
        // Harness config the bootstrap writes is never kit content either.
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::fs::write(ws.join(".claude/settings.json"), "{\"env\":{}}").unwrap();

        let by_files = compute_baseline_hash(&[
            file("main.go", b"package main\n"),
            file("tests/public/cases.json", b"{}\n"),
        ]);
        // The on-disk walk drops README.md, .promptly/**, and .git/**, so it
        // reproduces the same hash as the explicit canonical file set.
        assert_eq!(hash_workspace(&ws).unwrap(), by_files);
        assert!(verify_workspace(&ws, &by_files).unwrap().is_match());

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_foreign_file_is_detected_even_outside_the_editable_set() {
        let ws = temp_ws("foreign");
        pristine(&ws);
        let clean = hash_workspace(&ws).unwrap();
        // A file the allowlist would never include still moves the hash.
        write(&ws, "stolen/solution.py", "print('answer')\n");
        match verify_workspace(&ws, &clean).unwrap() {
            BaselineStatus::Mismatch { computed } => assert_ne!(computed, clean),
            BaselineStatus::Match => panic!("foreign code must be detected"),
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn reset_backs_up_then_restores_the_canonical_starter() {
        let ws = temp_ws("reset");
        pristine(&ws);
        let baseline = hash_workspace(&ws).unwrap();

        // Seed the canonical cache from the pristine workspace (what `init` or a
        // prior clean start would do), then tamper the workspace.
        let cache = temp_ws("reset-cache");
        cache_canonical(&ws, &cache).unwrap();
        write(&ws, "main.go", "package main // SOLVED ELSEWHERE\n");
        write(&ws, "stolen.py", "print('x')\n");
        assert!(!verify_workspace(&ws, &baseline).unwrap().is_match());

        let starter = CachedStarter::new(cache.clone());
        let report = reset_workspace(&ws, &starter, 1_700_000_000_123).unwrap();

        // Post-reset the workspace reproduces the genuine baseline...
        assert_eq!(report.restored_hash, baseline);
        assert!(verify_workspace(&ws, &baseline).unwrap().is_match());
        // ...the foreign file is gone...
        assert!(!ws.join("stolen.py").exists());
        // ...and the player's tampered files are preserved in the backup.
        assert!(report
            .backup_dir
            .starts_with(ws.join(".promptly").join("backup")));
        let backed_up = std::fs::read_to_string(report.backup_dir.join("main.go")).unwrap();
        assert!(backed_up.contains("SOLVED ELSEWHERE"));

        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn reset_without_a_cached_starter_is_refused() {
        let ws = temp_ws("nocache");
        pristine(&ws);
        let starter = CachedStarter::new(temp_ws("empty-cache"));
        assert!(matches!(
            reset_workspace(&ws, &starter, 0).unwrap_err(),
            ResetError::NoCanonicalSource
        ));
        std::fs::remove_dir_all(&ws).ok();
    }
}
