//! `promptly start` / `stop` / `reset` — the scored-attempt session lifecycle,
//! driving the daemon's loopback control API (`18`).
//!
//! `start` previews via `GET /session/preflight`, renders the baseline/consent
//! decisions the player owns (resetting a tampered workspace; writing the OTEL
//! env into the project Claude settings), then executes `POST /session/start`
//! with those decisions. `stop` ends the window and reverts the harness settings;
//! `reset` restores the canonical starter (after a backup the daemon writes).
//!
//! The daemon owns the session and acts on *its* `--workspace`; these commands
//! are the player-facing driver over that seam.

use clap::Args;

use crate::cloud::Cloud;
use crate::daemon_client::{
    BaselineStatus, DaemonApi, StartDecisions, StartOutcome, StartedSession,
};
use crate::prompt::Ask;
use crate::style::Style;
use crate::CommandExit;

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Optional level slug to guard against — errors if the daemon is bound to a
    /// different level (i.e. you're not in that level's workspace).
    level: Option<String>,

    /// Answer yes to every prompt (confirm a reset, consent to the OTEL bootstrap).
    #[arg(long)]
    yes: bool,

    /// Consent to writing the OTEL telemetry env into the project settings.
    #[arg(long, conflicts_with = "no_consent")]
    consent: bool,

    /// Decline the OTEL bootstrap; capture falls back to JSONL-only (lower confidence).
    #[arg(long)]
    no_consent: bool,
}

impl StartArgs {
    /// Build start args for `promptly play`: no level guard (the daemon was just
    /// scoped to the level), carrying the consent / `--yes` choices through.
    pub fn for_play(yes: bool, consent: bool, no_consent: bool) -> Self {
        Self {
            level: None,
            yes,
            consent,
            no_consent,
        }
    }
}

#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Skip the confirmation prompt.
    #[arg(long)]
    yes: bool,
}

/// Reconcile the consent flags into an explicit choice, or `None` to prompt.
fn consent_choice(args: &StartArgs) -> Option<bool> {
    match (args.consent, args.no_consent) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
    }
}

/// Does the bound slug satisfy the level the player named (exact, or the short
/// `stage-N-MM` prefix of the full slug)?
fn slug_matches(bound: &str, named: &str) -> bool {
    bound == named || bound.starts_with(&format!("{named}-"))
}

pub fn run_start(
    client: &dyn DaemonApi,
    cloud: &dyn Cloud,
    asker: &mut dyn Ask,
    args: StartArgs,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let consent_flag = consent_choice(&args);
    // `--yes` answers every prompt below (reset confirm + bootstrap consent).
    let mut gate = OverrideYes {
        inner: asker,
        yes: args.yes,
    };
    let asker: &mut dyn Ask = &mut gate;
    let plan = client.preflight()?;

    if let Some(named) = &args.level {
        if !slug_matches(&plan.level.slug, named) {
            println!(
                "{}",
                style.red(&format!(
                    "daemon is bound to '{}', not '{named}' — run from that level's workspace",
                    plan.level.slug,
                )),
            );
            return Ok(CommandExit::Failure);
        }
    }

    println!(
        "{} {} {}",
        style.dim("level"),
        style.accent(&plan.level.slug),
        style.dim(&plan.level.title),
    );

    let mut decisions = StartDecisions::default();

    if plan.kind == "resume" {
        println!(
            "{}",
            style.dim("resuming the in-progress attempt — no baseline reset, same attempt nonce"),
        );
    } else if let Some(BaselineStatus::Mismatch { computed }) = &plan.baseline {
        println!(
            "{}",
            style.yellow("this workspace does not match the level's genuine starter:"),
        );
        println!(
            "  {}",
            style.dim(&format!("current content hash {}", short_hash(computed))),
        );
        if !plan.can_reset {
            println!(
                "{}",
                style.red(
                    "cannot reset offline — no cached starter is available. Re-fetch with \
                     `promptly init <level>` (or connect) before starting.",
                ),
            );
            return Ok(CommandExit::Failure);
        }
        let confirmed = asker.confirm(
            "Reset the workspace to the genuine starter before capturing? \
             Your current files will be backed up first.",
            false,
            false,
        );
        if !confirmed {
            println!(
                "{}",
                style.dim("aborted — no session started, workspace untouched")
            );
            return Ok(CommandExit::Failure);
        }
        decisions.confirm_reset = true;
    }

    // Consent to the OTEL bootstrap (fresh starts only; resume re-asserts whatever
    // the bound attempt already recorded).
    if plan.kind != "resume" {
        decisions.consent_bootstrap = resolve_consent(&plan, consent_flag, asker, style);
        // Ask the cloud to issue a server-side attempt nonce so the capture can
        // reach `verified`. Unpaired play is fine (local nonce, capped at
        // `unverified`); a paired-but-unreachable server only warns, never blocks —
        // the player can always capture offline.
        match cloud.prepare_attempt(&plan.level.slug) {
            Ok(Some(nonce)) => decisions.server_nonce = Some(nonce),
            Ok(None) => {}
            Err(err) => println!(
                "{}",
                style.yellow(&format!(
                    "couldn't reach Promptly to bind this attempt ({err}); \
                     starting offline — integrity caps at 'unverified'"
                )),
            ),
        }
    }

    match client.start(decisions)? {
        StartOutcome::Started(session) => {
            print!("{}", render_started(&session, style));
            Ok(CommandExit::Success)
        }
        StartOutcome::NeedsReset(mismatch) => {
            // We confirmed above, so this is a rare race (the workspace changed
            // between preflight and start).
            println!(
                "{}",
                style.red(&format!(
                    "baseline changed between preflight and start (expected {}, got {}) — \
                     re-run `promptly start`",
                    short_hash(&mismatch.expected),
                    short_hash(&mismatch.computed),
                )),
            );
            Ok(CommandExit::Failure)
        }
    }
}

/// Resolve the bootstrap-consent decision: an explicit flag wins; otherwise an
/// already-applied bootstrap is kept, and a first-time bootstrap prompts (naming
/// the exact file and keys, per `18`).
fn resolve_consent(
    plan: &crate::daemon_client::StartPlan,
    flag: Option<bool>,
    asker: &mut dyn Ask,
    style: Style,
) -> bool {
    if let Some(choice) = flag {
        return choice;
    }
    if plan.bootstrap_already_applied {
        println!(
            "{}",
            style.dim("OTEL telemetry is already configured for this workspace"),
        );
        return true;
    }
    let question = format!(
        "Allow Promptly to write OTEL telemetry settings into the project .claude/settings.json \
         (keys: {})? Reverted on `promptly stop`.",
        plan.bootstrap_keys.join(", "),
    );
    // Pressing Enter consents (capture works best with OTEL); a non-interactive
    // start never silently writes — it falls back to JSONL-only.
    asker.confirm(&question, true, false)
}

pub fn run_stop(client: &dyn DaemonApi, style: Style) -> anyhow::Result<CommandExit> {
    let report = client.stop()?;
    match report.marker {
        Some(marker) => {
            println!(
                "{} {}",
                style.green("session stopped"),
                style.dim(&format!("({})", marker.slug)),
            );
            if report.reverted_bootstrap {
                println!(
                    "  {}",
                    style.dim("restored the workspace's prior Claude settings")
                );
            }
            println!(
                "  {}",
                style
                    .dim("`promptly score` for the projected score · `promptly submit` to rank it"),
            );
        }
        None => println!("{}", style.dim("no active session to stop")),
    }
    Ok(CommandExit::Success)
}

pub fn run_reset(
    client: &dyn DaemonApi,
    asker: &mut dyn Ask,
    args: ResetArgs,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let mut asker = OverrideYes {
        inner: asker,
        yes: args.yes,
    };
    let confirmed = asker.confirm(
        "Reset this workspace to the level's canonical starter? \
         Your current files will be backed up under .promptly/backup/ first.",
        false,
        false,
    );
    if !confirmed {
        println!("{}", style.dim("aborted — workspace unchanged"));
        return Ok(CommandExit::Failure);
    }
    let report = client.reset()?;
    println!(
        "{} {}",
        style.green("workspace reset to the canonical starter"),
        style.dim(&format!("(baseline {})", short_hash(&report.restored_hash))),
    );
    println!(
        "  {}",
        style.dim(&format!(
            "your previous files are backed up at {}",
            report.backup_dir
        )),
    );
    Ok(CommandExit::Success)
}

/// Wraps an `Ask` to force "yes" when `--yes` was passed.
struct OverrideYes<'a> {
    inner: &'a mut dyn Ask,
    yes: bool,
}

impl Ask for OverrideYes<'_> {
    fn confirm(&mut self, question: &str, on_empty: bool, on_noninteractive: bool) -> bool {
        self.yes || self.inner.confirm(question, on_empty, on_noninteractive)
    }
}

/// Render a successful start: the bound level, the capture confidence, the
/// integrity ceiling, any reset, and the next steps.
fn render_started(session: &StartedSession, style: Style) -> String {
    let mut out = String::new();
    let kind = if session.kind == "resume" {
        "resumed"
    } else {
        "started"
    };
    out.push_str(&format!(
        "{} {} {}\n",
        style.green(&format!("session {kind}")),
        style.accent(&session.level.slug),
        style.dim(&format!("({})", session.level.title)),
    ));

    if let Some(reset) = &session.reset {
        out.push_str(&format!(
            "  {}\n",
            style.yellow(&format!(
                "workspace reset to the genuine starter — your files backed up at {}",
                reset.backup_dir,
            )),
        ));
    }

    let capture = if session.jsonl_only {
        style.yellow("JSONL only (consent declined) — lower-confidence capture")
    } else {
        style.dim("OTEL + JSONL")
    };
    out.push_str(&format!("  {} {}\n", style.dim("capture:"), capture));

    out.push_str(&format!(
        "  {} {}{}\n",
        style.dim("integrity cap:"),
        style.bold(&session.integrity_cap),
        if session.integrity_cap == "unverified" {
            style.dim("  (local nonce — pairing reaches 'verified' once the cloud release lands)")
        } else {
            String::new()
        },
    ));

    out.push_str(&format!(
        "  {}\n",
        style.dim(
            "run Claude Code here · `promptly watch` to follow burn · `promptly stop` when done"
        ),
    ));
    out
}

/// First 12 hex chars of a content hash, for compact display.
fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{CaptureUpload, CloudError, RemoteStatus, SubmitReceipt};
    use crate::daemon_client::{
        DaemonError, Health, LevelBinding, ResetReport, SessionMarker, SessionSnapshot, StartPlan,
        StopReport,
    };
    use crate::prompt::ScriptedAsk;
    use crate::submission::SubmissionBundle;
    use std::cell::RefCell;

    fn level() -> LevelBinding {
        LevelBinding {
            level_id: "lvl-1".into(),
            slug: "stage-1-01-lru-eviction-debug".into(),
            title: "LRU Eviction".into(),
            language: "Go".into(),
            runtime_version: "go1.22".into(),
            execution_harness: "stdin_stdout".into(),
        }
    }

    fn marker() -> SessionMarker {
        SessionMarker {
            version: 1,
            session_id: "s1".into(),
            workspace: std::path::PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01-lru-eviction-debug".into(),
            started_at_ms: 1000,
            stopped_at_ms: None,
            attempt_nonce: "n".into(),
            nonce_origin: promptlyd::scoping::NonceOrigin::Local,
            file_allowlist: vec!["lru.go".into()],
            code_reset_count: 0,
            bootstrap: None,
        }
    }

    fn started(reset: Option<ResetReport>, jsonl_only: bool) -> StartedSession {
        StartedSession {
            marker: marker(),
            kind: "fresh".into(),
            level: level(),
            reset,
            bootstrap_applied: !jsonl_only,
            jsonl_only,
            integrity_cap: "unverified".into(),
        }
    }

    /// A daemon fake that returns a canned plan and records the start decisions.
    struct FakeDaemon {
        plan: StartPlan,
        start_calls: RefCell<Vec<StartDecisions>>,
    }

    impl FakeDaemon {
        fn new(plan: StartPlan) -> Self {
            Self {
                plan,
                start_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl DaemonApi for FakeDaemon {
        fn health(&self) -> Result<Health, DaemonError> {
            unreachable!()
        }
        fn session(&self) -> Result<SessionSnapshot, DaemonError> {
            unreachable!()
        }
        fn preflight(&self) -> Result<StartPlan, DaemonError> {
            Ok(self.plan.clone())
        }
        fn start(&self, decisions: StartDecisions) -> Result<StartOutcome, DaemonError> {
            // Read the Copy fields before moving `decisions` into the call log
            // (it now carries a non-Copy `server_nonce`).
            let confirm_reset = decisions.confirm_reset;
            let consent_bootstrap = decisions.consent_bootstrap;
            self.start_calls.borrow_mut().push(decisions);
            Ok(StartOutcome::Started(Box::new(started(
                confirm_reset.then(|| ResetReport {
                    backup_dir: "/ws/.promptly/backup/1".into(),
                    restored_hash: "abc123def456ghi".into(),
                }),
                !consent_bootstrap,
            ))))
        }
        fn stop(&self) -> Result<StopReport, DaemonError> {
            Ok(StopReport {
                marker: Some(marker()),
                reverted_bootstrap: true,
            })
        }
        fn reset(&self) -> Result<ResetReport, DaemonError> {
            Ok(ResetReport {
                backup_dir: "/ws/.promptly/backup/2".into(),
                restored_hash: "abc123def456ghi".into(),
            })
        }
    }

    /// A cloud fake for the start flow's `prepare_attempt` step.
    enum FakeCloud {
        /// Unpaired — offline play (`Ok(None)`), the pre-`20` default.
        Offline,
        /// Paired — issues this server nonce (`Ok(Some(_))`).
        Nonce(&'static str),
        /// Paired, but the server couldn't be reached (`Err`).
        Unreachable,
    }

    impl Cloud for FakeCloud {
        fn pair(&self) -> Result<(), CloudError> {
            unreachable!("start never pairs")
        }
        fn prepare_attempt(&self, _slug: &str) -> Result<Option<String>, CloudError> {
            match self {
                FakeCloud::Offline => Ok(None),
                FakeCloud::Nonce(n) => Ok(Some((*n).to_string())),
                FakeCloud::Unreachable => Err(CloudError::Other("server unreachable".into())),
            }
        }
        fn submit(
            &self,
            _slug: &str,
            _bundle: &SubmissionBundle,
            _capture: &CaptureUpload,
        ) -> Result<SubmitReceipt, CloudError> {
            unreachable!("start never submits")
        }
        fn submission_status(&self, _submission_id: &str) -> Result<RemoteStatus, CloudError> {
            unreachable!("start never polls a submission")
        }
    }

    fn plan(kind: &str, baseline: Option<BaselineStatus>, can_reset: bool) -> StartPlan {
        StartPlan {
            level: level(),
            kind: kind.into(),
            baseline,
            can_reset,
            bootstrap_keys: vec!["CLAUDE_CODE_ENABLE_TELEMETRY".into()],
            bootstrap_already_applied: false,
            integrity_cap: "unverified".into(),
        }
    }

    fn no_args() -> StartArgs {
        StartArgs {
            level: None,
            yes: false,
            consent: false,
            no_consent: false,
        }
    }

    #[test]
    fn clean_fresh_start_consents_and_does_not_reset() {
        let fake = FakeDaemon::new(plan("fresh", Some(BaselineStatus::Match), true));
        let mut ask = ScriptedAsk::new([true]); // consent: yes
        let exit = run_start(
            &fake,
            &FakeCloud::Offline,
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        let calls = fake.start_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].confirm_reset, "a matching baseline never resets");
        assert!(calls[0].consent_bootstrap, "consent was given");
        assert!(
            calls[0].server_nonce.is_none(),
            "offline cloud issues no nonce"
        );
    }

    #[test]
    fn a_server_nonce_from_the_cloud_is_forwarded_to_the_daemon() {
        let fake = FakeDaemon::new(plan("fresh", Some(BaselineStatus::Match), true));
        let mut ask = ScriptedAsk::new([true]); // consent: yes
        let exit = run_start(
            &fake,
            &FakeCloud::Nonce("srv-9"),
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        // The paired cloud's server nonce rides into the daemon start request, so
        // the daemon binds the attempt with `NonceOrigin::Server`.
        assert_eq!(
            fake.start_calls.borrow()[0].server_nonce.as_deref(),
            Some("srv-9"),
        );
    }

    #[test]
    fn an_unreachable_cloud_still_starts_with_a_local_nonce() {
        let fake = FakeDaemon::new(plan("fresh", Some(BaselineStatus::Match), true));
        let mut ask = ScriptedAsk::new([true]); // consent: yes
        let exit = run_start(
            &fake,
            &FakeCloud::Unreachable,
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        // A paired-but-unreachable server doesn't block the start; it falls back to
        // a local nonce (the daemon then caps the attempt at `unverified`).
        assert_eq!(exit, CommandExit::Success);
        assert_eq!(fake.start_calls.borrow()[0].server_nonce, None);
    }

    #[test]
    fn tampered_workspace_confirmed_resets_and_starts() {
        let fake = FakeDaemon::new(plan(
            "fresh",
            Some(BaselineStatus::Mismatch {
                computed: "beefbeefbeef00".into(),
            }),
            true,
        ));
        // Answers in order: confirm reset = yes, consent = yes.
        let mut ask = ScriptedAsk::new([true, true]);
        let exit = run_start(
            &fake,
            &FakeCloud::Offline,
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        let calls = fake.start_calls.borrow();
        assert!(calls[0].confirm_reset, "confirmed reset is forwarded");
    }

    #[test]
    fn tampered_workspace_declined_aborts_without_starting() {
        let fake = FakeDaemon::new(plan(
            "fresh",
            Some(BaselineStatus::Mismatch {
                computed: "beef".into(),
            }),
            true,
        ));
        let mut ask = ScriptedAsk::new([false]); // decline the reset
        let exit = run_start(
            &fake,
            &FakeCloud::Offline,
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Failure);
        assert!(
            fake.start_calls.borrow().is_empty(),
            "start was never called"
        );
    }

    #[test]
    fn tampered_workspace_without_a_cached_starter_errors_offline() {
        let fake = FakeDaemon::new(plan(
            "fresh",
            Some(BaselineStatus::Mismatch {
                computed: "beef".into(),
            }),
            false, // can_reset = false
        ));
        let mut ask = ScriptedAsk::new([]); // never prompted
        let exit = run_start(
            &fake,
            &FakeCloud::Offline,
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Failure);
        assert!(fake.start_calls.borrow().is_empty());
    }

    #[test]
    fn no_consent_flag_falls_back_to_jsonl_only() {
        let fake = FakeDaemon::new(plan("fresh", Some(BaselineStatus::Match), true));
        let mut ask = ScriptedAsk::new([]); // flag decides, no prompt
        let args = StartArgs {
            level: None,
            yes: false,
            consent: false,
            no_consent: true,
        };
        run_start(&fake, &FakeCloud::Offline, &mut ask, args, Style::plain()).unwrap();
        assert!(!fake.start_calls.borrow()[0].consent_bootstrap);
    }

    #[test]
    fn a_level_guard_mismatch_does_not_start() {
        let fake = FakeDaemon::new(plan("fresh", Some(BaselineStatus::Match), true));
        let mut ask = ScriptedAsk::new([]);
        let args = StartArgs {
            level: Some("stage-2-06".into()),
            yes: false,
            consent: false,
            no_consent: false,
        };
        let exit = run_start(&fake, &FakeCloud::Offline, &mut ask, args, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);
        assert!(fake.start_calls.borrow().is_empty());
    }

    #[test]
    fn the_short_stage_prefix_satisfies_the_level_guard() {
        assert!(slug_matches("stage-1-01-lru-eviction-debug", "stage-1-01"));
        assert!(slug_matches(
            "stage-1-01-lru-eviction-debug",
            "stage-1-01-lru-eviction-debug"
        ));
        assert!(!slug_matches("stage-1-01-lru-eviction-debug", "stage-1-1"));
        assert!(!slug_matches("stage-1-10-foo", "stage-1-1"));
    }

    #[test]
    fn resume_starts_without_prompting_or_resetting() {
        let fake = FakeDaemon::new(plan("resume", None, false));
        let mut ask = ScriptedAsk::new([]); // resume never prompts
        let exit = run_start(
            &fake,
            &FakeCloud::Offline,
            &mut ask,
            no_args(),
            Style::plain(),
        )
        .unwrap();
        assert_eq!(exit, CommandExit::Success);
        assert!(!fake.start_calls.borrow()[0].confirm_reset);
    }

    #[test]
    fn reset_requires_confirmation() {
        let fake = FakeDaemon::new(plan("fresh", Some(BaselineStatus::Match), true));
        let mut ask = ScriptedAsk::new([false]); // decline
        let exit = run_reset(&fake, &mut ask, ResetArgs { yes: false }, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);

        let mut ask = ScriptedAsk::new([true]); // confirm
        let exit = run_reset(&fake, &mut ask, ResetArgs { yes: false }, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Success);
    }

    #[test]
    fn render_started_shows_reset_and_jsonl_only() {
        let text = render_started(
            &started(
                Some(ResetReport {
                    backup_dir: "/ws/.promptly/backup/1".into(),
                    restored_hash: "abc".into(),
                }),
                true,
            ),
            Style::plain(),
        );
        assert!(text.contains("session started"));
        assert!(text.contains("backed up at /ws/.promptly/backup/1"));
        assert!(text.contains("JSONL only"));
        assert!(text.contains("unverified"));
    }
}
