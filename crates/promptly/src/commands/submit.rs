//! `promptly submit` / `pair` — ranked auto-submission and device pairing.
//!
//! `submit` is the daemon path's finish line (`20`): it packages the solution
//! locally (mirroring `10`), **redacts** any secret-shaped spans before anything
//! leaves the machine, reads the active capture session from the daemon (the turns
//! to sign + the server-issued attempt nonce), and hands it all to the cloud seam,
//! which signs the turn chain and uploads for ranked grading. It then polls the
//! grade and compares it to the local best-case projection (the parity check).
//! `pair` drives the device-authorization flow through the same seam.

use std::path::Path;

use clap::Args;
use promptlyd::manifest::Manifest;
use promptlyd::model::{Agreement, Confidence, Plausibility};

use crate::cloud::{parity_report, CaptureUpload, Cloud, CloudError, GradedScore, ParityReport};
use crate::daemon_client::DaemonApi;
use crate::fmt;
use crate::projection::{LiveAttempt, DEFAULT_CHALLENGE_TYPE};
use crate::prompt::Ask;
use crate::redaction::{self, RedactionError};
use crate::style::Style;
use crate::submission::{self, SubmissionBundle};
use crate::visual;
use crate::CommandExit;

/// How many times to poll for the grade before leaving it to finish in the
/// background, and how long to wait between polls.
const GRADE_POLLS: u32 = 30;
const GRADE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Args)]
pub struct SubmitArgs {
    /// Skip the confirmation prompt for a clean capture (non-interactive submits).
    #[arg(long)]
    yes: bool,
    /// Submit even when the capture carries integrity warnings (cross-source
    /// disagreements or implausible turns). Required to push a flagged capture
    /// non-interactively; implies `--yes`.
    #[arg(long)]
    force: bool,
}

pub fn run_submit(
    workspace: &Path,
    manifest: Option<&Manifest>,
    daemon: &dyn DaemonApi,
    cloud: &dyn Cloud,
    asker: &mut dyn Ask,
    args: SubmitArgs,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let Some(manifest) = manifest else {
        println!(
            "{}",
            style.red("not a Promptly workspace — run `promptly init <level>` first"),
        );
        return Ok(CommandExit::Failure);
    };

    let bundle = match submission::gather_submission(workspace, manifest) {
        Ok(bundle) => bundle,
        Err(err) => {
            println!("{} {err}", style.red("submission invalid:"));
            return Ok(CommandExit::Failure);
        }
    };

    // Redact secret-shaped spans before the solution leaves the machine; abort the
    // whole upload if a high-confidence secret can't be cleanly bounded.
    let redacted = match redaction::redact_bundle(&bundle) {
        Ok(redacted) => redacted,
        Err(RedactionError::Uncleanable(category)) => {
            println!(
                "{} an unredactable {category} block was found in the solution — remove it and resubmit",
                style.red("blocked:"),
            );
            return Ok(CommandExit::Failure);
        }
    };
    print!("{}", render_bundle(&redacted.bundle, style));
    if !redacted.categories.is_empty() {
        println!(
            "{} {}",
            style.yellow("redacted before upload:"),
            redacted.categories.join(", "),
        );
    }

    // The captured session supplies the turns to sign and the bound attempt nonce.
    let snapshot = match daemon.session() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            println!("{} {err}", style.red("daemon:"));
            return Ok(CommandExit::Failure);
        }
    };
    // A stopped session is the normal finish line: `promptly stop` closes the
    // capture window (stamping `stopped_at_ms`) and then points you here, but the
    // signed turns and the bound nonce are still on the marker. So accept any
    // session bound to this workspace — active or stopped — and only bail when the
    // daemon is truly idle (nothing was ever started here) and has no marker to sign.
    let Some(marker) = snapshot.session else {
        println!(
            "{}",
            style.yellow(
                "no capture session for this workspace — run `promptly start <level>` and solve before submitting",
            ),
        );
        return Ok(CommandExit::Failure);
    };
    if marker.slug != manifest.slug {
        println!(
            "{} the daemon is capturing {} but this workspace is {} — start a session here first",
            style.red("level mismatch:"),
            marker.slug,
            manifest.slug,
        );
        return Ok(CommandExit::Failure);
    }

    let capture = CaptureUpload {
        turns: &snapshot.captured,
        attempt_nonce: Some(&marker.attempt_nonce),
        telemetry_session_id: &marker.session_id,
        // The signed capture summary: the daemon's session provenance the server's
        // trust policy reads to decide the verified tier, plus the prompt count `P`
        // it scores against.
        capture_summary: build_capture_summary(
            &marker,
            &snapshot.signals,
            session_prompt_count(&snapshot.captured),
        ),
    };

    // Read the capture's integrity signals (cross-source agreement + plausibility)
    // and show them. The server re-derives the authoritative verdict at grade time;
    // this is the local fail-safe so a player consciously sees — and must accept — a
    // capture that carries tampering fingerprints before it's recorded as ranked.
    let integrity = CaptureIntegrity::of(&snapshot.captured);
    print!("{}", integrity.render(style));

    // Show the integrity tier this capture will most likely receive before the
    // irreversible ranked submit, so the player knows whether it earns the verified
    // badge (and why not, if not) rather than finding out after grading.
    let (tier, tier_reason) = projected_tier(&marker, &snapshot.captured);
    if tier == "verified" {
        println!(
            "{} {}",
            style.green("capture tier: verified-eligible"),
            style.dim(&format!("— {tier_reason}")),
        );
    } else {
        println!(
            "{} {}",
            style.yellow("capture tier: unverified"),
            style.dim(&format!(
                "— {tier_reason}; it still ranks, without the badge"
            )),
        );
    }

    // A ranked submission is irreversible — it records an attempt against your
    // account and posts to the leaderboard — so confirm before anything leaves the
    // machine. Enter defaults to no.
    let confirmed = if integrity.flagged() {
        // A flagged capture fails closed: the routine `--yes` is deliberately not
        // enough (it would let a scripted run push a tampered capture silently). You
        // must `--force`, or acknowledge the warning at an interactive prompt.
        args.force
            || asker.confirm(
                &format!(
                    "This capture shows integrity warnings ({}). Submit it for '{}' for ranked \
                     grading anyway? The server re-checks these signals and can reject or flag a \
                     tampered capture.",
                    integrity.summary_phrase(),
                    manifest.slug,
                ),
                false,
                false,
            )
    } else {
        args.yes
            || args.force
            || asker.confirm(
                &format!(
                    "Submit this solution for '{}' for ranked grading? \
                     This records a ranked attempt and can't be undone.",
                    manifest.slug,
                ),
                false,
                false,
            )
    };
    if !confirmed {
        let message = if integrity.flagged() {
            "not submitted — capture shows integrity warnings; re-capture, or pass --force to submit anyway"
        } else {
            "not submitted — nothing was uploaded"
        };
        println!("{}", style.dim(message));
        return Ok(CommandExit::Failure);
    }

    let receipt = match cloud.submit(&manifest.slug, &redacted.bundle, &capture) {
        Ok(receipt) => receipt,
        Err(CloudError::NotPaired) => {
            println!(
                "{}",
                style.yellow(
                    "validated locally — ranked submission needs a paired device: run `promptly pair`",
                ),
            );
            return Ok(CommandExit::Failure);
        }
        Err(err) => {
            println!("{} {err}", style.red("submission failed:"));
            return Ok(CommandExit::Failure);
        }
    };
    println!(
        "{} {} {}",
        style.green("submitted"),
        style.dim(&format!("({})", receipt.submission_id)),
        style.dim(&format!("— grading: {}", receipt.status)),
    );

    // The local best-case projection (assumes a clear, run time floored) — unlike
    // `watch`/`score`, which assume the HUD's 2s, this one must upper-bound the
    // graded score, so it uses the scoring floor (see `project_best_case`).
    let projected = project_best_case(manifest, &snapshot.captured);
    print!(
        "{}",
        await_and_report_parity(cloud, &receipt.submission_id, projected, style),
    );
    Ok(CommandExit::Success)
}

pub fn run_pair(cloud: &dyn Cloud, style: Style) -> anyhow::Result<CommandExit> {
    match cloud.pair() {
        Ok(()) => {
            println!("{}", style.green("device paired"));
            Ok(CommandExit::Success)
        }
        Err(err) => {
            println!("{} {err}", style.red("pairing failed:"));
            Ok(CommandExit::Failure)
        }
    }
}

/// A quick read of the capture's integrity signals over the turns being submitted,
/// for the submit-time fail-safe. The daemon derives these per turn; here they are
/// only counted and surfaced — the server re-derives the binding verdict (`25`).
struct CaptureIntegrity {
    total: usize,
    /// Turns where OTEL and JSONL observed the same turn but disagreed on the model
    /// or token counts — a cross-source tampering/forgery fingerprint.
    disagreements: usize,
    /// Turns flagged implausible (zero tokens, or tokens with zero cost/duration).
    low_plausibility: usize,
    /// Turns whose model couldn't be resolved or whose counts were inferred — a
    /// confidence tier, surfaced but not treated as a tampering signal on its own.
    estimated: usize,
    /// Reasons the turn timing is implausibly paced (backwards jumps, impossible
    /// bursts) — the fingerprint of a fabricated or replayed capture (`25`).
    pacing: Vec<String>,
}

impl CaptureIntegrity {
    fn of(turns: &[crate::daemon_client::NormalizedTurn]) -> Self {
        let mut me = Self {
            total: turns.len(),
            disagreements: 0,
            low_plausibility: 0,
            estimated: 0,
            pacing: promptlyd::pacing::pacing_reasons(turns),
        };
        for turn in turns {
            if matches!(turn.agreement, Agreement::Disagree { .. }) {
                me.disagreements += 1;
            }
            if matches!(turn.plausibility, Plausibility::Low { .. }) {
                me.low_plausibility += 1;
            }
            if matches!(turn.confidence, Confidence::Estimated) {
                me.estimated += 1;
            }
        }
        me
    }

    /// Whether the capture carries a tampering fingerprint that should gate submit.
    /// A confidence downgrade (`estimated`) alone doesn't — adapters legitimately
    /// report it — so only active disagreements and implausible turns flag.
    fn flagged(&self) -> bool {
        self.disagreements > 0 || self.low_plausibility > 0 || !self.pacing.is_empty()
    }

    fn summary_phrase(&self) -> String {
        let mut parts = Vec::new();
        if self.disagreements > 0 {
            parts.push(format!(
                "{} cross-source disagreement(s)",
                self.disagreements
            ));
        }
        if self.low_plausibility > 0 {
            parts.push(format!("{} implausible turn(s)", self.low_plausibility));
        }
        if !self.pacing.is_empty() {
            parts.push(format!("implausible pacing ({})", self.pacing.join("; ")));
        }
        parts.join(", ")
    }

    fn render(&self, style: Style) -> String {
        if self.total == 0 {
            return String::new();
        }
        if self.flagged() {
            return format!(
                "{} {}\n",
                style.yellow("capture integrity:"),
                self.summary_phrase(),
            );
        }
        let mut note = format!(
            "{} {} turn(s) corroborated",
            style.dim("capture:"),
            self.total,
        );
        if self.estimated > 0 {
            note.push_str(&format!(" · {} estimated", self.estimated));
        }
        note.push('\n');
        note
    }
}

/// The session's distinct-prompt tally `P` (`13`), folded from the captured turns
/// exactly as the live projection does (`projection::LiveAttempt`). This is the
/// number the v4 capture summary signs, so the server scores the daemon's real
/// prompt count rather than approximating it with the turn count — an agentic run
/// drives many turns off a single prompt, and `P` counts prompts, not turns.
fn session_prompt_count(turns: &[crate::daemon_client::NormalizedTurn]) -> u32 {
    let mut attempt = LiveAttempt::new();
    for turn in turns {
        attempt.observe(turn);
    }
    attempt.prompt_count().min(u32::MAX as u64) as u32
}

/// Assemble the v4 capture summary the signed chain binds (`20`): the session's
/// nonce origin, baseline attestation, reset count, paste provenance, and the
/// prompt count `P` — read from the daemon's `/session` state and stamped with the
/// signing time. Signing these into the terminal entry makes them tamper-evident;
/// the server's trust policy (`25`) reads them to decide the verified tier (it
/// requires a server-origin nonce and an attested baseline) and scores `P` from the
/// signed count. Pause accounting and the untracked-edit/ignore-file signals aren't
/// tracked yet and report conservatively.
fn build_capture_summary(
    marker: &promptlyd::scoping::SessionMarker,
    signals: &[serde_json::Value],
    prompt_count: u32,
) -> crate::signing::CaptureSummary {
    // Count the bulk-paste provenance signals the daemon raised this session (the
    // "paste the whole answer" fingerprint); the server weighs these for review.
    let bulk_paste_events = signals
        .iter()
        .filter(|s| s.get("kind").and_then(|k| k.as_str()) == Some("bulk_replace"))
        .count() as u32;
    let nonce_origin = match marker.nonce_origin {
        promptlyd::scoping::NonceOrigin::Server => "server",
        promptlyd::scoping::NonceOrigin::Local => "local",
    };
    crate::signing::CaptureSummary {
        baseline_attested: marker.baseline_attested,
        baseline_reset_count: marker.code_reset_count,
        bulk_paste_events,
        // Reserved for a future watcher signal; a clean session reports none.
        ignore_changed: false,
        nonce_origin: nonce_origin.to_string(),
        // Pause accounting is not yet tracked — signed as zero (schema-complete and
        // conservative); the server's coherence check reads the turn timestamps.
        pause_count: 0,
        paused_ms_total: 0,
        prompt_count,
        signed_at_ms: promptlyd::clock::now_ms(),
        started_at_ms: marker.started_at_ms,
        untracked_edit_windows: 0,
    }
}

/// The integrity tier this capture will most likely receive (`25`), with the reason
/// — shown before the ranked submit. Mirrors the server's trust policy on the
/// signals available locally (nonce origin, baseline attestation, per-turn
/// confidence/source); the server re-derives the authoritative verdict from the
/// signed chain, so this is guidance, not a promise. Only OTEL-backed Claude Code
/// with a server nonce and an attested baseline is verified-eligible — every other
/// capture (offline, adapter, JSONL-only, estimated) ranks as `unverified`.
fn projected_tier(
    marker: &promptlyd::scoping::SessionMarker,
    turns: &[crate::daemon_client::NormalizedTurn],
) -> (&'static str, String) {
    use promptlyd::model::{Confidence, Source};
    if marker.nonce_origin == promptlyd::scoping::NonceOrigin::Local {
        return (
            "unverified",
            "started offline (local nonce) — pair and start online to bind a server attempt".into(),
        );
    }
    if !marker.baseline_attested {
        return (
            "unverified",
            "the kit baseline wasn't attested by the server".into(),
        );
    }
    if turns.is_empty() {
        return ("unverified", "no turns were captured".into());
    }
    if turns
        .iter()
        .any(|t| matches!(t.confidence, Confidence::Estimated))
    {
        return (
            "unverified",
            "some turns have estimated token counts (the model couldn't be resolved)".into(),
        );
    }
    if turns.iter().any(|t| {
        t.sources
            .iter()
            .any(|s| !matches!(s, Source::Otel | Source::Jsonl))
    }) {
        return (
            "unverified",
            "captured via a reverse-engineered adapter (Cursor/Codex/Copilot), which isn't \
             verified-eligible"
                .into(),
        );
    }
    if !turns
        .iter()
        .any(|t| t.sources.iter().any(|s| matches!(s, Source::Otel)))
    {
        return (
            "unverified",
            "JSONL-only capture — only OTEL-backed Claude Code earns the verified badge".into(),
        );
    }
    (
        "verified",
        "OTEL-backed Claude Code, server-issued nonce, attested baseline".into(),
    )
}

/// Project the captured turns' best-case score: the manifest's token weights, a
/// full clear, run time floored.
///
/// Deliberately projects with 0.0 seconds (→ the engine's 1.0s floor), NOT
/// [`crate::projection::DEFAULT_PROJECTED_EXECUTION_SECONDS`]: this number is the
/// parity *upper bound* the graded score is checked against, and a graded run can
/// finish faster than the HUD's assumed 2s — projecting with 2s would halve the
/// bound and raise false "server exceeds projection" parity warnings. Don't
/// "harmonize" it with the live projections.
fn project_best_case(manifest: &Manifest, turns: &[crate::daemon_client::NormalizedTurn]) -> f64 {
    let mut attempt = LiveAttempt::new();
    for turn in turns {
        attempt.observe(turn);
    }
    let challenge_type = if manifest.challenge_type.is_empty() {
        DEFAULT_CHALLENGE_TYPE.to_string()
    } else {
        manifest.challenge_type.clone()
    };
    let overrides = manifest.token_weight_overrides.clone();
    attempt
        .project(&challenge_type, overrides.as_ref(), 100.0, 0.0)
        .score
}

/// Poll for the grade (briefly), then render the parity comparison. The submission
/// is already durable, so a slow grade just defers the score — never fails submit.
fn await_and_report_parity(
    cloud: &dyn Cloud,
    submission_id: &str,
    projected: f64,
    style: Style,
) -> String {
    match await_grade(cloud, submission_id) {
        Ok(Some(graded)) => render_parity(&parity_report(projected, &graded), style),
        Ok(None) => format!(
            "{}\n",
            style.dim("still grading — check back shortly for the final score"),
        ),
        Err(err) => format!("{} {err}\n", style.dim("couldn't read the grade:")),
    }
}

/// Poll the grading status until it's terminal or the budget elapses. `Ok(None)`
/// means still in flight (or failed) — not an error.
fn await_grade(cloud: &dyn Cloud, submission_id: &str) -> Result<Option<GradedScore>, CloudError> {
    for poll in 0..GRADE_POLLS {
        let status = cloud.submission_status(submission_id)?;
        if status.is_terminal() {
            // Graded → Some(score); failed (no score) → None (try again later).
            return Ok(status.graded);
        }
        if poll + 1 < GRADE_POLLS {
            std::thread::sleep(GRADE_POLL_INTERVAL);
        }
    }
    Ok(None)
}

/// Render the parity comparison: the local best-case projection vs the server's
/// graded score as two aligned bars scaled to the larger of the pair, flagging
/// an unrecognized model and any parity violation.
fn render_parity(report: &ParityReport, style: Style) -> String {
    let top = report.projected.max(report.graded).max(f64::MIN_POSITIVE);
    let mut out = format!(
        "{}\n  {} {}  {} {}\n  {} {}  {} {}\n",
        style.dim("parity: local best-case projection vs the server's grade"),
        style.dim("projected"),
        style.dim(&visual::meter(report.projected / top, 20)),
        fmt::score(report.projected),
        style.dim("(assumes a clear)"),
        style.dim("graded   "),
        style.accent(&visual::meter(report.graded / top, 20)),
        style.bold(&style.accent(&fmt::score(report.graded))),
        style.dim(&format!("(C={:.0}%)", report.correctness_pct)),
    );
    if !report.recognized {
        out.push_str(&format!(
            "{}\n",
            style
                .yellow("note: model wasn't recognized server-side — scored at the baseline floor"),
        ));
    }
    if let Some(warning) = &report.warning {
        out.push_str(&format!("{} {warning}\n", style.red("parity warning:")));
    }
    out
}

/// Render the packaged bundle: the file list and total size.
fn render_bundle(bundle: &SubmissionBundle, style: Style) -> String {
    let mut out = format!(
        "{} {}\n",
        style.green(&format!("packaged {} file(s)", bundle.files.len())),
        style.dim(&format!(
            "({} bytes)",
            fmt::thousands(bundle.total_bytes as u128)
        )),
    );
    for file in &bundle.files {
        out.push_str(&format!(
            "  {} {}\n",
            style.dim(&file.path),
            style.dim(&format!("({} B)", file.bytes.len())),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{PreparedAttempt, RemoteStatus, SubmitReceipt, UnpairedCloud};
    use crate::daemon_client::{
        DaemonError, Health, ResetReport, SessionSnapshot, StartDecisions, StartOutcome, StartPlan,
        StopReport,
    };
    use crate::prompt::ScriptedAsk;
    use crate::submission::SubmissionFile;
    use promptlyd::engine::Totals;
    use promptlyd::model::{Agreement, Confidence, NormalizedTurn, Plausibility, Source};
    use promptlyd::scoping::{NonceOrigin, SessionMarker};
    use std::path::PathBuf;

    fn temp_workspace(label: &str, allowlist: &str, entry: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "promptly-cmd-submit-{}-{label}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".promptly")).unwrap();
        std::fs::write(dir.join("lru.go"), "package main\n").unwrap();
        std::fs::write(dir.join("main.go"), "package main\n").unwrap();
        std::fs::write(
            dir.join(".promptly/manifest.json"),
            format!(
                r#"{{"schema_version":1,"level_id":"x","slug":"stage-1-01","baseline_hash":"y",
                    "file_allowlist":["{allowlist}"],"entry_points":["{entry}"]}}"#
            ),
        )
        .unwrap();
        dir
    }

    fn captured_turn() -> NormalizedTurn {
        NormalizedTurn {
            schema_version: 1,
            turn_id: "t1".into(),
            model: "claude-opus-4-8".into(),
            harness: "claude_code_cli".into(),
            tokens_input: 1000,
            tokens_output: 500,
            tokens_thinking: 0,
            tokens_cache: 0,
            prompt_id: Some("p1".into()),
            timestamp_ms: 1,
            confidence: Confidence::Otel,
            cost_usd: None,
            duration_ms: None,
            sources: vec![Source::Otel],
            session_id: Some("sess-1".into()),
            attempt_nonce: Some("nonce-1".into()),
            workspace: None,
            agreement: Agreement::Single,
            plausibility: Plausibility::Plausible,
        }
    }

    /// A captured turn carrying a cross-source disagreement — the tampering
    /// fingerprint the submit gate flags.
    fn flagged_turn() -> NormalizedTurn {
        NormalizedTurn {
            turn_id: "t2".into(),
            agreement: Agreement::Disagree {
                fields: vec!["tokens_output".into()],
            },
            ..captured_turn()
        }
    }

    fn active_marker(slug: &str) -> SessionMarker {
        SessionMarker {
            version: 1,
            session_id: "sess-1".into(),
            workspace: PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: slug.into(),
            started_at_ms: 1000,
            stopped_at_ms: None,
            attempt_nonce: "nonce-1".into(),
            nonce_origin: NonceOrigin::Server,
            file_allowlist: vec![],
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    /// The same session after `promptly stop` closed its capture window
    /// (`stopped_at_ms` set) — the finish-line state a player submits from.
    fn stopped_marker(slug: &str) -> SessionMarker {
        SessionMarker {
            stopped_at_ms: Some(2000),
            ..active_marker(slug)
        }
    }

    /// A daemon fake exposing one active session with a captured turn.
    struct FakeDaemon {
        snapshot: SessionSnapshot,
    }

    impl FakeDaemon {
        fn active(slug: &str) -> Self {
            Self {
                snapshot: SessionSnapshot {
                    session: Some(active_marker(slug)),
                    totals: Totals::default(),
                    turns: 1,
                    signals: vec![],
                    captured: vec![captured_turn()],
                },
            }
        }

        fn idle() -> Self {
            Self {
                snapshot: SessionSnapshot {
                    session: None,
                    totals: Totals::default(),
                    turns: 0,
                    signals: vec![],
                    captured: vec![],
                },
            }
        }

        /// A stopped (window-closed) session still carrying its captured turn — the
        /// state after `promptly stop`, from which submit must still work.
        fn stopped(slug: &str) -> Self {
            Self {
                snapshot: SessionSnapshot {
                    session: Some(stopped_marker(slug)),
                    totals: Totals::default(),
                    turns: 1,
                    signals: vec![],
                    captured: vec![captured_turn()],
                },
            }
        }

        /// An active session whose capture carries a tampering fingerprint.
        fn active_flagged(slug: &str) -> Self {
            Self {
                snapshot: SessionSnapshot {
                    session: Some(active_marker(slug)),
                    totals: Totals::default(),
                    turns: 1,
                    signals: vec![],
                    captured: vec![flagged_turn()],
                },
            }
        }
    }

    impl DaemonApi for FakeDaemon {
        fn session(&self) -> Result<SessionSnapshot, DaemonError> {
            Ok(self.snapshot.clone())
        }
        fn health(&self) -> Result<Health, DaemonError> {
            Err(DaemonError::Api("unused".into()))
        }
        fn preflight(&self) -> Result<StartPlan, DaemonError> {
            Err(DaemonError::Api("unused".into()))
        }
        fn start(&self, _decisions: StartDecisions) -> Result<StartOutcome, DaemonError> {
            Err(DaemonError::Api("unused".into()))
        }
        fn stop(&self) -> Result<StopReport, DaemonError> {
            Err(DaemonError::Api("unused".into()))
        }
        fn reset(&self) -> Result<ResetReport, DaemonError> {
            Err(DaemonError::Api("unused".into()))
        }
    }

    /// A cloud fake that accepts the submission and grades it immediately.
    struct PairedCloud {
        graded: GradedScore,
    }
    impl PairedCloud {
        fn with_score(score: f64) -> Self {
            Self {
                graded: GradedScore {
                    score,
                    correctness_pct: 100.0,
                    recognized: true,
                },
            }
        }
    }
    impl Cloud for PairedCloud {
        fn pair(&self) -> Result<(), CloudError> {
            Ok(())
        }
        fn prepare_attempt(&self, _slug: &str) -> Result<Option<PreparedAttempt>, CloudError> {
            Ok(Some(PreparedAttempt {
                nonce: "nonce-1".into(),
                baseline_hash: None,
            }))
        }
        fn submit(
            &self,
            _slug: &str,
            _bundle: &SubmissionBundle,
            capture: &CaptureUpload,
        ) -> Result<SubmitReceipt, CloudError> {
            // The capture carries the bound nonce + the turns to sign.
            assert_eq!(capture.attempt_nonce, Some("nonce-1"));
            assert_eq!(capture.turns.len(), 1);
            Ok(SubmitReceipt {
                submission_id: "sub-1".into(),
                status: "queued".into(),
            })
        }
        fn submission_status(&self, _submission_id: &str) -> Result<RemoteStatus, CloudError> {
            Ok(RemoteStatus {
                status: "graded".into(),
                graded: Some(self.graded.clone()),
            })
        }
    }

    /// A cloud that must never be asked to upload — proves a declined confirmation
    /// aborts before anything leaves the machine.
    struct NoUploadCloud;
    impl Cloud for NoUploadCloud {
        fn pair(&self) -> Result<(), CloudError> {
            unreachable!("a declined submit never pairs")
        }
        fn prepare_attempt(&self, _slug: &str) -> Result<Option<PreparedAttempt>, CloudError> {
            unreachable!("a declined submit never prepares an attempt")
        }
        fn submit(
            &self,
            _slug: &str,
            _bundle: &SubmissionBundle,
            _capture: &CaptureUpload,
        ) -> Result<SubmitReceipt, CloudError> {
            panic!("a declined submit must not upload")
        }
        fn submission_status(&self, _submission_id: &str) -> Result<RemoteStatus, CloudError> {
            unreachable!("a declined submit never polls")
        }
    }

    /// Run submit with the confirmation pre-accepted (`--yes`) — the common case for
    /// the validation tests, which exercise paths other than the prompt itself.
    fn submit_confirmed(
        ws: &Path,
        manifest: &Manifest,
        daemon: &dyn DaemonApi,
        cloud: &dyn Cloud,
    ) -> CommandExit {
        run_submit(
            ws,
            Some(manifest),
            daemon,
            cloud,
            &mut ScriptedAsk::new([]),
            SubmitArgs {
                yes: true,
                force: false,
            },
            Style::plain(),
        )
        .unwrap()
    }

    #[test]
    fn submit_packages_then_routes_to_pairing_when_unpaired() {
        let ws = temp_workspace("unpaired", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        let daemon = FakeDaemon::active("stage-1-01");
        let exit = submit_confirmed(&ws, &manifest, &daemon, &UnpairedCloud);
        // Local packaging succeeds, but the ranked upload needs a paired device.
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_succeeds_and_reports_parity_against_a_paired_cloud() {
        let ws = temp_workspace("paired", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        let daemon = FakeDaemon::active("stage-1-01");
        // A low graded score stays under the projection — no parity warning.
        let exit = submit_confirmed(&ws, &manifest, &daemon, &PairedCloud::with_score(1.0));
        assert_eq!(exit, CommandExit::Success);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_requires_a_bound_session() {
        let ws = temp_workspace("idle", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        let exit = submit_confirmed(
            &ws,
            &manifest,
            &FakeDaemon::idle(),
            &PairedCloud::with_score(1.0),
        );
        // No session bound here at all (idle daemon) → nothing to sign → fail before
        // upload. A *stopped* session, by contrast, still submits (next test).
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_accepts_a_stopped_session() {
        // The documented finish line is `promptly stop` → `promptly submit`, and the
        // stop command itself points the player at submit. Stopping only closes the
        // capture window (`stopped_at_ms`); the turns to sign and the bound nonce
        // remain, so submit must package and upload — not reject it as "no session".
        let ws = temp_workspace("stopped", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        let exit = submit_confirmed(
            &ws,
            &manifest,
            &FakeDaemon::stopped("stage-1-01"),
            &PairedCloud::with_score(1.0),
        );
        assert_eq!(exit, CommandExit::Success);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_rejects_a_level_mismatch_between_daemon_and_workspace() {
        let ws = temp_workspace("mismatch", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        // The daemon is bound to a different level than this workspace.
        let daemon = FakeDaemon::active("stage-2-06");
        let exit = submit_confirmed(&ws, &manifest, &daemon, &PairedCloud::with_score(1.0));
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_reports_an_invalid_submission() {
        // Allowlist matches nothing in the workspace → invalid, no cloud call.
        let ws = temp_workspace("invalid", "nonexistent.rs", "Service.Start");
        let manifest = Manifest::load(&ws).unwrap();
        let exit = submit_confirmed(
            &ws,
            &manifest,
            &FakeDaemon::active("stage-1-01"),
            &PairedCloud::with_score(1.0),
        );
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_aborts_when_the_confirmation_is_declined() {
        let ws = temp_workspace("declined", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        // NoUploadCloud panics if asked to upload, so reaching it would fail the test.
        let mut ask = ScriptedAsk::new([false]);
        let exit = run_submit(
            &ws,
            Some(&manifest),
            &FakeDaemon::active("stage-1-01"),
            &NoUploadCloud,
            &mut ask,
            SubmitArgs {
                yes: false,
                force: false,
            },
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn submit_uploads_once_the_confirmation_is_accepted() {
        let ws = temp_workspace("accepted", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        let mut ask = ScriptedAsk::new([true]);
        let exit = run_submit(
            &ws,
            Some(&manifest),
            &FakeDaemon::active("stage-1-01"),
            &PairedCloud::with_score(1.0),
            &mut ask,
            SubmitArgs {
                yes: false,
                force: false,
            },
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_flagged_capture_ignores_yes_and_blocks_when_declined() {
        let ws = temp_workspace("flagged-declined", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        // `--yes` is set, but the capture is flagged, so the gate still prompts; the
        // player declines, and NoUploadCloud (which panics on upload) is never hit.
        let mut ask = ScriptedAsk::new([false]);
        let exit = run_submit(
            &ws,
            Some(&manifest),
            &FakeDaemon::active_flagged("stage-1-01"),
            &NoUploadCloud,
            &mut ask,
            SubmitArgs {
                yes: true,
                force: false,
            },
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_flagged_capture_submits_with_force() {
        let ws = temp_workspace("flagged-force", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        // `--force` overrides the integrity gate without an interactive prompt.
        let exit = run_submit(
            &ws,
            Some(&manifest),
            &FakeDaemon::active_flagged("stage-1-01"),
            &PairedCloud::with_score(1.0),
            &mut ScriptedAsk::new([]),
            SubmitArgs {
                yes: false,
                force: true,
            },
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_flagged_capture_submits_when_acknowledged_interactively() {
        let ws = temp_workspace("flagged-ack", "lru.go", "main.go");
        let manifest = Manifest::load(&ws).unwrap();
        // Acknowledging the warning at the prompt proceeds with the upload.
        let mut ask = ScriptedAsk::new([true]);
        let exit = run_submit(
            &ws,
            Some(&manifest),
            &FakeDaemon::active_flagged("stage-1-01"),
            &PairedCloud::with_score(1.0),
            &mut ask,
            SubmitArgs {
                yes: false,
                force: false,
            },
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn capture_integrity_counts_and_flags_tampering_signals() {
        let clean = captured_turn();
        let mut implausible = captured_turn();
        implausible.plausibility = Plausibility::Low {
            reasons: vec!["zero tokens reported".into()],
        };
        let flagged = CaptureIntegrity::of(&[clean.clone(), flagged_turn(), implausible]);
        assert_eq!(flagged.total, 3);
        assert_eq!(flagged.disagreements, 1);
        assert_eq!(flagged.low_plausibility, 1);
        assert!(flagged.flagged());
        assert!(flagged.summary_phrase().contains("disagreement"));

        let corroborated = CaptureIntegrity::of(&[clean.clone(), clean]);
        assert!(!corroborated.flagged(), "a clean capture doesn't gate");
        assert_eq!(corroborated.disagreements, 0);
    }

    #[test]
    fn capture_integrity_flags_an_impossibly_paced_capture() {
        // 40 turns packed into ~4s — no interactive session is this tight.
        let turns: Vec<NormalizedTurn> = (0..40)
            .map(|i| {
                let mut t = captured_turn();
                t.turn_id = format!("burst-{i}");
                t.timestamp_ms = i * 100;
                t
            })
            .collect();
        let integrity = CaptureIntegrity::of(&turns);
        assert!(integrity.flagged(), "an impossible burst gates submit");
        assert!(integrity.summary_phrase().contains("pacing"));
    }

    #[test]
    fn projected_tier_is_verified_only_for_attested_otel_claude_code() {
        // Server nonce + attested baseline + an OTEL-backed turn → verified-eligible.
        let mut marker = active_marker("stage-1-01");
        marker.baseline_attested = true;
        assert_eq!(projected_tier(&marker, &[captured_turn()]).0, "verified");
    }

    #[test]
    fn projected_tier_caps_unverified_for_the_weak_paths() {
        // Local nonce (offline start) → unverified even with an attested baseline.
        let mut local = active_marker("s");
        local.nonce_origin = NonceOrigin::Local;
        local.baseline_attested = true;
        assert_eq!(projected_tier(&local, &[captured_turn()]).0, "unverified");

        // Server nonce but the baseline wasn't attested → unverified.
        let unattested = active_marker("s"); // baseline_attested: false by default
        assert_eq!(
            projected_tier(&unattested, &[captured_turn()]).0,
            "unverified"
        );

        // Attested server nonce, but the weak capture paths each cap at unverified.
        let mut attested = active_marker("s");
        attested.baseline_attested = true;

        let mut adapter = captured_turn();
        adapter.sources = vec![Source::Cursor];
        assert_eq!(projected_tier(&attested, &[adapter]).0, "unverified");

        let mut jsonl = captured_turn();
        jsonl.sources = vec![Source::Jsonl];
        jsonl.confidence = Confidence::Jsonl;
        assert_eq!(projected_tier(&attested, &[jsonl]).0, "unverified");

        let mut estimated = captured_turn();
        estimated.confidence = Confidence::Estimated;
        assert_eq!(projected_tier(&attested, &[estimated]).0, "unverified");
    }

    #[test]
    fn pair_fails_cleanly_when_unpaired() {
        assert_eq!(
            run_pair(&UnpairedCloud, Style::plain()).unwrap(),
            CommandExit::Failure
        );
    }

    #[test]
    fn render_parity_flags_a_violation_and_an_unrecognized_model() {
        let report = ParityReport {
            projected: 100.0,
            graded: 250.0,
            correctness_pct: 80.0,
            recognized: false,
            warning: Some("server score 250 exceeds the local best-case projection 100".into()),
        };
        let text = render_parity(&report, Style::plain());
        assert!(text.contains("parity warning:"));
        assert!(text.contains("baseline floor"));
        assert!(text.contains("C=80%"));
        // Bars scale to the larger score: graded (250) fills the meter, the
        // lower projection (100) leaves track showing.
        assert!(text.contains("graded    ████████████████████  250.00"));
        assert!(text.contains('░'));
    }

    #[test]
    fn render_bundle_lists_files_and_size() {
        let bundle = SubmissionBundle {
            files: vec![SubmissionFile {
                path: "lru.go".into(),
                bytes: vec![b'x'; 12],
            }],
            total_bytes: 12,
        };
        let text = render_bundle(&bundle, Style::plain());
        assert!(text.contains("packaged 1 file(s)"));
        assert!(text.contains("lru.go"));
        assert!(text.contains("(12 B)"));
    }
}
