//! The unified telemetry schema.
//!
//! Every capture source produces a [`RawTurn`]; the normalization layer
//! (`crate::normalize`, `crate::correlate`) turns those into [`NormalizedTurn`]
//! records — the one shape the cloud upload (`20`) and the web bridge (`22`)
//! consume. Field names mirror the `submissions` columns (`04`) so downstream
//! mapping is a rename-free copy.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Harness strings — each mirrors a `harness_type` Postgres enum value (`04`)
/// and the `HarnessType` union in `lib/levels/types.ts`. The enum already carries
/// every value the `21` adapters emit, so no migration is needed. `cursor` is
/// also the value scoring keys on to add the Composer coordination modifier
/// (`13`), which is why the Cursor adapter reports it.
pub const HARNESS_CLAUDE_CODE_CLI: &str = "claude_code_cli";
/// Cursor's agent/Composer (`21`).
pub const HARNESS_CURSOR: &str = "cursor";
/// OpenAI's Codex CLI (`21`).
pub const HARNESS_CODEX_CLI: &str = "codex_cli";
/// GitHub Copilot Chat (`21`).
pub const HARNESS_COPILOT_CHAT: &str = "copilot_chat";

/// Schema version stamped on every [`NormalizedTurn`]. Bump only with a matching
/// reader change in `20`/`22`.
pub const TURN_SCHEMA_VERSION: u32 = 1;

/// Which local source observed a raw turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The embedded OTLP receiver (Claude Code native OpenTelemetry).
    Otel,
    /// The Claude Code JSONL session-log watcher.
    Jsonl,
    /// Cursor's `state.vscdb` capture adapter (`21`).
    Cursor,
    /// OpenAI Codex CLI's rollout-transcript adapter (`21`).
    Codex,
    /// GitHub Copilot Chat's session-log adapter (`21`).
    Copilot,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Otel => "otel",
            Source::Jsonl => "jsonl",
            Source::Cursor => "cursor",
            Source::Codex => "codex",
            Source::Copilot => "copilot",
        }
    }

    /// Are these two sources cross-checked against each other for the agreement
    /// signal (`17`)? Only Claude Code's OTEL and JSONL ever describe the *same*
    /// turn twice; each reverse-engineered adapter (`21`) is a lone source, so it
    /// is never correlated (and so never spuriously merged with a Claude turn that
    /// happens to be close in time).
    pub fn correlates_with(self, other: Source) -> bool {
        matches!(
            (self, other),
            (Source::Otel, Source::Jsonl) | (Source::Jsonl, Source::Otel)
        )
    }
}

/// How trustworthy a normalized turn is — mirrors the
/// `submissions.telemetry_confidence` constraint (`04`); the daemon emits the
/// first three (`manual` is the web form's, `14`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// From the OTLP receiver: highest local confidence.
    Otel,
    /// From the JSONL watcher only.
    Jsonl,
    /// The underlying model could not be resolved; scores against the baseline
    /// floor tier downstream (`13a`).
    Estimated,
}

/// Cross-source agreement marker. When both OTEL and JSONL observe one turn we
/// don't just de-duplicate — we record whether they *agree*, a strong honesty
/// signal carried to `20`/`25`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Agreement {
    /// Only one source observed this turn.
    Single,
    /// Both sources observed it and their model + token counts agree.
    Agree,
    /// Both sources observed it but disagree — a tampering/forgery signal. The
    /// listed fields differ; the OTEL values are kept (JSONL never overrides).
    Disagree { fields: Vec<String> },
}

/// Per-turn plausibility. Implausible turns are never dropped locally — they are
/// annotated so the server (`25`) can weigh them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Plausibility {
    Plausible,
    Low { reasons: Vec<String> },
}

/// A single observed turn straight from one source, before normalization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawTurn {
    pub source: Source,
    /// The resolved underlying model, if the source reported one.
    pub model: Option<String>,
    pub harness: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    /// Thinking tokens. OTEL bills these inside output and does not break them
    /// out (so `0`); the JSONL watcher estimates them from thinking blocks.
    pub tokens_thinking: u64,
    pub tokens_cache: u64,
    /// Correlates all events from one user prompt (OTEL `prompt.id`).
    pub prompt_id: Option<String>,
    pub timestamp_ms: i64,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// The harness session id (JSONL `sessionId` / OTEL resource attribute).
    pub session_id: Option<String>,
    /// The launch cwd carried by the source, used to scope/attribute the turn.
    pub workspace: Option<String>,
    /// The token counts were *inferred* (e.g. char/4 when Cursor records a
    /// zero-token bubble, `21`) rather than reported by the source. Such a turn is
    /// downgraded to `estimated` confidence even when its model is resolved. OTEL
    /// and JSONL always report real counts, so they leave this `false`.
    #[serde(default)]
    pub counts_estimated: bool,
    /// A stable per-turn identity from the source, when it carries one. Claude
    /// Code's projects JSONL writes **one line per content block**, so a single
    /// physical assistant turn arrives as 2-3 `assistant` lines — each with its
    /// own timestamp but the same `message.id` (fallback: `requestId`) and the
    /// identical whole-message `usage` repeated. This id is what lets the engine
    /// collapse those lines into one observation ([`RawTurn::dedup_id`]).
    /// `None` (sources without a per-turn id, or data from an older checkpoint —
    /// hence `serde(default)`) falls back to the content fingerprint.
    #[serde(default)]
    pub event_id: Option<String>,
}

impl RawTurn {
    /// A stable content id for this exact observation. Used both as the emitted
    /// turn's `turn_id` and as the de-duplication key so a re-read JSONL line or
    /// a resent OTEL event after a restart is not counted twice.
    pub fn content_id(&self) -> String {
        let model = self.model.as_deref().unwrap_or("");
        let prompt = self.prompt_id.as_deref().unwrap_or("");
        fingerprint(&[
            self.source.as_str(),
            model,
            &self.tokens_input.to_string(),
            &self.tokens_output.to_string(),
            &self.tokens_cache.to_string(),
            &self.timestamp_ms.to_string(),
            prompt,
        ])
    }

    /// The de-duplication key for this observation. When the source supplied a
    /// stable per-turn id ([`RawTurn::event_id`]), the key is `[source, event_id]`
    /// — so the several block-lines Claude Code writes for ONE assistant turn
    /// (same `message.id`, same repeated usage, but distinct per-line timestamps)
    /// collapse to a single stored turn instead of each counting as its own
    /// (the v0.1.9 ~3× turn/token inflation). Keep-first is the right merge:
    /// the thinking block is written first, so its thinking estimate survives.
    /// Without an event id this is exactly [`RawTurn::content_id`], the old
    /// behavior. Only the dedup key changes — `content_id` still names the
    /// emitted turn (`turn_id`).
    pub fn dedup_id(&self) -> String {
        match self.event_id.as_deref().filter(|id| !id.is_empty()) {
            Some(event_id) => fingerprint(&[self.source.as_str(), event_id]),
            None => self.content_id(),
        }
    }

    /// The resolved model: the reported string trimmed, or `None` if blank.
    pub fn resolved_model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
    }
}

/// A fully-normalized turn — the unit streamed over the API and uploaded later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedTurn {
    pub schema_version: u32,
    pub turn_id: String,
    /// Always the resolved underlying model; empty string when unresolved
    /// (`confidence = estimated`). Downstream uses this as-is and never
    /// re-resolves.
    pub model: String,
    pub harness: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_thinking: u64,
    pub tokens_cache: u64,
    pub prompt_id: Option<String>,
    pub timestamp_ms: i64,
    pub confidence: Confidence,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// The source(s) that contributed — one, or both when correlated.
    pub sources: Vec<Source>,
    pub session_id: Option<String>,
    /// Stamped by session scoping (`18`); carried here so `20`/`25` can bind
    /// telemetry to the attempt being submitted.
    pub attempt_nonce: Option<String>,
    pub workspace: Option<String>,
    pub agreement: Agreement,
    pub plausibility: Plausibility,
}

/// Lowercase-hex SHA-256 over the NUL-delimited parts, truncated to 16 chars —
/// enough to identify a turn without bloating the checkpoint.
pub fn fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Build a minimal [`RawTurn`] for tests across the normalization modules.
#[cfg(test)]
pub(crate) fn sample_raw(source: Source, model: Option<&str>, in_: u64, out: u64) -> RawTurn {
    RawTurn {
        source,
        model: model.map(str::to_string),
        harness: HARNESS_CLAUDE_CODE_CLI.to_string(),
        tokens_input: in_,
        tokens_output: out,
        tokens_thinking: 0,
        tokens_cache: 0,
        prompt_id: None,
        timestamp_ms: 1_700_000_000_000,
        cost_usd: None,
        duration_ms: None,
        session_id: None,
        workspace: None,
        counts_estimated: false,
        event_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::sample_raw as raw;
    use super::*;

    #[test]
    fn content_id_is_stable_and_distinguishes_sources() {
        let a = raw(Source::Otel, Some("claude-opus-4-8"), 100, 50);
        assert_eq!(a.content_id(), a.content_id(), "deterministic");
        assert_eq!(a.content_id().len(), 16);

        let mut b = a.clone();
        b.source = Source::Jsonl;
        assert_ne!(a.content_id(), b.content_id(), "source is part of the id");

        let mut c = a.clone();
        c.tokens_output = 51;
        assert_ne!(a.content_id(), c.content_id(), "token counts change the id");
    }

    #[test]
    fn dedup_id_is_stable_across_block_line_timestamps() {
        // Two block-lines of ONE physical turn: same message.id, identical usage,
        // but written ~2s apart (each line carries its own timestamp).
        let mut first = raw(Source::Jsonl, Some("claude-haiku-4-5"), 8, 833);
        first.event_id = Some("msg_01NXRTGdWCZY2iP6CrHs5UcV".to_string());
        let mut second = first.clone();
        second.timestamp_ms += 2_046;

        assert_ne!(
            first.content_id(),
            second.content_id(),
            "the per-line timestamp makes the content ids differ (the old inflation)"
        );
        assert_eq!(
            first.dedup_id(),
            second.dedup_id(),
            "the shared message.id collapses the block-lines"
        );
    }

    #[test]
    fn dedup_id_distinguishes_messages_and_sources() {
        let mut a = raw(Source::Jsonl, Some("m"), 8, 833);
        a.event_id = Some("msg_aaa".to_string());
        let mut b = a.clone();
        b.event_id = Some("msg_bbb".to_string());
        assert_ne!(
            a.dedup_id(),
            b.dedup_id(),
            "distinct messages stay distinct"
        );

        let mut other_source = a.clone();
        other_source.source = Source::Copilot;
        assert_ne!(
            a.dedup_id(),
            other_source.dedup_id(),
            "the source is part of the key"
        );
    }

    #[test]
    fn dedup_id_falls_back_to_content_id_without_an_event_id() {
        let a = raw(Source::Otel, Some("m"), 100, 50);
        assert_eq!(a.dedup_id(), a.content_id(), "no event id -> old behavior");

        let mut blank = a.clone();
        blank.event_id = Some(String::new());
        assert_eq!(
            blank.dedup_id(),
            blank.content_id(),
            "a blank event id never keys the dedup"
        );
    }

    #[test]
    fn resolved_model_trims_and_blanks() {
        assert_eq!(
            raw(Source::Otel, Some("  claude-opus-4-8 "), 1, 1).resolved_model(),
            Some("claude-opus-4-8"),
        );
        assert_eq!(raw(Source::Otel, Some("   "), 1, 1).resolved_model(), None);
        assert_eq!(raw(Source::Otel, None, 1, 1).resolved_model(), None);
    }

    #[test]
    fn confidence_and_source_serialize_to_lowercase() {
        assert_eq!(
            serde_json::to_string(&Confidence::Otel).unwrap(),
            "\"otel\""
        );
        assert_eq!(
            serde_json::to_string(&Confidence::Estimated).unwrap(),
            "\"estimated\""
        );
        assert_eq!(serde_json::to_string(&Source::Jsonl).unwrap(), "\"jsonl\"");
        assert_eq!(
            serde_json::to_string(&Source::Cursor).unwrap(),
            "\"cursor\""
        );
        assert_eq!(serde_json::to_string(&Source::Codex).unwrap(), "\"codex\"");
        assert_eq!(
            serde_json::to_string(&Source::Copilot).unwrap(),
            "\"copilot\""
        );
    }

    #[test]
    fn only_otel_and_jsonl_correlate() {
        assert!(Source::Otel.correlates_with(Source::Jsonl));
        assert!(Source::Jsonl.correlates_with(Source::Otel));
        // A reverse-engineered adapter is a lone source — never correlated, so it
        // can't spuriously merge with a Claude turn that's close in time.
        assert!(!Source::Cursor.correlates_with(Source::Otel));
        assert!(!Source::Otel.correlates_with(Source::Cursor));
        assert!(!Source::Codex.correlates_with(Source::Jsonl));
        assert!(!Source::Cursor.correlates_with(Source::Codex));
        // A source never correlates with itself.
        assert!(!Source::Otel.correlates_with(Source::Otel));
    }

    #[test]
    fn agreement_tagging_round_trips() {
        let dis = Agreement::Disagree {
            fields: vec!["model".into()],
        };
        let json = serde_json::to_string(&dis).unwrap();
        assert!(json.contains("\"status\":\"disagree\""));
        assert!(json.contains("\"fields\":[\"model\"]"));
        assert_eq!(serde_json::from_str::<Agreement>(&json).unwrap(), dis);
    }
}
