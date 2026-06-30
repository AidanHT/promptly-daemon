//! `promptly` — the Promptly CLI (`docs/plan/19`).
//!
//! The terminal-native control surface for the whole local workflow: fetch a
//! workspace (`init`), run a scored attempt with session boundaries
//! (`start`/`stop`/`reset`) by driving the daemon's loopback control API (`18`),
//! watch live token burn (`watch`), score with parity to the server (`score`,
//! mirroring `13`), test locally (`test`), and diagnose setup problems
//! (`doctor`). Cloud auth/pairing and ranked upload (`submit`/`pair`)
//! are owned by `20`; this crate establishes their command surface and the local
//! work that precedes the cloud call.
//!
//! Logic lives here (not in `main.rs`) so every command is testable without a
//! live daemon or network: the daemon/web seams sit behind small traits with
//! in-memory fakes, and the scoring port is driven by the shared `13`/`19`
//! parity fixture.

use std::process::ExitCode;

pub mod cli;
pub mod cloud;
pub mod config;
pub mod credentials;
pub mod daemon_client;
pub mod daemon_process;
pub mod fmt;
pub mod projection;
pub mod prompt;
pub mod redaction;
pub mod runner;
pub mod scoring;
pub mod signing;
pub mod style;
pub mod submission;
pub mod web_client;

pub mod commands;

pub use cli::run;

/// A command's outcome, mapped to the process exit code in [`cli::run`].
///
/// Commands return this rather than [`ExitCode`] directly so their decision is
/// comparable in tests (`ExitCode` is opaque and not `PartialEq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandExit {
    /// The command did what it set out to do.
    Success,
    /// A diagnostic or check failed (e.g. `doctor`/`test`); the work itself ran.
    Failure,
}

impl CommandExit {
    pub fn into_code(self) -> ExitCode {
        match self {
            CommandExit::Success => ExitCode::SUCCESS,
            CommandExit::Failure => ExitCode::FAILURE,
        }
    }
}
