//! `promptly restart [level]` — discard the current attempt and re-fetch the
//! level fresh, in the same folder.
//!
//! [`super::session::run_reset`] restores the starter files but keeps the same
//! attempt: the original solve clock and the bound attempt nonce both survive, so
//! it's a mid-solve "undo my edits", not a do-over. `restart` is the harder reset
//! a player reaches for when they'd rather start the level over from zero. It stops
//! the daemon, clears the bound session marker and the solve clock, wipes the
//! workspace (everything but `.git`), and re-downloads the pristine kit — so the
//! next `promptly start` mints a brand-new attempt with a fresh clock and a
//! re-verified baseline. It's the one-command equivalent of deleting the folder and
//! re-`init`-ing it, but in place.
//!
//! The pristine kit is downloaded *before* anything is deleted, so a failed
//! download (e.g. offline) leaves the workspace exactly as it was.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;

use crate::commands::init;
use crate::daemon_process;
use crate::prompt::Ask;
use crate::style::Style;
use crate::web_client::KitSource;
use crate::CommandExit;

#[derive(Debug, Args)]
pub struct RestartArgs {
    /// Optional level slug guard — `restart` refuses unless this folder holds that
    /// level, so a stray run in the wrong directory can't wipe it.
    level: Option<String>,

    /// Skip the confirmation prompt.
    #[arg(long)]
    yes: bool,
}

pub fn run(
    kits: &dyn KitSource,
    asker: &mut dyn Ask,
    api_port: u16,
    workspace: PathBuf,
    args: RestartArgs,
    now_ms: i64,
    style: Style,
) -> anyhow::Result<CommandExit> {
    // The folder must already be a level workspace: it tells us which kit to
    // re-fetch, and it's the guard that we're not about to wipe an unrelated dir.
    let manifest = promptlyd::manifest::Manifest::load(&workspace).map_err(|_| {
        anyhow::anyhow!(
            "this folder isn't a Promptly level workspace (no .promptly/manifest.json) — \
             nothing to restart. Fetch a level first with `promptly init <level>`."
        )
    })?;
    let slug = manifest.slug.clone();

    if let Some(named) = &args.level {
        if !slug_matches(&slug, named) {
            println!(
                "{}",
                style.red(&format!(
                    "this folder holds '{slug}', not '{named}' — \
                     run `restart` from that level's workspace"
                )),
            );
            return Ok(CommandExit::Failure);
        }
    }

    // Destructive and unrecoverable — spell out exactly what goes. Enter defaults to
    // no, and a non-interactive shell never wipes silently.
    let confirmed = args.yes
        || asker.confirm(
            &format!(
                "Restart '{slug}' from scratch? This permanently deletes everything in {} \
                 (except .git) and starts a brand-new attempt. Commit or copy the folder \
                 first if you want to keep your work.",
                workspace.display(),
            ),
            false,
            false,
        );
    if !confirmed {
        println!("{}", style.dim("aborted — workspace unchanged"));
        return Ok(CommandExit::Failure);
    }

    // Stop the daemon before we touch its marker or the workspace, so it isn't
    // holding the session file, open file handles, or the single-instance lock.
    daemon_process::stop_background(api_port).context("stopping the daemon before restart")?;

    // The session marker lives in the daemon's machine-level data dir (`~/.promptly`).
    let data_dir = promptlyd::paths::data_dir();
    let (cleared, acquired) = perform_restart(kits, &data_dir, &workspace, &slug, now_ms)?;

    println!(
        "{} {} {}",
        style.green("level restarted"),
        style.accent(&slug),
        style.dim(&format!(
            "({} files, baseline {})",
            acquired.file_count,
            short(&acquired.manifest.baseline_hash),
        )),
    );
    if cleared {
        println!(
            "  {}",
            style.dim("cleared the previous attempt — fresh solve clock and capture"),
        );
    }
    println!(
        "  {}",
        style.dim("`promptly start` to begin the new attempt (or `promptly play`)"),
    );
    Ok(CommandExit::Success)
}

/// The destructive core, kept free of the daemon stop and the prompt so it's
/// unit-testable: download the pristine kit, clear the bound marker, wipe the
/// folder, and unpack fresh. Returns whether a marker was cleared, plus the
/// acquisition outcome.
///
/// The download happens first, before any deletion, so an offline `restart` aborts
/// without having touched the workspace.
fn perform_restart(
    kits: &dyn KitSource,
    data_dir: &Path,
    workspace: &Path,
    slug: &str,
    now_ms: i64,
) -> anyhow::Result<(bool, init::Acquired)> {
    let bytes = kits
        .download_kit(slug)
        .context("re-fetching the level's starter kit")?;
    let cleared = clear_marker_for(data_dir, workspace)?;
    wipe_workspace(workspace).context("clearing the workspace for the fresh starter")?;
    let acquired = init::unpack_and_record(&bytes, workspace, now_ms)
        .context("unpacking the fresh starter")?;
    Ok((cleared, acquired))
}

/// Delete the daemon's session marker, but only when it binds *this* workspace —
/// a marker for another folder is a different attempt and must be left alone.
/// Clearing it is what makes the next `start` a fresh attempt (new nonce,
/// re-verified baseline) instead of a resume. Returns whether one was removed.
fn clear_marker_for(data_dir: &Path, workspace: &Path) -> anyhow::Result<bool> {
    let store = promptlyd::scoping::SessionStore::new(data_dir.to_path_buf());
    let Some(marker) = store.load_marker() else {
        return Ok(false);
    };
    if !same_path(&marker.workspace, workspace) {
        return Ok(false);
    }
    let path = store.marker_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            Err(e).with_context(|| format!("removing the session marker at {}", path.display()))
        }
    }
}

/// Remove every entry in `workspace` except `.git` — never destroy a player's VCS
/// history. The caller unpacks the fresh kit (including a new `.promptly/`) over the
/// emptied folder.
fn wipe_workspace(workspace: &Path) -> anyhow::Result<()> {
    for entry in
        std::fs::read_dir(workspace).with_context(|| format!("reading {}", workspace.display()))?
    {
        let entry = entry?;
        // Preserve a git repo so a player who versioned their attempt keeps it.
        if entry.file_name().to_str() == Some(".git") {
            continue;
        }
        let path = entry.path();
        // A symlink is removed as a link; we never recurse through one.
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing {}", path.display()))?;
        } else {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Canonicalized path equality, falling back to a literal compare when a path can't
/// be canonicalized (mirrors [`crate::daemon_process`]'s same-workspace check).
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Exact slug, or the short `stage-N-MM` prefix of the full slug (mirrors the guard
/// in [`crate::commands::session`]).
fn slug_matches(bound: &str, named: &str) -> bool {
    bound == named || bound.starts_with(&format!("{named}-"))
}

/// First 12 hex chars of a content hash, for compact display.
fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedAsk;
    use crate::web_client::WebError;
    use promptlyd::scoping::{NonceOrigin, SessionMarker, SessionStore, SESSION_MARKER_VERSION};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const SLUG: &str = "stage-1-01-lru-eviction-debug";
    const NOW: i64 = 1_700_000_000_000;

    // ---- fakes & fixtures -------------------------------------------------

    /// A valid kit zip whose unpacked tree reproduces its own `baseline_hash`
    /// (mirrors `init`'s test kit) so `unpack_and_record` accepts it.
    fn build_kit_zip(slug: &str) -> Vec<u8> {
        use promptlyd::baseline::{compute_baseline_hash, CanonicalFile};
        let starter = [
            ("lru.go", "package main // TODO\n"),
            (
                "tests/public/cases.json",
                "{\"harness\":\"stdin_stdout\"}\n",
            ),
        ];
        let canonical: Vec<CanonicalFile> = starter
            .iter()
            .map(|(p, c)| CanonicalFile {
                path: (*p).to_string(),
                content: c.as_bytes().to_vec(),
            })
            .collect();
        let baseline = compute_baseline_hash(&canonical);
        let manifest = format!(
            r#"{{"schema_version":1,"kit_version":2,"level_id":"lvl-1","slug":"{slug}","title":"LRU",
                "language":"Go","runtime_version":"go1.22","challenge_type":"debugging",
                "execution_harness":"stdin_stdout","file_allowlist":["lru.go"],"entry_points":["main.go"],
                "baseline_hash":"{baseline}"}}"#
        );

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (path, content) in starter {
            zip.start_file(path, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.start_file("README.md", opts).unwrap();
        zip.write_all(b"# generated\n").unwrap();
        zip.start_file(".promptly/manifest.json", opts).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    struct FakeKits {
        zip: Vec<u8>,
    }

    impl KitSource for FakeKits {
        fn download_kit(&self, _slug: &str) -> Result<Vec<u8>, WebError> {
            Ok(self.zip.clone())
        }
    }

    /// A unique, empty scratch dir for one test (keyed by pid + label).
    fn base(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptly-restart-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A manifest just valid enough for `Manifest::load` (the `run` guard); the
    /// `baseline_hash` is irrelevant because `run` never verifies it.
    fn write_manifest(ws: &Path, slug: &str) {
        let dir = ws.join(".promptly");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = format!(
            r#"{{"schema_version":1,"kit_version":2,"level_id":"lvl-1","slug":"{slug}","title":"LRU",
                "language":"Go","runtime_version":"go1.22","challenge_type":"debugging",
                "execution_harness":"stdin_stdout","file_allowlist":["lru.go"],"entry_points":["main.go"],
                "baseline_hash":"abc"}}"#
        );
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
    }

    fn marker_bound_to(ws: &Path) -> SessionMarker {
        SessionMarker {
            version: SESSION_MARKER_VERSION,
            session_id: "old-session".into(),
            workspace: ws.to_path_buf(),
            level_id: "lvl-1".into(),
            slug: SLUG.into(),
            started_at_ms: 1_000,
            stopped_at_ms: Some(2_000),
            attempt_nonce: "old-nonce".into(),
            nonce_origin: NonceOrigin::Local,
            file_allowlist: vec!["lru.go".into()],
            code_reset_count: 0,
            bootstrap: None,
        }
    }

    // ---- perform_restart (the destructive core) ---------------------------

    #[test]
    fn replaces_all_files_resets_the_clock_and_clears_a_bound_marker() {
        let root = base("core");
        let ws = root.join("ws");
        let data = root.join("data");
        std::fs::create_dir_all(&ws).unwrap();

        // A used workspace: an old solve clock, the player's edited solution, a
        // stray file the kit never had, and a git repo.
        write_manifest(&ws, SLUG);
        std::fs::write(
            ws.join(".promptly/acquisition.json"),
            "{\"acquired_at_ms\":1}",
        )
        .unwrap();
        std::fs::write(ws.join("lru.go"), "package main // MY SOLUTION\n").unwrap();
        std::fs::write(ws.join("notes.txt"), "scratch").unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

        // A daemon marker bound to this very workspace.
        SessionStore::new(data.clone())
            .save_marker(&marker_bound_to(&ws))
            .unwrap();

        let kits = FakeKits {
            zip: build_kit_zip(SLUG),
        };
        let (cleared, acquired) = perform_restart(&kits, &data, &ws, SLUG, NOW).unwrap();

        assert!(cleared, "the bound marker was cleared");
        assert!(
            SessionStore::new(data.clone()).load_marker().is_none(),
            "the marker file is gone"
        );

        // The fresh kit is laid down; the player's edit and the stray file are gone.
        assert_eq!(
            std::fs::read_to_string(ws.join("lru.go")).unwrap(),
            "package main // TODO\n"
        );
        assert!(ws.join("tests/public/cases.json").exists());
        assert!(ws.join(".promptly/manifest.json").exists());
        assert!(!ws.join("notes.txt").exists(), "stray files are wiped");

        // Git history survives the wipe.
        assert_eq!(
            std::fs::read_to_string(ws.join(".git/HEAD")).unwrap(),
            "ref: refs/heads/main"
        );

        // The solve clock restarts at `now` (the old acquisition was wiped).
        assert_eq!(acquired.acquired_at_ms, NOW, "fresh solve clock");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn leaves_a_marker_that_binds_a_different_workspace() {
        let root = base("other-marker");
        let ws = root.join("ws");
        let other = root.join("other");
        let data = root.join("data");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        // The marker belongs to a *different* folder's attempt.
        SessionStore::new(data.clone())
            .save_marker(&marker_bound_to(&other))
            .unwrap();

        let kits = FakeKits {
            zip: build_kit_zip(SLUG),
        };
        let (cleared, _) = perform_restart(&kits, &data, &ws, SLUG, NOW).unwrap();

        assert!(!cleared, "another workspace's marker is untouched");
        let kept = SessionStore::new(data.clone()).load_marker().unwrap();
        assert!(same_path(&kept.workspace, &other));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restarts_cleanly_when_no_marker_exists() {
        let root = base("no-marker");
        let ws = root.join("ws");
        let data = root.join("data"); // never written to
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("stale.txt"), "x").unwrap();

        let kits = FakeKits {
            zip: build_kit_zip(SLUG),
        };
        let (cleared, acquired) = perform_restart(&kits, &data, &ws, SLUG, NOW).unwrap();

        assert!(!cleared, "nothing to clear");
        assert!(ws.join("lru.go").exists(), "the kit is still laid down");
        assert!(!ws.join("stale.txt").exists());
        // lru.go, tests/public/cases.json, README.md, .promptly/manifest.json.
        assert_eq!(acquired.file_count, 4);

        std::fs::remove_dir_all(&root).ok();
    }

    // ---- wipe_workspace ---------------------------------------------------

    #[test]
    fn wipe_removes_everything_except_git() {
        let root = base("wipe");
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join("sub/deep/b.txt"), "b").unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/objects/o"), "o").unwrap();

        wipe_workspace(&root).unwrap();

        let mut left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec![".git".to_string()]);
        assert!(root.join(".git/objects/o").exists(), "git contents intact");

        std::fs::remove_dir_all(&root).ok();
    }

    // ---- run() guard / confirm branches (no daemon, no network) -----------

    #[test]
    fn run_aborts_on_decline_and_changes_nothing() {
        // A decline returns before the daemon stop and before any deletion.
        let root = base("decline");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        write_manifest(&ws, SLUG);
        std::fs::write(ws.join("notes.txt"), "keep").unwrap();

        let kits = FakeKits {
            zip: build_kit_zip(SLUG),
        };
        let mut ask = ScriptedAsk::new([false]); // decline
        let exit = run(
            &kits,
            &mut ask,
            1,
            ws.clone(),
            RestartArgs {
                level: None,
                yes: false,
            },
            NOW,
            Style::plain(),
        )
        .unwrap();

        assert_eq!(exit, CommandExit::Failure);
        assert!(ws.join("notes.txt").exists(), "no wipe on decline");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_refuses_a_level_guard_mismatch() {
        let root = base("guard");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        write_manifest(&ws, SLUG);
        std::fs::write(ws.join("notes.txt"), "keep").unwrap();

        let kits = FakeKits {
            zip: build_kit_zip(SLUG),
        };
        let mut ask = ScriptedAsk::new([]); // never prompted
        let exit = run(
            &kits,
            &mut ask,
            1,
            ws.clone(),
            RestartArgs {
                level: Some("stage-2-06".into()),
                yes: false,
            },
            NOW,
            Style::plain(),
        )
        .unwrap();

        assert_eq!(exit, CommandExit::Failure);
        assert!(
            ws.join("notes.txt").exists(),
            "a guard failure changes nothing"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_errors_outside_a_level_workspace() {
        let root = base("no-manifest");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap(); // no .promptly/manifest.json

        let kits = FakeKits {
            zip: build_kit_zip(SLUG),
        };
        let mut ask = ScriptedAsk::new([]);
        let result = run(
            &kits,
            &mut ask,
            1,
            ws,
            RestartArgs {
                level: None,
                yes: false,
            },
            NOW,
            Style::plain(),
        );
        assert!(
            result.is_err(),
            "a non-workspace folder is rejected, not wiped"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_short_stage_prefix_satisfies_the_guard() {
        assert!(slug_matches(SLUG, "stage-1-01"));
        assert!(slug_matches(SLUG, SLUG));
        assert!(!slug_matches(SLUG, "stage-1-02"));
    }
}
