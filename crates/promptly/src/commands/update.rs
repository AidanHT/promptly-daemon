//! `promptly update` — upgrade the installed `promptly` + `promptlyd` binaries
//! to the latest GitHub release.
//!
//! The mechanism (resolve latest, download, extract, swap) lives in
//! [`crate::updater`]; this command sequences it with the user-facing messaging:
//! confirm the jump, stop the running daemon so its binary is free to replace,
//! then swap both — the daemon first and the running CLI last, so the binary
//! we're executing is the last thing touched.

use std::time::Duration;

use clap::Args;

use crate::daemon_process;
use crate::prompt::Ask;
use crate::style::Style;
use crate::updater::{self, Version};
use crate::CommandExit;

/// How long to wait on the GitHub API for an explicitly-requested update — more
/// patient than the background notifier.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Report whether a newer release is available and exit without installing it.
    #[arg(long)]
    check: bool,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    yes: bool,
}

pub fn run(
    asker: &mut dyn Ask,
    api_port: u16,
    args: UpdateArgs,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let current = Version::current();

    let tag = updater::fetch_latest_tag(RESOLVE_TIMEOUT)
        .map_err(|e| e.context("checking for the latest release"))?;
    let latest = Version::parse(&tag).ok_or_else(|| {
        anyhow::anyhow!("the latest release tag '{tag}' isn't a version I recognize")
    })?;

    if latest <= current {
        println!("{} promptly is up to date (v{current})", style.green("✓"));
        return Ok(CommandExit::Success);
    }

    println!(
        "{} v{current} {} {}",
        style.bold("Update available:"),
        style.dim("→"),
        style.bold(&style.accent(&format!("v{latest}"))),
    );

    if args.check {
        println!("  {}", style.dim("run `promptly update` to install it"));
        return Ok(CommandExit::Success);
    }

    // Confirm before swapping binaries: Enter (empty) means yes, and a
    // non-interactive shell proceeds since the user explicitly asked to update.
    if !args.yes && !asker.confirm("Update now?", true, true) {
        println!("{}", style.dim("update cancelled"));
        return Ok(CommandExit::Success);
    }

    let layout = updater::installed_layout()?;
    updater::ensure_not_dev_build(&layout)?;

    // A running promptlyd can't be cleanly replaced — stop it first. A foreign
    // process on the port means our daemon isn't running (so its binary is free to
    // swap): note the clash and carry on rather than aborting the whole update.
    match daemon_process::stop_background(api_port)? {
        daemon_process::BackgroundStop::Stopped { ended_session } => {
            if let Some(slug) = ended_session {
                println!(
                    "  {}",
                    style.dim(&format!("ended the open capture session for {slug}"))
                );
            }
            println!("  {}", style.dim("stopped the running daemon"));
        }
        daemon_process::BackgroundStop::NotRunning => {}
        daemon_process::BackgroundStop::ForeignPort(msg) => {
            println!("  {} {msg}", style.yellow("note:"));
        }
    }

    let asset = updater::asset_name(&tag);
    println!("  {}", style.dim(&format!("downloading {asset}")));
    let archive = updater::download(&updater::download_url(&tag, &asset))?;
    let bins = updater::extract_binaries(&archive, asset.ends_with(".zip"))?;

    // Daemon first, then the running CLI last.
    updater::replace_sibling(&layout.promptlyd, &bins.promptlyd)?;
    updater::replace_self(&layout.dir, &bins.promptly)?;

    println!(
        "{} {}",
        style.green("✓"),
        style.bold(&format!("Updated promptly + promptlyd to v{latest}")),
    );
    println!(
        "  {}",
        style.dim(
            "`promptly start` or `promptly play` will relaunch the daemon on the new version."
        ),
    );
    Ok(CommandExit::Success)
}
