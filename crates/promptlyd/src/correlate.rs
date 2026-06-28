//! Cross-source correlation.
//!
//! When the OTLP receiver and the JSONL watcher both observe the same turn we
//! must not double-count it — but we also do more than de-duplicate: we
//! *compare* the two independent local records. Agreement is a strong honesty
//! signal; a disagreement (JSONL reporting a different model or far more tokens
//! than OTEL) is a tampering signal. Either way the OTEL values are authoritative
//! — JSONL never silently overrides them.
//!
//! The matcher and merge are pure; the [`Correlator`] buffers turns for a short
//! window so a counterpart can arrive, but takes "now" as an argument so the
//! buffering is deterministic under test (no wall clock).

use crate::model::{Agreement, NormalizedTurn, RawTurn, Source};
use crate::normalize::{assess_plausibility, confidence_for, normalize};

/// How close two independent observations must be to be treated as the same turn.
#[derive(Debug, Clone)]
pub struct Tolerance {
    /// Absolute token slack (covers rounding/cache accounting differences).
    pub token_abs: u64,
    /// Relative token slack as a fraction of the larger count.
    pub token_pct: f64,
    /// How far apart two observations of one turn may be timestamped, and how
    /// long a single-source turn waits for its counterpart before being emitted.
    pub window_ms: i64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            token_abs: 50,
            token_pct: 0.05,
            // Comfortably above the JSONL poll interval (so a counterpart still
            // merges) while keeping single-source turns near-live for the HUD.
            window_ms: 2_000,
        }
    }
}

impl Tolerance {
    fn tokens_close(&self, a: u64, b: u64) -> bool {
        let allowed = (self.token_pct * a.max(b) as f64).ceil() as u64;
        a.abs_diff(b) <= self.token_abs.max(allowed)
    }
}

/// Do two raw turns from *different* sources describe the same turn? Matched by
/// timing plus either an equal model or close token counts — so that a turn with
/// a single tampered field (only the model, or only the tokens) still correlates
/// and surfaces as a disagreement instead of being double-counted.
pub fn matches(a: &RawTurn, b: &RawTurn, tol: &Tolerance) -> bool {
    // Only the two Claude Code sources (OTEL + JSONL) ever describe one turn
    // twice; the reverse-engineered adapters (`21`) are each a lone source and
    // must never be merged with anything (and so are never double-counted away).
    if !a.source.correlates_with(b.source) {
        return false;
    }
    // Saturate the diff so a hostile far-future / `i64::MIN` timestamp can't
    // overflow the subtraction (debug-panic / release-wrap); normal in-window
    // inputs are unaffected.
    if a.timestamp_ms
        .saturating_sub(b.timestamp_ms)
        .saturating_abs()
        > tol.window_ms
    {
        return false;
    }
    let models_equal =
        matches!((a.resolved_model(), b.resolved_model()), (Some(x), Some(y)) if x == y);
    let tokens_close = tol.tokens_close(a.tokens_output, b.tokens_output)
        && tol.tokens_close(a.tokens_input, b.tokens_input);
    models_equal || tokens_close
}

/// Merge a correlated OTEL + JSONL pair into one normalized turn. OTEL is
/// authoritative for counts; JSONL supplies the thinking-token detail OTEL never
/// breaks out. The `agreement` marker records whether they actually concur.
pub fn merge(otel: &RawTurn, jsonl: &RawTurn, tol: &Tolerance) -> NormalizedTurn {
    debug_assert_eq!(otel.source, Source::Otel);
    debug_assert_eq!(jsonl.source, Source::Jsonl);

    let resolved = otel.resolved_model().or_else(|| jsonl.resolved_model());
    let mut turn = normalize(otel);
    turn.model = resolved.unwrap_or("").to_string();
    // A correlated pair is always two real-count Claude Code sources.
    turn.confidence = confidence_for(Source::Otel, resolved.is_some(), false);
    // OTEL bills thinking inside output and never breaks it out; take JSONL's.
    turn.tokens_thinking = jsonl.tokens_thinking;
    turn.prompt_id = otel.prompt_id.clone().or_else(|| jsonl.prompt_id.clone());
    turn.cost_usd = otel.cost_usd.or(jsonl.cost_usd);
    turn.duration_ms = otel.duration_ms.or(jsonl.duration_ms);
    turn.sources = vec![Source::Otel, Source::Jsonl];
    turn.session_id = jsonl.session_id.clone().or_else(|| otel.session_id.clone());
    turn.workspace = jsonl.workspace.clone().or_else(|| otel.workspace.clone());
    turn.agreement = compare(otel, jsonl, tol);
    turn.plausibility = assess_plausibility(otel);
    turn
}

/// Compare a correlated pair field-by-field, listing what disagrees.
fn compare(otel: &RawTurn, jsonl: &RawTurn, tol: &Tolerance) -> Agreement {
    let mut fields = Vec::new();
    if let (Some(mo), Some(mj)) = (otel.resolved_model(), jsonl.resolved_model()) {
        if mo != mj {
            fields.push("model".to_string());
        }
    }
    if !tol.tokens_close(otel.tokens_output, jsonl.tokens_output) {
        fields.push("tokens_output".to_string());
    }
    if !tol.tokens_close(otel.tokens_input, jsonl.tokens_input) {
        fields.push("tokens_input".to_string());
    }
    if fields.is_empty() {
        Agreement::Agree
    } else {
        Agreement::Disagree { fields }
    }
}

/// Buffers single-source turns briefly so a counterpart from the other source can
/// arrive and be merged, then emits whatever is left as single-source.
pub struct Correlator {
    tol: Tolerance,
    pending: Vec<Pending>,
}

struct Pending {
    raw: RawTurn,
    received_ms: i64,
}

impl Correlator {
    pub fn new(tol: Tolerance) -> Self {
        Self {
            tol,
            pending: Vec::new(),
        }
    }

    /// Ingest a raw turn observed at logical `now_ms`. Returns a merged turn when
    /// it pairs with a buffered counterpart; otherwise buffers it and returns
    /// `None` (it will be flushed as single-source if no counterpart arrives).
    pub fn ingest(&mut self, raw: RawTurn, now_ms: i64) -> Option<NormalizedTurn> {
        if let Some(idx) = self
            .pending
            .iter()
            .position(|p| matches(&p.raw, &raw, &self.tol))
        {
            let other = self.pending.remove(idx).raw;
            // `matches` guarantees an OTEL/JSONL pair, so `raw` and `other` are
            // opposite Claude Code sources; order them OTEL-first for `merge`.
            let merged = if raw.source == Source::Otel {
                merge(&raw, &other, &self.tol)
            } else {
                merge(&other, &raw, &self.tol)
            };
            return Some(merged);
        }
        self.pending.push(Pending {
            raw,
            received_ms: now_ms,
        });
        None
    }

    /// Emit (as single-source turns) every buffered turn whose correlation window
    /// has elapsed without a counterpart.
    pub fn flush_expired(&mut self, now_ms: i64) -> Vec<NormalizedTurn> {
        let window = self.tol.window_ms;
        let (expired, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| now_ms - p.received_ms >= window);
        self.pending = keep;
        expired.iter().map(|p| normalize(&p.raw)).collect()
    }

    /// Emit all buffered turns immediately (used at shutdown so nothing is lost).
    pub fn drain(&mut self) -> Vec<NormalizedTurn> {
        std::mem::take(&mut self.pending)
            .iter()
            .map(|p| normalize(&p.raw))
            .collect()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{sample_raw, Confidence};

    fn at(source: Source, model: Option<&str>, in_: u64, out: u64, ts: i64) -> RawTurn {
        let mut r = sample_raw(source, model, in_, out);
        r.timestamp_ms = ts;
        r
    }

    #[test]
    fn agreeing_pair_merges_to_one_turn() {
        let mut c = Correlator::new(Tolerance::default());
        let mut jsonl = at(Source::Jsonl, Some("claude-opus-4-8"), 100, 50, 1_000);
        jsonl.tokens_thinking = 30;

        assert!(c
            .ingest(at(Source::Otel, Some("claude-opus-4-8"), 100, 50, 1_000), 0)
            .is_none());
        let merged = c.ingest(jsonl, 100).expect("counterpart merges");

        assert_eq!(merged.agreement, Agreement::Agree);
        assert_eq!(merged.sources, vec![Source::Otel, Source::Jsonl]);
        assert_eq!(merged.confidence, Confidence::Otel);
        assert_eq!(merged.tokens_thinking, 30, "thinking comes from JSONL");
        assert_eq!(c.pending_len(), 0, "no double-counting");
    }

    #[test]
    fn order_independent_jsonl_first() {
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 10, 20, 500), 0)
            .is_none());
        let merged = c
            .ingest(at(Source::Otel, Some("m"), 10, 20, 500), 50)
            .expect("merges regardless of arrival order");
        assert_eq!(merged.agreement, Agreement::Agree);
        assert_eq!(merged.sources, vec![Source::Otel, Source::Jsonl]);
    }

    #[test]
    fn edited_jsonl_token_count_disagrees_without_overriding_otel() {
        let mut c = Correlator::new(Tolerance::default());
        // Same model + timing, but the JSONL output count was inflated.
        c.ingest(at(Source::Otel, Some("m"), 100, 50, 1_000), 0);
        let merged = c
            .ingest(at(Source::Jsonl, Some("m"), 100, 9_000, 1_000), 10)
            .expect("still correlates via the matching model");

        match &merged.agreement {
            Agreement::Disagree { fields } => {
                assert!(fields.contains(&"tokens_output".to_string()))
            }
            other => panic!("expected disagreement, got {other:?}"),
        }
        assert_eq!(
            merged.tokens_output, 50,
            "OTEL value is kept, not overridden"
        );
    }

    #[test]
    fn edited_jsonl_model_disagrees() {
        let mut c = Correlator::new(Tolerance::default());
        // Only the model differs; identical tokens still correlate the pair.
        c.ingest(at(Source::Otel, Some("claude-opus-4-8"), 100, 50, 1_000), 0);
        let merged = c
            .ingest(
                at(Source::Jsonl, Some("claude-haiku-4-5"), 100, 50, 1_000),
                10,
            )
            .expect("correlates via close tokens");

        match &merged.agreement {
            Agreement::Disagree { fields } => assert!(fields.contains(&"model".to_string())),
            other => panic!("expected disagreement, got {other:?}"),
        }
        assert_eq!(
            merged.model, "claude-opus-4-8",
            "OTEL model is authoritative"
        );
    }

    #[test]
    fn unmatched_turn_flushes_as_single_source_after_window() {
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Otel, Some("m"), 5, 5, 1_000), 0)
            .is_none());

        // Before the window elapses: nothing flushes.
        assert!(c.flush_expired(1_999).is_empty());
        assert_eq!(c.pending_len(), 1);

        // After the window: emitted as a lone OTEL turn.
        let flushed = c.flush_expired(2_000);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].agreement, Agreement::Single);
        assert_eq!(flushed[0].sources, vec![Source::Otel]);
        assert_eq!(c.pending_len(), 0);
    }

    #[test]
    fn an_adapter_turn_never_merges_with_a_claude_turn() {
        let tol = Tolerance::default();
        // Same model, identical tokens, same instant — but one is a Cursor turn,
        // which is a lone source and must not be folded into the OTEL turn.
        let otel = at(Source::Otel, Some("claude-opus-4-8"), 100, 50, 1_000);
        let cursor = at(Source::Cursor, Some("claude-opus-4-8"), 100, 50, 1_000);
        assert!(!matches(&otel, &cursor, &tol));

        let mut c = Correlator::new(tol);
        assert!(
            c.ingest(otel, 0).is_none(),
            "buffered for a JSONL counterpart"
        );
        assert!(
            c.ingest(cursor, 1).is_none(),
            "the Cursor turn buffers separately, not merged"
        );
        // Both flush as their own single-source turns.
        let flushed = c.flush_expired(5_000);
        assert_eq!(flushed.len(), 2);
        assert!(flushed.iter().all(|t| t.agreement == Agreement::Single));
    }

    #[test]
    fn distinct_turns_far_apart_do_not_merge() {
        let tol = Tolerance::default();
        let a = at(Source::Otel, Some("m"), 10, 10, 0);
        let b = at(Source::Jsonl, Some("m"), 10, 10, 60_000);
        assert!(!matches(&a, &b, &tol), "outside the window");
    }

    #[test]
    fn hostile_extreme_timestamps_do_not_overflow() {
        let tol = Tolerance::default();
        // A hostile i64::MIN / i64::MAX pair would overflow a plain subtraction;
        // the saturating diff must not panic and must read as out-of-window.
        let a = at(Source::Otel, Some("m"), 10, 10, i64::MAX);
        let b = at(Source::Jsonl, Some("m"), 10, 10, i64::MIN);
        assert!(!matches(&a, &b, &tol), "far apart, no panic");
        assert!(!matches(&b, &a, &tol), "symmetric, no panic");
    }
}
