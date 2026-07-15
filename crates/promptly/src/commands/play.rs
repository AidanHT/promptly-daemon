//! `promptly play [level]` — the one-command path from nothing to capturing.
//!
//! With a level slug it fetches that level's starter workspace (like `init`), then
//! launches the background daemon scoped to it and opens a scored session — so a
//! single command takes you from "I want to try this level" to "I'm being scored."
//! With no slug it plays the level already in the current directory.

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
    // Expand a short alias (`lru`, `7`, `stage-1-01`) up front so the fetch and
    // the daemon scoping speak the one canonical slug the kit route expects.
    let level = args.level.as_deref().map(crate::levels::resolve);

    // 1. Resolve the workspace: fetch it if a level was named, else use the cwd.
    // When we fetch, remember the folder as created (`./lru` — the level's short
    // keyword) so the closing hint can spell the exact `cd`.
    let mut fetched_dir = None;
    let workspace = match &level {
        Some(level) => {
            let init_args = InitArgs::for_level(level.clone(), args.force);
            // Fetch WITHOUT init's "next: … `promptly start`" epilogue — play
            // starts the session itself, so its own closing hint (below) is the
            // one instruction set the player sees.
            let Some(target) = init::fetch_workspace(kits, init_args, now_ms, style)? else {
                return Ok(CommandExit::Failure);
            };
            fetched_dir = Some(target.clone());
            // Scope the daemon to the unpacked folder (absolute path).
            std::fs::canonicalize(&target).unwrap_or(target)
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            // Same wrong-directory guard as `start`: a no-arg `play` from a
            // non-workspace folder must fail HERE, before `ensure_running` —
            // which would otherwise end the active scored session and rescope
            // the daemon to this unrelated folder.
            if let Some(exit) = crate::cli::start_workspace_guard(&cwd, style) {
                return Ok(exit);
            }
            cwd
        }
    };

    // 2. Make sure the daemon is up and scoped to that workspace.
    daemon_process::ensure_running(api_port, &workspace, style)?;

    // 3. Open the scored session (preflight + start) against the daemon.
    let client = DaemonClient::new(api_port);
    let start_args = StartArgs::for_play(args.yes, args.consent, args.no_consent);
    let exit = session::run_start(&client, cloud, asker, start_args, style)?;

    // 4. We fetched into a subdir, so the player still has to cd there to run their
    //    AI harness — spell out the last steps, naming the folder exactly as created.
    if exit == CommandExit::Success {
        if let Some(dir) = &fetched_dir {
            println!(
                "  {}",
                style.dim(&format!(
                    "now: cd {} · solve with your AI harness · `promptly submit` when done",
                    dir.display()
                )),
            );
        }
    }
    Ok(exit)
}
