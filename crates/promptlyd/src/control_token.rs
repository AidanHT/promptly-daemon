//! The loopback control-API capability token (`auth`).
//!
//! The daemon's mutating routes (`POST /session/start|stop|reset`, `/shutdown`)
//! were guarded only by the *presence* of the `X-Promptly-Control` header. That
//! stops a browser — the GET-only, origin-locked CORS blocks a cross-origin custom
//! header — but not a local non-browser process, which could otherwise stop a
//! rival's session, force a shutdown, or inject a session start by setting the same
//! constant header.
//!
//! This mints a per-process **capability token**: the daemon writes a fresh random
//! secret at startup, owner-only (`0600` on Unix; the user-profile ACL on Windows)
//! to `~/.promptly/control.json`, and requires it as the control header's *value*.
//! Only a process that can read the owning user's data dir — the `promptly` CLI —
//! can present it, so another user's process (the file is unreadable to them) and
//! any browser are both shut out. The token rotates every start, so a stale file
//! can never authenticate to a new daemon.
//!
//! Same-user malware can read the file — but it can already read the device
//! credentials beside it, so that boundary is out of scope: the OS user is the
//! trust boundary, exactly as [`crate::baseline`]'s sibling `credentials.rs`
//! documents. The token is loopback-only and short-lived; it is one more layer,
//! not a replacement for the server-side attestation that anchors scoring.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

/// Schema version of the on-disk control-auth file.
pub const CONTROL_AUTH_VERSION: u32 = 1;

/// File name under [`paths::data_dir`] holding the running daemon's control token.
pub const CONTROL_AUTH_FILE: &str = "control.json";

/// The running daemon's loopback control credential: written at startup, read by
/// the CLI to authenticate its control requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlAuth {
    /// On-disk schema version.
    pub version: u32,
    /// The shared secret the CLI echoes in the `X-Promptly-Control` header.
    pub token: String,
    /// The API port this daemon bound — lets a reader confirm it's the same daemon.
    pub api_port: u16,
    /// The daemon process id (diagnostics only).
    pub pid: u32,
}

/// Full path of the control-auth file under [`paths::data_dir`].
pub fn control_auth_path() -> PathBuf {
    paths::data_dir().join(CONTROL_AUTH_FILE)
}

/// A fresh, unguessable token: 256 bits from two v4 UUIDs (122 bits of entropy
/// each, from the OS CSPRNG), hex with no separators. Reuses the daemon's existing
/// `uuid` dependency rather than pulling in a second RNG.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Mint a fresh token, persist it owner-only under `data_dir`, and return it.
/// Called once at daemon startup, before the API binds — so the file exists by the
/// time the CLI (which polls `/health` first, an unauthenticated read) issues any
/// control request.
pub fn write(data_dir: &Path, api_port: u16) -> io::Result<String> {
    let token = generate_token();
    let auth = ControlAuth {
        version: CONTROL_AUTH_VERSION,
        token: token.clone(),
        api_port,
        pid: std::process::id(),
    };
    std::fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(&auth).map_err(io::Error::other)?;
    // Write to a sibling temp file (owner-only from birth on Unix), then rename over
    // the target: the move is atomic and carries the tight permissions, so the
    // secret never sits world-readable, mirroring `credentials.rs`.
    let path = data_dir.join(CONTROL_AUTH_FILE);
    let tmp = path.with_extension("json.tmp");
    write_owner_only(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(token)
}

/// Read the control auth for the daemon using `data_dir`. `Ok(None)` when no file
/// exists yet (no daemon has started). A malformed file is an error so a caller can
/// tell the user to restart the daemon rather than fail opaquely.
pub fn read(data_dir: &Path) -> io::Result<Option<ControlAuth>> {
    let path = data_dir.join(CONTROL_AUTH_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let auth = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            Ok(Some(auth))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Constant-time token equality, so a wrong guess can't be recovered byte-by-byte
/// from response timing. The token length is fixed and public, so an early
/// length-mismatch return leaks nothing.
pub fn token_matches(provided: &[u8], expected: &[u8]) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    let diff = provided
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

/// Create `path` fresh and owner-readable-only, then write `bytes`. On Unix the
/// file is born `0600` (a stale temp from a crashed write is discarded first so
/// `create_new` always gets a clean inode). Off Unix there's no portable `chmod`,
/// so we rely on the user-profile ACL Windows applies to `%USERPROFILE%` — the same
/// posture as `credentials.rs`.
fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let _ = std::fs::remove_file(path);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptlyd-control-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generate_token_is_long_and_unique() {
        let a = generate_token();
        let b = generate_token();
        // 2 × 32 hex chars = 256 bits of entropy, all hex.
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "every mint is fresh");
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = temp_dir("round-trip");
        let token = write(&dir, 8765).unwrap();
        let auth = read(&dir).unwrap().expect("the file was just written");
        assert_eq!(auth.token, token);
        assert_eq!(auth.api_port, 8765);
        assert_eq!(auth.version, CONTROL_AUTH_VERSION);
        assert_eq!(auth.pid, std::process::id());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_rotates_the_token_each_time() {
        let dir = temp_dir("rotate");
        let first = write(&dir, 8765).unwrap();
        let second = write(&dir, 8765).unwrap();
        assert_ne!(
            first, second,
            "a new daemon start invalidates the old token"
        );
        assert_eq!(read(&dir).unwrap().unwrap().token, second);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_missing_is_none() {
        let dir = temp_dir("missing");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read(&dir).unwrap().is_none(), "no daemon has started yet");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_matches_is_exact() {
        assert!(token_matches(b"abc123", b"abc123"));
        assert!(!token_matches(b"abc123", b"abc124"));
        assert!(
            !token_matches(b"abc", b"abc123"),
            "length mismatch is rejected"
        );
        assert!(!token_matches(b"", b"x"));
        assert!(token_matches(b"", b""));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        write(&dir, 8765).unwrap();
        let mode = std::fs::metadata(dir.join(CONTROL_AUTH_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the control token must be owner-only");
        std::fs::remove_dir_all(&dir).ok();
    }
}
