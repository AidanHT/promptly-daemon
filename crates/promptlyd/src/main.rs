//! `promptlyd` binary entrypoint. All logic lives in the library crate so it can
//! be exercised by unit and integration tests; `main` only dispatches the CLI.

use std::process::ExitCode;

fn main() -> ExitCode {
    promptlyd::cli::main()
}
