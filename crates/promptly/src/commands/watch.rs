//! `promptly watch` — stream the daemon's live token feed to the terminal during
//! an attempt: per-turn tokens, running totals, and a live projected score (`19`,
//! over the `17` live stream).
//!
//! The projection flows through the shared parity port, so the number tracks what
//! the server will assign (assuming a clear, run time floored — the HUD framing,
//! `11`). Token weights come from the workspace manifest's
//! `challenge_type`/`token_weight_overrides`.

use std::collections::HashMap;

use crate::daemon_client::{DaemonApi, DaemonClient, NormalizedTurn};
use crate::fmt;
use crate::projection::{LiveAttempt, DEFAULT_CHALLENGE_TYPE};
use crate::style::Style;
use crate::CommandExit;

use promptlyd::manifest::Manifest;
use promptlyd::model::Confidence;

pub fn run(
    client: &DaemonClient,
    manifest: Option<&Manifest>,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let snapshot = client.session()?;
    let Some(marker) = snapshot.session.clone().filter(|m| m.is_active()) else {
        println!(
            "{}",
            style.yellow("no active capture session — run `promptly start` first"),
        );
        return Ok(CommandExit::Failure);
    };

    let challenge_type = manifest
        .map(|m| m.challenge_type.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CHALLENGE_TYPE.to_string());
    let overrides = manifest.and_then(|m| m.token_weight_overrides.clone());

    println!(
        "{} {} {}",
        style.dim("watching"),
        style.accent(&marker.slug),
        style.dim("— Ctrl-C to stop"),
    );

    let mut attempt = LiveAttempt::new();
    for turn in &snapshot.captured {
        attempt.observe(turn);
    }
    print!(
        "{}",
        render_projected(&attempt, &challenge_type, overrides.as_ref(), style)
    );

    // Block on the live stream; each captured turn updates the totals + score.
    let stream = client.stream()?;
    for item in stream {
        match item {
            Ok(turn) => {
                attempt.observe(&turn);
                print!("{}", render_turn(&turn, style));
                print!(
                    "{}",
                    render_projected(&attempt, &challenge_type, overrides.as_ref(), style)
                );
            }
            Err(err) => {
                eprintln!("{} {err}", style.yellow("stream ended:"));
                break;
            }
        }
    }
    Ok(CommandExit::Success)
}

/// One per-turn line: model, the turn's tokens, and the capture confidence.
pub fn render_turn(turn: &NormalizedTurn, style: Style) -> String {
    let model = if turn.model.is_empty() {
        "estimated"
    } else {
        turn.model.as_str()
    };
    format!(
        "  {} {}  in {} · out {} · think {}  {}\n",
        style.dim("↳ turn"),
        style.accent(model),
        fmt::thousands(turn.tokens_input as u128),
        fmt::thousands(turn.tokens_output as u128),
        fmt::thousands(turn.tokens_thinking as u128),
        style.dim(&format!("[{}]", confidence_label(turn.confidence))),
    )
}

/// The running totals + projected score line.
pub fn render_projected(
    attempt: &LiveAttempt,
    challenge_type: &str,
    overrides: Option<&HashMap<String, f64>>,
    style: Style,
) -> String {
    let result = attempt.project(challenge_type, overrides, 100.0, 0.0);
    let tokens = attempt.tokens();
    format!(
        "  {} {} prompts · {} in / {} out / {} think → {} {}\n",
        style.dim("Σ"),
        attempt.prompt_count(),
        fmt::thousands(tokens.input as u128),
        fmt::thousands(tokens.output as u128),
        fmt::thousands(tokens.thinking as u128),
        style.dim("projected"),
        style.bold(&style.accent(&fmt::score(result.score))),
    )
}

fn confidence_label(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Otel => "otel",
        Confidence::Jsonl => "jsonl",
        Confidence::Estimated => "estimated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use promptlyd::model::{Agreement, Plausibility, Source};

    fn turn(model: &str, prompt: Option<&str>, input: u64, output: u64) -> NormalizedTurn {
        NormalizedTurn {
            schema_version: 1,
            turn_id: format!("{model}-{input}"),
            model: model.to_string(),
            harness: "claude_code_cli".to_string(),
            tokens_input: input,
            tokens_output: output,
            tokens_thinking: 0,
            tokens_cache: 0,
            prompt_id: prompt.map(str::to_string),
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
    fn render_turn_shows_model_tokens_and_confidence() {
        let line = render_turn(
            &turn("claude-opus-4-8", Some("p"), 1200, 800),
            Style::plain(),
        );
        assert!(line.contains("claude-opus-4-8"));
        assert!(line.contains("in 1,200"));
        assert!(line.contains("out 800"));
        assert!(line.contains("[otel]"));
    }

    #[test]
    fn estimated_turns_label_the_model_estimated() {
        let line = render_turn(&turn("", None, 10, 10), Style::plain());
        assert!(line.contains("estimated"));
    }

    #[test]
    fn projected_line_grows_with_observed_turns() {
        let mut attempt = LiveAttempt::new();
        attempt.observe(&turn("claude-sonnet-4-6", Some("p1"), 5000, 3000));
        let line = render_projected(&attempt, "debugging", None, Style::plain());
        assert!(line.contains("1 prompts"));
        assert!(line.contains("5,000 in"));
        assert!(line.contains("projected"));
        // A non-trivial finite number is rendered.
        assert!(line.chars().any(|c| c.is_ascii_digit()));
    }
}
