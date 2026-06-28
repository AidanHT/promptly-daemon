//! The single-session mutex.
//!
//! At most one capture process may hold this at a time, so telemetry is never
//! ambiguously attributed. The mutex is an OS advisory lock on a file under the
//! data dir: acquiring it succeeds once; a second acquisition fails while the
//! first is held. The *level* binding and the attempt nonce live in the session
//! marker (`crate::scoping`); this just guarantees singularity.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use fs4::FileExt;

/// Holds the single-session lock for as long as it lives; releasing it (drop)
/// frees the lock so the next session can start.
#[derive(Debug)]
pub struct SessionGuard {
    _file: File,
}

impl SessionGuard {
    /// Acquire the session lock, or fail if another session already holds it.
    pub fn acquire(lock_path: &Path) -> io::Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        file.try_lock_exclusive().map_err(|e| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another promptlyd capture session is already active ({}): {e}",
                    lock_path.display()
                ),
            )
        })?;
        // Best-effort PID stamp for diagnostics; the lock, not the file, is the
        // source of truth.
        let _ = write!(file, "{}", std::process::id());
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lock_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("promptlyd-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{label}.lock"))
    }

    #[test]
    fn only_one_session_can_hold_the_lock() {
        let path = lock_path("single");
        let first = SessionGuard::acquire(&path).expect("first session acquires");

        let second = SessionGuard::acquire(&path);
        assert!(second.is_err(), "a second session must be refused");
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::WouldBlock);

        // Releasing the first frees the lock for the next session.
        drop(first);
        let third = SessionGuard::acquire(&path);
        assert!(third.is_ok(), "lock is reusable once released");

        std::fs::remove_file(&path).ok();
    }
}
