//! `promptly score` — compute an attempt's projected score locally, with parity
//! to the server's `13` scoring engine.
//!
//! Two modes share the one parity port (`crate::scoring`), so both match what the
//! server would assign:
//! - **Live** (default): read the active session's captured turns from the daemon
//!   and project the score (assuming a clear and the HUD's 2s run time), the way
//!   the web HUD projects before a ranked grade (`11`). Token weights come from
//!   the workspace manifest's `challenge_type`/`token_weight_overrides`.
//! - **Explicit** (`--model …`): score a given input vector — the mode the
//!   `13`/`19` parity fixture exercises — for a quick "what would this score".

use clap::Args;

use crate::commands::session_view;
use crate::daemon_client::{DaemonApi, DaemonClient};
use crate::fmt;
use crate::projection::{LiveAttempt, DEFAULT_CHALLENGE_TYPE, DEFAULT_PROJECTED_EXECUTION_SECONDS};
use crate::scoring::{self, ScoreInput, ScoreResult, Tokens};
use crate::style::Style;
use crate::visual;
use crate::CommandExit;

use promptlyd::manifest::Manifest;

#[derive(Debug, Args)]
pub struct ScoreArgs {
    /// Score this explicit model id instead of the live session. Unknown ids
    /// score at the baseline-floor tier, exactly as the server resolves them.
    #[arg(long)]
    model: Option<String>,

    /// Challenge type (selects default token weights). Defaults to the workspace
    /// manifest's value; required in explicit mode when no manifest is present.
    #[arg(long, value_parser = ["debugging", "implementation", "generation"])]
    challenge_type: Option<String>,

    /// Prompt count `P` (explicit mode; floored at 1). Default 1.
    #[arg(long)]
    prompts: Option<u64>,

    /// Summed input/context tokens (explicit mode).
    #[arg(long, default_value_t = 0)]
    tokens_in: u64,

    /// Summed generated output tokens (explicit mode).
    #[arg(long, default_value_t = 0)]
    tokens_out: u64,

    /// Summed thinking tokens (explicit mode; drives the thinking overhead).
    #[arg(long, default_value_t = 0)]
    tokens_thinking: u64,

    /// Correctness percentage `C` (0–100); defaults to a projected full clear.
    #[arg(long, default_value_t = 100.0)]
    correctness: f64,

    /// Summed hidden-suite run time `S` in seconds (floored at 1.0s). Defaults:
    /// live mode assumes the web HUD's 2s, so both project the same number;
    /// explicit mode floors at 1s (the parity fixture's framing).
    #[arg(long)]
    seconds: Option<f64>,

    /// Harness used; `cursor` adds the +0.20 Composer modifier (explicit mode).
    #[arg(long, default_value = "claude_code_cli")]
    harness: String,
}

pub fn run(
    client: &DaemonClient,
    manifest: Option<&Manifest>,
    args: ScoreArgs,
    style: Style,
) -> anyhow::Result<CommandExit> {
    if args.model.is_some() {
        let input = explicit_input(&args, manifest)?;
        let result = scoring::score_submission(&input, None);
        // Explicit mode has no cache concept — the flags don't carry one.
        print!("{}", render_score(&result, 0, style));
        return Ok(CommandExit::Success);
    }

    // Live mode: project from the daemon's captured turns.
    let snapshot = client.session()?;
    // A stopped session still projects — `promptly stop` sends you here for the
    // final number — so read any session bound to this workspace, active or stopped;
    // only an idle daemon (nothing started here) has no marker to score.
    let Some(marker) = snapshot.session else {
        println!(
            "{}",
            style.yellow(
                "no capture session — run `promptly start`, then score after some turns \
                 (or pass --model … to score an explicit vector)",
            ),
        );
        return Ok(CommandExit::Failure);
    };

    let mut attempt = LiveAttempt::new();
    for turn in &snapshot.captured {
        attempt.observe(turn);
    }
    let challenge_type =
        challenge_type_for(&args, manifest).unwrap_or_else(|| DEFAULT_CHALLENGE_TYPE.to_string());
    let overrides = manifest.and_then(|m| m.token_weight_overrides.clone());
    let seconds = args.seconds.unwrap_or(DEFAULT_PROJECTED_EXECUTION_SECONDS);
    let result = attempt.project(
        &challenge_type,
        overrides.as_ref(),
        args.correctness,
        seconds,
    );

    // Context before the number: warn if this folder is bound to a different
    // level, and how old the session is (a stale resumed capture reads inflated).
    if let Some(warn) = session_view::mismatch_warning(&marker, manifest, style) {
        println!("{warn}");
    }
    let resumed = !snapshot.captured.is_empty();
    println!(
        "{}",
        session_view::age_line(&marker, promptlyd::clock::now_ms(), resumed, style),
    );
    println!(
        "{}",
        style.dim(&format!(
            "live projection · {} · {} turns · assumes a clear · 2s exec",
            marker.slug,
            attempt.turns(),
        )),
    );
    print!("{}", render_score(&result, attempt.cache_tokens(), style));
    Ok(CommandExit::Success)
}

/// Build the explicit-mode score input from flags, taking the challenge type from
/// the manifest when not given.
fn explicit_input(args: &ScoreArgs, manifest: Option<&Manifest>) -> anyhow::Result<ScoreInput> {
    let model = args.model.clone().expect("explicit mode requires --model");
    let challenge_type = challenge_type_for(args, manifest).ok_or_else(|| {
        anyhow::anyhow!("--challenge-type is required with --model (or run inside a workspace)")
    })?;
    Ok(ScoreInput {
        correctness_pct: args.correctness,
        prompt_count: args.prompts.unwrap_or(1) as f64,
        tokens: Tokens {
            input: args.tokens_in as f64,
            output: args.tokens_out as f64,
            thinking: args.tokens_thinking as f64,
        },
        execution_time_seconds: args.seconds.unwrap_or(1.0),
        challenge_type,
        model_identifier: model,
        harness_used: args.harness.clone(),
    })
}

/// The challenge type to score with: the flag wins, else a non-empty manifest value.
fn challenge_type_for(args: &ScoreArgs, manifest: Option<&Manifest>) -> Option<String> {
    args.challenge_type.clone().or_else(|| {
        manifest
            .map(|m| m.challenge_type.clone())
            .filter(|s| !s.is_empty())
    })
}

/// Render the score and its five-factor breakdown from the result alone,
/// surfacing every floored input (the factors the HUD shows, `11`). `cache` is
/// the run's cache-token total — not a scoring input, shown for context when
/// non-zero (usually the dominant usage on a real run); pass 0 when unknown.
pub fn render_score(result: &ScoreResult, cache: u64, style: Style) -> String {
    let b = &result.breakdown;
    let w_c = scoring::constants().w_c;
    let pct = if w_c != 0.0 {
        b.correctness_value / w_c
    } else {
        0.0
    };
    let mut out = String::new();

    out.push_str(&format!("{}\n", visual::header(style, "projected score")));
    out.push_str(&format!(
        "  {}\n",
        style.bold(&style.accent(&fmt::score(result.score))),
    ));
    // Pad the label to the column width BEFORE styling: `{:<12}` over an
    // ANSI-wrapped string would count the (zero-width) escape bytes and collapse
    // the padding on a real TTY, misaligning the breakdown only when colored.
    let label = |name: &str| style.dim(&format!("{name:<12}"));
    out.push_str(&format!(
        "  {} C={pct:.0}%  {}  × W_c {}\n",
        label("correctness"),
        correctness_meter(pct, style),
        fmt::compact(w_c),
    ));
    out.push_str(&format!(
        "  {} P={}{}\n",
        label("prompts"),
        fmt::compact(b.prompts_effective),
        floor_tag(b.prompts_floored, style),
    ));
    out.push_str(&format!(
        "  {} M_effort {}  (base {}{}{}){}  [{}]\n",
        label("effort"),
        fmt::compact(b.effort.value),
        fmt::compact(b.effort.base),
        signed_part("thinking", b.effort.thinking_overhead),
        signed_part("composer", b.effort.composer_modifier),
        clamp_tag(b.effort.clamped, style),
        model_label(result, style),
    ));
    out.push_str(&format!(
        "  {} in {} ·{}  out {} ·{}  think {} ·{}  → weighted {}{}{}\n",
        label("tokens"),
        fmt::compact(b.tokens.input),
        fmt::compact(b.weights.w_in),
        fmt::compact(b.tokens.output),
        fmt::compact(b.weights.w_out),
        fmt::compact(b.tokens.thinking),
        fmt::compact(b.weights.w_think),
        fmt::compact(b.effective_weighted),
        floor_tag(b.tokens_floored, style),
        session_view::cache_note(cache, style),
    ));
    // The same tokens as a composition bar, so where the burn went is visible
    // at a glance (input `█` / output `▓` / thinking `▒`).
    let mix = visual::token_mix(
        style,
        24,
        b.tokens.input,
        b.tokens.output,
        b.tokens.thinking,
    );
    if !mix.is_empty() {
        out.push_str(&format!(
            "  {} {}  {}\n",
            label(""),
            mix,
            visual::token_mix_legend(style),
        ));
    }
    out.push_str(&format!(
        "  {} S={}s{}\n",
        label("speed"),
        fmt::compact(b.seconds_effective),
        floor_tag(b.speed_floored, style),
    ));

    if result.baseline_floor_fallback {
        out.push_str(&format!(
            "{}\n",
            style.yellow("note: model not in the economics matrix — scored at the baseline floor"),
        ));
    }
    out
}

fn model_label(result: &ScoreResult, style: Style) -> String {
    let model = &result.breakdown.effort.model_identifier;
    if result.baseline_floor_fallback {
        style.yellow(&format!("{model} (unrecognized → baseline floor)"))
    } else {
        model.clone()
    }
}

/// The correctness percentage as a colored meter: green for a full clear,
/// yellow for a partial one, red when most of the suite fails.
fn correctness_meter(pct: f64, style: Style) -> String {
    let bar = visual::meter(pct / 100.0, 16);
    if pct >= 99.5 {
        style.green(&bar)
    } else if pct >= 50.0 {
        style.yellow(&bar)
    } else {
        style.red(&bar)
    }
}

fn floor_tag(floored: bool, style: Style) -> String {
    if floored {
        format!(" {}", style.yellow("(floored)"))
    } else {
        String::new()
    }
}

fn clamp_tag(clamped: bool, style: Style) -> String {
    if clamped {
        format!(" {}", style.yellow("(clamped)"))
    } else {
        String::new()
    }
}

fn signed_part(label: &str, value: f64) -> String {
    if value > 0.0 {
        format!(" + {label} {}", fmt::compact(value))
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_args() -> ScoreArgs {
        ScoreArgs {
            model: Some("claude-sonnet-4-6".into()),
            challenge_type: Some("debugging".into()),
            prompts: Some(2),
            tokens_in: 5000,
            tokens_out: 3000,
            tokens_thinking: 0,
            correctness: 100.0,
            seconds: Some(4.0),
            harness: "claude_code_cli".into(),
        }
    }

    #[test]
    fn explicit_mode_reproduces_the_anchor_parity_vector() {
        let input = explicit_input(&anchor_args(), None).unwrap();
        let result = scoring::score_submission(&input, None);
        assert!((result.score - 183823.5294117647).abs() / result.score < 1e-9);
        let text = render_score(&result, 0, Style::plain());
        assert!(text.contains("183,823.53"), "{text}");
        assert!(text.contains("C=100%"));
        assert!(text.contains("weighted 6800"));
        // Zero cache stays uncluttered — no cache note in explicit mode.
        assert!(!text.contains("cache"), "{text}");
        assert!(!text.contains('\u{1b}'), "plain mode has no escapes");
    }

    #[test]
    fn the_cache_total_shows_on_the_token_line_when_present() {
        let input = explicit_input(&anchor_args(), None).unwrap();
        let result = scoring::score_submission(&input, None);
        let text = render_score(&result, 128_000, Style::plain());
        assert!(text.contains("cache 128,000"), "{text}");
    }

    #[test]
    fn explicit_mode_requires_a_challenge_type_without_a_manifest() {
        let mut args = anchor_args();
        args.challenge_type = None;
        assert!(explicit_input(&args, None).is_err());
    }

    #[test]
    fn an_unrecognized_model_is_flagged_in_the_render() {
        let mut args = anchor_args();
        args.model = Some("mystery".into());
        let input = explicit_input(&args, None).unwrap();
        let result = scoring::score_submission(&input, None);
        assert!(result.baseline_floor_fallback);
        assert!(render_score(&result, 0, Style::plain()).contains("baseline floor"));
    }
}
