//! Cross-source correlation.
//!
//! When the OTLP receiver and the JSONL watcher both observe the same turn we
//! must not double-count it — but we also do more than de-duplicate: we
//! *compare* the two independent local records. Agreement is a strong honesty
//! signal; a disagreement (JSONL reporting a different model or far more tokens
//! than OTEL) is a tampering signal. Either way the OTEL values are authoritative
//! — JSONL never silently overrides them.
//!
//! The matcher and merge are pure; the [`Correlator`] buffers turns so a
//! counterpart can arrive, but takes "now" as an argument so the buffering is
//! deterministic under test (no wall clock).
//!
//! Timing matters here: the JSONL watcher sees a turn within its 500 ms poll,
//! but Claude Code exports OTEL logs through the SDK's default *batch*
//! processor, so the matching `api_request` event typically lands 1-5 s later
//! (occasionally more). The correlator therefore holds an unmatched turn for a
//! generous [`PAIRING_HORIZON_MS`] **when an OTEL counterpart is expected**
//! (the session was bootstrapped with an ingest token) so the pair still merges
//! into one `[otel, jsonl]` turn — losing the merge silently downgraded whole
//! runs from `otel` to `jsonl` confidence (and the server's `verified` tier).
//! When no OTEL can arrive (JSONL-only session, idle) buffering would be pure
//! latency, so turns flush on the next engine tick instead.

use crate::model::{Agreement, NormalizedTurn, RawTurn, Source};
use crate::normalize::{assess_plausibility, confidence_for, normalize};

/// How long an unmatched turn stays eligible to merge with its cross-source
/// counterpart before being emitted single-source, when the other Claude Code
/// source is expected. OTEL's batch export usually trails the JSONL line by
/// 1-5 s; 20 s covers the slow tail with a wide margin while still bounding how
/// long a genuinely single-source turn can lag the live stream. Session stop
/// flushes the buffer immediately, so this horizon never delays a submit.
pub const PAIRING_HORIZON_MS: i64 = 20_000;

/// How long a flushed single-source turn is remembered so a straggler
/// counterpart that missed the horizon is *suppressed* (its counts already
/// emitted) instead of becoming a phantom standalone turn.
const RECENT_TTL_MS: i64 = 60_000;

/// How close two independent observations must be to be treated as the same turn.
#[derive(Debug, Clone)]
pub struct Tolerance {
    /// Absolute token slack (covers rounding/cache accounting differences).
    pub token_abs: u64,
    /// Relative token slack as a fraction of the larger count.
    pub token_pct: f64,
    /// How far apart two observations of one turn may be timestamped.
    pub window_ms: i64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            token_abs: 50,
            token_pct: 0.05,
            // A merged pair's timestamps can differ by the whole intra-message
            // block span (the retained JSONL timestamp is the FIRST block-line's;
            // the OTEL event is stamped at request end) plus the batch-export
            // delay — several seconds each. 15 s covers both comfortably; the
            // model-equality/token checks in `matches`, arrival order, and the
            // engine's event-id dedup keep distinct turns from cross-merging.
            window_ms: 15_000,
        }
    }
}

impl Tolerance {
    fn tokens_close(&self, a: u64, b: u64) -> bool {
        let allowed = (self.token_pct * a.max(b) as f64).ceil() as u64;
        a.abs_diff(b) <= self.token_abs.max(allowed)
    }
}

/// The non-content half of a match: could these two observations even describe
/// one turn? Only cross-source (OTEL↔JSONL) pairs within the timestamp window.
fn correlatable(a: &RawTurn, b: &RawTurn, tol: &Tolerance) -> bool {
    // Only the two Claude Code sources (OTEL + JSONL) ever describe one turn
    // twice; the reverse-engineered adapters (`21`) are each a lone source and
    // must never be merged with anything (and so are never double-counted away).
    if !a.source.correlates_with(b.source) {
        return false;
    }
    // Saturate the diff so a hostile far-future / `i64::MIN` timestamp can't
    // overflow the subtraction (debug-panic / release-wrap); normal in-window
    // inputs are unaffected.
    a.timestamp_ms
        .saturating_sub(b.timestamp_ms)
        .saturating_abs()
        <= tol.window_ms
}

fn models_equal(a: &RawTurn, b: &RawTurn) -> bool {
    matches!((a.resolved_model(), b.resolved_model()), (Some(x), Some(y)) if x == y)
}

fn tokens_agree(a: &RawTurn, b: &RawTurn, tol: &Tolerance) -> bool {
    tol.tokens_close(a.tokens_output, b.tokens_output) && input_close(a, b, tol)
}

/// Input counts agree either directly or once the cache accounting is netted in.
/// The two Claude Code sources split cache tokens differently on a cache-writing
/// turn: OTEL can fold cache-creation into `input_tokens` (cache field 0) while
/// the JSONL usage reports a tiny `input_tokens` beside a large
/// `cache_creation_input_tokens` — the same turn, read as e.g. in=531/cache=0 vs
/// in=8/cache=523. Comparing `input + cache` on both sides makes those agree
/// without loosening the tolerance on genuinely different counts.
fn input_close(a: &RawTurn, b: &RawTurn, tol: &Tolerance) -> bool {
    tol.tokens_close(a.tokens_input, b.tokens_input)
        || tol.tokens_close(
            a.tokens_input.saturating_add(a.tokens_cache),
            b.tokens_input.saturating_add(b.tokens_cache),
        )
}

/// Do two raw turns from *different* sources describe the same turn? Matched by
/// timing plus either an equal model or close token counts — so that a turn with
/// a single tampered field (only the model, or only the tokens) still correlates
/// and surfaces as a disagreement instead of being double-counted.
pub fn matches(a: &RawTurn, b: &RawTurn, tol: &Tolerance) -> bool {
    correlatable(a, b, tol) && (models_equal(a, b) || tokens_agree(a, b, tol))
}

/// How far apart two observations' counts (then timestamps) are — the sort key
/// picking the best counterpart among several correlatable candidates.
fn pair_distance(a: &RawTurn, b: &RawTurn) -> (u64, u64) {
    let tokens = a
        .tokens_output
        .abs_diff(b.tokens_output)
        .saturating_add(a.tokens_input.abs_diff(b.tokens_input));
    let time = a
        .timestamp_ms
        .saturating_sub(b.timestamp_ms)
        .saturating_abs() as u64;
    (tokens, time)
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
    // Cache-netted: a cache-writing turn's input splits differently across the
    // two sources (see `input_close`) — that accounting gap is not tampering.
    if !input_close(otel, jsonl, tol) {
        fields.push("tokens_input".to_string());
    }
    if fields.is_empty() {
        Agreement::Agree
    } else {
        Agreement::Disagree { fields }
    }
}

/// Buffers single-source turns so a counterpart from the other source can arrive
/// and be merged, then emits whatever is left as single-source — and remembers
/// what it just emitted so a straggler counterpart is suppressed, not recounted.
pub struct Correlator {
    tol: Tolerance,
    pending: Vec<Pending>,
    /// Recently-flushed single-source Claude Code turns. A counterpart that
    /// arrives *after* its turn already flushed (missed the pairing horizon —
    /// rare) matches here and is dropped: its token counts were already emitted,
    /// so counting it again would fabricate a turn. The already-emitted turn
    /// keeps its single-source confidence (the merge itself can't be replayed).
    recent: Vec<Pending>,
}

struct Pending {
    raw: RawTurn,
    /// Arrival time for `pending` entries; flush time for `recent` entries.
    received_ms: i64,
}

impl Correlator {
    pub fn new(tol: Tolerance) -> Self {
        Self {
            tol,
            pending: Vec::new(),
            recent: Vec::new(),
        }
    }

    /// The buffered counterpart `raw` should merge with, if any.
    ///
    /// A same-model burst is the normal Claude Code shape: several pending turns
    /// all share one model, so model equality alone matches *every* candidate and
    /// a first-positional pick pairs the event with the wrong neighbor — whose
    /// token counts then read as false cross-source "disagreements" (and the run
    /// is downgraded from `verified` server-side). So:
    ///
    /// 1. among correlatable (source + window) candidates, prefer those whose
    ///    tokens agree, taking the smallest (token distance, time distance);
    /// 2. fall back to model equality only when it is unambiguous — exactly one
    ///    candidate is pending (the single-tampered-field case must still merge
    ///    and surface as a disagreement).
    ///
    /// Accepted trade-off: with several same-model candidates and none token-close
    /// (a tampered turn inside a burst), nothing force-merges into a `Disagree` —
    /// both observations eventually flush single-source. Double-emitting counts
    /// only *lowers* a score (not exploitable), and the plausibility/pacing checks
    /// still apply.
    fn best_pending_match(&self, raw: &RawTurn) -> Option<usize> {
        let candidates: Vec<usize> = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, p)| correlatable(&p.raw, raw, &self.tol))
            .map(|(idx, _)| idx)
            .collect();
        let closest_agreeing = candidates
            .iter()
            .copied()
            .filter(|&idx| tokens_agree(&self.pending[idx].raw, raw, &self.tol))
            .min_by_key(|&idx| pair_distance(&self.pending[idx].raw, raw));
        if closest_agreeing.is_some() {
            return closest_agreeing;
        }
        match candidates.as_slice() {
            [only] if models_equal(&self.pending[*only].raw, raw) => Some(*only),
            _ => None,
        }
    }

    /// Ingest a raw turn observed at logical `now_ms`. Returns a merged turn when
    /// it pairs with a buffered counterpart; returns `None` after either
    /// suppressing it (it matches a turn that already flushed single-source) or
    /// buffering it (it will flush as single-source if no counterpart arrives).
    pub fn ingest(&mut self, raw: RawTurn, now_ms: i64) -> Option<NormalizedTurn> {
        if let Some(idx) = self.best_pending_match(&raw) {
            let other = self.pending.remove(idx).raw;
            // A match is always an OTEL/JSONL pair, so `raw` and `other` are
            // opposite Claude Code sources; order them OTEL-first for `merge`.
            let merged = if raw.source == Source::Otel {
                merge(&raw, &other, &self.tol)
            } else {
                merge(&other, &raw, &self.tol)
            };
            return Some(merged);
        }
        // A counterpart of a turn that already flushed single-source: drop it.
        // Each remembered turn suppresses at most one straggler (one turn is at
        // most one OTEL + one JSONL observation), so it is consumed here. This
        // scan keeps the plain positional `matches` — a mispair between two
        // identical-count same-model stragglers consumes one memory entry either
        // way, so best-candidate selection would change nothing.
        if let Some(idx) = self
            .recent
            .iter()
            .position(|p| matches(&p.raw, &raw, &self.tol))
        {
            self.recent.remove(idx);
            tracing::debug!(
                source = raw.source.as_str(),
                "suppressing a late counterpart of an already-emitted turn"
            );
            return None;
        }
        self.pending.push(Pending {
            raw,
            received_ms: now_ms,
        });
        None
    }

    /// Emit (as single-source turns) every buffered turn that has waited long
    /// enough without a counterpart. `otel_expected` selects the wait: the full
    /// [`PAIRING_HORIZON_MS`] while the active session's harness exports OTEL
    /// (its batch export runs seconds behind the JSONL line), or none at all
    /// when no counterpart can arrive (JSONL-only session, stopped, idle) — so
    /// JSONL-only capture stays near-live and a stop drains the buffer at once.
    pub fn flush_expired(&mut self, now_ms: i64, otel_expected: bool) -> Vec<NormalizedTurn> {
        let window = if otel_expected { PAIRING_HORIZON_MS } else { 0 };
        let (expired, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| now_ms - p.received_ms >= window);
        self.pending = keep;
        let flushed: Vec<NormalizedTurn> = expired.iter().map(|p| normalize(&p.raw)).collect();
        // Remember the flushed Claude Code turns so a straggler counterpart is
        // suppressed rather than emitted as a phantom turn, and age the memory.
        self.recent.extend(
            expired
                .into_iter()
                .filter(|p| matches!(p.raw.source, Source::Otel | Source::Jsonl))
                .map(|p| Pending {
                    raw: p.raw,
                    received_ms: now_ms,
                }),
        );
        self.recent
            .retain(|p| now_ms - p.received_ms < RECENT_TTL_MS);
        flushed
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
    fn unmatched_turn_flushes_single_source_after_the_pairing_horizon() {
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 5, 5, 1_000), 0)
            .is_none());

        // While an OTEL counterpart is still expected, the turn keeps waiting.
        assert!(c.flush_expired(PAIRING_HORIZON_MS - 1, true).is_empty());
        assert_eq!(c.pending_len(), 1);

        // Past the horizon: emitted as a lone JSONL turn.
        let flushed = c.flush_expired(PAIRING_HORIZON_MS, true);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].agreement, Agreement::Single);
        assert_eq!(flushed[0].sources, vec![Source::Jsonl]);
        assert_eq!(c.pending_len(), 0);
    }

    #[test]
    fn no_expected_otel_means_no_hold_at_all() {
        // A JSONL-only session (or a stop/idle daemon) can never receive a
        // counterpart — buffering would only delay the HUD and the stop→submit
        // finish line, so the very next flush emits everything.
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 5, 5, 1_000), 500)
            .is_none());
        let flushed = c.flush_expired(500, false);
        assert_eq!(flushed.len(), 1, "flushed immediately, no horizon");
        assert_eq!(flushed[0].sources, vec![Source::Jsonl]);
    }

    #[test]
    fn otel_arriving_seconds_late_still_merges() {
        // THE verified-badge regression: Claude Code's OTEL batch export lands
        // seconds after the JSONL line. The JSONL turn must still be pending —
        // and the widened timestamp window must still match — so the pair merges
        // to one `[otel, jsonl]` turn instead of two single-source ones.
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 100, 50, 1_000), 0)
            .is_none());
        // 4 s of engine ticks pass without a counterpart.
        assert!(c.flush_expired(1_000, true).is_empty());
        assert!(c.flush_expired(4_000, true).is_empty());
        // The OTEL event arrives 4 s later, content-stamped 10 s after the line.
        let merged = c
            .ingest(at(Source::Otel, Some("m"), 100, 50, 11_000), 4_100)
            .expect("late counterpart still merges");
        assert_eq!(merged.sources, vec![Source::Otel, Source::Jsonl]);
        assert_eq!(merged.confidence, Confidence::Otel);
        assert_eq!(c.pending_len(), 0, "no leftover to double-count");
    }

    #[test]
    fn a_straggler_counterpart_of_a_flushed_turn_is_suppressed_once() {
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 100, 50, 1_000), 0)
            .is_none());
        // No OTEL within the horizon: the turn flushes single-source.
        assert_eq!(c.flush_expired(PAIRING_HORIZON_MS, true).len(), 1);

        // Its OTEL event finally arrives (horizon exceeded — rare): its counts
        // were already emitted, so it must NOT become a standalone turn.
        assert!(c
            .ingest(
                at(Source::Otel, Some("m"), 100, 50, 2_500),
                PAIRING_HORIZON_MS + 1_000,
            )
            .is_none());
        assert_eq!(c.pending_len(), 0, "suppressed, not buffered");
        assert!(
            c.flush_expired(i64::MAX / 2, true).is_empty(),
            "nothing phantom ever flushes"
        );

        // The memory is consumed: a *second* close-by OTEL turn is a genuinely
        // new observation and buffers normally (one turn suppresses one).
        assert!(c
            .ingest(
                at(Source::Otel, Some("m"), 100, 50, 3_000),
                PAIRING_HORIZON_MS + 2_000,
            )
            .is_none());
        assert_eq!(c.pending_len(), 1, "second counterpart is not swallowed");
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
        let flushed = c.flush_expired(PAIRING_HORIZON_MS + 5_000, true);
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

    #[test]
    fn same_model_burst_pairs_each_event_with_its_own_turn() {
        // THE false-disagreement regression (observed in the field: 6/16 turns
        // "disagreed" on tokens_output while every input was ~8): in a one-model
        // burst, model equality matches every pending candidate, so the old
        // first-positional pick merged each OTEL event with the OLDEST pending
        // JSONL turn — the wrong neighbor. Token distance must pick the right one.
        let mut c = Correlator::new(Tolerance::default());
        for (out, ts) in [(100, 1_000), (900, 2_000), (50, 3_000)] {
            assert!(c
                .ingest(at(Source::Jsonl, Some("m"), 8, out, ts), ts)
                .is_none());
        }
        // The OTEL batch export delivers the counterparts together, in order.
        for (out, ts) in [(100, 1_000), (900, 2_000), (50, 3_000)] {
            let merged = c
                .ingest(at(Source::Otel, Some("m"), 8, out, ts + 1_500), 4_000)
                .expect("each event pairs with its own turn");
            assert_eq!(merged.agreement, Agreement::Agree, "out={out}");
            assert_eq!(merged.tokens_output, out);
        }
        assert_eq!(c.pending_len(), 0, "no strays left behind");
    }

    #[test]
    fn burst_events_arriving_out_of_order_still_pair_correctly() {
        let mut c = Correlator::new(Tolerance::default());
        for (out, ts) in [(100, 1_000), (900, 2_000), (50, 3_000)] {
            assert!(c
                .ingest(at(Source::Jsonl, Some("m"), 8, out, ts), ts)
                .is_none());
        }
        for (out, ts) in [(900, 2_000), (50, 3_000), (100, 1_000)] {
            let merged = c
                .ingest(at(Source::Otel, Some("m"), 8, out, ts + 1_500), 5_000)
                .expect("arrival order doesn't matter");
            assert_eq!(merged.agreement, Agreement::Agree, "out={out}");
        }
        assert_eq!(c.pending_len(), 0);
    }

    #[test]
    fn ambiguous_same_model_candidates_without_token_agreement_do_not_merge() {
        // Two same-model turns are pending and the incoming event is token-far
        // from both: model equality alone is ambiguous (which one would it
        // "disagree" with?), so it buffers instead of force-merging.
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 8, 100, 1_000), 1_000)
            .is_none());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 8, 900, 2_000), 2_000)
            .is_none());
        assert!(
            c.ingest(at(Source::Otel, Some("m"), 8, 5_000, 2_500), 2_500)
                .is_none(),
            "ambiguous — buffered, not mispaired"
        );
        assert_eq!(c.pending_len(), 3);
    }

    #[test]
    fn a_cache_writing_first_turn_agrees_across_the_accounting_split() {
        // The observed first-turn false positive: OTEL folded the cache-creation
        // tokens into input (531/cache 0); JSONL split them (8 + cache 523).
        // Netted, both sides say 531 — the pair must merge as an agreement, with
        // the OTEL counts kept.
        let mut c = Correlator::new(Tolerance::default());
        let otel = at(Source::Otel, Some("m"), 531, 16, 1_000);
        let mut jsonl = at(Source::Jsonl, Some("m"), 8, 16, 1_000);
        jsonl.tokens_cache = 523;

        assert!(c.ingest(otel, 0).is_none());
        let merged = c.ingest(jsonl, 100).expect("correlates");
        assert_eq!(merged.agreement, Agreement::Agree, "not a disagreement");
        assert_eq!(merged.tokens_input, 531, "OTEL stays authoritative");
    }

    #[test]
    fn a_genuinely_inflated_input_still_disagrees() {
        // Netting must not blind the check: a JSONL input inflated far beyond
        // any cache accounting still reads as a tokens_input disagreement.
        let mut c = Correlator::new(Tolerance::default());
        c.ingest(at(Source::Otel, Some("m"), 100, 50, 1_000), 0);
        let merged = c
            .ingest(at(Source::Jsonl, Some("m"), 9_000, 50, 1_000), 10)
            .expect("single candidate merges on the model");
        match &merged.agreement {
            Agreement::Disagree { fields } => {
                assert!(fields.contains(&"tokens_input".to_string()))
            }
            other => panic!("expected disagreement, got {other:?}"),
        }
    }

    #[test]
    fn a_single_pending_candidate_still_merges_on_model_alone() {
        // The single-tampered-field case: exactly one candidate is pending, so
        // model equality is unambiguous and the pair must still merge (and
        // surface the token gap as a disagreement, not two phantom turns).
        let mut c = Correlator::new(Tolerance::default());
        assert!(c
            .ingest(at(Source::Jsonl, Some("m"), 8, 9_000, 1_000), 1_000)
            .is_none());
        let merged = c
            .ingest(at(Source::Otel, Some("m"), 8, 50, 1_200), 1_200)
            .expect("unambiguous model match merges");
        assert!(matches!(merged.agreement, Agreement::Disagree { .. }));
        assert_eq!(c.pending_len(), 0);
    }
}
