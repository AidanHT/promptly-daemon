//! `promptlyd` — Promptly's local telemetry daemon (`docs/plan/17`).
//!
//! It captures Claude Code AI usage two ways (an embedded OTLP receiver and a
//! JSONL session-log watcher), normalizes every source into one unified turn
//! record, and exposes the live stream over a localhost-only HTTP API. Everything
//! is local: cloud pairing and upload are a later slice (`20`).
//!
//! The modules are layered so the pure data core (encoding, normalization,
//! correlation, checkpointing) is unit-testable without sockets, a runtime, or a
//! live Claude Code, while the async shell (`engine`, `api`, the source watchers)
//! wires them onto Tokio.

pub mod api;
pub mod baseline;
pub mod bootstrap;
pub mod checkpoint;
pub mod cli;
pub mod clock;
pub mod config;
pub mod correlate;
pub mod diagnostics;
pub mod engine;
pub mod manifest;
pub mod model;
pub mod model_map;
pub mod normalize;
pub mod otlp;
pub mod paths;
pub mod provenance;
pub mod scoping;
pub mod service;
pub mod session;
pub mod sources;
pub mod status;
pub mod watcher;
