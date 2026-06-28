//! The scoped workspace file watcher.
//!
//! It records how the workspace's files evolve during a session — evidence for
//! `18`'s edit-provenance and `25`'s integrity checks — but only ever within the
//! active workspace. Two rules enforce the boundary:
//!
//! 1. **Canonicalize before scope-checking.** A path is resolved (symlinks and
//!    `..` collapsed) *before* the prefix check, so a symlink inside the
//!    workspace that points outside it can't smuggle the watcher out.
//! 2. **Honor `.promptlyignore`** (plus the daemon's own `.git`/`.promptly`),
//!    so secrets and noise are never read.
//!
//! [`Scope`] is the pure gate (unit-tested); [`watch`] is the thin `notify` loop
//! that consults it before touching any path.

use std::io;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{RecursiveMode, Watcher};
use serde::Serialize;

use crate::clock::now_ms;
use crate::sources::{wait_for_shutdown, Shutdown};

/// The kind of change observed for a watched file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Created,
    Modified,
    Removed,
}

/// One recorded, in-scope workspace change. Metadata only (path, size, time) —
/// never file contents, so nothing sensitive is read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub size: u64,
    pub timestamp_ms: i64,
}

/// The active-workspace boundary: a canonical root, an ignore matcher, and the
/// rules that decide whether a path may be watched.
#[derive(Debug, Clone)]
pub struct Scope {
    root: PathBuf,
    ignore: GlobSet,
}

impl Scope {
    /// Build a scope rooted at `workspace_root` (which must exist — it is
    /// canonicalized). Loads `.promptlyignore` from the root if present.
    pub fn new(workspace_root: &Path) -> io::Result<Self> {
        let root = std::fs::canonicalize(workspace_root)?;
        let mut builder = GlobSetBuilder::new();
        // The daemon's own dirs are never workspace edits.
        for builtin in [".git", ".promptly"] {
            add_pattern(&mut builder, builtin);
        }
        if let Ok(text) = std::fs::read_to_string(root.join(".promptlyignore")) {
            for line in text.lines() {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with('#') {
                    add_pattern(&mut builder, line);
                }
            }
        }
        let ignore = builder
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Self { root, ignore })
    }

    /// The canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Is `path` inside the workspace once symlinks/`..` are resolved?
    pub fn contains(&self, path: &Path) -> bool {
        self.resolve(path).starts_with(&self.root)
    }

    /// Does an ignore rule (`.promptlyignore` or a builtin) cover `path`?
    pub fn is_ignored(&self, path: &Path) -> bool {
        let resolved = self.resolve(path);
        let Ok(rel) = resolved.strip_prefix(&self.root) else {
            return false;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        !rel.is_empty() && self.ignore.is_match(rel.as_str())
    }

    /// The single gate the watcher consults before reading anything: in-scope and
    /// not ignored.
    pub fn should_watch(&self, path: &Path) -> bool {
        self.contains(path) && !self.is_ignored(path)
    }

    /// Resolve symlinks/`..` where possible; for paths that no longer exist (e.g.
    /// a delete event) fall back to a lexical normalization under the root —
    /// there is nothing to read, so symlink resolution is moot.
    fn resolve(&self, path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| lexical_join(&self.root, path))
    }
}

/// Expand one ignore pattern into the forms that make it match at the root and at
/// any depth, for both files and directory subtrees.
fn add_pattern(builder: &mut GlobSetBuilder, raw: &str) {
    let p = raw.trim().trim_start_matches("./").trim_end_matches('/');
    if p.is_empty() {
        return;
    }
    for form in [
        p.to_string(),
        format!("**/{p}"),
        format!("{p}/**"),
        format!("**/{p}/**"),
    ] {
        if let Ok(glob) = Glob::new(&form) {
            builder.add(glob);
        }
    }
}

/// Lexically resolve `path` against `root` without touching the filesystem,
/// collapsing `.` and `..`. Used only for non-existent paths.
fn lexical_join(root: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Probe metadata for an in-scope path, producing a [`FileChange`]. A missing
/// file is reported as `Removed`.
async fn probe(path: &Path, kind: ChangeKind) -> FileChange {
    let size = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    FileChange {
        path: path.to_path_buf(),
        kind,
        size,
        timestamp_ms: now_ms(),
    }
}

fn classify(kind: &notify::EventKind) -> ChangeKind {
    use notify::EventKind;
    match kind {
        EventKind::Create(_) => ChangeKind::Created,
        EventKind::Remove(_) => ChangeKind::Removed,
        _ => ChangeKind::Modified,
    }
}

/// Watch the workspace recursively until `shutdown`, forwarding in-scope changes
/// to `sink`. Every event path passes [`Scope::should_watch`] before any
/// metadata is read, so files outside the workspace (or ignored) are untouched.
pub async fn watch(
    scope: Scope,
    sink: tokio::sync::mpsc::Sender<FileChange>,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // `notify` calls back on its own thread; bridge into the async world.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })?;
    watcher.watch(scope.root(), RecursiveMode::Recursive)?;
    tracing::info!(root = %scope.root().display(), "workspace watcher started");

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(event) = maybe else { break };
                let kind = classify(&event.kind);
                for path in event.paths {
                    if scope.should_watch(&path) {
                        let change = probe(&path, kind).await;
                        if sink.send(change).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            () = wait_for_shutdown(&mut shutdown) => break,
        }
    }
    tracing::info!("workspace watcher stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptlyd-scope-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn paths_inside_are_in_scope_outside_are_not() {
        let root = temp_root("inside");
        let inside = root.join("src/main.rs");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "fn main() {}").unwrap();

        let scope = Scope::new(&root).unwrap();
        assert!(scope.should_watch(&inside));

        // A sibling directory next to the root is outside.
        let outside = root
            .parent()
            .unwrap()
            .join("promptlyd-scope-outside-victim");
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();
        assert!(!scope.contains(&secret), "sibling dir is out of scope");

        // A `..` escape from inside the root resolves outside and is rejected.
        assert!(!scope.contains(&root.join("../promptlyd-scope-outside-victim/secret.txt")));

        std::fs::remove_dir_all(&outside).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn promptlyignore_entries_are_skipped() {
        let root = temp_root("ignore");
        std::fs::write(root.join(".promptlyignore"), "*.log\nsecrets/\n").unwrap();
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for (rel, _) in [
            ("app.log", ()),
            ("src/app.log", ()),
            ("secrets/key.txt", ()),
            ("src/main.rs", ()),
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        let scope = Scope::new(&root).unwrap();

        assert!(scope.is_ignored(&root.join("app.log")));
        assert!(
            scope.is_ignored(&root.join("src/app.log")),
            "*.log matches at depth"
        );
        assert!(scope.is_ignored(&root.join("secrets/key.txt")));
        assert!(
            scope.is_ignored(&root.join(".promptly/checkpoint.json")),
            "builtin ignore"
        );
        assert!(!scope.is_ignored(&root.join("src/main.rs")));
        assert!(!scope.should_watch(&root.join("app.log")));
        assert!(scope.should_watch(&root.join("src/main.rs")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_pointing_outside_is_rejected() {
        let root = temp_root("symlink");
        let outside = root
            .parent()
            .unwrap()
            .join("promptlyd-scope-symlink-target");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("loot.txt"), "loot").unwrap();

        // A symlink *inside* the workspace pointing at the outside dir.
        let link = root.join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let scope = Scope::new(&root).unwrap();
        // The lexical path looks inside, but canonicalization resolves the symlink
        // to the outside target, so it is correctly rejected.
        assert!(!scope.contains(&link.join("loot.txt")));

        std::fs::remove_dir_all(&outside).ok();
        std::fs::remove_dir_all(&root).ok();
    }
}
