//! Pacing plausibility (`25`).
//!
//! A human-driven harness session can't pack an unbounded number of turns into a
//! moment. This module flags a burst of turns tighter than any interactive session
//! — the fingerprint of a scripted or fabricated capture. It inspects only the turn
//! timestamps (already captured), returns human-readable reasons, and is
//! deliberately **generous** — a real session must never trip it.
//!
//! It intentionally does **not** flag backwards-moving timestamps. Chain/arrival
//! order is not timestamp order: a parallel harness call (Claude Code fires a
//! title/quota request early) keeps its early start time but is delivered after
//! later turns finish, and OTLP exporters batch, so an honest capture legitimately
//! regresses without bound. A forger controls turn order anyway (they would simply
//! sort before signing), so a monotonicity check caught honest parallelism far more
//! often than fabrication. The authoritative timing check is server-side over the
//! *signed* window bounds (`25`); this pure analyzer is the local fail-closed early
//! warning the submit gate (`19`) reads before anything is uploaded.

use crate::model::NormalizedTurn;

/// More turns than this inside [`BURST_WINDOW_MS`] is tighter than any interactive
/// session — the fingerprint of a scripted or fabricated burst.
const MAX_TURNS_PER_WINDOW: usize = 30;
/// The sliding window the burst check counts turns within.
const BURST_WINDOW_MS: i64 = 10_000;

/// Reasons the turn sequence is implausibly paced (empty = plausible). Generous by
/// construction; a real interactive session returns `[]`.
pub fn pacing_reasons(turns: &[NormalizedTurn]) -> Vec<String> {
    let mut reasons = Vec::new();

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
    fn arrival_order_timestamp_regressions_are_tolerated() {
        // A parallel harness call keeps its early start time but is delivered last,
        // so it lands far below the max already seen. Honest — never flagged (chain
        // order is arrival order, not timestamp order); the burst check is the only
        // signal here.
        let turns = vec![turn_at(0), turn_at(300_000), turn_at(20_000)];
        assert!(pacing_reasons(&turns).is_empty(), "{turns:?}");
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
