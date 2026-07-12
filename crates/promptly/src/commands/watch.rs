//! `promptly watch` — stream the daemon's live token feed to the terminal during
//! an attempt: per-turn tokens, running totals, and a live projected score (`19`,
//! over the `17` live stream).
//!
//! The projection flows through the shared parity port, so the number tracks what
//! the server will assign (assuming a clear and the HUD's 2s run time — the HUD
//! framing, `11`). Token weights come from the workspace manifest's
//! `challenge_type`/`token_weight_overrides`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::commands::session_view;
use crate::daemon_client::{DaemonApi, DaemonClient, DaemonError, NormalizedTurn, SessionMarker};
use crate::daemon_process;
use crate::fmt;
use crate::projection::{LiveAttempt, DEFAULT_CHALLENGE_TYPE, DEFAULT_PROJECTED_EXECUTION_SECONDS};
use crate::style::Style;
use crate::visual;
use crate::CommandExit;

use promptlyd::manifest::Manifest;
use promptlyd::model::Confidence;

pub fn run(
    client: &DaemonClient,
    manifest: Option<&Manifest>,
    style: Style,
) -> anyhow::Result<CommandExit> {
    // Watch is strictly read-only: no daemon means nothing to attach to — report
    // and fail rather than launching (or worse, rescoping) anything.
    let snapshot = match client.session() {
        Ok(snapshot) => snapshot,
        Err(DaemonError::NotRunning(_)) => {
            println!("{}", style.yellow(NO_DAEMON_LINE));
            return Ok(CommandExit::Failure);
        }
        Err(err) => return Err(err.into()),
    };
    let Some(marker) = snapshot.session.clone().filter(|m| m.is_active()) else {
        println!("{}", style.yellow(NO_SESSION_LINE));
        return Ok(CommandExit::Failure);
    };

    let challenge_type = manifest
        .map(|m| m.challenge_type.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CHALLENGE_TYPE.to_string());
    let overrides = manifest.and_then(|m| m.token_weight_overrides.clone());

    println!(
        "{}",
        visual::header(style, &format!("watching {}", marker.slug)),
    );
    println!("  {}", style.dim("Ctrl-C to stop"));

    // Context before the numbers: warn if this folder is bound to a different
    // level, and show how old the session is (a stale resumed capture is common).
    if let Some(warn) = session_view::mismatch_warning(&marker, manifest, style) {
        println!("  {warn}");
    }
    let resumed = !snapshot.captured.is_empty();
    println!(
        "  {}",
        session_view::age_line(&marker, promptlyd::clock::now_ms(), resumed, style),
    );
    // Read-only watch attaches to the session wherever it lives — say so when
    // that isn't the folder the command ran from.
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if let Some(note) = foreign_workspace_note(&marker, &cwd, style) {
        println!("  {note}");
    }

    // Seed from the snapshot, remembering each turn_id so a streamed replay of an
    // already-captured turn can't double-count into the totals/score.
    let mut seen: HashSet<String> = HashSet::with_capacity(snapshot.captured.len());
    let mut attempt = LiveAttempt::new();
    let mut history: Vec<u64> = Vec::with_capacity(snapshot.captured.len());
    for turn in &snapshot.captured {
        fold_turn(turn, &mut seen, &mut attempt, &mut history);
    }
    print!(
        "{}",
        render_projected(
            &attempt,
            &history,
            &challenge_type,
            overrides.as_ref(),
            style
        )
    );

    // Block on the live stream; each captured turn updates the totals + score.
    let stream = client.stream()?;
    for item in stream {
        match item {
            Ok(turn) => {
                // The stream opens after the snapshot, so it can replay a turn we
                // already seeded; `fold_turn` returns false for a duplicate, so
                // skip it rather than double-count a resumed session's turns.
                if !fold_turn(&turn, &mut seen, &mut attempt, &mut history) {
                    continue;
                }
                // On a TTY the scoreboard block is live: erase the previous one
                // (cursor up + clear per line) so it re-renders beneath the
                // newest turn instead of duplicating down the scrollback.
                // Piped/plain output stays append-only.
                if style.is_enabled() {
                    print!("{}", "\x1b[1A\x1b[2K".repeat(SCOREBOARD_LINES));
                }
                print!("{}", render_turn(&turn, style));
                print!(
                    "{}",
                    render_projected(
                        &attempt,
                        &history,
                        &challenge_type,
                        overrides.as_ref(),
                        style
                    )
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

/// Shown when nothing answers on the daemon's control port. Watch never launches
/// the daemon itself — it only observes — so it points at the commands that do.
const NO_DAEMON_LINE: &str = "the capture daemon isn't running — `promptly start` in your level \
                              workspace (or `promptly play <level>`) begins a scored session";

/// Shown when the daemon is up but no capture session is active.
const NO_SESSION_LINE: &str = "no active capture session — run `promptly start` in your level \
                               workspace (or `promptly play <level>`)";

/// A dim note when the attached session is bound to a different folder than the
/// cwd — watch is read-only and attaches to wherever the session lives, so the
/// numbers belong to that folder's attempt, not this one's.
fn foreign_workspace_note(marker: &SessionMarker, cwd: &Path, style: Style) -> Option<String> {
    let bound = marker.workspace.to_string_lossy();
    if daemon_process::same_workspace(&bound, cwd) {
        return None;
    }
    Some(style.dim(&format!(
        "watching the session bound to {} (you're in {}) — output reflects that session",
        marker.workspace.display(),
        cwd.display(),
    )))
}

/// How many lines [`render_projected`] prints — the block the TTY redraw erases.
const SCOREBOARD_LINES: usize = 2;

/// How many recent turns the scoreboard's sparkline shows.
const SPARK_WINDOW: usize = 24;

/// One turn's total burn (the sparkline's unit).
fn turn_total(turn: &NormalizedTurn) -> u64 {
    turn.tokens_input
        .saturating_add(turn.tokens_output)
        .saturating_add(turn.tokens_thinking)
}

/// Fold one turn into the running state, deduping on `turn_id`. Returns `false`
/// (and changes nothing) when the id was already counted, so a seed→stream replay
/// can't double-count. Extracted so the dedup is unit-testable without a socket.
fn fold_turn(
    turn: &NormalizedTurn,
    seen: &mut HashSet<String>,
    attempt: &mut LiveAttempt,
    history: &mut Vec<u64>,
) -> bool {
    if !seen.insert(turn.turn_id.clone()) {
        return false;
    }
    attempt.observe(turn);
    history.push(turn_total(turn));
    true
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

/// The live scoreboard: running totals, then a per-turn burn sparkline beside
/// the projected score. Always [`SCOREBOARD_LINES`] lines, so the TTY redraw
/// erases exactly what was printed.
pub fn render_projected(
    attempt: &LiveAttempt,
    history: &[u64],
    challenge_type: &str,
    overrides: Option<&HashMap<String, f64>>,
    style: Style,
) -> String {
    let result = attempt.project(
        challenge_type,
        overrides,
        100.0,
        DEFAULT_PROJECTED_EXECUTION_SECONDS,
    );
    let tokens = attempt.tokens();
    let spark = visual::spark(history, SPARK_WINDOW);
    let trend = if spark.is_empty() {
        String::new()
    } else {
        format!("{} ", style.accent(&spark))
    };
    format!(
        "  {} {} prompts · {} in / {} out / {} think{}\n  {}{} {}\n",
        style.dim("Σ"),
        attempt.prompt_count(),
        fmt::thousands(tokens.input as u128),
        fmt::thousands(tokens.output as u128),
        fmt::thousands(tokens.thinking as u128),
        session_view::cache_note(attempt.cache_tokens(), style),
        trend,
        // The projection assumes a full clear and the web HUD's 2s run time, so
        // this number matches the browser's — label both assumptions.
        style.dim("projected (assumes clear · 2s exec)"),
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
    fn projected_scoreboard_grows_with_observed_turns() {
        let mut attempt = LiveAttempt::new();
        attempt.observe(&turn("claude-sonnet-4-6", Some("p1"), 5000, 3000));
        let board = render_projected(&attempt, &[8000], "debugging", None, Style::plain());
        assert!(board.contains("1 prompts"));
        assert!(board.contains("5,000 in"));
        assert!(board.contains("projected"));
        // A non-trivial finite number is rendered.
        assert!(board.chars().any(|c| c.is_ascii_digit()));
        // The per-turn sparkline rides next to the projection.
        assert!(board.contains('█'));
        // The block is exactly the advertised height, so the TTY redraw erases
        // precisely what was printed.
        assert_eq!(board.lines().count(), SCOREBOARD_LINES);
    }

    #[test]
    fn a_streamed_duplicate_of_a_seeded_turn_is_not_double_counted() {
        let mut seen = HashSet::new();
        let mut attempt = LiveAttempt::new();
        let mut history = Vec::new();
        let seeded = turn("claude-opus-4-8", Some("p1"), 1000, 500);
        // Seed it, then have the "stream" replay the very same turn_id.
        assert!(fold_turn(&seeded, &mut seen, &mut attempt, &mut history));
        assert!(
            !fold_turn(&seeded, &mut seen, &mut attempt, &mut history),
            "a duplicate turn_id folds nothing in"
        );
        // A genuinely new turn still counts.
        assert!(fold_turn(
            &turn("claude-opus-4-8", Some("p2"), 200, 100),
            &mut seen,
            &mut attempt,
            &mut history,
        ));
        // Exactly the two distinct turns are counted, not the replay.
        assert_eq!(attempt.turns(), 2);
        assert_eq!(attempt.tokens().input, 1200.0);
        assert_eq!(attempt.tokens().output, 600.0);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn scoreboard_shows_cache_and_labels_the_projection_a_ceiling() {
        let mut attempt = LiveAttempt::new();
        let mut cached = turn("claude-sonnet-4-6", Some("p1"), 5000, 3000);
        cached.tokens_cache = 40_000;
        attempt.observe(&cached);
        let board = render_projected(&attempt, &[8000], "debugging", None, Style::plain());
        // The dominant cache usage is surfaced as a note.
        assert!(board.contains("cache 40,000"), "{board}");
        // The projection names both of its assumptions — a clear, and the web
        // HUD's 2s run time — so the number reads as the browser's twin.
        assert!(
            board.contains("projected (assumes clear · 2s exec)"),
            "{board}"
        );
        // Still exactly the advertised height, so the TTY redraw stays correct.
        assert_eq!(board.lines().count(), SCOREBOARD_LINES);
    }

    #[test]
    fn an_empty_history_renders_a_scoreboard_without_a_sparkline() {
        let attempt = LiveAttempt::new();
        let board = render_projected(&attempt, &[], "debugging", None, Style::plain());
        assert_eq!(board.lines().count(), SCOREBOARD_LINES);
        assert!(board.contains("projected"));
        assert!(!board.contains('▁'));
    }

    #[test]
    fn the_guidance_lines_name_start_and_play_but_never_a_daemon_launch() {
        for line in [NO_DAEMON_LINE, NO_SESSION_LINE] {
            assert!(line.contains("promptly start"), "{line}");
            assert!(line.contains("promptly play"), "{line}");
        }
    }

    fn session_marker(workspace: std::path::PathBuf) -> SessionMarker {
        SessionMarker {
            version: 1,
            session_id: "s1".into(),
            workspace,
            level_id: "lvl-1".into(),
            slug: "stage-1-01".into(),
            started_at_ms: 1_000,
            stopped_at_ms: None,
            attempt_nonce: "n".into(),
            nonce_origin: promptlyd::scoping::NonceOrigin::Local,
            file_allowlist: Vec::new(),
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    #[test]
    fn foreign_workspace_note_fires_only_when_the_folders_differ() {
        let dir = std::env::temp_dir();
        let marker = session_marker(dir.clone());
        // The same folder — even spelled differently — is not foreign.
        assert!(foreign_workspace_note(&marker, &dir.join("."), Style::plain()).is_none());
        // A different folder gets the note, naming where the session lives.
        let elsewhere = dir.join("promptly-watch-elsewhere-xyz");
        let note = foreign_workspace_note(&marker, &elsewhere, Style::plain())
            .expect("a different cwd is noted");
        assert!(note.contains(&dir.display().to_string()), "{note}");
        assert!(note.contains("you're in"), "{note}");
    }
}
