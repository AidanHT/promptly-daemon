//! The one place wall-clock time is read, so the rest of the daemon takes
//! timestamps as plain `i64` epoch millis and stays deterministic under test.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in milliseconds (0 if the clock is before the epoch).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
