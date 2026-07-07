//! `promptly status` — report whether the daemon is running and, if so, whether
//! it's capturing a bound session (`17` status surface, driven by the CLI).

use crate::daemon_client::{DaemonApi, DaemonError, Health, SessionSnapshot};
use crate::style::Style;
use crate::visual;
use crate::CommandExit;

pub fn run(client: &dyn DaemonApi, style: Style) -> anyhow::Result<CommandExit> {
    let health = match client.health() {
        Ok(h) => h,
        Err(DaemonError::NotRunning(base)) => {
            println!("{}", style.red("● daemon not running"));
            println!(
                "  {}",
                style.dim(&format!(
                    "no daemon at {base} — `promptly start` launches it automatically (or `promptly up`)"
                )),
            );
            return Ok(CommandExit::Failure);
        }
        Err(err) => return Err(err.into()),
    };
    // Health succeeded, so the session read should too; treat a hiccup as idle.
    let session = client.session().ok();
    print!("{}", render_status(&health, session.as_ref(), style));
    Ok(CommandExit::Success)
}

/// Render the status report. Returns the text so it's unit-testable.
pub fn render_status(health: &Health, session: Option<&SessionSnapshot>, style: Style) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}\n",
        style.green("● daemon running"),
        style.dim(&format!(
            "v{}, up {}",
            health.version,
            format_uptime(health.uptime_ms)
        )),
    ));

    let capturing = session
        .and_then(|s| s.session.as_ref())
        .filter(|m| m.is_active());
    match capturing {
        Some(marker) => {
            let turns = session.map(|s| s.turns).unwrap_or(0);
            out.push_str(&format!(
                "  {} {} · {} turns · integrity cap {}\n",
                style.dim("capturing"),
                style.accent(&marker.slug),
                turns,
                style.bold(marker.integrity_cap()),
            ));
            // The captured token totals — the number an attempt is scored on —
            // with their composition as a bar (input `█` / output `▓` / think `▒`).
            if let Some(t) = session.map(|s| &s.totals).filter(|t| t.turns > 0) {
                out.push_str(&format!(
                    "  {} {} in · {} out · {} think\n",
                    style.dim("tokens"),
                    crate::fmt::thousands(t.tokens_input as u128),
                    crate::fmt::thousands(t.tokens_output as u128),
                    crate::fmt::thousands(t.tokens_thinking as u128),
                ));
                let mix = visual::token_mix(
                    style,
                    24,
                    t.tokens_input as f64,
                    t.tokens_output as f64,
                    t.tokens_thinking as f64,
                );
                if !mix.is_empty() {
                    out.push_str(&format!(
                        "  {} {}  {}\n",
                        style.dim("mix   "),
                        mix,
                        visual::token_mix_legend(style),
                    ));
                }
            }
            if marker.code_reset_count > 0 {
                out.push_str(&format!(
                    "  {}\n",
                    style.dim(&format!("workspace resets: {}", marker.code_reset_count)),
                ));
            }
        }
        None => {
            out.push_str(&format!(
                "  {}\n",
                style.dim("idle — no active capture session")
            ));
        }
    }

    if !health.recent_errors.is_empty() {
        out.push_str(&format!(
            "  {}\n",
            style.yellow(&format!(
                "{} recent daemon warning(s) — see `promptly doctor`",
                health.recent_errors.len()
            )),
        ));
    }
    out
}

/// Human-readable uptime: `42s`, `5m`, `2h 3m`.
fn format_uptime(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_client::{
        DiagEvent, ResetReport, SessionMarker, StartDecisions, StartOutcome, StartPlan, StopReport,
        Totals,
    };

    fn health(errors: Vec<DiagEvent>) -> Health {
        Health {
            status: "ok".into(),
            version: "0.1.0".into(),
            workspace: "/ws".into(),
            uptime_ms: 65_000,
            otlp_endpoint: "http://127.0.0.1:4318".into(),
            turns: 3,
            recent_errors: errors,
            adapters: Vec::new(),
        }
    }

    fn marker(active: bool) -> SessionMarker {
        SessionMarker {
            version: 1,
            session_id: "s1".into(),
            workspace: std::path::PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01".into(),
            started_at_ms: 1000,
            stopped_at_ms: if active { None } else { Some(2000) },
            attempt_nonce: "n".into(),
            nonce_origin: promptlyd::scoping::NonceOrigin::Local,
            file_allowlist: vec!["lru.go".into()],
            code_reset_count: 1,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    fn snapshot(marker: Option<SessionMarker>) -> SessionSnapshot {
        SessionSnapshot {
            session: marker,
            totals: Totals {
                turns: 5,
                tokens_input: 12_400,
                tokens_output: 3_200,
                tokens_thinking: 0,
                tokens_cache: 0,
            },
            turns: 5,
            signals: vec![],
            captured: vec![],
        }
    }

    #[test]
    fn reports_capturing_with_the_bound_level_and_reset_count() {
        let snap = snapshot(Some(marker(true)));
        let text = render_status(&health(vec![]), Some(&snap), Style::plain());
        assert!(text.contains("daemon running"));
        assert!(text.contains("up 1m"));
        assert!(text.contains("capturing"));
        assert!(text.contains("stage-1-01"));
        assert!(text.contains("5 turns"));
        assert!(text.contains("12,400 in · 3,200 out"));
        // The composition bar under the totals: mostly input (█) with some
        // output (▓), and a legend naming the glyphs.
        assert!(text.contains("mix"));
        assert!(text.contains('█'));
        assert!(text.contains('▓'));
        assert!(text.contains("█ in"));
        assert!(text.contains("integrity cap unverified"));
        assert!(text.contains("workspace resets: 1"));
    }

    #[test]
    fn reports_idle_when_no_active_session_and_surfaces_warnings() {
        // A stopped marker is not "capturing".
        let snap = snapshot(Some(marker(false)));
        let errs = vec![DiagEvent {
            timestamp_ms: 1,
            level: "WARN".into(),
            message: "x".into(),
        }];
        let text = render_status(&health(errs), Some(&snap), Style::plain());
        assert!(text.contains("idle"));
        assert!(text.contains("1 recent daemon warning"));
    }

    /// A fake daemon used by the command-level test (no sockets).
    struct FakeDaemon {
        health: Result<Health, DaemonErrorKind>,
    }

    enum DaemonErrorKind {
        NotRunning,
    }

    impl DaemonApi for FakeDaemon {
        fn health(&self) -> Result<Health, DaemonError> {
            match &self.health {
                Ok(h) => Ok(h.clone()),
                Err(DaemonErrorKind::NotRunning) => {
                    Err(DaemonError::NotRunning("http://127.0.0.1:8765".into()))
                }
            }
        }
        fn session(&self) -> Result<SessionSnapshot, DaemonError> {
            Ok(snapshot(None))
        }
        fn preflight(&self) -> Result<StartPlan, DaemonError> {
            unreachable!()
        }
        fn start(&self, _: StartDecisions) -> Result<StartOutcome, DaemonError> {
            unreachable!()
        }
        fn stop(&self) -> Result<StopReport, DaemonError> {
            unreachable!()
        }
        fn reset(&self) -> Result<ResetReport, DaemonError> {
            unreachable!()
        }
    }

    #[test]
    fn run_returns_failure_when_the_daemon_is_down() {
        let fake = FakeDaemon {
            health: Err(DaemonErrorKind::NotRunning),
        };
        let exit = run(&fake, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);
    }
}
