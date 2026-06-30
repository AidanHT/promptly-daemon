//! The `promptly` subcommand handlers.
//!
//! Each module owns one command, takes its parsed args plus the resolved
//! [`crate::style::Style`], and returns the process exit code. Handlers keep
//! their I/O at the edges so the decision logic stays unit-testable.

pub mod daemon;
pub mod doctor;
pub mod help;
pub mod init;
pub mod play;
pub mod restart;
pub mod score;
pub mod session;
pub mod status;
pub mod submit;
pub mod test;
pub mod update;
pub mod watch;
