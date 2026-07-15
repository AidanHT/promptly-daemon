//! Projecting a live attempt's score from captured turns (`watch`/`score`).
//!
//! Both `promptly watch` and `promptly score` (with no explicit inputs) turn the
//! daemon's captured turns into the inputs the `13` fitness function needs:
//! summed tokens, a prompt count, and the dominant resolved model. Correctness
//! and run time aren't known locally before a ranked grade, so a projection
//! assumes a full clear (`C = 100`) and the web HUD's 2s run time
//! ([`DEFAULT_PROJECTED_EXECUTION_SECONDS`]) — the same "projected" framing the
//! HUD uses (`11`). The scoring itself flows through the shared parity port
//! (`crate::scoring`), so the projected number matches the server's.

use std::collections::{HashMap, HashSet};

use crate::daemon_client::NormalizedTurn;
use crate::scoring::{self, ScoreInput, ScoreResult, Tokens};

/// Harness assumed when a turn doesn't name one (Claude Code is the first and
/// only captured harness; `21` adds others).
pub const DEFAULT_HARNESS: &str = "claude_code_cli";
/// Token-weight tier used when no workspace manifest names a challenge type.
pub const DEFAULT_CHALLENGE_TYPE: &str = "implementation";
/// Run time the live projections (`watch`/`score`) assume — the same 2s the web
/// HUD assumes (`lib/hud/projection.ts`, `DEFAULT_PROJECTED_EXECUTION_SECONDS`),
/// so the CLI and the browser project the same number for the same capture. The
/// ranked-submit parity bound deliberately does NOT use this — see
/// `commands::submit::project_best_case`.
pub const DEFAULT_PROJECTED_EXECUTION_SECONDS: f64 = 2.0;

/// An accumulator over an attempt's captured turns. Fold turns in with
/// [`observe`](LiveAttempt::observe) (seeding from a snapshot, then streaming),
/// then [`project`](LiveAttempt::project) for the current projected score.
#[derive(Debug, Default)]
pub struct LiveAttempt {
    tokens_input: u64,
    tokens_output: u64,
    tokens_thinking: u64,
    /// Cache-read/creation tokens — not a scoring input, but usually the dominant
    /// usage on a real Claude Code run, so `watch`/`score` surface it for context.
    tokens_cache: u64,
    turns: u64,
    /// Distinct OTEL `prompt.id`s — all events of one user prompt share one.
    prompt_ids: HashSet<String>,
    /// Turns carrying no prompt id (Codex, or a JSONL turn observed before its
    /// prompt boundary after a mid-prompt restart); each counts as its own prompt.
    bare_turns: u64,
    /// Total `input + output` tokens per resolved model, to pick the one the run
    /// is scored against (mirrors the server's `deriveCaptureTelemetry`).
    model_tokens: HashMap<String, u64>,
    /// Models in first-seen order, so a tie resolves to the earliest — matching
    /// the server's insertion-order tie-break.
    model_order: Vec<String>,
    /// Turn weight (`in + out + think + 1`) per harness, to pick the dominant one
    /// exactly as the server does from the signed source sets.
    harness_weights: HashMap<&'static str, u64>,
    /// Harnesses in first-seen order — the server's insertion-order tie-break.
    harness_order: Vec<&'static str>,
    /// Whether any turn carried adapter-ESTIMATED counts. One estimated turn
    /// taints the capture (the server's weakest-confidence rule), flooring the
    /// effort base at anchor parity when projecting.
    any_estimated: bool,
}

/// The harness a turn's source set attributes it to — the mirror of the server's
/// `turnHarness` (`lib/devices/capture.ts`): OTEL/JSONL only ever correlate with
/// each other, so a non-Claude source names the editor adapter that captured it.
fn turn_harness(turn: &NormalizedTurn) -> &'static str {
    use promptlyd::model::Source;
    for source in &turn.sources {
        match source {
            Source::Cursor => return "cursor",
            Source::Codex => return "codex_cli",
            Source::Copilot => return "copilot_chat",
            Source::Otel | Source::Jsonl => {}
        }
    }
    DEFAULT_HARNESS
}

impl LiveAttempt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one captured turn into the running totals.
    pub fn observe(&mut self, turn: &NormalizedTurn) {
        self.turns += 1;
        // Saturate: hostile/over-huge counts clamp instead of wrapping or
        // panicking across an unbounded turn stream.
        self.tokens_input = self.tokens_input.saturating_add(turn.tokens_input);
        self.tokens_output = self.tokens_output.saturating_add(turn.tokens_output);
        self.tokens_thinking = self.tokens_thinking.saturating_add(turn.tokens_thinking);
        self.tokens_cache = self.tokens_cache.saturating_add(turn.tokens_cache);
        match turn.prompt_id.as_deref() {
            Some(id) if !id.is_empty() => {
                self.prompt_ids.insert(id.to_string());
            }
            _ => self.bare_turns += 1,
        }
        if !self.model_tokens.contains_key(&turn.model) {
            self.model_order.push(turn.model.clone());
        }
        let model_total = self.model_tokens.entry(turn.model.clone()).or_insert(0);
        *model_total =
            model_total.saturating_add(turn.tokens_input.saturating_add(turn.tokens_output));
        // Weigh the turn's harness the way the server does over the signed chain:
        // `in + out + think + 1` — the +1 keeps near-zero estimated turns counted,
        // and thinking counts because estimated Cursor turns are often thinking-only.
        let harness = turn_harness(turn);
        if !self.harness_weights.contains_key(harness) {
            self.harness_order.push(harness);
        }
        let weight = turn
            .tokens_input
            .saturating_add(turn.tokens_output)
            .saturating_add(turn.tokens_thinking)
            .saturating_add(1);
        let harness_total = self.harness_weights.entry(harness).or_insert(0);
        *harness_total = harness_total.saturating_add(weight);
        if turn.confidence == promptlyd::model::Confidence::Estimated {
            self.any_estimated = true;
        }
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    pub fn tokens(&self) -> Tokens {
        Tokens {
            input: self.tokens_input as f64,
            output: self.tokens_output as f64,
            thinking: self.tokens_thinking as f64,
        }
    }

    /// Total cache-read/creation tokens observed (context only — never scored).
    pub fn cache_tokens(&self) -> u64 {
        self.tokens_cache
    }

    /// The multi-turn prompt count `P`: distinct OTEL prompts plus prompt-less
    /// turns. (Scoring floors this at 1, so zero turns still scores.)
    pub fn prompt_count(&self) -> u64 {
        self.prompt_ids.len() as u64 + self.bare_turns
    }

    /// The model the run is scored against: the one that burned the most
    /// `input + output` tokens, ties broken by first-seen. Mirrors the server's
    /// `deriveCaptureTelemetry` (`lib/devices/capture.ts`) so this projection
    /// predicts the graded score rather than diverging from it. Empty when nothing
    /// has been observed — scoring floors an empty/unknown id to the baseline tier.
    pub fn resolved_model(&self) -> &str {
        let mut best: Option<&str> = None;
        let mut best_total = 0u64;
        for model in &self.model_order {
            let total = self.model_tokens.get(model).copied().unwrap_or(0);
            if best.is_none() || total > best_total {
                best = Some(model.as_str());
                best_total = total;
            }
        }
        best.unwrap_or("")
    }

    /// The dominant harness by turn weight, ties broken by first-seen — the
    /// mirror of the server's `deriveCaptureTelemetry`, so a Cursor-driven run
    /// projects with the +0.20 Composer modifier the grade will apply.
    fn harness(&self) -> &str {
        let mut best: Option<&'static str> = None;
        let mut best_total = 0u64;
        for harness in &self.harness_order {
            let total = self.harness_weights.get(harness).copied().unwrap_or(0);
            if best.is_none() || total > best_total {
                best = Some(harness);
                best_total = total;
            }
        }
        best.unwrap_or(DEFAULT_HARNESS)
    }

    /// Whether the projection must score as an estimated-count capture: any
    /// estimated turn taints the run (and an empty capture is estimated), the
    /// exact rule the submit path reports and the server grades by.
    fn counts_estimated(&self) -> bool {
        self.turns == 0 || self.any_estimated
    }

    /// Project the current score. `correctness` defaults to a full clear and
    /// `seconds` to 0 (floored), reflecting that the hidden-suite result isn't
    /// known before a ranked grade. The breakdown marks the floored inputs.
    pub fn project(
        &self,
        challenge_type: &str,
        overrides: Option<&HashMap<String, f64>>,
        correctness: f64,
        seconds: f64,
    ) -> ScoreResult {
        let input = ScoreInput {
            correctness_pct: correctness,
            prompt_count: self.prompt_count() as f64,
            tokens: self.tokens(),
            execution_time_seconds: seconds,
            challenge_type: challenge_type.to_string(),
            model_identifier: self.resolved_model().to_string(),
            harness_used: self.harness().to_string(),
            counts_estimated: self.counts_estimated(),
        };
        scoring::score_submission(&input, overrides)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use promptlyd::model::{Agreement, Confidence, Plausibility, Source};

    fn turn(model: &str, prompt_id: Option<&str>, input: u64, output: u64) -> NormalizedTurn {
        NormalizedTurn {
            schema_version: 1,
            turn_id: format!("{model}-{input}-{output}-{prompt_id:?}"),
            model: model.to_string(),
            harness: "claude_code_cli".to_string(),
            tokens_input: input,
            tokens_output: output,
            tokens_thinking: 0,
            tokens_cache: 0,
            prompt_id: prompt_id.map(str::to_string),
            timestamp_ms: 1,
            confidence: Confidence::Otel,
            cost_usd: None,
            duration_ms: None,
            sources: vec![Source::Otel],
            session_id: None,
            attempt_nonce: Some("n".into()),
            workspace: None,
            agreement: Agreement::Single,
            plausibility: Plausibility::Plausible,
        }
    }

    #[test]
    fn counts_distinct_prompts_and_sums_tokens() {
        let mut attempt = LiveAttempt::new();
        // Two OTEL events sharing one prompt id, plus a prompt-less turn (Codex).
        attempt.observe(&turn("claude-opus-4-8", Some("p1"), 100, 50));
        attempt.observe(&turn("claude-opus-4-8", Some("p1"), 60, 40));
        attempt.observe(&turn("claude-opus-4-8", None, 20, 10));
        assert_eq!(attempt.turns(), 3);
        assert_eq!(
            attempt.prompt_count(),
            2,
            "one shared prompt + one bare turn"
        );
        let tokens = attempt.tokens();
        assert_eq!(tokens.input, 180.0);
        assert_eq!(tokens.output, 100.0);
    }

    #[test]
    fn resolved_model_is_the_one_that_burned_the_most_tokens() {
        let mut attempt = LiveAttempt::new();
        // Opus burns the most input+output tokens, so it is the scored model even
        // though Haiku is the most recent turn — mirroring deriveCaptureTelemetry.
        attempt.observe(&turn("claude-opus-4-8", Some("a"), 400, 100));
        attempt.observe(&turn("claude-haiku-4-5", Some("b"), 10, 10));
        assert_eq!(attempt.resolved_model(), "claude-opus-4-8");
    }

    #[test]
    fn projects_a_clear_through_the_shared_scoring_port() {
        let mut attempt = LiveAttempt::new();
        // Mirror the anchor parity vector: 2 prompts, 5000 in / 3000 out, debugging.
        attempt.observe(&turn("claude-sonnet-4-6", Some("p1"), 5000, 3000));
        attempt.observe(&turn("claude-sonnet-4-6", Some("p2"), 0, 0));
        let result = attempt.project("debugging", None, 100.0, 4.0);
        // Same inputs as the anchor vector → the same score the server computes.
        assert!((result.score - 183.8235294117647).abs() / result.score < 1e-9);
        assert!(!result.baseline_floor_fallback);
    }

    #[test]
    fn the_hud_default_projects_half_of_the_floored_score() {
        // The speed factor divides linearly, so 2s (the web HUD's assumption)
        // scores exactly half of the 1s floor. This is the divergence the CLI
        // used to show: `watch` projected 2× the browser's number for the same
        // capture until both assumed the same 2s.
        assert_eq!(DEFAULT_PROJECTED_EXECUTION_SECONDS, 2.0);
        let mut attempt = LiveAttempt::new();
        attempt.observe(&turn("claude-sonnet-4-6", Some("p1"), 5000, 3000));
        let floored = attempt.project("debugging", None, 100.0, 0.0);
        let hud = attempt.project(
            "debugging",
            None,
            100.0,
            DEFAULT_PROJECTED_EXECUTION_SECONDS,
        );
        assert!((hud.score * 2.0 - floored.score).abs() / floored.score < 1e-12);
    }

    #[test]
    fn an_empty_attempt_projects_against_the_floor_model() {
        let attempt = LiveAttempt::new();
        assert_eq!(attempt.resolved_model(), "");
        let result = attempt.project(DEFAULT_CHALLENGE_TYPE, None, 100.0, 0.0);
        // No model resolved → baseline-floor tier; tokens floored; still finite.
        assert!(result.baseline_floor_fallback);
        assert!(result.score.is_finite());
    }

    fn estimated_cursor_turn(model: &str, input: u64, output: u64) -> NormalizedTurn {
        let mut t = turn(model, None, input, output);
        t.harness = "cursor".to_string();
        t.sources = vec![Source::Cursor];
        t.confidence = Confidence::Estimated;
        t
    }

    #[test]
    fn one_estimated_turn_floors_the_projected_effort_base() {
        // A Cursor estimated capture on a sub-anchor model projects with the
        // server's estimated-counts floor (base 1.00) + the Composer +0.20 — the
        // number grading assigns, not the optimistic sub-anchor one the CLI
        // projected before v1.0.0.
        let mut attempt = LiveAttempt::new();
        attempt.observe(&estimated_cursor_turn("composer-2-5", 500, 300));
        let result = attempt.project("debugging", None, 100.0, 1.0);
        assert!(result.breakdown.effort.base_floored);
        assert!((result.breakdown.effort.value - 1.2).abs() < 1e-12);

        // The same turns measured (OTEL) keep the model's own economics.
        let mut measured = LiveAttempt::new();
        measured.observe(&turn("composer-2-5", Some("p1"), 500, 300));
        let plain = measured.project("debugging", None, 100.0, 1.0);
        assert!(!plain.breakdown.effort.base_floored);
        assert!(plain.score > result.score);
    }

    #[test]
    fn the_dominant_harness_by_turn_weight_picks_the_scored_modifier() {
        // Two heavy Cursor turns + one light Claude turn → the run projects as
        // `cursor` (weight in+out+think+1, the server's deriveCaptureTelemetry
        // rule), so the +0.20 Composer modifier matches the grade.
        let mut attempt = LiveAttempt::new();
        attempt.observe(&estimated_cursor_turn("composer-2-5", 400, 200));
        attempt.observe(&estimated_cursor_turn("composer-2-5", 300, 100));
        attempt.observe(&turn("claude-sonnet-4-6", Some("p1"), 10, 5));
        let result = attempt.project("debugging", None, 100.0, 1.0);
        assert!(
            (result.breakdown.effort.composer_modifier - 0.2).abs() < 1e-12,
            "cursor dominates by weight"
        );

        // Flip the weights and the Claude turns dominate — no Composer modifier.
        let mut attempt = LiveAttempt::new();
        attempt.observe(&estimated_cursor_turn("composer-2-5", 10, 5));
        attempt.observe(&turn("claude-sonnet-4-6", Some("p1"), 400, 200));
        let result = attempt.project("debugging", None, 100.0, 1.0);
        assert_eq!(result.breakdown.effort.composer_modifier, 0.0);
    }
}
