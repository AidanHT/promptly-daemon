//! `promptly doctor` — diagnose the local setup with clear pass/fail and
//! remediation (`19`): is the daemon running? is Claude Code's OTEL export
//! configured and pointing at the daemon? is the workspace manifest present and
//! valid? is the pinned runtime installed? is the Judge0 backend reachable?
//!
//! Each check is a small pure function over already-fetched data, so the verdicts
//! are unit-testable; `run` does the I/O (querying the daemon/web, reading the
//! workspace) and renders the report.

use std::net::Ipv4Addr;
use std::path::Path;

use promptlyd::bootstrap::BootstrapPlan;
use promptlyd::config::DEFAULT_OTLP_PORT;
use promptlyd::manifest::{Manifest, ManifestError};

use crate::daemon_client::{
    AdapterState, AdapterStatus, DaemonApi, DaemonClient, DaemonError, Health,
};
use crate::runner::LocalRuntime;
use crate::style::Style;
use crate::visual;
use crate::web_client::{ExecutionHealth, WebClient, WebError};
use crate::CommandExit;

/// A single diagnostic's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    /// Healthy.
    Ok,
    /// Degraded but usable (capture/scoring still works).
    Warn,
    /// Broken — the core workflow won't function until fixed.
    Fail,
}

/// One diagnostic line.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub level: CheckLevel,
    pub detail: String,
    /// How to fix it (shown for warns/fails).
    pub hint: Option<String>,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: CheckLevel::Ok,
            detail: detail.into(),
            hint: None,
        }
    }
    fn warn(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: CheckLevel::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
    fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: CheckLevel::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

pub fn run(
    client: &DaemonClient,
    web: &WebClient,
    workspace: &Path,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let health = client.health();
    let manifest = Manifest::load(workspace);

    // Point the OTEL check at the daemon's actual endpoint when we can learn it,
    // else the loopback default.
    let endpoint = health
        .as_ref()
        .ok()
        .map(|h| h.otlp_endpoint.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_otlp_endpoint);
    let otel = promptlyd::bootstrap::plan(workspace, &endpoint);

    let local_installed = manifest.as_ref().ok().map(|m| {
        LocalRuntime::from_runtime_version(&m.runtime_version)
            .map(|rt| rt.resolve_program().is_some())
    });

    let mut checks = vec![
        check_daemon(&health),
        check_otel(otel.as_ref().map_err(|e| e.to_string())),
    ];
    // Per-harness capture adapters (`21`), exactly as the daemon reports them.
    if let Ok(h) = &health {
        for status in &h.adapters {
            checks.push(check_adapter(status));
        }
    }
    checks.push(check_manifest(&manifest));
    if let Ok(m) = &manifest {
        checks.push(check_runtime(&m.runtime_version, local_installed.flatten()));
    }
    // One probe of the web app feeds both checks: which app the CLI talks to
    // (pairing/init/grading) and the execution backend behind it.
    let exec_health = web.execution_health();
    checks.push(check_web(web.base_url(), &exec_health));
    checks.push(check_judge0(exec_health));
    // Last: whether a newer release is available (cached; offline is fine).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    checks.push(check_update(&crate::update_check::status(now_ms, true)));

    let worst_is_fail = checks.iter().any(|c| c.level == CheckLevel::Fail);
    print!("{}", render_report(&checks, style));

    Ok(if worst_is_fail {
        CommandExit::Failure
    } else {
        CommandExit::Success
    })
}

/// Render the whole report: a section rule, every check with its name padded to
/// one shared column (so the details align down the list), then a one-line
/// verdict fronted by the per-check mark strip.
fn render_report(checks: &[Check], style: Style) -> String {
    let name_col = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = format!("{}\n", visual::header(style, "doctor"));
    for check in checks {
        out.push_str(&render_check(check, name_col, style));
    }
    out.push_str(&render_summary(checks, style));
    out
}

/// One mark per check in report order (`✓`/`!`/`✗` in its level's color) — the
/// whole diagnosis compressed into a glanceable strip.
fn verdict_strip(checks: &[Check], style: Style) -> String {
    checks
        .iter()
        .map(|c| match c.level {
            CheckLevel::Ok => style.green("✓"),
            CheckLevel::Warn => style.yellow("!"),
            CheckLevel::Fail => style.red("✗"),
        })
        .collect()
}

/// The closing verdict: all clear, or the warn/fail counts with the worst
/// level's color, so the report ends on an unambiguous answer.
fn render_summary(checks: &[Check], style: Style) -> String {
    let warns = checks
        .iter()
        .filter(|c| c.level == CheckLevel::Warn)
        .count();
    let fails = checks
        .iter()
        .filter(|c| c.level == CheckLevel::Fail)
        .count();
    let verdict = match (fails, warns) {
        (0, 0) => style.green(&format!("all {} checks passed", checks.len())),
        (0, _) => style.yellow(&format!(
            "{} of {} checks need attention ({warns} warning{})",
            warns,
            checks.len(),
            plural(warns),
        )),
        _ => style.red(&format!(
            "{} of {} checks failed ({fails} failure{}, {warns} warning{})",
            fails + warns,
            checks.len(),
            plural(fails),
            plural(warns),
        )),
    };
    format!("\n{} {verdict}\n", verdict_strip(checks, style))
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn default_otlp_endpoint() -> String {
    format!("http://{}:{}", Ipv4Addr::LOCALHOST, DEFAULT_OTLP_PORT)
}

fn check_daemon(health: &Result<Health, DaemonError>) -> Check {
    match health {
        Ok(h) if h.recent_errors.is_empty() => {
            Check::ok("daemon", format!("running (v{})", h.version))
        }
        Ok(h) => Check::warn(
            "daemon",
            format!("running (v{}) with {} recent warning(s)", h.version, h.recent_errors.len()),
            "inspect the daemon logs; recent capture/watcher errors are listed above its output",
        ),
        Err(DaemonError::NotRunning(_)) => Check::fail(
            "daemon",
            "not running",
            "`promptly start` launches the daemon automatically; or run `promptly up` to start it on its own",
        ),
        Err(err) => Check::warn("daemon", format!("reachable but erroring: {err}"), "check the daemon logs"),
    }
}

/// `otel` carries the bootstrap plan, or an error string if the settings file
/// couldn't be read.
fn check_otel(otel: Result<&BootstrapPlan, String>) -> Check {
    match otel {
        Ok(plan) if plan.already_applied => {
            Check::ok("otel config", format!("exporting to {}", plan.endpoint))
        }
        Ok(plan) if plan.file_exists => Check::warn(
            "otel config",
            "project Claude settings exist but don't export to this daemon",
            "run `promptly start` to (re)configure the OTEL export, or check .claude/settings.json",
        ),
        Ok(_) => Check::warn(
            "otel config",
            "not configured — capture will be JSONL-only (lower confidence)",
            "run `promptly start` and consent to the OTEL bootstrap for full-confidence capture",
        ),
        Err(err) => Check::warn(
            "otel config",
            format!("couldn't read project Claude settings: {err}"),
            "check that .claude/settings.json is valid JSON",
        ),
    }
}

fn check_manifest(manifest: &Result<Manifest, ManifestError>) -> Check {
    match manifest {
        Ok(m) => Check::ok(
            "manifest",
            format!("bound to {} (kit v{})", m.slug, m.kit_version),
        ),
        Err(err) => Check::fail(
            "manifest",
            err.to_string(),
            "run `promptly init <level>` to acquire a valid workspace",
        ),
    }
}

/// `local_installed`: `Some(true)` installed, `Some(false)` supported but missing,
/// `None` no local runner for this runtime.
fn check_runtime(runtime_version: &str, local_installed: Option<bool>) -> Check {
    match local_installed {
        Some(true) => Check::ok("runtime", format!("{runtime_version} toolchain installed")),
        Some(false) => Check::warn(
            "runtime",
            format!("{runtime_version} toolchain not found on PATH"),
            format!(
                "install {runtime_version} to run public tests locally (else they run remotely)"
            ),
        ),
        None => Check::warn(
            "runtime",
            format!("no local runner for {runtime_version} — public tests run server-side"),
            "this is expected for compiled/transpiled languages; no action needed",
        ),
    }
}

/// Report which Promptly web app the CLI is configured to talk to (used by
/// `pair`, `init`, and remote grading) and whether it answered — so a
/// player setting up production can confirm `PROMPTLY_API_URL` points at the
/// deployed app, not the localhost default. Reachability reuses the
/// execution-health probe: `Err(NotReachable)` means nothing answered, while any
/// response (even an unhealthy 503) proves the web app is up.
fn check_web(base_url: &str, health: &Result<ExecutionHealth, WebError>) -> Check {
    let is_local = base_url.contains("localhost") || base_url.contains("127.0.0.1");
    let reachable = !matches!(health, Err(WebError::NotReachable(_)));
    let descriptor = if is_local { "local dev" } else { "production" };
    match (reachable, is_local) {
        (true, _) => Check::ok(
            "web app",
            format!("configured for {base_url} ({descriptor})"),
        ),
        (false, true) => Check::warn(
            "web app",
            format!("configured for {base_url} (local dev) — not reachable"),
            "start the web app with `npm run dev`, or set \
             PROMPTLY_API_URL=https://trypromptly.vercel.app to play against production",
        ),
        (false, false) => Check::warn(
            "web app",
            format!("configured for {base_url} — not reachable"),
            "check your connection and the URL (--api-url / PROMPTLY_API_URL)",
        ),
    }
}

fn check_judge0(health: Result<ExecutionHealth, WebError>) -> Check {
    match health {
        Ok(h) if h.healthy => Check::ok("judge0", "execution backend reachable"),
        Ok(h) => Check::warn(
            "judge0",
            format!("execution backend unhealthy ({})", h.reason),
            "remote grading is unavailable; local `promptly test` still works",
        ),
        Err(WebError::NotReachable(base)) => Check::warn(
            "judge0",
            format!("web app unreachable at {base}"),
            "set --api-url or PROMPTLY_API_URL if the web app is elsewhere",
        ),
        Err(err) => Check::warn(
            "judge0",
            format!("health check failed: {err}"),
            "retry, or check --api-url",
        ),
    }
}

/// Map one harness adapter's reported state to a diagnostic (`21`). These sources
/// are best-effort, so a missing one is informational, not a problem; only an
/// unrecognized format (capture from it silently paused) warrants a warning. None
/// is ever a hard failure — Claude Code capture is unaffected.
fn check_adapter(status: &AdapterStatus) -> Check {
    let name = format!("{} adapter", status.name);
    match status.state {
        // Located and reading its source.
        AdapterState::Detected => Check::ok(&name, status.detail.clone()),
        // The harness isn't installed, or has no data for this workspace yet —
        // expected for an unused harness, so informational rather than a problem.
        AdapterState::NotFound => Check::ok(&name, status.detail.clone()),
        // The source exists but its schema/version isn't recognized (it likely
        // updated); capture from it is paused while the rest keeps working.
        AdapterState::Unsupported => Check::warn(
            &name,
            status.detail.clone(),
            "this harness changed its log/storage format; capture from it is paused until the adapter is updated",
        ),
    }
}

/// Report whether a newer release is available (`promptly update`). Best-effort:
/// when the check couldn't reach GitHub it's reported as OK ("couldn't check"),
/// never a failure — being offline mustn't turn `doctor` red.
fn check_update(status: &crate::update_check::UpdateStatus) -> Check {
    match status.latest {
        Some(latest) if latest > status.current => Check::warn(
            "version",
            format!("v{} installed — v{latest} available", status.current),
            "run `promptly update` to upgrade promptly + promptlyd",
        ),
        Some(_) => Check::ok("version", format!("v{} (latest)", status.current)),
        None => Check::ok(
            "version",
            format!("v{} (couldn't check for updates)", status.current),
        ),
    }
}

/// One check line. `name_col` is the shared width the (visible) name is padded
/// to — padded *before* styling so the ANSI escapes never eat the alignment.
fn render_check(check: &Check, name_col: usize, style: Style) -> String {
    let padded = format!("{:<name_col$}", check.name);
    let (symbol, name) = match check.level {
        CheckLevel::Ok => (style.green("✓"), style.bold(&padded)),
        CheckLevel::Warn => (style.yellow("!"), style.bold(&padded)),
        CheckLevel::Fail => (style.red("✗"), style.bold(&padded)),
    };
    let mut line = format!("{symbol} {name}  {}\n", style.dim(&check.detail));
    if check.level != CheckLevel::Ok {
        if let Some(hint) = &check.hint {
            line.push_str(&format!("    {} {}\n", style.dim("→"), style.dim(hint)));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(recent: usize) -> Health {
        Health {
            status: "ok".into(),
            version: "0.1.0".into(),
            workspace: "/ws".into(),
            uptime_ms: 1000,
            otlp_endpoint: "http://127.0.0.1:4318".into(),
            turns: 0,
            recent_errors: (0..recent)
                .map(|i| crate::daemon_client::DiagEvent {
                    timestamp_ms: i as i64,
                    level: "WARN".into(),
                    message: "x".into(),
                })
                .collect(),
            adapters: Vec::new(),
        }
    }

    fn adapter(state: AdapterState) -> AdapterStatus {
        AdapterStatus {
            name: "cursor".into(),
            state,
            detail: "detail".into(),
        }
    }

    #[test]
    fn daemon_check_distinguishes_down_warnings_and_ok() {
        assert_eq!(check_daemon(&Ok(health(0))).level, CheckLevel::Ok);
        assert_eq!(check_daemon(&Ok(health(2))).level, CheckLevel::Warn);
        let down: Result<Health, DaemonError> = Err(DaemonError::NotRunning("x".into()));
        let check = check_daemon(&down);
        assert_eq!(check.level, CheckLevel::Fail);
        assert!(check.hint.unwrap().contains("promptly start"));
    }

    #[test]
    fn manifest_check_fails_with_a_remediation() {
        let err: Result<Manifest, ManifestError> = Err(ManifestError::Missing(
            std::path::PathBuf::from("/ws/.promptly/manifest.json"),
        ));
        let check = check_manifest(&err);
        assert_eq!(check.level, CheckLevel::Fail);
        assert!(check.hint.unwrap().contains("promptly init"));
    }

    #[test]
    fn runtime_check_reflects_the_three_states() {
        assert_eq!(check_runtime("go1.22", Some(true)).level, CheckLevel::Ok);
        assert_eq!(check_runtime("go1.22", Some(false)).level, CheckLevel::Warn);
        // No local runner (e.g. Rust) is an expected warn, not a failure.
        assert_eq!(check_runtime("rust1.75", None).level, CheckLevel::Warn);
    }

    #[test]
    fn web_check_reports_the_configured_url_and_reachability() {
        let healthy = Ok(ExecutionHealth {
            healthy: true,
            reason: "ok".into(),
            message: None,
            version: None,
        });
        // A reachable production URL is OK and names the host.
        let prod = check_web("https://trypromptly.vercel.app", &healthy);
        assert_eq!(prod.level, CheckLevel::Ok);
        assert!(prod.detail.contains("trypromptly.vercel.app"));
        assert!(prod.detail.contains("production"));

        // A reachable-but-unhealthy backend still proves the web app is up.
        let unhealthy = Ok(ExecutionHealth {
            healthy: false,
            reason: "not_configured".into(),
            message: None,
            version: None,
        });
        assert_eq!(
            check_web("http://localhost:3000", &unhealthy).level,
            CheckLevel::Ok,
        );

        // An unreachable localhost default warns and points at PROMPTLY_API_URL.
        let unreachable: Result<ExecutionHealth, WebError> =
            Err(WebError::NotReachable("x".into()));
        let local = check_web("http://localhost:3000", &unreachable);
        assert_eq!(local.level, CheckLevel::Warn);
        assert!(local.hint.unwrap().contains("PROMPTLY_API_URL"));
    }

    #[test]
    fn judge0_check_warns_but_never_fails() {
        let healthy = ExecutionHealth {
            healthy: true,
            reason: "ok".into(),
            message: None,
            version: None,
        };
        assert_eq!(check_judge0(Ok(healthy)).level, CheckLevel::Ok);
        let down: Result<ExecutionHealth, WebError> = Err(WebError::NotReachable("x".into()));
        // A missing backend degrades remote grading but local test still works.
        assert_eq!(check_judge0(down).level, CheckLevel::Warn);
    }

    #[test]
    fn adapter_check_maps_states_to_levels() {
        // Detected and not-found are both fine (best-effort sources); only an
        // unrecognized format warns, and nothing here ever fails.
        assert_eq!(
            check_adapter(&adapter(AdapterState::Detected)).level,
            CheckLevel::Ok,
        );
        assert_eq!(
            check_adapter(&adapter(AdapterState::NotFound)).level,
            CheckLevel::Ok,
        );
        let unsupported = check_adapter(&adapter(AdapterState::Unsupported));
        assert_eq!(unsupported.level, CheckLevel::Warn);
        assert_eq!(unsupported.name, "cursor adapter");
        assert!(unsupported.hint.is_some());
    }

    #[test]
    fn render_includes_a_remediation_arrow_for_problems() {
        let check = Check::fail("daemon", "not running", "start it");
        let text = render_check(&check, 6, Style::plain());
        assert!(text.contains("✗ daemon"));
        assert!(text.contains("→ start it"));
        // An OK check shows no arrow.
        let ok = render_check(&Check::ok("manifest", "fine"), 8, Style::plain());
        assert!(!ok.contains("→"));
    }

    #[test]
    fn report_aligns_details_and_ends_with_a_verdict() {
        let checks = vec![
            Check::ok("daemon", "running"),
            Check::warn("otel config", "not configured", "run `promptly start`"),
            Check::fail("manifest", "missing", "run `promptly init`"),
        ];
        let text = render_report(&checks, Style::plain());
        // Details share one column: each name is padded to the widest name.
        let running = text.lines().find(|l| l.contains("running")).unwrap();
        let missing = text.lines().find(|l| l.contains("missing")).unwrap();
        assert_eq!(
            running.find("running").unwrap(),
            missing.find("missing").unwrap(),
        );
        // The report opens with its section rule and closes on an explicit
        // verdict: the per-check mark strip (report order) plus the counts.
        assert!(text.starts_with("── doctor "));
        assert!(text.contains("✓!✗ 2 of 3 checks failed"));
        assert!(text.contains("1 failure"));
        assert!(text.contains("1 warning"));

        let clean = render_report(&[Check::ok("daemon", "running")], Style::plain());
        assert!(clean.contains("✓ all 1 checks passed"));
    }

    #[test]
    fn update_check_warns_only_when_a_newer_release_exists() {
        use crate::update_check::UpdateStatus;
        use crate::updater::Version;
        let v = |s: &str| Version::parse(s).unwrap();
        let outdated = UpdateStatus {
            current: v("0.1.0"),
            latest: Some(v("0.2.0")),
        };
        assert_eq!(check_update(&outdated).level, CheckLevel::Warn);
        let current = UpdateStatus {
            current: v("0.2.0"),
            latest: Some(v("0.2.0")),
        };
        assert_eq!(check_update(&current).level, CheckLevel::Ok);
        // Offline (no latest known) is OK, not a failure.
        let unknown = UpdateStatus {
            current: v("0.1.0"),
            latest: None,
        };
        assert_eq!(check_update(&unknown).level, CheckLevel::Ok);
    }
}
