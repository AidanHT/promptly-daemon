//! Pacing plausibility (`25`).
//!
//! A human-driven harness session produces turns spread over real seconds. This
//! module flags sequences that couldn't have: timestamps that jump backwards, or a
//! burst of turns tighter than any interactive session. It inspects only the turn
//! timestamps (already captured), returns human-readable reasons, and is
//! deliberately **generous** — a real session must never trip it; it exists to
//! catch a fabricated or replayed capture (e.g. a forged chain with invented
//! timestamps, or turns spliced from another run).
//!
//! The authoritative pacing check runs server-side over the *signed* timestamps
//! (`25`); this pure analyzer is the local fail-closed early warning the submit gate
//! (`19`) reads before anything is uploaded, so an obviously-rigged capture is
//! surfaced to the player (and blocked without `--force`) rather than shipped.

use crate::model::NormalizedTurn;

/// A timestamp may sit at most this far below the max already seen before it reads
/// as a genuine backwards jump rather than benign reordering (out-of-order delivery
/// across the two telemetry sources, or minor clock skew, is normal within a few
/// seconds).
const REGRESSION_SLACK_MS: i64 = 5_000;
/// More turns than this inside [`BURST_WINDOW_MS`] is tighter than any interactive
/// session — the fingerprint of a scripted or fabricated burst.
const MAX_TURNS_PER_WINDOW: usize = 30;
/// The sliding window the burst check counts turns within.
const BURST_WINDOW_MS: i64 = 10_000;

/// Reasons the turn sequence is implausibly paced (empty = plausible). Generous by
/// construction; a real interactive session returns `[]`.
pub fn pacing_reasons(turns: &[NormalizedTurn]) -> Vec<String> {
    let mut reasons = Vec::new();

    // Timestamps: count hard backwards jumps beyond the reordering slack. A genuine
    // session only moves forward; a spliced/forged one can regress.
    let mut max_seen = i64::MIN;
    let mut regressions = 0usize;
    for turn in turns {
        if max_seen != i64::MIN && turn.timestamp_ms < max_seen - REGRESSION_SLACK_MS {
            regressions += 1;
        }
        max_seen = max_seen.max(turn.timestamp_ms);
    }
    if regressions > 0 {
        reasons.push(format!("{regressions} turn timestamp(s) jump backwards"));
    }

    // Burst: the tightest window of turns. Timestamps can arrive out of order, so
    // count over a sorted copy (a two-pointer sweep).
    let mut stamps: Vec<i64> = turns.iter().map(|t| t.timestamp_ms).collect();
    stamps.sort_unstable();
    let mut start = 0usize;
    let mut worst = 0usize;
    for end in 0..stamps.len() {
        while stamps[end] - stamps[start] > BURST_WINDOW_MS {
            start += 1;
        }
        worst = worst.max(end - start + 1);
    }
    if worst > MAX_TURNS_PER_WINDOW {
        reasons.push(format!(
            "{worst} turns within {}s — tighter than an interactive session",
            BURST_WINDOW_MS / 1000
        ));
    }

    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agreement, Confidence, Plausibility, Source};

    fn turn_at(ts: i64) -> NormalizedTurn {
        NormalizedTurn {
            schema_version: 1,
            turn_id: format!("t{ts}"),
            model: "claude-opus-4-8".into(),
            harness: "claude_code_cli".into(),
            tokens_input: 100,
            tokens_output: 50,
            tokens_thinking: 0,
            tokens_cache: 0,
            prompt_id: None,
            timestamp_ms: ts,
            confidence: Confidence::Otel,
            cost_usd: None,
            duration_ms: None,
            sources: vec![Source::Otel],
            session_id: Some("s".into()),
            attempt_nonce: Some("n".into()),
            workspace: None,
            agreement: Agreement::Single,
            plausibility: Plausibility::Plausible,
        }
    }

    #[test]
    fn a_normal_forward_session_is_plausible() {
        // Ten turns, ~30s apart — an ordinary interactive session.
        let turns: Vec<NormalizedTurn> = (0..10).map(|i| turn_at(i * 30_000)).collect();
        assert!(pacing_reasons(&turns).is_empty());
    }

    #[test]
    fn small_reordering_within_the_slack_is_tolerated() {
        // A later turn lands 3s before its predecessor (source reordering) — under
        // the 5s slack, so not flagged.
        let turns = vec![turn_at(10_000), turn_at(7_000)];
        assert!(pacing_reasons(&turns).is_empty());
    }

    #[test]
    fn a_hard_backwards_jump_is_flagged() {
        // A turn 20s before the max seen — a spliced/forged timestamp.
        let turns = vec![turn_at(0), turn_at(60_000), turn_at(40_000)];
        let reasons = pacing_reasons(&turns);
        assert!(
            reasons.iter().any(|r| r.contains("backwards")),
            "{reasons:?}"
        );
    }

    #[test]
    fn an_impossible_burst_is_flagged() {
        // 40 turns packed into ~4s — tighter than any human session.
        let turns: Vec<NormalizedTurn> = (0..40).map(|i| turn_at(i * 100)).collect();
        let reasons = pacing_reasons(&turns);
        assert!(reasons.iter().any(|r| r.contains("tighter")), "{reasons:?}");
    }

    #[test]
    fn an_empty_capture_is_plausible() {
        assert!(pacing_reasons(&[]).is_empty());
    }
}
