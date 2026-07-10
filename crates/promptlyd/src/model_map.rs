//! Resolving a harness's reported model name to a canonical `model_economics`
//! identifier (`21`).
//!
//! Claude Code emits canonical identifiers directly (`claude-opus-4-8`), so its
//! sources pass the model through untouched. The reverse-engineered adapters
//! (`21`) report whatever string their editor stored — `Claude Opus 4.8`,
//! `anthropic/claude-opus-4-8`, the reordered `claude-4-opus`, … — which must be
//! mapped to the canonical id the scoring matrix (`13a`) prices, so the turn
//! scores against its own row rather than the baseline floor.
//!
//! This is **best-effort** (the plan calls these sources version-fragile): a
//! confident match returns the canonical id; anything unrecognized returns
//! `None`, so the turn is marked `estimated` and scores against the baseline
//! floor downstream (`17`). The resolver never *guesses across* model families.
//!
//! The targets are exactly the priced rows of the matrix (the markers
//! `cursor-composer`/`baseline-floor-tier` are never "an underlying model"); a
//! unit test cross-checks [`RESOLVABLE`] against the shared scoring fixture so it
//! can't silently drift from `lib/economics/model-economics.ts`.

/// The canonical `model_economics` model identifiers an adapter may resolve to —
/// every priced row except the baseline-floor catch-all. Kept in sync with the
/// matrix by `resolvable_matches_the_economics_matrix` below.
pub const RESOLVABLE: &[&str] = &[
    // Anthropic
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-sonnet-4",
    "claude-haiku-4-5",
    // OpenAI
    "gpt-5-6-sol",
    "gpt-5-6-terra",
    "gpt-5-6-luna",
    "gpt-5-5",
    "gpt-5-4",
    "gpt-5-4-mini",
    "gpt-5-4-nano",
    "gpt-5-3-codex",
    "gpt-5-2",
    "gpt-5-2-codex",
    "gpt-5-1-codex",
    "gpt-5-1-codex-mini",
    "gpt-5-codex",
    "gpt-5",
    "gpt-5-mini",
    // Google
    "gemini-3-5-flash",
    "gemini-3-1-pro",
    "gemini-3-pro",
    "gemini-3-flash",
    "gemini-2-5-flash",
    "gemini-3-1-flash-lite",
    // xAI
    "grok-4-3",
    "grok-4-20",
    "grok-4",
    "grok-build-0-1",
    "grok-4-1-fast",
    // Cursor
    "composer-2-5",
    "composer-2",
    "composer-1-5",
    "composer-1",
    // Moonshot
    "kimi-k2-7-code",
    "kimi-k2-6",
    // DeepSeek
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    // Z.ai
    "glm-5-2",
];

/// Resolve a reported model name to a canonical `model_economics` id, or `None`
/// when it can't be matched confidently (the turn then scores at the baseline
/// floor as `estimated`).
pub fn resolve(reported: &str) -> Option<&'static str> {
    let norm = normalize(reported);
    if norm.is_empty() {
        return None;
    }
    // 1. The common case: a current model whose spelling normalizes straight to a
    //    canonical id (`Claude Opus 4.8` / `anthropic/claude-opus-4-8` / `gpt-5.5`).
    if let Some(c) = exact(&norm) {
        return Some(c);
    }
    // 2. Anthropic's family/version ordering varies the most across editors;
    //    reassemble `claude-<tier>-<version>` and complete to the latest in-tier.
    if let Some(c) = anthropic(&norm) {
        return Some(c);
    }
    // 3. The matrix prices several Codex rows individually, and step 1 already
    //    matched those. Any *other* `*-codex` spelling is still the Codex CLI's
    //    model, so fall back to a real, priced Codex row rather than the floor.
    if norm == "codex" || norm.ends_with("-codex") {
        return exact("gpt-5-3-codex");
    }
    // 4. A less-specific name that completes to exactly one canonical id
    //    (`gpt-5-3` → `gpt-5-3-codex`, `grok-build-0` → `grok-build-0-1`).
    //    Ambiguous prefixes (`kimi-k2`, `deepseek-v4`) stay unresolved rather
    //    than guess. A prefix that is itself a priced row (`gpt-5`) never gets
    //    here — step 1 matched it exactly.
    unique_completion(&norm)
}

/// Lowercase, drop a provider prefix (`anthropic/…`), and collapse every run of
/// non-alphanumeric characters to a single `-` (so dots, spaces, and underscores
/// all read the same), then trim leading/trailing `-`.
fn normalize(raw: &str) -> String {
    let tail = raw.rsplit('/').next().unwrap_or(raw);
    let mut out = String::with_capacity(tail.len());
    let mut prev_dash = false;
    for ch in tail.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// An exact (already-normalized) canonical id, returned as the `'static` literal.
fn exact(norm: &str) -> Option<&'static str> {
    RESOLVABLE.iter().copied().find(|c| *c == norm)
}

/// Resolve an Anthropic name whose tier/version may be reordered or partial
/// (`claude-4-opus`, `claude-opus-4`) by rebuilding `claude-<tier>-<version>` and
/// completing a partial version to the latest in-tier row. Same-tier Anthropic
/// rows price identically (`13a`), so completing to the latest is score-safe.
fn anthropic(norm: &str) -> Option<&'static str> {
    if !norm.starts_with("claude") {
        return None;
    }
    let tier = ["opus", "sonnet", "haiku"]
        .into_iter()
        .find(|t| norm.split('-').any(|seg| seg == *t))?;
    let mut version: Vec<&str> = norm
        .split('-')
        .filter(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    // Drop a trailing `20\d{6}` datestamp segment: the Cursor/Copilot/Codex
    // adapters pass Claude Code's datestamped id straight through
    // (`claude-haiku-4-5-20251001`), and the 8-digit stamp is not a version.
    // Mirrors `canonicalize_model_id` (scoring) / the web's `model-id.ts`.
    if version
        .last()
        .is_some_and(|seg| seg.len() == 8 && seg.starts_with("20"))
    {
        version.pop();
    }
    let prefix = format!("claude-{tier}-{}", version.join("-"));
    if let Some(c) = exact(&prefix) {
        return Some(c);
    }
    // Latest row in this tier whose version begins with the parsed digits.
    RESOLVABLE
        .iter()
        .copied()
        .filter(|c| c.starts_with(&prefix) && c.as_bytes().get(prefix.len()) == Some(&b'-'))
        .max()
}

/// Complete a less-specific name to a canonical id only when exactly one row
/// extends it (`{norm}-…`); ambiguous prefixes return `None` rather than guess.
fn unique_completion(norm: &str) -> Option<&'static str> {
    let with_sep = format!("{norm}-");
    let mut matches = RESOLVABLE
        .iter()
        .copied()
        .filter(|c| c.starts_with(&with_sep));
    let first = matches.next()?;
    match matches.next() {
        None => Some(first),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_spellings_normalize_straight_to_canonical() {
        assert_eq!(resolve("claude-opus-4-8"), Some("claude-opus-4-8"));
        assert_eq!(resolve("Claude Opus 4.8"), Some("claude-opus-4-8"));
        assert_eq!(
            resolve("anthropic/claude-opus-4-8"),
            Some("claude-opus-4-8")
        );
        assert_eq!(resolve("gpt-5.5"), Some("gpt-5-5"));
        assert_eq!(resolve("gpt-5.3-codex"), Some("gpt-5-3-codex"));
        assert_eq!(resolve("gemini-3.1-pro"), Some("gemini-3-1-pro"));
        assert_eq!(resolve("Gemini 3.5 Flash"), Some("gemini-3-5-flash"));
        assert_eq!(resolve("grok-4.3"), Some("grok-4-3"));
        assert_eq!(resolve("kimi-k2.6"), Some("kimi-k2-6"));
        assert_eq!(resolve("deepseek-v4-flash"), Some("deepseek-v4-flash"));
    }

    #[test]
    fn anthropic_reordered_and_partial_names_resolve() {
        // Cursor commonly stores the tier last and the version dotted.
        assert_eq!(resolve("claude-4-opus"), Some("claude-opus-4-8"));
        assert_eq!(resolve("claude-4.5-sonnet"), Some("claude-sonnet-4-5"));
        assert_eq!(resolve("claude-4-sonnet"), Some("claude-sonnet-4"));
        assert_eq!(resolve("claude-4.5-haiku"), Some("claude-haiku-4-5"));
        assert_eq!(resolve("claude-5-sonnet"), Some("claude-sonnet-5"));
        // A bare tier+major completes to the latest same-tier row (score-safe:
        // Anthropic prices a tier identically across versions).
        assert_eq!(resolve("claude-opus-4"), Some("claude-opus-4-8"));
        // A tier+version the matrix no longer prices resolves to nothing rather
        // than sliding onto a differently-priced row; the turn goes `estimated`.
        assert_eq!(resolve("claude-3.5-haiku"), None);
    }

    #[test]
    fn datestamped_anthropic_ids_drop_the_trailing_stamp() {
        // Claude Code reports datestamped ids; the adapters relay them verbatim.
        // The 8-digit `20…` stamp is not a version segment, so it must be dropped
        // before completing to the priced row.
        assert_eq!(
            resolve("claude-haiku-4-5-20251001"),
            Some("claude-haiku-4-5")
        );
        assert_eq!(resolve("claude-opus-4-8-20260115"), Some("claude-opus-4-8"));
    }

    #[test]
    fn codex_variants_resolve_to_a_priced_codex_row() {
        // The matrix prices these individually now, so each keeps its own row.
        assert_eq!(resolve("gpt-5-codex"), Some("gpt-5-codex"));
        assert_eq!(resolve("gpt-5.1-codex"), Some("gpt-5-1-codex"));
        assert_eq!(resolve("gpt-5.2-codex"), Some("gpt-5-2-codex"));
        // A bare or unpriced Codex spelling still lands on a real Codex row.
        assert_eq!(resolve("codex"), Some("gpt-5-3-codex"));
        assert_eq!(resolve("gpt-6-codex"), Some("gpt-5-3-codex"));
    }

    #[test]
    fn unique_prefixes_complete_but_ambiguous_ones_do_not() {
        // Exactly one row extends the prefix.
        assert_eq!(resolve("gpt-5.3"), Some("gpt-5-3-codex"));
        assert_eq!(resolve("grok-build-0"), Some("grok-build-0-1"));
        // Ambiguous: several rows share the prefix — don't guess.
        assert_eq!(resolve("kimi-k2"), None);
        assert_eq!(resolve("deepseek-v4"), None);
        assert_eq!(resolve("gpt-5.6"), None);
        // A prefix that is itself a priced row matches exactly (step 1), so it
        // never reaches the ambiguity check.
        assert_eq!(resolve("gpt-5"), Some("gpt-5"));
        assert_eq!(resolve("grok-4"), Some("grok-4"));
    }

    #[test]
    fn unknown_older_and_blank_models_stay_unresolved() {
        // Pre-matrix models score at the baseline floor (`estimated`), never a
        // wrong-family guess.
        assert_eq!(resolve("gpt-4o"), None);
        assert_eq!(resolve("o3-mini"), None);
        assert_eq!(resolve("claude-3.5-sonnet"), None);
        assert_eq!(resolve("llama-4-maverick"), None);
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("   "), None);
    }

    #[test]
    fn never_resolves_to_a_marker_row() {
        // The Composer/baseline-floor markers are not underlying models.
        assert!(!RESOLVABLE.contains(&"cursor-composer"));
        assert!(!RESOLVABLE.contains(&"baseline-floor-tier"));
        assert_eq!(resolve("cursor-composer"), None);
        assert_eq!(resolve("baseline-floor-tier"), None);
    }

    /// `RESOLVABLE` must equal the matrix's priced, non-floor rows so the adapter
    /// targets can't drift from `lib/economics/model-economics.ts`. The shared
    /// scoring fixture (the CLI embeds the same file for parity) is the source.
    #[test]
    fn resolvable_matches_the_economics_matrix() {
        const FIXTURE: &str = include_str!("../../../vendor/parity-fixture.json");
        #[derive(serde::Deserialize)]
        struct Row {
            model_identifier: String,
            input_cost: Option<f64>,
            #[serde(default)]
            is_baseline_floor: bool,
        }
        #[derive(serde::Deserialize)]
        struct Fixture {
            economics: Vec<Row>,
        }
        let fixture: Fixture = serde_json::from_str(FIXTURE).expect("parity fixture parses");

        let mut from_matrix: Vec<String> = fixture
            .economics
            .into_iter()
            // Priced (non-null cost) and not the baseline-floor catch-all: i.e.
            // the rows that are a real underlying model. The Composer marker has a
            // null cost and is excluded here.
            .filter(|r| r.input_cost.is_some() && !r.is_baseline_floor)
            .map(|r| r.model_identifier)
            .collect();
        from_matrix.sort();

        let mut declared: Vec<String> = RESOLVABLE.iter().map(|s| s.to_string()).collect();
        declared.sort();

        assert_eq!(
            declared, from_matrix,
            "model_map::RESOLVABLE drifted from the economics matrix",
        );
    }
}
