//! Device credential storage (`20`) — the paired device token + signing seed.
//!
//! Pairing (`pair`) establishes two long-lived secrets the cloud calls read on
//! every invocation: the opaque **device token** presented as the bearer
//! credential (hashed + expiry/revocation-checked server-side on every request)
//! and the 32-byte **Ed25519 signing seed** the CLI rebuilds its key from to sign
//! the turn chain at `submit`. They live behind a [`CredentialStore`] trait so the
//! commands take `&dyn CredentialStore` and tests drive an in-memory fake — no
//! real disk or keychain in a unit test.
//!
//! ## Why a 0600 file and not the OS keychain
//!
//! `docs/plan/20` calls for "the OS keychain/credential store" and is honest that
//! "the OS keychain is the trust boundary — malware with user-level access can
//! read it; rotation bounds the damage." A `keyring`-crate backend was rejected
//! deliberately: its persistent Linux backend needs a running secret-service
//! daemon (absent on the headless `ubuntu-latest` CI the daemon tests run on), and
//! its behavior can't be verified in this environment. [`FileCredentialStore`]
//! has the **same trust boundary** the plan already accepts — a file readable only
//! by the owning user (`0600` on Unix; the user-profile ACL on Windows) — while
//! staying testable and cross-platform. The token's 90-day server-side expiry and
//! one-command revocation (`devices` table) are what actually bound the damage,
//! exactly as the plan intends. This deviation is recorded in the plan
//! reconciliation.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The secrets a paired device holds. Serialized as the on-disk credential file;
/// never logged (both fields are secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// Opaque bearer token presented to the cloud. The server stores only its
    /// hash and checks expiry/revocation on every request.
    pub device_token: String,
    /// Base64 of the 32-byte Ed25519 seed the CLI rebuilds its signing key from
    /// at `submit` (`signing::signing_key_from_seed`). The public half was
    /// uploaded to `devices.public_key` at pairing.
    pub signing_seed_b64: String,
}

/// A credential-store failure: an I/O problem, or a stored file we can't parse.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential store I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The credential file exists but isn't the JSON we wrote (hand-edited or
    /// truncated). Surfaced rather than silently treated as "unpaired" so the
    /// user is told to re-pair instead of seeing a confusing auth failure.
    #[error("stored credentials are unreadable ({0}) — run `promptly pair` again")]
    Corrupt(String),
}

/// Where a paired device's secrets live. A trait so commands depend on the
/// behavior, not the storage medium, and tests use [`MemoryCredentialStore`].
pub trait CredentialStore {
    /// The stored credentials, or `Ok(None)` when the device isn't paired yet.
    fn load(&self) -> Result<Option<Credentials>, CredentialError>;
    /// Persist `creds`, replacing any prior pairing.
    fn save(&self, creds: &Credentials) -> Result<(), CredentialError>;
    /// Forget the pairing (`logout`). A no-op when nothing is stored.
    fn clear(&self) -> Result<(), CredentialError>;
}

/// An in-memory store for tests and ephemeral flows — never touches disk.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    inner: Mutex<Option<Credentials>>,
}

impl MemoryCredentialStore {
    /// An empty (unpaired) store.
    pub fn new() -> Self {
        Self::default()
    }

    /// A store pre-seeded with `creds` (a device that's already paired).
    pub fn with(creds: Credentials) -> Self {
        Self {
            inner: Mutex::new(Some(creds)),
        }
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        Ok(self.inner.lock().unwrap().clone())
    }

    fn save(&self, creds: &Credentials) -> Result<(), CredentialError> {
        *self.inner.lock().unwrap() = Some(creds.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), CredentialError> {
        *self.inner.lock().unwrap() = None;
        Ok(())
    }
}

/// Credentials persisted to a JSON file, restricted to the owning user.
///
/// Defaults to `~/.promptly/credentials.json` (sharing the daemon's data dir, so
/// `PROMPTLY_DATA_DIR`/`PROMPTLY_HOME` redirect it in tests). Writes are atomic
/// (temp file + rename) and owner-only (`0600`) on Unix.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// A store backed by `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default location: `~/.promptly/credentials.json`, reusing the daemon's
    /// data-dir resolution (env-overridable, same as the checkpoint/lock files).
    pub fn default_store() -> Self {
        Self::new(promptlyd::paths::data_dir().join("credentials.json"))
    }

    /// The backing file path (for diagnostics and tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            // Not-yet-paired is the common case, not an error.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let creds = serde_json::from_slice(&bytes)
            .map_err(|err| CredentialError::Corrupt(err.to_string()))?;
        Ok(Some(creds))
    }

    fn save(&self, creds: &Credentials) -> Result<(), CredentialError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(creds).map_err(io::Error::other)?;
        // Write to a sibling temp file (owner-only from birth on Unix), then rename
        // over the target: the move is atomic and carries the tight permissions, so
        // there's never a window where the secrets sit world-readable.
        let tmp = self.path.with_extension("json.tmp");
        write_owner_only(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn clear(&self) -> Result<(), CredentialError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

/// Create `path` fresh and owner-readable-only, then write `bytes`. On Unix the
/// file is born `0600` (a stale temp from a crashed write is discarded first so
/// `create_new` always gets a clean inode). Off Unix there's no portable `chmod`,
/// so we rely on the user-profile ACL Windows applies to `%USERPROFILE%`.
fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    // Discard any leftover temp from a prior interrupted write so `create_new`
    // (which fails on an existing file) starts from a guaranteed-fresh inode.
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

    fn sample() -> Credentials {
        Credentials {
            device_token: "dev-token-abc".into(),
            signing_seed_b64: "AQIDBA==".into(),
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("promptly-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{label}.json"))
    }

    #[test]
    fn memory_store_round_trips_and_clears() {
        let store = MemoryCredentialStore::new();
        assert_eq!(store.load().unwrap(), None, "starts unpaired");
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), Some(sample()));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None, "clear forgets the pairing");
    }

    #[test]
    fn memory_store_seeded_with_credentials_loads_them() {
        let store = MemoryCredentialStore::with(sample());
        assert_eq!(store.load().unwrap(), Some(sample()));
    }

    #[test]
    fn file_store_round_trips_through_disk() {
        let store = FileCredentialStore::new(temp_path("roundtrip"));
        assert_eq!(store.load().unwrap(), None, "missing file -> unpaired");
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), Some(sample()));
        // Overwrite (re-pair) replaces the prior credentials.
        let next = Credentials {
            device_token: "dev-token-2".into(),
            signing_seed_b64: "BQYHCA==".into(),
        };
        store.save(&next).unwrap();
        assert_eq!(store.load().unwrap(), Some(next));
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        std::fs::remove_file(store.path()).ok();
    }

    #[test]
    fn file_store_clear_is_idempotent_when_absent() {
        let store = FileCredentialStore::new(temp_path("absent"));
        std::fs::remove_file(store.path()).ok();
        store.clear().unwrap();
        store.clear().unwrap();
    }

    #[test]
    fn file_store_reports_corruption_rather_than_silent_unpaired() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{not json").unwrap();
        let store = FileCredentialStore::new(&path);
        let err = store.load().expect_err("corrupt file is surfaced");
        assert!(matches!(err, CredentialError::Corrupt(_)));
        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let store = FileCredentialStore::new(temp_path("perms"));
        store.save(&sample()).unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "credential file must be owner-only");
        std::fs::remove_file(store.path()).ok();
    }
}
