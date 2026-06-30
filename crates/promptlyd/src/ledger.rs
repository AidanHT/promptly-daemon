//! The tamper-evident capture ledger.
//!
//! A deterministic hash chain over the session's attributed turns. Each link
//! commits to more than the token counts: it folds in the *integrity signals* the
//! live pipeline derives — the contributing sources, the cross-source agreement,
//! and the plausibility verdict — so the ordered, complete capture can't be
//! silently truncated, reordered, or edited without changing the head.
//!
//! It is a pure function of `(session identity, turns)`. The daemon recomputes it
//! to seal the crash checkpoint and rejects a checkpoint whose turns no longer
//! match its seal — catching an offline edit of the persisted capture. The same
//! head is recomputed by the CLI and re-verified server-side at submit (`20`/`25`),
//! where it gains real teeth: there it is signed and checked against an authority
//! the player doesn't control, so a fork that fabricates turns has to fabricate a
//! fully self-consistent chain that also survives the server's re-derivation.
//!
//! The canonical encoding is explicit — a fixed field order, unit-separated —
//! rather than struct-serialization order, so a second implementation (the
//! server's TypeScript) can reproduce the same bytes.

use sha2::{Digest, Sha256};

use crate::model::{Agreement, Confidence, NormalizedTurn, Plausibility, Source};

/// Bump when the canonical per-turn encoding or the chaining changes; both the
/// daemon's seal and the server's recomputation key on it.
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

/// Domain-separation prefix so a ledger hash can never collide with another
/// SHA-256 use in the system.
const LEDGER_DOMAIN: &str = "promptly-ledger:v1";

/// Field separator: ASCII Unit Separator. It never appears in an encoded field —
/// ids/models/nonces are printable text, the rest are decimal numbers or fixed
/// lowercase tags — so the encoding is unambiguous.
const SEP: &str = "\u{1f}";

/// The head of the capture ledger: the chain hash over `turn_count` turns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LedgerHead {
    /// The canonical-encoding version this head was computed under.
    pub schema: u32,
    /// How many turns the chain covers.
    pub turn_count: usize,
    /// Lowercase-hex SHA-256 chain head (64 chars).
    pub head: String,
}

/// Compute the ledger head over `turns` for a session. The genesis binds the
/// session identity (so two sessions with identical turns still differ); each
/// subsequent link folds in one turn's canonical encoding. Pure and deterministic.
pub fn compute_head(session_id: &str, started_at_ms: i64, turns: &[NormalizedTurn]) -> LedgerHead {
    let mut prev = sha256_hex(
        format!("{LEDGER_DOMAIN}{SEP}genesis{SEP}{session_id}{SEP}{started_at_ms}").as_bytes(),
    );
    for turn in turns {
        let link = format!("{prev}{SEP}{}", canonical_turn(turn));
        prev = sha256_hex(link.as_bytes());
    }
    LedgerHead {
        schema: LEDGER_SCHEMA_VERSION,
        turn_count: turns.len(),
        head: prev,
    }
}

/// The canonical, reproducible byte encoding of one turn for the chain. Explicit
/// field order; the integrity signals are included so the seal commits to them.
fn canonical_turn(t: &NormalizedTurn) -> String {
    [
        t.turn_id.clone(),
        t.timestamp_ms.to_string(),
        t.model.clone(),
        t.tokens_input.to_string(),
        t.tokens_output.to_string(),
        t.tokens_thinking.to_string(),
        t.tokens_cache.to_string(),
        confidence_tag(t.confidence).to_string(),
        agreement_tag(&t.agreement),
        plausibility_tag(&t.plausibility),
        sources_tag(&t.sources),
        t.attempt_nonce.clone().unwrap_or_default(),
    ]
    .join(SEP)
}

fn confidence_tag(c: Confidence) -> &'static str {
    match c {
        Confidence::Otel => "otel",
        Confidence::Jsonl => "jsonl",
        Confidence::Estimated => "estimated",
    }
}

fn agreement_tag(a: &Agreement) -> String {
    match a {
        Agreement::Single => "single".to_string(),
        Agreement::Agree => "agree".to_string(),
        Agreement::Disagree { fields } => format!("disagree:{}", fields.join(",")),
    }
}

fn plausibility_tag(p: &Plausibility) -> String {
    match p {
        Plausibility::Plausible => "plausible".to_string(),
        Plausibility::Low { reasons } => format!("low:{}", reasons.join(",")),
    }
}

fn sources_tag(sources: &[Source]) -> String {
    sources
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Lowercase-hex SHA-256 (the full 64-char digest — unlike `model::fingerprint`,
/// which truncates, the chain needs the whole digest).
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{sample_raw, Source};
    use crate::normalize::normalize;

    fn turn(model: &str, tokens_out: u64) -> NormalizedTurn {
        let mut t = normalize(&sample_raw(Source::Otel, Some(model), 100, tokens_out));
        t.attempt_nonce = Some("nonce-1".into());
        t
    }

    #[test]
    fn head_is_deterministic_and_64_hex() {
        let turns = vec![turn("claude-opus-4-8", 50), turn("claude-opus-4-8", 80)];
        let a = compute_head("sess-1", 1_000, &turns);
        let b = compute_head("sess-1", 1_000, &turns);
        assert_eq!(a, b, "pure function of its inputs");
        assert_eq!(a.head.len(), 64);
        assert!(a.head.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.turn_count, 2);
        assert_eq!(a.schema, LEDGER_SCHEMA_VERSION);
    }

    #[test]
    fn empty_capture_has_a_stable_genesis_head() {
        let a = compute_head("sess-1", 1_000, &[]);
        let b = compute_head("sess-1", 1_000, &[]);
        assert_eq!(a.head, b.head);
        assert_eq!(a.turn_count, 0);
        // A different session identity yields a different genesis.
        assert_ne!(compute_head("sess-2", 1_000, &[]).head, a.head);
        assert_ne!(compute_head("sess-1", 2_000, &[]).head, a.head);
    }

    #[test]
    fn editing_a_token_count_changes_the_head() {
        let base = vec![turn("claude-opus-4-8", 50)];
        let mut edited = base.clone();
        edited[0].tokens_output = 5; // the classic "report fewer tokens" tamper
        assert_ne!(
            compute_head("s", 0, &base).head,
            compute_head("s", 0, &edited).head,
        );
    }

    #[test]
    fn editing_an_integrity_signal_changes_the_head() {
        // The seal commits to the agreement verdict, not just the counts: flipping
        // a Disagree to Agree (to hide a tampering signal) changes the head.
        let mut base = vec![turn("claude-opus-4-8", 50)];
        base[0].agreement = Agreement::Disagree {
            fields: vec!["tokens_output".into()],
        };
        let mut hidden = base.clone();
        hidden[0].agreement = Agreement::Agree;
        assert_ne!(
            compute_head("s", 0, &base).head,
            compute_head("s", 0, &hidden).head,
        );
    }

    #[test]
    fn reordering_turns_changes_the_head() {
        let a = turn("claude-opus-4-8", 50);
        let b = turn("claude-sonnet-4-6", 80);
        assert_ne!(
            compute_head("s", 0, &[a.clone(), b.clone()]).head,
            compute_head("s", 0, &[b, a]).head,
        );
    }
}
