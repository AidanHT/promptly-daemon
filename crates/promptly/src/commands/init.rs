//! `promptly init <level>` — workspace acquisition (`19`).
//!
//! Download the level's starter kit (`07`), unpack it into the target directory
//! (including `.promptly/manifest.json`), and verify the unpacked tree reproduces
//! the manifest's pinned `baseline_hash` (`18`/`baseline.rs`) so a corrupt or
//! tampered download is caught immediately. Acquisition starts the solve clock
//! (`11`): online this binds the server-side attempt, but offline (this slice; the
//! cloud path is `20`) it's recorded locally in `.promptly/acquisition.json` and
//! reconciled on first connect, keeping the **earliest** timestamp per `11`'s
//! single-clock rule.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::style::Style;
use crate::web_client::KitSource;
use crate::CommandExit;

/// Schema version of the local acquisition record (so `20` can evolve it).
const ACQUISITION_SCHEMA: u32 = 1;
/// The local acquisition record, under the (hash-excluded) `.promptly/` dir.
const ACQUISITION_FILE: &str = "acquisition.json";

#[derive(Debug, Args)]
pub struct InitArgs {
    /// The level to fetch: a short alias (`lru`), its number (`1`), a `stage-N-NN`
    /// prefix, or the full slug `stage-1-01-lru-eviction-debug`.
    level: String,

    /// Target directory to unpack into (default: the level's short keyword,
    /// e.g. `./lru`).
    #[arg(long)]
    dir: Option<PathBuf>,

    /// Overwrite a non-empty target directory with the fresh starter.
    #[arg(long)]
    force: bool,
}

impl InitArgs {
    /// Build init args for `promptly play`: fetch `level` into its default
    /// short-keyword directory (`./lru`), optionally overwriting a non-empty one.
    pub fn for_level(level: String, force: bool) -> Self {
        Self {
            level,
            dir: None,
            force,
        }
    }
}

/// The offline acquisition record `20` reconciles against the server attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acquisition {
    pub schema: u32,
    pub slug: String,
    pub level_id: String,
    pub kit_version: u32,
    /// When the solve clock started (epoch millis); the earliest wins (`11`).
    pub acquired_at_ms: i64,
}

pub fn run(
    kits: &dyn KitSource,
    args: InitArgs,
    now_ms: i64,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let Some(target) = fetch_workspace(kits, args, now_ms, style)? else {
        return Ok(CommandExit::Failure);
    };
    // Standalone init's correct next step really is `promptly start`. `play`
    // deliberately bypasses this epilogue (via `fetch_workspace`) because it
    // starts the session itself — two competing instruction sets confused players.
    println!(
        "  {}",
        style.dim(&format!(
            "next: cd {} · `promptly start`  (the daemon starts automatically)",
            target.display()
        )),
    );
    Ok(CommandExit::Success)
}

/// Fetch, unpack, verify, and stamp the workspace, printing acquisition progress
/// but not the "next steps" epilogue. Returns the target directory, or `None`
/// when a non-empty target was refused (already reported to the player).
pub(crate) fn fetch_workspace(
    kits: &dyn KitSource,
    args: InitArgs,
    now_ms: i64,
    style: Style,
) -> anyhow::Result<Option<PathBuf>> {
    // Expand a short alias (`lru`, `7`, `stage-1-01`) to the canonical slug the
    // kit route expects. An unknown name passes through unchanged, so the server
    // still owns "no such level". The workspace folder takes the *short* keyword
    // (`./lru`), not the slug — easy to cd into; the slug only names unknown
    // levels, where no keyword exists.
    let slug = crate::levels::resolve(&args.level);
    let target = args
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(crate::levels::workspace_dir_name(&slug)));

    if is_nonempty_dir(&target) && !args.force {
        println!(
            "{}",
            style.red(&format!(
                "{} already exists and is not empty — pass --dir to choose another, or --force to overwrite",
                target.display()
            )),
        );
        return Ok(None);
    }

    println!(
        "{} {} {}",
        style.dim("fetching kit"),
        style.accent(&slug),
        style.dim(&format!("→ {}", target.display())),
    );
    let bytes = kits.download_kit(&slug)?;
    let Acquired {
        file_count,
        manifest,
        acquired_at_ms,
    } = unpack_and_record(&bytes, &target, now_ms)?;

    println!(
        "{} {} {}",
        style.green("workspace ready"),
        style.dim(&format!("({file_count} files,")),
        style.dim(&format!("baseline {})", short(&manifest.baseline_hash))),
    );
    if acquired_at_ms < now_ms {
        println!(
            "  {}",
            style.dim("kept the earlier acquisition timestamp already recorded here"),
        );
    }
    Ok(Some(target))
}

/// What a successful acquisition produces: the number of files written, the parsed
/// manifest, and the recorded solve-clock time (the earliest already on disk wins,
/// per `11`). Shared by `init` and `restart`.
pub(crate) struct Acquired {
    pub(crate) file_count: usize,
    pub(crate) manifest: promptlyd::manifest::Manifest,
    pub(crate) acquired_at_ms: i64,
}

/// Unpack an already-downloaded kit into `target`, verify the unpacked tree
/// reproduces the manifest's pinned `baseline_hash` (so a corrupt or tampered
/// download is caught immediately), and stamp the solve clock. This is the
/// post-download half of acquisition, split out so `restart` can download *before*
/// it wipes the folder — a failed download then never destroys the player's work.
pub(crate) fn unpack_and_record(
    bytes: &[u8],
    target: &Path,
    now_ms: i64,
) -> anyhow::Result<Acquired> {
    let file_count = unpack_kit(bytes, target).context("unpacking the starter kit")?;

    let manifest = promptlyd::manifest::Manifest::load(target)
        .context("the kit did not contain a valid .promptly/manifest.json")?;
    match promptlyd::baseline::verify_workspace(target, &manifest.baseline_hash)
        .context("hashing the unpacked workspace")?
    {
        promptlyd::baseline::BaselineStatus::Match => {}
        promptlyd::baseline::BaselineStatus::Mismatch { computed } => {
            anyhow::bail!(
                "downloaded kit failed its integrity check (expected baseline {}, got {}) — try again",
                short(&manifest.baseline_hash),
                short(&computed),
            );
        }
    }

    let acquired_at_ms = record_acquisition(target, &manifest, now_ms)?;
    Ok(Acquired {
        file_count,
        manifest,
        acquired_at_ms,
    })
}

fn is_nonempty_dir(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Unpack a kit zip into `target`, rejecting any entry whose path escapes the
/// target (defense-in-depth even though kits come from our server). Returns the
/// number of files written.
fn unpack_kit(bytes: &[u8], target: &Path) -> anyhow::Result<usize> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("kit is not a valid zip archive")?;
    std::fs::create_dir_all(target)?;
    let mut written = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        // `enclosed_name` returns `None` for an absolute path or one containing
        // `..` that would escape the target — refuse it.
        let Some(rel) = entry.enclosed_name() else {
            anyhow::bail!("kit contains an unsafe path: {}", entry.name());
        };
        let out_path = target.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("writing {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out)?;
        written += 1;
    }
    Ok(written)
}

/// Write the acquisition record, preserving the earliest `acquired_at_ms` if one
/// is already present (the single-clock rule, `11`). Returns the recorded time.
fn record_acquisition(
    target: &Path,
    manifest: &promptlyd::manifest::Manifest,
    now_ms: i64,
) -> anyhow::Result<i64> {
    let path = target
        .join(promptlyd::manifest::PROMPTLY_DIR)
        .join(ACQUISITION_FILE);
    let acquired_at_ms = read_existing(&path)
        .map(|prev| prev.acquired_at_ms.min(now_ms))
        .unwrap_or(now_ms);
    let record = Acquisition {
        schema: ACQUISITION_SCHEMA,
        slug: manifest.slug.clone(),
        level_id: manifest.level_id.clone(),
        kit_version: manifest.kit_version,
        acquired_at_ms,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(&record)?;
    json.push('\n');
    std::fs::write(&path, json).context("writing the acquisition record")?;
    Ok(acquired_at_ms)
}

fn read_existing(path: &Path) -> Option<Acquisition> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use promptlyd::baseline::{compute_baseline_hash, CanonicalFile};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Build a minimal, valid kit zip whose unpacked tree reproduces its own
    /// `baseline_hash` (the contract `07` ships and `init` verifies).
    fn build_kit_zip(slug: &str) -> Vec<u8> {
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

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
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
        fn download_kit(&self, _slug: &str) -> Result<Vec<u8>, crate::web_client::WebError> {
            Ok(self.zip.clone())
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptly-init-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn unpacks_a_kit_verifies_the_baseline_and_stamps_acquisition() {
        let slug = "stage-1-01-lru-eviction-debug";
        let kits = FakeKits {
            zip: build_kit_zip(slug),
        };
        let target = temp_dir("ok");
        let args = InitArgs {
            level: slug.to_string(),
            dir: Some(target.clone()),
            force: false,
        };
        let exit = run(&kits, args, 1_700_000_000_000, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Success);

        // Files unpacked, including the manifest.
        assert!(target.join("lru.go").exists());
        assert!(target.join("tests/public/cases.json").exists());
        assert!(target.join(".promptly/manifest.json").exists());

        // Acquisition recorded with the solve-clock start.
        let acq: Acquisition = serde_json::from_slice(
            &std::fs::read(target.join(".promptly/acquisition.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(acq.slug, slug);
        assert_eq!(acq.level_id, "lvl-1");
        assert_eq!(acq.kit_version, 2);
        assert_eq!(acq.acquired_at_ms, 1_700_000_000_000);

        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn re_init_keeps_the_earliest_acquisition_timestamp() {
        let slug = "stage-1-01-lru-eviction-debug";
        let kits = FakeKits {
            zip: build_kit_zip(slug),
        };
        let target = temp_dir("earliest");

        // First acquisition at an earlier time.
        run(
            &kits,
            InitArgs {
                level: slug.into(),
                dir: Some(target.clone()),
                force: false,
            },
            1_000,
            Style::plain(),
        )
        .unwrap();

        // Re-init later with --force; the earlier clock must survive.
        run(
            &kits,
            InitArgs {
                level: slug.into(),
                dir: Some(target.clone()),
                force: true,
            },
            9_999,
            Style::plain(),
        )
        .unwrap();

        let acq: Acquisition = serde_json::from_slice(
            &std::fs::read(target.join(".promptly/acquisition.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            acq.acquired_at_ms, 1_000,
            "earliest acquisition wins (single clock)"
        );

        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn refuses_a_nonempty_target_without_force() {
        let target = temp_dir("nonempty");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("existing.txt"), "keep me").unwrap();
        let kits = FakeKits {
            zip: build_kit_zip("stage-1-01-lru-eviction-debug"),
        };
        let exit = run(
            &kits,
            InitArgs {
                level: "stage-1-01-lru-eviction-debug".into(),
                dir: Some(target.clone()),
                force: false,
            },
            1,
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Failure);
        // The existing file is untouched.
        assert_eq!(
            std::fs::read_to_string(target.join("existing.txt")).unwrap(),
            "keep me"
        );
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn fetch_workspace_returns_the_target_and_skips_the_epilogue_path() {
        // `play` builds on this: the fetch half hands back the real target dir
        // (so the daemon is scoped correctly) and leaves next-step wording to
        // the caller.
        let slug = "stage-1-01-lru-eviction-debug";
        let kits = FakeKits {
            zip: build_kit_zip(slug),
        };
        let target = temp_dir("fetch-only");
        let args = InitArgs {
            level: slug.to_string(),
            dir: Some(target.clone()),
            force: false,
        };
        let fetched = fetch_workspace(&kits, args, 1_700_000_000_000, Style::plain())
            .unwrap()
            .expect("fetch succeeds");
        assert_eq!(fetched, target);
        assert!(target.join(".promptly/manifest.json").exists());
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn fetch_workspace_refuses_a_nonempty_target() {
        let target = temp_dir("fetch-nonempty");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("existing.txt"), "keep me").unwrap();
        let kits = FakeKits {
            zip: build_kit_zip("stage-1-01-lru-eviction-debug"),
        };
        let args = InitArgs {
            level: "stage-1-01-lru-eviction-debug".into(),
            dir: Some(target.clone()),
            force: false,
        };
        assert!(fetch_workspace(&kits, args, 1, Style::plain())
            .unwrap()
            .is_none());
        assert_eq!(
            std::fs::read_to_string(target.join("existing.txt")).unwrap(),
            "keep me"
        );
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn rejects_a_path_traversal_entry() {
        // A kit zip with an escaping entry must be refused by `unpack_kit`.
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        zip.start_file("../escape.txt", opts).unwrap();
        zip.write_all(b"x").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let target = temp_dir("traversal");
        // Either the entry is sanitized to a safe relative path, or it's rejected;
        // in neither case may a file land outside the target.
        let _ = unpack_kit(&bytes, &target);
        assert!(!target.parent().unwrap().join("escape.txt").exists());
        std::fs::remove_dir_all(&target).ok();
    }
}
