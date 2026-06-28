//! `promptly` — the player's terminal control surface for the Promptly local
//! workflow (`docs/plan/19`). The binary is a one-liner into [`promptly::run`];
//! all logic lives in the library so the command surface is unit-testable.

use std::process::ExitCode;

fn main() -> ExitCode {
    promptly::run()
}
