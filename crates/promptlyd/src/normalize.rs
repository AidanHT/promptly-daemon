//! Single-source normalization: one [`RawTurn`] becomes one [`NormalizedTurn`].
//!
//! Correlation across sources (the agreement signal, de-duplication) lives in
//! `crate::correlate`, which reuses the helpers here for the single-source case.

use crate::model::{
    Agreement, Confidence, NormalizedTurn, Plausibility, RawTurn, Source, TURN_SCHEMA_VERSION,
};

/// Normalize a single observed turn (no counterpart from the other source).
pub fn normalize(raw: &RawTurn) -> NormalizedTurn {
    let resolved = raw.resolved_model();
    NormalizedTurn {
        schema_version: TURN_SCHEMA_VERSION,
        turn_id: raw.content_id(),
        model: resolved.unwrap_or("").to_string(),
        harness: raw.harness.clone(),
        tokens_input: raw.tokens_input,
        tokens_output: raw.tokens_output,
        tokens_thinking: raw.tokens_thinking,
        tokens_cache: raw.tokens_cache,
        prompt_id: raw.prompt_id.clone(),
        timestamp_ms: raw.timestamp_ms,
        confidence: confidence_for(raw.source, resolved.is_some(), raw.counts_estimated),
        cost_usd: raw.cost_usd,
        duration_ms: raw.duration_ms,
        sources: vec![raw.source],
        session_id: raw.session_id.clone(),
        attempt_nonce: None,
        workspace: raw.workspace.clone(),
        agreement: Agreement::Single,
        plausibility: assess_plausibility(raw),
    }
}

/// The confidence for a source, downgraded to `Estimated` when the underlying
/// model could not be resolved or the token counts were inferred rather than
/// reported (`17`/`21`: such a turn scores against the baseline floor tier
/// downstream). `otel` is reserved for Claude Code's native OpenTelemetry; the
/// reverse-engineered adapters report at most `jsonl`-grade (real reported
/// counts) and otherwise `estimated`.
pub fn confidence_for(source: Source, model_resolved: bool, counts_estimated: bool) -> Confidence {
    if !model_resolved || counts_estimated {
        return Confidence::Estimated;
    }
    match source {
        Source::Otel => Confidence::Otel,
        Source::Jsonl | Source::Cursor | Source::Codex | Source::Copilot => Confidence::Jsonl,
    }
}

/// Flag implausible turns for the server's integrity weighing (`25`). The turn
/// is never dropped locally — only annotated.
pub fn assess_plausibility(raw: &RawTurn) -> Plausibility {
    let mut reasons = Vec::new();
    let total = raw
        .tokens_input
        .saturating_add(raw.tokens_output)
        .saturating_add(raw.tokens_thinking)
        .saturating_add(raw.tokens_cache);
    if total == 0 {
        reasons.push("zero tokens reported".to_string());
    } else {
        if raw.duration_ms == Some(0) {
            reasons.push("nonzero tokens with zero duration".to_string());
        }
        if matches!(raw.cost_usd, Some(c) if c.abs() < f64::EPSILON) {
            reasons.push("nonzero tokens with zero cost".to_string());
        }
    }
    if reasons.is_empty() {
        Plausibility::Plausible
    } else {
        Plausibility::Low { reasons }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_raw;

    #[test]
    fn otel_turn_with_model_is_high_confidence_and_single() {
        let turn = normalize(&sample_raw(Source::Otel, Some("claude-opus-4-8"), 120, 40));
        assert_eq!(turn.confidence, Confidence::Otel);
        assert_eq!(turn.model, "claude-opus-4-8");
        assert_eq!(turn.sources, vec![Source::Otel]);
        assert_eq!(turn.agreement, Agreement::Single);
        assert_eq!(turn.plausibility, Plausibility::Plausible);
    }

    #[test]
    fn jsonl_turn_carries_thinking_tokens() {
        let mut raw = sample_raw(Source::Jsonl, Some("claude-sonnet-4-6"), 80, 200);
        raw.tokens_thinking = 64;
        let turn = normalize(&raw);
        assert_eq!(turn.confidence, Confidence::Jsonl);
        assert_eq!(turn.tokens_thinking, 64);
    }

    #[test]
    fn unresolved_model_downgrades_to_estimated() {
        let turn = normalize(&sample_raw(Source::Otel, None, 10, 10));
        assert_eq!(turn.confidence, Confidence::Estimated);
        assert_eq!(turn.model, "");
    }

    #[test]
    fn adapter_sources_report_jsonl_grade_when_counts_are_real() {
        for source in [Source::Cursor, Source::Codex, Source::Copilot] {
            let turn = normalize(&sample_raw(source, Some("claude-opus-4-8"), 80, 120));
            assert_eq!(turn.confidence, Confidence::Jsonl, "{source:?}");
            assert_eq!(turn.sources, vec![source]);
        }
    }

    #[test]
    fn inferred_counts_downgrade_to_estimated_even_with_a_model() {
        // A Cursor zero-token bubble we estimated via char/4: model resolved, but
        // the counts are inferred, so it can't claim reported-grade confidence.
        let mut raw = sample_raw(Source::Cursor, Some("claude-opus-4-8"), 0, 200);
        raw.counts_estimated = true;
        assert_eq!(normalize(&raw).confidence, Confidence::Estimated);
    }

    #[test]
    fn plausibility_flags_zero_and_inconsistent_turns() {
        // Zero tokens.
        assert!(matches!(
            assess_plausibility(&sample_raw(Source::Otel, Some("m"), 0, 0)),
            Plausibility::Low { .. }
        ));

        // Tokens but zero duration.
        let mut zero_dur = sample_raw(Source::Otel, Some("m"), 100, 100);
        zero_dur.duration_ms = Some(0);
        assert!(matches!(
            assess_plausibility(&zero_dur),
            Plausibility::Low { .. }
        ));

        // Tokens with a real duration: plausible.
        let mut ok = sample_raw(Source::Otel, Some("m"), 100, 100);
        ok.duration_ms = Some(1_200);
        ok.cost_usd = Some(0.03);
        assert_eq!(assess_plausibility(&ok), Plausibility::Plausible);
    }
}
