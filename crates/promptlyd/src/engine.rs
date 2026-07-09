//! The ingest engine — the heart that ties the capture sources to the outputs.
//!
//! Raw turns arrive from every source on one channel. The engine de-duplicates
//! them (so a re-read JSONL line or resent OTEL event is never double-counted),
//! runs them through cross-source correlation, then **attributes** the normalized
//! result to the active session (`18`): a turn is counted only if it falls within
//! the start/stop window and comes from the bound workspace, and is stamped with
//! the attempt nonce. Attributed turns are appended to the session store,
//! broadcast to live subscribers, and periodically checkpointed so a restart
//! resumes cleanly. Turns outside the session are dropped — unrelated AI usage
//! never inflates an attempt.
//!
//! `process_raw`/`flush`/`checkpoint` are synchronous and take "now" explicitly,
//! so the whole pipeline is unit-testable without timers; `run` is the async
//! driver that calls them on channel events and a flush tick.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::checkpoint::{offsets_to_strings, Checkpoint, CHECKPOINT_VERSION};
use crate::clock::now_ms;
use crate::correlate::{Correlator, Tolerance};
use crate::model::{NormalizedTurn, RawTurn};
use crate::provenance::ProvenanceSignal;
use crate::scoping::SessionMarker;
use crate::sources::jsonl::SharedOffsets;
use crate::sources::{wait_for_shutdown, Shutdown};

/// Live-stream backlog kept for slow subscribers before lagging.
const BROADCAST_CAPACITY: usize = 256;
/// How often the engine flushes correlation buffers and checkpoints if dirty.
const DEFAULT_FLUSH: Duration = Duration::from_millis(1_000);

/// Aggregate token totals over the session's captured turns. `Deserialize` is
/// derived so the `promptly` CLI (`19`) reads this back from `GET /session`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub turns: usize,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_thinking: u64,
    pub tokens_cache: u64,
}

/// State shared with the HTTP API: the session binding, the captured turns, the
/// provenance signals, and the live broadcast. Cloneable `Arc`; the engine and
/// the control endpoints write, the API reads.
#[derive(Debug)]
pub struct SharedState {
    /// The active scoped session (`None` = idle). Updated by `start`/`stop` (`18`)
    /// and read per-turn for attribution.
    binding: Mutex<Option<SessionMarker>>,
    turns: Mutex<Vec<NormalizedTurn>>,
    signals: Mutex<Vec<ProvenanceSignal>>,
    broadcast: broadcast::Sender<NormalizedTurn>,
}

impl SharedState {
    pub(crate) fn new(binding: Option<SessionMarker>, initial: Vec<NormalizedTurn>) -> Arc<Self> {
        let (broadcast, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            binding: Mutex::new(binding),
            turns: Mutex::new(initial),
            signals: Mutex::new(Vec::new()),
            broadcast,
        })
    }

    /// Subscribe to the live stream of normalized turns.
    pub fn subscribe(&self) -> broadcast::Receiver<NormalizedTurn> {
        self.broadcast.subscribe()
    }

    /// A copy of the active session binding (`None` when idle).
    pub fn binding(&self) -> Option<SessionMarker> {
        self.binding.lock().unwrap().clone()
    }

    /// Replace the active session binding (the `start`/`stop` control path).
    pub fn set_binding(&self, binding: Option<SessionMarker>) {
        *self.binding.lock().unwrap() = binding;
    }

    /// Begin a fresh attempt: bind the new session and clear the prior attempt's
    /// captured turns and provenance signals, so its totals never bleed into the
    /// new one. (Resume keeps the turns; it uses [`set_binding`] instead.)
    pub fn begin_session(&self, marker: SessionMarker) {
        *self.binding.lock().unwrap() = Some(marker);
        self.turns.lock().unwrap().clear();
        self.signals.lock().unwrap().clear();
    }

    /// A copy of every captured turn so far.
    pub fn snapshot(&self) -> Vec<NormalizedTurn> {
        self.turns.lock().unwrap().clone()
    }

    pub fn turn_count(&self) -> usize {
        self.turns.lock().unwrap().len()
    }

    /// Record a provenance signal (`18`) for the server's integrity checks (`25`).
    pub fn record_signal(&self, signal: ProvenanceSignal) {
        self.signals.lock().unwrap().push(signal);
    }

    /// The provenance signals raised this session.
    pub fn signals(&self) -> Vec<ProvenanceSignal> {
        self.signals.lock().unwrap().clone()
    }

    /// Summed token totals over the captured turns.
    pub fn totals(&self) -> Totals {
        let turns = self.turns.lock().unwrap();
        let mut totals = Totals {
            turns: turns.len(),
            ..Totals::default()
        };
        for turn in turns.iter() {
            // Saturate: a hostile/over-huge count clamps rather than wrapping
            // (release) or panicking (debug) over an unbounded turn stream.
            totals.tokens_input = totals.tokens_input.saturating_add(turn.tokens_input);
            totals.tokens_output = totals.tokens_output.saturating_add(turn.tokens_output);
            totals.tokens_thinking = totals.tokens_thinking.saturating_add(turn.tokens_thinking);
            totals.tokens_cache = totals.tokens_cache.saturating_add(turn.tokens_cache);
        }
        totals
    }

    /// Attribute a normalized turn to the active session, stamping the attempt
    /// nonce. Returns `None` (the turn is dropped, not counted) when there is no
    /// active session or the turn falls outside its window/workspace.
    pub fn attribute(&self, mut turn: NormalizedTurn) -> Option<NormalizedTurn> {
        let guard = self.binding.lock().unwrap();
        let marker = guard.as_ref()?;
        if !marker.attributes(turn.timestamp_ms, turn.workspace.as_deref()) {
            return None;
        }
        turn.attempt_nonce = Some(marker.attempt_nonce.clone());
        // Bind the turn to the workspace even when the source carried no cwd, so
        // downstream (`20`/`25`) can tie it to the attempt unambiguously.
        if turn.workspace.is_none() {
            turn.workspace = Some(marker.workspace.to_string_lossy().into_owned());
        }
        Some(turn)
    }

    fn push(&self, turn: NormalizedTurn) {
        self.turns.lock().unwrap().push(turn.clone());
        // Err just means no live subscribers; the turn is still stored.
        let _ = self.broadcast.send(turn);
    }
}

/// Everything the engine needs at construction, including any restored state.
pub struct EngineInit {
    /// The active session binding restored from the marker (`None` = idle).
    pub binding: Option<SessionMarker>,
    pub checkpoint_path: PathBuf,
    pub jsonl_offsets: SharedOffsets,
    pub restored_turns: Vec<NormalizedTurn>,
    pub restored_seen: Vec<String>,
}

/// The ingest engine. Owns the raw-turn receiver and the correlation/dedup state.
pub struct Engine {
    rx: mpsc::Receiver<RawTurn>,
    correlator: Correlator,
    seen: HashSet<String>,
    shared: Arc<SharedState>,
    checkpoint_path: PathBuf,
    jsonl_offsets: SharedOffsets,
    flush_interval: Duration,
    dirty: bool,
}

impl Engine {
    /// Build the engine and the shared state the API will read. The shared state
    /// is pre-seeded with any turns restored from the checkpoint.
    pub fn new(rx: mpsc::Receiver<RawTurn>, init: EngineInit) -> (Self, Arc<SharedState>) {
        let shared = SharedState::new(init.binding, init.restored_turns);
        let engine = Self {
            rx,
            correlator: Correlator::new(Tolerance::default()),
            seen: init.restored_seen.into_iter().collect(),
            shared: Arc::clone(&shared),
            checkpoint_path: init.checkpoint_path,
            jsonl_offsets: init.jsonl_offsets,
            flush_interval: DEFAULT_FLUSH,
            dirty: false,
        };
        (engine, shared)
    }

    /// Ingest one raw turn. Already-seen turns (by [`RawTurn::dedup_id`]) are
    /// dropped: a re-read line or resent event after a restart is never counted
    /// twice, and — because Claude Code's JSONL writes one line per content block
    /// — the 2-3 block-lines of a single assistant turn (same `message.id`,
    /// identical repeated usage) collapse to the first one seen, whose thinking
    /// block carries the thinking estimate.
    pub fn process_raw(&mut self, raw: RawTurn, now_ms: i64) {
        if !self.seen.insert(raw.dedup_id()) {
            return;
        }
        if let Some(turn) = self.correlator.ingest(raw, now_ms) {
            self.emit(turn);
        }
    }

    /// Emit any correlation-buffered turns whose window elapsed without a
    /// counterpart (as single-source turns).
    pub fn flush(&mut self, now_ms: i64) {
        for turn in self.correlator.flush_expired(now_ms) {
            self.emit(turn);
        }
    }

    /// Attribute a normalized turn to the active session and, if it counts, store
    /// + broadcast it. Turns outside the session are silently dropped.
    fn emit(&mut self, turn: NormalizedTurn) {
        if let Some(attributed) = self.shared.attribute(turn) {
            self.shared.push(attributed);
            self.dirty = true;
        }
    }

    /// Persist the current session, turns, JSONL offsets, and dedup set. A no-op
    /// when idle (there is no session to checkpoint).
    pub fn checkpoint(&mut self) -> std::io::Result<()> {
        let Some(binding) = self.shared.binding() else {
            self.dirty = false;
            return Ok(());
        };
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            session_id: binding.session_id,
            started_at_ms: binding.started_at_ms,
            turns: self.shared.snapshot(),
            jsonl_offsets: offsets_to_strings(&self.jsonl_offsets.lock().unwrap()),
            seen: self.seen.iter().cloned().collect(),
        };
        checkpoint.save(&self.checkpoint_path)?;
        self.dirty = false;
        Ok(())
    }

    /// Drive the engine until shutdown: ingest raw turns, flush + checkpoint on a
    /// tick, and on stop drain any buffered turns and checkpoint a final time.
    pub async fn run(mut self, mut shutdown: Shutdown) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.flush_interval);
        loop {
            tokio::select! {
                maybe = self.rx.recv() => {
                    match maybe {
                        Some(raw) => self.process_raw(raw, now_ms()),
                        None => break, // all sources gone
                    }
                }
                _ = ticker.tick() => {
                    self.flush(now_ms());
                    self.checkpoint_if_dirty();
                }
                () = wait_for_shutdown(&mut shutdown) => break,
            }
        }
        for turn in self.correlator.drain() {
            self.emit(turn);
        }
        self.checkpoint_if_dirty();
        tracing::info!("ingest engine stopped");
        Ok(())
    }

    fn checkpoint_if_dirty(&mut self) {
        if self.dirty {
            if let Err(err) = self.checkpoint() {
                tracing::warn!(%err, "checkpoint write failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{sample_raw, Source};
    use crate::scoping::{NonceOrigin, SessionMarker, SESSION_MARKER_VERSION};
    use std::collections::HashMap;

    /// An active session binding that attributes the sample turns (started at the
    /// epoch, bound to `/ws`).
    fn active_marker(session_id: &str) -> SessionMarker {
        SessionMarker {
            version: SESSION_MARKER_VERSION,
            session_id: session_id.to_string(),
            workspace: PathBuf::from("/ws"),
            level_id: "lvl-1".into(),
            slug: "stage-1-01".into(),
            started_at_ms: 0,
            stopped_at_ms: None,
            attempt_nonce: "nonce-xyz".into(),
            nonce_origin: NonceOrigin::Local,
            file_allowlist: vec!["lru.go".into()],
            code_reset_count: 0,
            bootstrap: None,
            otlp_token: None,
            baseline_attested: false,
        }
    }

    fn init(checkpoint_path: PathBuf) -> EngineInit {
        EngineInit {
            binding: Some(active_marker("sess-1")),
            checkpoint_path,
            jsonl_offsets: Arc::new(Mutex::new(HashMap::from([(
                PathBuf::from("/x/s.jsonl"),
                42,
            )]))),
            restored_turns: Vec::new(),
            restored_seen: Vec::new(),
        }
    }

    fn tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("promptlyd-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{label}.json"))
    }

    #[test]
    fn emits_stamps_the_nonce_dedups_and_checkpoints() {
        let cp = tmp("emit");
        let (_tx, rx) = mpsc::channel(8);
        let (mut engine, shared) = Engine::new(rx, init(cp.clone()));
        let mut stream = shared.subscribe();

        // A single-source turn is buffered until its window elapses.
        let raw = sample_raw(Source::Jsonl, Some("claude-opus-4-8"), 100, 50);
        engine.process_raw(raw.clone(), 0);
        assert_eq!(shared.turn_count(), 0, "buffered for correlation");
        engine.flush(10_000);
        assert_eq!(shared.turn_count(), 1, "flushed as single-source");

        let streamed = stream.try_recv().expect("subscriber receives the turn");
        assert_eq!(streamed.tokens_output, 50);
        // The attributed turn is stamped with the session's attempt nonce.
        assert_eq!(streamed.attempt_nonce.as_deref(), Some("nonce-xyz"));

        // The same raw turn again is de-duplicated.
        engine.process_raw(raw, 11_000);
        engine.flush(20_000);
        assert_eq!(shared.turn_count(), 1, "deduped by content id");

        let totals = shared.totals();
        assert_eq!(totals.turns, 1);
        assert_eq!(totals.tokens_input, 100);

        engine.checkpoint().unwrap();
        let restored = Checkpoint::load(&cp).expect("checkpoint persisted");
        assert_eq!(restored.turns.len(), 1);
        assert_eq!(restored.session_id, "sess-1");
        assert_eq!(restored.jsonl_offsets.get("/x/s.jsonl").copied(), Some(42));
        std::fs::remove_file(&cp).ok();
    }

    #[test]
    fn turns_outside_the_session_are_not_attributed() {
        let cp = tmp("scope");
        let (_tx, rx) = mpsc::channel(8);
        let (mut engine, shared) = Engine::new(rx, init(cp.clone()));

        // A turn from a different workspace during the window is dropped.
        let mut foreign = sample_raw(Source::Jsonl, Some("m"), 10, 10);
        foreign.workspace = Some("/somewhere/else".into());
        engine.process_raw(foreign, 0);
        engine.flush(10_000);
        assert_eq!(shared.turn_count(), 0, "foreign workspace not counted");

        // Idle (no binding) drops everything, even from the right workspace.
        shared.set_binding(None);
        let mut local = sample_raw(Source::Jsonl, Some("m"), 10, 10);
        local.workspace = Some("/ws".into());
        local.timestamp_ms = 5_000;
        engine.process_raw(local, 0);
        engine.flush(20_000);
        assert_eq!(shared.turn_count(), 0, "idle daemon attributes nothing");
        std::fs::remove_file(&cp).ok();
    }

    #[test]
    fn restart_restores_turns_without_reprocessing() {
        let cp = tmp("restart");
        let (_tx, rx) = mpsc::channel(8);
        let (mut engine, _shared) = Engine::new(rx, init(cp.clone()));
        let raw = sample_raw(Source::Jsonl, Some("m"), 10, 20);
        engine.process_raw(raw.clone(), 0);
        engine.flush(10_000);
        engine.checkpoint().unwrap();

        // Simulate a restart: a new engine seeded from the checkpoint + marker.
        let saved = Checkpoint::load(&cp).unwrap();
        let (_tx2, rx2) = mpsc::channel(8);
        let (mut engine2, shared2) = Engine::new(
            rx2,
            EngineInit {
                binding: Some(active_marker(&saved.session_id)),
                checkpoint_path: cp.clone(),
                jsonl_offsets: Arc::new(Mutex::new(HashMap::new())),
                restored_turns: saved.turns,
                restored_seen: saved.seen,
            },
        );
        assert_eq!(
            shared2.turn_count(),
            1,
            "captured turn survives the restart"
        );

        // The same raw arriving again post-restart is ignored.
        engine2.process_raw(raw, 0);
        engine2.flush(10_000);
        assert_eq!(shared2.turn_count(), 1, "no duplication after restart");
        std::fs::remove_file(&cp).ok();
    }

    #[test]
    fn totals_saturate_instead_of_overflowing() {
        use crate::normalize::normalize;
        // Two turns each near u64::MAX on input: the sum would overflow a plain
        // `+` (debug-panic / release-wrap). `totals()` must clamp to u64::MAX.
        let mut a = sample_raw(Source::Jsonl, Some("m"), u64::MAX, 0);
        a.tokens_cache = u64::MAX;
        let b = sample_raw(Source::Otel, Some("m"), u64::MAX, 0);
        let shared = SharedState::new(None, vec![normalize(&a), normalize(&b)]);

        let totals = shared.totals();
        assert_eq!(totals.tokens_input, u64::MAX, "clamped, not wrapped");
        assert_eq!(totals.tokens_cache, u64::MAX, "clamped, not wrapped");
    }

    #[test]
    fn block_lines_of_one_message_collapse_to_one_turn() {
        let cp = tmp("blocklines");
        let (_tx, rx) = mpsc::channel(8);
        let (mut engine, shared) = Engine::new(rx, init(cp.clone()));

        // One physical assistant turn exactly as Claude Code logs it: three
        // `assistant` lines (thinking, text, tool_use — one per content block),
        // each with its own timestamp but the same `message.id` and the
        // identical whole-message usage repeated on every line.
        let mut first = sample_raw(Source::Jsonl, Some("claude-haiku-4-5"), 8, 252);
        first.tokens_cache = 32_194;
        first.tokens_thinking = 40; // the thinking block is written first
        first.event_id = Some("msg_01Pu9mafrim5pcVGjSaLordi".into());
        let mut second = first.clone();
        second.timestamp_ms += 210;
        second.tokens_thinking = 0;
        let mut third = first.clone();
        third.timestamp_ms += 452;
        third.tokens_thinking = 0;

        engine.process_raw(first, 0);
        engine.process_raw(second, 210);
        engine.process_raw(third, 452);
        engine.flush(60_000);

        let totals = shared.totals();
        assert_eq!(totals.turns, 1, "three block-lines, ONE stored turn");
        assert_eq!(totals.tokens_output, 252, "usage counted once, not 756");
        assert_eq!(totals.tokens_cache, 32_194, "not 96_582");
        assert_eq!(
            totals.tokens_thinking, 40,
            "keep-first preserves the thinking estimate"
        );
        std::fs::remove_file(&cp).ok();
    }

    #[test]
    fn otel_plus_block_lines_merge_to_one_turn_with_no_leftovers() {
        let cp = tmp("otelblocks");
        let (_tx, rx) = mpsc::channel(8);
        let (mut engine, shared) = Engine::new(rx, init(cp.clone()));

        let mut jsonl = sample_raw(Source::Jsonl, Some("m"), 8, 136);
        jsonl.event_id = Some("msg_014CCDMEjKvEpyRLASBqDUiV".into());
        let mut second = jsonl.clone();
        second.timestamp_ms += 210;
        let mut third = jsonl.clone();
        third.timestamp_ms += 500;
        let mut otel = sample_raw(Source::Otel, Some("m"), 8, 136);
        otel.timestamp_ms = jsonl.timestamp_ms + 1_500; // batch-exported later

        engine.process_raw(jsonl, 0);
        engine.process_raw(second, 210); // block-line 2: deduped
        engine.process_raw(otel, 1_500); // merges with the pending first line
        engine.process_raw(third, 1_600); // block-line 3: deduped
        engine.flush(120_000); // nothing left to flush

        assert_eq!(shared.turn_count(), 1, "four raw observations, ONE turn");
        let turn = &shared.snapshot()[0];
        assert_eq!(turn.sources, vec![Source::Otel, Source::Jsonl]);
        assert_eq!(turn.tokens_output, 136);
        std::fs::remove_file(&cp).ok();
    }

    #[test]
    fn correlated_pair_counts_once() {
        let cp = tmp("pair");
        let (_tx, rx) = mpsc::channel(8);
        let (mut engine, shared) = Engine::new(rx, init(cp.clone()));

        let mut otel = sample_raw(Source::Otel, Some("m"), 100, 50);
        otel.timestamp_ms = 1_000;
        let mut jsonl = sample_raw(Source::Jsonl, Some("m"), 100, 50);
        jsonl.timestamp_ms = 1_000;

        engine.process_raw(otel, 0);
        engine.process_raw(jsonl, 50); // merges immediately
        assert_eq!(shared.turn_count(), 1, "two sources, one turn");
        std::fs::remove_file(&cp).ok();
    }
}
