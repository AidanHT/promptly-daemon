//! Self-update mechanism for the `promptly` + `promptlyd` binaries.
//!
//! `promptly update` ([`crate::commands::update`]) drives this: resolve the
//! latest GitHub release, download the platform archive, and swap both installed
//! binaries in place. The HTTP / extraction / file-replacement I/O lives here
//! behind small functions; the pure pieces — semantic-version compare, asset-name
//! construction, and archive extraction — are unit-tested.
//!
//! Both archive formats are handled on every platform (`.tar.gz` via
//! `flate2`+`tar`, `.zip` via `zip`) and selected at runtime by the asset's
//! extension, so a build on *any* host type-checks the whole module rather than
//! hiding one branch behind `#[cfg]`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;

/// The release repo, taken from the crate's own `repository` metadata so it
/// tracks that field rather than duplicating the URL.
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// The target triple this binary was built for, captured by `build.rs` from
/// Cargo's `TARGET`. Names the release asset to download.
const BUILD_TARGET: &str = env!("PROMPTLY_BUILD_TARGET");

/// GitHub requires a User-Agent on API calls; identify as the CLI.
const USER_AGENT: &str = concat!("promptly/", env!("CARGO_PKG_VERSION"));

/// Hard cap on any single download / archive entry, so a hostile or oversized
/// response can't exhaust memory. The real binaries are a few MB; 256 MiB is far
/// beyond that.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// `owner/repo`, parsed from [`REPO_URL`] for the GitHub API and download URLs.
fn repo_slug() -> &'static str {
    REPO_URL
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
}

/// A semantic version — the `MAJOR.MINOR.PATCH` core. Any pre-release/build
/// suffix is dropped, which is all our plain `vX.Y.Z` tags need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parse `v1.2.3`, `1.2.3`, `1.2`, or `1`, tolerating a leading `v` and a
    /// trailing `-rc.1` / `+build` suffix. Missing components default to 0;
    /// anything non-numeric yields `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let core = s.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next().unwrap_or(core);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    /// The version of this running binary (`CARGO_PKG_VERSION`).
    pub fn current() -> Version {
        // Cargo guarantees the crate version is valid semver.
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version {
            major: 0,
            minor: 0,
            patch: 0,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Whether this build targets Windows — selects the `.zip` asset and the `.exe`
/// binary names. Derived from the build triple so it matches the asset the
/// release workflow produced for this platform.
fn is_windows_target() -> bool {
    BUILD_TARGET.contains("windows")
}

fn promptly_bin_name() -> &'static str {
    if is_windows_target() {
        "promptly.exe"
    } else {
        "promptly"
    }
}

fn promptlyd_bin_name() -> &'static str {
    if is_windows_target() {
        "promptlyd.exe"
    } else {
        "promptlyd"
    }
}

/// The release asset file name for this platform and `tag`, matching the release
/// workflow's packaging: `promptly-<tag>-<target>.{tar.gz|zip}`.
pub fn asset_name(tag: &str) -> String {
    let ext = if is_windows_target() { "zip" } else { "tar.gz" };
    format!("promptly-{tag}-{BUILD_TARGET}.{ext}")
}

/// The browser download URL for a release asset.
pub fn download_url(tag: &str, asset: &str) -> String {
    format!(
        "https://github.com/{}/releases/download/{tag}/{asset}",
        repo_slug()
    )
}

/// Build a blocking HTTP agent. `read_timeout` is short for the background
/// notifier and longer for an explicit download.
fn agent(read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read_timeout)
        .build()
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
}

/// Fetch the latest published release's tag (e.g. `v0.2.0`) from the GitHub API.
/// `releases/latest` excludes drafts and pre-releases, so only stable versions
/// surface. `read_timeout` bounds the call.
pub fn fetch_latest_tag(read_timeout: Duration) -> anyhow::Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        repo_slug()
    );
    let resp = agent(read_timeout)
        .get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => {
                anyhow!("no published releases for {} yet", repo_slug())
            }
            ureq::Error::Status(code, _) => anyhow!("the GitHub API returned HTTP {code}"),
            ureq::Error::Transport(t) => anyhow!("couldn't reach the GitHub API: {t}"),
        })?;
    // `into_string` needs no extra ureq feature (unlike `into_json`); parse with
    // the serde_json already in the tree.
    let body = resp
        .into_string()
        .context("reading the GitHub API response")?;
    let release: ApiRelease =
        serde_json::from_str(&body).context("parsing the GitHub release response")?;
    Ok(release.tag_name)
}

/// Download a release asset fully into memory.
pub fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    let resp = agent(Duration::from_secs(120))
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => anyhow!(
                "no prebuilt binary for this platform ({BUILD_TARGET}). \
                 Reinstall from source: cargo install --git {REPO_URL} promptly promptlyd"
            ),
            ureq::Error::Status(code, _) => anyhow!("download failed: HTTP {code}"),
            ureq::Error::Transport(t) => anyhow!("download failed: {t}"),
        })?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_DOWNLOAD_BYTES)
        .read_to_end(&mut buf)
        .context("reading the downloaded archive")?;
    Ok(buf)
}

/// The two binaries pulled out of a release archive.
pub struct ExtractedBins {
    pub promptly: Vec<u8>,
    pub promptlyd: Vec<u8>,
}

/// Extract `promptly` and `promptlyd` from a release archive held in memory.
/// `is_zip` selects the format (`.zip` on Windows, `.tar.gz` elsewhere); both
/// code paths are always compiled, so either host type-checks the whole
/// function. The binaries are matched by file name, ignoring the archive's
/// top-level directory.
pub fn extract_binaries(archive: &[u8], is_zip: bool) -> anyhow::Result<ExtractedBins> {
    let (want_cli, want_daemon) = (promptly_bin_name(), promptlyd_bin_name());
    let mut cli: Option<Vec<u8>> = None;
    let mut daemon: Option<Vec<u8>> = None;

    if is_zip {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
            .context("opening the release zip")?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).context("reading a zip entry")?;
            match base_name(entry.name()).as_str() {
                b if b == want_cli => cli = Some(read_capped(&mut entry)?),
                b if b == want_daemon => daemon = Some(read_capped(&mut entry)?),
                _ => {}
            }
        }
    } else {
        let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(archive));
        let mut tar = tar::Archive::new(decoder);
        for entry in tar.entries().context("reading the release tar")? {
            let mut entry = entry.context("reading a tar entry")?;
            let name = entry
                .path()
                .context("a tar entry had a bad path")?
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match name.as_str() {
                b if b == want_cli => cli = Some(read_capped(&mut entry)?),
                b if b == want_daemon => daemon = Some(read_capped(&mut entry)?),
                _ => {}
            }
        }
    }

    match (cli, daemon) {
        (Some(promptly), Some(promptlyd)) => Ok(ExtractedBins {
            promptly,
            promptlyd,
        }),
        _ => bail!("the release archive didn't contain both promptly and promptlyd"),
    }
}

fn read_capped(r: &mut impl Read) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(MAX_DOWNLOAD_BYTES)
        .read_to_end(&mut buf)
        .context("reading an archive entry")?;
    Ok(buf)
}

/// The final path component of an archive entry name, handling both `/` and `\`
/// separators (zip entries may use either).
fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
}

/// Where the binaries live: the directory of the running `promptly`, plus the
/// resolved path of each binary in it.
pub struct InstallLayout {
    pub dir: PathBuf,
    pub promptly: PathBuf,
    pub promptlyd: PathBuf,
}

/// Resolve the install layout from the running executable. Both binaries are
/// installed together (the release archive and `cargo install` co-locate them),
/// so the daemon sits next to this CLI.
pub fn installed_layout() -> anyhow::Result<InstallLayout> {
    let promptly = std::env::current_exe().context("locating the running promptly binary")?;
    let dir = promptly
        .parent()
        .ok_or_else(|| anyhow!("the promptly binary has no parent directory"))?
        .to_path_buf();
    let promptlyd = dir.join(if cfg!(windows) {
        "promptlyd.exe"
    } else {
        "promptlyd"
    });
    Ok(InstallLayout {
        dir,
        promptly,
        promptlyd,
    })
}

/// Reject self-updating a development build (a binary sitting in a Cargo
/// `target/debug` or `target/release` tree), which would clobber a local
/// checkout's build output with a downloaded release.
pub fn ensure_not_dev_build(layout: &InstallLayout) -> anyhow::Result<()> {
    if is_cargo_target_dir(&layout.dir) {
        bail!(
            "refusing to update a development build at {} — `promptly update` upgrades an \
             installed binary, not a `cargo build` output",
            layout.promptly.display()
        );
    }
    Ok(())
}

/// True when `dir` is a Cargo build-output dir (`…/target/debug` or
/// `…/target/release`): its leaf is `debug`/`release` and its parent is `target`.
fn is_cargo_target_dir(dir: &Path) -> bool {
    let leaf = dir.file_name().and_then(|s| s.to_str());
    let parent = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str());
    matches!((parent, leaf), (Some("target"), Some("debug" | "release")))
}

/// Replace the running `promptly` binary with `bytes`. Uses `self-replace`,
/// which performs the platform's rename-aside dance so even the *currently
/// executing* binary can be swapped (notably on Windows, where a running `.exe`
/// can't be overwritten directly).
pub fn replace_self(dir: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let staged = write_staged(dir, "promptly", bytes)?;
    let result =
        self_replace::self_replace(&staged).context("replacing the running promptly binary");
    let _ = std::fs::remove_file(&staged);
    result
}

/// Replace a sibling binary (`promptlyd`) that is *not* the running process.
/// Moves any existing file aside first — a rename succeeds even when a plain
/// overwrite wouldn't — then drops the new file into place and removes the
/// backup, rolling back if the swap fails.
pub fn replace_sibling(dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow!("the destination has no parent directory"))?;
    let staged = write_staged(dir, "promptlyd", bytes)?;

    let backup = dest.with_extension("old");
    if dest.exists() {
        // A stale backup would make the aside-rename fail on Windows (rename
        // won't overwrite an existing file there).
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(dest, &backup)
            .with_context(|| format!("moving {} aside", dest.display()))?;
    }
    match std::fs::rename(&staged, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup);
            Ok(())
        }
        Err(e) => {
            // Roll back to the original before surfacing the failure.
            let _ = std::fs::remove_file(&staged);
            if backup.exists() {
                let _ = std::fs::rename(&backup, dest);
            }
            Err(e).with_context(|| format!("installing the new {}", dest.display()))
        }
    }
}

/// Write `bytes` to a uniquely-named staging file in `dir` (the same filesystem
/// as the target, so the install step is an atomic rename), with the executable
/// bit set on Unix.
fn write_staged(dir: &Path, label: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let path = dir.join(format!(".{label}-update-{}.tmp", std::process::id()));
    std::fs::write(&path, bytes)
        .with_context(|| format!("writing the new binary to {}", path.display()))?;
    set_executable(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("setting the executable bit on {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_versions_with_optional_prefix_and_suffix() {
        assert_eq!(Version::parse("v1.2.3"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("1.2.3").unwrap().to_string(), "1.2.3");
        // Missing components default to zero.
        assert_eq!(Version::parse("2"), Version::parse("2.0.0"));
        assert_eq!(Version::parse("2.1"), Version::parse("2.1.0"));
        // A pre-release / build suffix is dropped.
        assert_eq!(Version::parse("0.2.0-rc.1"), Version::parse("0.2.0"));
        assert_eq!(Version::parse("0.2.0+build5"), Version::parse("0.2.0"));
        // Garbage is rejected.
        assert!(Version::parse("v").is_none());
        assert!(Version::parse("notaversion").is_none());
    }

    #[test]
    fn versions_order_by_precedence() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.1") > v("0.1.0"));
        assert!(v("0.1.0") == v("v0.1.0"));
    }

    #[test]
    fn repo_slug_is_owner_and_repo() {
        // Derived from the crate's real `repository` field.
        assert_eq!(repo_slug(), "AidanHT/promptly-daemon");
    }

    #[test]
    fn asset_name_and_url_match_the_release_layout() {
        let asset = asset_name("v9.9.9");
        assert!(asset.starts_with("promptly-v9.9.9-"));
        assert!(asset.contains(BUILD_TARGET));
        // The extension matches the platform's archive format.
        if is_windows_target() {
            assert!(asset.ends_with(".zip"));
        } else {
            assert!(asset.ends_with(".tar.gz"));
        }
        assert_eq!(
            download_url("v9.9.9", &asset),
            format!("https://github.com/AidanHT/promptly-daemon/releases/download/v9.9.9/{asset}"),
        );
    }

    #[test]
    fn dev_build_dirs_are_recognized() {
        assert!(is_cargo_target_dir(Path::new("/home/me/proj/target/debug")));
        assert!(is_cargo_target_dir(Path::new(
            "/home/me/proj/target/release"
        )));
        // A real install location is not a dev build.
        assert!(!is_cargo_target_dir(Path::new("/home/me/.local/bin")));
        assert!(!is_cargo_target_dir(Path::new("/usr/local/bin")));
    }

    // Build a release-shaped archive in memory (binaries nested under a
    // top-level directory, exactly as `release.yml` packages them) so the
    // extraction is exercised end-to-end against the real zip/tar codecs.
    fn make_zip(cli: &[u8], daemon: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file(format!("pkg/{}", promptly_bin_name()), opts)
            .unwrap();
        zip.write_all(cli).unwrap();
        zip.start_file(format!("pkg/{}", promptlyd_bin_name()), opts)
            .unwrap();
        zip.write_all(daemon).unwrap();
        // `finish` consumes the writer and drops its borrow of `buf`.
        zip.finish().unwrap();
        buf
    }

    fn make_targz(cli: &[u8], daemon: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            append(&mut tar, &format!("pkg/{}", promptly_bin_name()), cli);
            append(&mut tar, &format!("pkg/{}", promptlyd_bin_name()), daemon);
            tar.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    fn append<W: Write>(tar: &mut tar::Builder<W>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, name, data).unwrap();
    }

    #[test]
    fn extracts_both_binaries_from_a_zip() {
        let archive = make_zip(b"CLI-BYTES", b"DAEMON-BYTES");
        let bins = extract_binaries(&archive, true).unwrap();
        assert_eq!(bins.promptly, b"CLI-BYTES");
        assert_eq!(bins.promptlyd, b"DAEMON-BYTES");
    }

    #[test]
    fn extracts_both_binaries_from_a_targz() {
        let archive = make_targz(b"CLI-BYTES", b"DAEMON-BYTES");
        let bins = extract_binaries(&archive, false).unwrap();
        assert_eq!(bins.promptly, b"CLI-BYTES");
        assert_eq!(bins.promptlyd, b"DAEMON-BYTES");
    }

    #[test]
    fn extraction_fails_when_a_binary_is_missing() {
        // An archive with only the CLI is rejected (we need both).
        let mut buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file(format!("pkg/{}", promptly_bin_name()), opts)
            .unwrap();
        zip.write_all(b"only-the-cli").unwrap();
        zip.finish().unwrap();
        assert!(extract_binaries(&buf, true).is_err());
    }

    #[test]
    fn replace_sibling_swaps_the_file_contents() {
        // A self-contained temp dir keyed by pid, so parallel tests don't collide
        // and we never touch the network or env.
        let dir = std::env::temp_dir().join(format!("promptly-upd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(if cfg!(windows) {
            "promptlyd.exe"
        } else {
            "promptlyd"
        });
        std::fs::write(&dest, b"OLD-BINARY").unwrap();

        replace_sibling(&dest, b"NEW-BINARY-CONTENTS").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"NEW-BINARY-CONTENTS");
        // The backup is cleaned up on success.
        assert!(!dest.with_extension("old").exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
