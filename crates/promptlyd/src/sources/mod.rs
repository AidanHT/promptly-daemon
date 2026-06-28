//! Capture sources.
//!
//! Every source — the embedded OTLP receiver, the JSONL watcher, and the
//! adapters `21` will add — implements [`TelemetrySource`]: it runs until told to
//! stop, pushing [`RawTurn`](crate::model::RawTurn)s into the engine's channel.
//! Keeping them behind one trait is what lets `21` add Cursor/Codex adapters
//! without touching the engine.

pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod jsonl;
pub mod otel;
pub mod registry;
pub mod vscode;

use async_trait::async_trait;

use crate::model::RawTurn;

/// The channel a source pushes observed raw turns into; the engine owns the
/// receiving end and normalizes/correlates them.
pub type RawTurnSink = tokio::sync::mpsc::Sender<RawTurn>;

/// A shutdown signal shared with every source: it flips to `true` once, and
/// sources awaiting [`wait`](wait_for_shutdown) wake and return.
pub type Shutdown = tokio::sync::watch::Receiver<bool>;

/// Await the shutdown flag flipping to `true` (or the sender dropping).
pub async fn wait_for_shutdown(shutdown: &mut Shutdown) {
    // Already shutting down? return immediately.
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

/// A local telemetry capture source.
#[async_trait]
pub trait TelemetrySource: Send {
    /// Stable identifier for logs and diagnostics.
    fn name(&self) -> &'static str;

    /// Run until `shutdown` flips, feeding observed turns into `sink`. Returning
    /// `Err` is a fatal capture failure the engine logs against this source.
    async fn run(self: Box<Self>, sink: RawTurnSink, shutdown: Shutdown) -> anyhow::Result<()>;
}
