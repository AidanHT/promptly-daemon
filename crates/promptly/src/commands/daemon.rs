//! `promptly up` / `promptly down` — explicit control of the background capture
//! daemon that the session commands otherwise manage for you.
//!
//! Most players never need these: `promptly start` and `promptly play` launch the
//! daemon automatically. They're here for when you'd rather start the daemon once
//! and leave it running, or stop it when you're done for the day.

use std::path::Path;

use crate::daemon_process::{self, Ensured};
use crate::style::Style;
use crate::CommandExit;

/// `promptly up` — make sure the background daemon is running, scoped to `workspace`.
pub fn run_up(api_port: u16, workspace: &Path, style: Style) -> anyhow::Result<CommandExit> {
    let here = workspace.display();
    match daemon_process::ensure_running(api_port, workspace, style)? {
        Ensured::AlreadyRunning => {
            println!("{}", style.green("● daemon already running"));
        }
        Ensured::Started => {
            println!(
                "{} {}",
                style.green("● daemon started"),
                style.dim(&format!("watching {here}")),
            );
        }
        Ensured::Restarted => {
            println!(
                "{} {}",
                style.green("● daemon restarted"),
                style.dim(&format!("now watching {here}")),
            );
        }
    }
    println!(
        "  {}",
        style.dim("`promptly start` to begin a session · `promptly down` to stop the daemon"),
    );
    Ok(CommandExit::Success)
}

/// `promptly down` — stop the background daemon if one is running.
pub fn run_down(api_port: u16, style: Style) -> anyhow::Result<CommandExit> {
    if daemon_process::stop_background(api_port)? {
        println!("{}", style.green("● daemon stopped"));
    } else {
        println!("{}", style.dim("no daemon was running"));
    }
    Ok(CommandExit::Success)
}
