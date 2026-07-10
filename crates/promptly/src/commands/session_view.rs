//! Shared context lines for `watch`/`score` so their numbers are trustworthy at
//! a glance: a session-age header (a days-old resumed capture is obvious), a
//! workspace/level-mismatch warning (a folder bound to a different level can't
//! silently show another attempt's numbers), and a dim cache-token note.
//!
//! Each helper returns a `String`/`Option<String>` — no I/O — so the command
//! renderers stay unit-testable.

use promptlyd::manifest::Manifest;

use crate::daemon_client::SessionMarker;
use crate::fmt;
use crate::style::Style;

/// The dim context line under a `watch`/`score` header: how long ago the session
/// started (so a days-old capture is obvious before its frozen numbers) and its
/// bound slug. Marked `(resumed)` when the seed snapshot already carried turns —
/// i.e. the command joined a session already in progress rather than from turn 0.
pub fn age_line(marker: &SessionMarker, now_ms: i64, resumed: bool, style: Style) -> String {
    let age = fmt::relative_age(now_ms.saturating_sub(marker.started_at_ms));
    let resumed = if resumed { " (resumed)" } else { "" };
    style.dim(&format!(
        "session started {age} ago{resumed} · {}",
        marker.slug
    ))
}

/// A yellow warning when the cwd manifest binds a *different* level than the
/// session marker — the case where `watch`/`score` would otherwise show frozen
/// numbers from another attempt with no hint they came from elsewhere. `None`
/// when there's no manifest, its slug is blank, or the slugs agree.
pub fn mismatch_warning(
    marker: &SessionMarker,
    manifest: Option<&Manifest>,
    style: Style,
) -> Option<String> {
    let manifest = manifest?;
    if manifest.slug.is_empty() || manifest.slug == marker.slug {
        return None;
    }
    Some(style.yellow(&format!(
        "⚠ session is bound to {}, but this folder is {} — run 'promptly start' here",
        marker.slug, manifest.slug,
    )))
}

/// A dim ` · cache <n>` suffix for the token lines, shown only when the run used
/// cache tokens (which usually dominate a real Claude Code run). Empty otherwise,
/// so explicit-mode scoring — which has no cache concept — stays uncluttered.
pub fn cache_note(cache: u64, style: Style) -> String {
    if cache == 0 {
        return String::new();
    }
    format!(
        " {}",
        style.dim(&format!("· cache {}", fmt::thousands(cache as u128))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn marker(slug: &str, started_at_ms: i64) -> SessionMarker {
        SessionMarker {
            version: 1,
            session_id: "s1".into(),
            workspace: PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: slug.into(),
            started_at_ms,
            stopped_at_ms: None,
            attempt_nonce: "n".into(),
            nonce_origin: promptlyd::scoping::NonceOrigin::Local,
            file_allowlist: vec![],
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    fn manifest(slug: &str) -> Manifest {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "level_id": "lvl-1",
            "slug": slug,
            "baseline_hash": "abc",
        }))
        .expect("test manifest deserializes")
    }

    #[test]
    fn age_line_reports_the_span_and_slug() {
        let m = marker("stage-1-01", 0);
        let line = age_line(&m, 4 * 86_400_000, false, Style::plain());
        assert!(line.contains("session started 4d ago"));
        assert!(line.contains("stage-1-01"));
        assert!(!line.contains("(resumed)"));
    }

    #[test]
    fn age_line_flags_a_resumed_session() {
        let m = marker("stage-1-01", 0);
        let line = age_line(&m, 3_600_000, true, Style::plain());
        assert!(line.contains("started 1h ago (resumed)"));
    }

    #[test]
    fn mismatch_warns_only_when_the_slugs_differ() {
        let m = marker("stage-1-01", 0);
        // Same slug → no warning.
        assert!(mismatch_warning(&m, Some(&manifest("stage-1-01")), Style::plain()).is_none());
        // No manifest → no warning.
        assert!(mismatch_warning(&m, None, Style::plain()).is_none());
        // Blank manifest slug → no warning (can't name the folder's level).
        assert!(mismatch_warning(&m, Some(&manifest("")), Style::plain()).is_none());
        // Different slug → the warning names both levels.
        let warn = mismatch_warning(&m, Some(&manifest("stage-1-02")), Style::plain())
            .expect("mismatch warns");
        assert!(warn.contains("bound to stage-1-01"));
        assert!(warn.contains("this folder is stage-1-02"));
        assert!(warn.contains("promptly start"));
    }

    #[test]
    fn cache_note_shows_only_when_nonzero() {
        assert_eq!(cache_note(0, Style::plain()), "");
        let note = cache_note(12_345, Style::plain());
        assert!(note.contains("cache 12,345"));
    }
}
