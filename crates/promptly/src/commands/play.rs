//! `promptly play [level]` — the one-command path from nothing to capturing.
//!
//! With a level slug it fetches that level's starter workspace (like `init`), then
//! launches the background daemon scoped to it and opens a scored session — so a
//! single command takes you from "I want to try this level" to "I'm being scored."
//! With no slug it plays the level already in the current directory.

use std::path::PathBuf;

use clap::Args;

use crate::cloud::Cloud;
use crate::commands::init::{self, InitArgs};
use crate::commands::session::{self, StartArgs};
use crate::daemon_client::DaemonClient;
use crate::daemon_process;
use crate::prompt::Ask;
use crate::style::Style;
use crate::web_client::KitSource;
use crate::CommandExit;

#[derive(Debug, Args)]
pub struct PlayArgs {
    /// Level to fetch and play: a short alias (`lru`), its number (`1`), or the
    /// full slug. Omit to play the level already in the current directory.
    level: Option<String>,
    /// When fetching, overwrite a non-empty target directory with the starter.
    #[arg(long)]
    force: bool,
    /// Answer yes to every prompt (reset confirm, OTEL consent).
    #[arg(long)]
    yes: bool,
    /// Consent to writing the OTEL telemetry env into the project settings.
    #[arg(long, conflicts_with = "no_consent")]
    consent: bool,
    /// Decline the OTEL bootstrap; capture falls back to JSONL-only.
    #[arg(long)]
    no_consent: bool,
}

pub fn run(
    kits: &dyn KitSource,
    cloud: &dyn Cloud,
    asker: &mut dyn Ask,
    api_port: u16,
    args: PlayArgs,
    now_ms: i64,
    style: Style,
) -> anyhow::Result<CommandExit> {
    // Expand a short alias (`lru`, `7`, `stage-1-01`) up front so the fetch, the
    // daemon scoping, and the closing hint all speak the one canonical slug that
    // `init` names the workspace directory after.
    let level = args.level.as_deref().map(crate::levels::resolve);

    // 1. Resolve the workspace: fetch it if a level was named, else use the cwd.
    let workspace = match &level {
        Some(level) => {
            let init_args = InitArgs::for_level(level.clone(), args.force);
            if init::run(kits, init_args, now_ms, style)? == CommandExit::Failure {
                return Ok(CommandExit::Failure);
            }
            // init unpacked into ./<slug>; scope the daemon there (absolute path).
            let target = PathBuf::from(level);
            std::fs::canonicalize(&target).unwrap_or(target)
        }
        None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
    };

    // 2. Make sure the daemon is up and scoped to that workspace.
    daemon_process::ensure_running(api_port, &workspace, style)?;

    // 3. Open the scored session (preflight + start) against the daemon.
    let client = DaemonClient::new(api_port);
    let start_args = StartArgs::for_play(args.yes, args.consent, args.no_consent);
    let exit = session::run_start(&client, cloud, asker, start_args, style)?;

    // 4. We fetched into a subdir, so the player still has to cd there to run their
    //    AI harness — spell out the last steps.
    if exit == CommandExit::Success {
        if let Some(level) = &level {
            println!(
                "  {}",
                style.dim(&format!(
                    "now: cd {level} · solve with your AI harness · `promptly submit` when done"
                )),
            );
        }
    }
    Ok(exit)
}
