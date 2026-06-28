//! Self-diagnostics: a bounded ring of the most recent warnings and errors.
//!
//! Capture failures, watcher errors, and OTLP problems are logged with context
//! via `tracing` and never silently swallowed. A small `tracing` layer mirrors
//! every WARN/ERROR event into this ring, which `GET /health` surfaces so the CLI
//! (`promptly doctor`, `19`) can show what recently went wrong. Crash/error
//! reporting to the cloud stays opt-in (`31`) — this is machine-local.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::clock::now_ms;

/// How many recent diagnostic events to retain.
const MAX_EVENTS: usize = 50;

/// One captured warning or error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagEvent {
    pub timestamp_ms: i64,
    pub level: String,
    pub message: String,
}

/// A shared, bounded ring of recent diagnostic events.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    inner: Arc<Mutex<VecDeque<DiagEvent>>>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an event, evicting the oldest once the ring is full.
    pub fn record(&self, level: &str, message: String) {
        let mut ring = self.inner.lock().unwrap();
        if ring.len() == MAX_EVENTS {
            ring.pop_front();
        }
        ring.push_back(DiagEvent {
            timestamp_ms: now_ms(),
            level: level.to_string(),
            message,
        });
    }

    /// The recent events, oldest first.
    pub fn recent(&self) -> Vec<DiagEvent> {
        self.inner.lock().unwrap().iter().cloned().collect()
    }

    /// A `tracing` layer that mirrors WARN/ERROR events into this ring.
    pub fn layer(&self) -> DiagLayer {
        DiagLayer { diag: self.clone() }
    }
}

/// The `tracing` layer produced by [`Diagnostics::layer`].
pub struct DiagLayer {
    diag: Diagnostics,
}

/// Extracts the human message from an event's fields.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && self.message.is_none() {
            self.message = Some(format!("{value:?}"));
        }
    }
}

impl<S: Subscriber> Layer<S> for DiagLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level != Level::WARN && level != Level::ERROR {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor
            .message
            .unwrap_or_else(|| event.metadata().name().to_string());
        self.diag.record(level.as_str(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_keeps_the_newest() {
        let diag = Diagnostics::new();
        for i in 0..(MAX_EVENTS + 5) {
            diag.record("WARN", format!("event {i}"));
        }
        let recent = diag.recent();
        assert_eq!(recent.len(), MAX_EVENTS);
        assert_eq!(
            recent.last().unwrap().message,
            format!("event {}", MAX_EVENTS + 4)
        );
    }

    #[test]
    fn layer_captures_warnings_and_errors_only() {
        use tracing_subscriber::prelude::*;

        let diag = Diagnostics::new();
        let subscriber = tracing_subscriber::registry().with(diag.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("ignored info");
            tracing::warn!("disk getting full");
            tracing::error!(code = 7, "capture failed");
        });

        let recent = diag.recent();
        assert_eq!(recent.len(), 2, "info is not captured");
        assert_eq!(recent[0].level, "WARN");
        assert!(recent[0].message.contains("disk getting full"));
        assert_eq!(recent[1].level, "ERROR");
        assert!(recent[1].message.contains("capture failed"));
    }
}
