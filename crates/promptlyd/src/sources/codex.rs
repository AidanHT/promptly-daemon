//! OpenAI Codex CLI capture adapter (`21`) — **best-effort, version-fragile**.
//!
//! Codex writes a per-session rollout transcript to
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. A session-meta line near the
//! top records the `model` and the launch `cwd`; subsequent `event_msg` lines of
//! type `token_count` carry usage in `info.total_token_usage` (cumulative) and,
//! in recent versions, `info.last_token_usage` (the just-completed turn). We emit
//! one normalized turn per token_count event: the per-turn `last_token_usage`
//! when present, else the delta of the running cumulative total. `cwd` scopes
//! capture to the bound workspace (`18`); `reasoning_output_tokens` map to
//! thinking tokens, matching how the Claude sources carry thinking.
//!
//! Reading mirrors the JSONL watcher: tail each rollout from a saved byte offset.
//! A session started for the attempt is a *new* file (read from its start, so its
//! meta and first turn are captured); pre-existing files are primed to their end.
//! When the sessions dir is absent or a line's shape isn't recognized the adapter
//! degrades and reports via the registry for `promptly doctor` (`19`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::jsonl::{cwd_matches, normalize_for_compare, parse_rfc3339_millis};
use super::registry::{AdapterRegistry, AdapterState};
use super::{wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::model::{RawTurn, Source, HARNESS_CODEX_CLI};
use crate::model_map;

const NAME: &str = "codex";
const DEFAULT_POLL: Duration = Duration::from_millis(750);

/// A snapshot of Codex's token usage (either a per-turn delta or a cumulative
/// total, depending on which field it came from).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl TokenTotals {
    /// Field-wise saturating difference (this − prev), for turning a cumulative
    /// total into the just-completed turn.
    fn delta_from(&self, prev: &TokenTotals) -> TokenTotals {
        TokenTotals {
            input: self.input.saturating_sub(prev.input),
            cached: self.cached.saturating_sub(prev.cached),
            output: self.output.saturating_sub(prev.output),
            reasoning: self.reasoning.saturating_sub(prev.reasoning),
        }
    }
}

/// One meaningful line of a rollout transcript.
#[derive(Debug, PartialEq, Eq)]
pub enum CodexEvent {
    /// Session metadata: the model and/or launch cwd (and session id).
    Meta {
        model: Option<String>,
        cwd: Option<String>,
        session_id: Option<String>,
    },
    /// A `token_count` event: the per-turn and/or cumulative usage.
    Usage {
        last: Option<TokenTotals>,
        total: Option<TokenTotals>,
        timestamp_ms: Option<i64>,
    },
    /// Anything else (user/assistant message text, tool calls, …).
    Other,
}

/// Read a token-usage object (`{input_tokens, cached_input_tokens, …}`), or
/// `None` if `v` carries no usage fields at all.
fn parse_totals(v: &Value) -> Option<TokenTotals> {
    let obj = v.as_object()?;
    let known = [
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ];
    if !known.iter().any(|k| obj.contains_key(*k)) {
        return None;
    }
    let g = |k: &str| obj.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(TokenTotals {
        input: g("input_tokens"),
        cached: g("cached_input_tokens"),
        output: g("output_tokens"),
        reasoning: g("reasoning_output_tokens"),
    })
}

/// Parse one rollout line. Lenient: the envelope may wrap fields in `payload`,
/// and minor key drift is tolerated.
pub fn parse_line(line: &str) -> CodexEvent {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return CodexEvent::Other;
    };
    // Fields live either at the top level or inside a `payload` envelope.
    let payload = v.get("payload").unwrap_or(&v);
    let kind = v
        .get("type")
        .or_else(|| payload.get("type"))
        .and_then(Value::as_str);

    let info = payload.get("info").unwrap_or(payload);
    let last = info.get("last_token_usage").and_then(parse_totals);
    let total = info
        .get("total_token_usage")
        .and_then(parse_totals)
        .or_else(|| parse_totals(info));
    if kind == Some("token_count") || last.is_some() || total.is_some() {
        let timestamp_ms = v.get("timestamp").and_then(|t| {
            t.as_str()
                .and_then(parse_rfc3339_millis)
                .or_else(|| t.as_i64())
        });
        return CodexEvent::Usage {
            last,
            total,
            timestamp_ms,
        };
    }

    let field = |k: &str| {
        payload
            .get(k)
            .or_else(|| v.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let model = field("model");
    let cwd = field("cwd");
    let session_id = field("id").or_else(|| field("session_id"));
    if model.is_some() || cwd.is_some() {
        return CodexEvent::Meta {
            model,
            cwd,
            session_id,
        };
    }
    CodexEvent::Other
}

/// Per-rollout-file capture state, carried across polls: the resolved model and
/// cwd from the meta line, the last cumulative total (for delta-ing), and the
/// byte offset tailed to.
#[derive(Debug, Default)]
struct FileState {
    model: Option<String>,
    cwd: Option<String>,
    session_id: Option<String>,
    last_total: Option<TokenTotals>,
    offset: u64,
}

impl FileState {
    /// Fold one line into the state, returning a turn to emit when it's an
    /// in-scope `token_count`. `bound_norm` is the normalized bound workspace.
    fn observe(&mut self, line: &str, observed_ms: i64, bound_norm: &str) -> Option<RawTurn> {
        match parse_line(line) {
            CodexEvent::Meta {
                model,
                cwd,
                session_id,
            } => {
                if let Some(m) = model {
                    // Resolve to a canonical id (or leave unresolved → estimated).
                    self.model = model_map::resolve(&m).map(str::to_string);
                }
                if cwd.is_some() {
                    self.cwd = cwd;
                }
                if session_id.is_some() {
                    self.session_id = session_id;
                }
                None
            }
            CodexEvent::Usage {
                last,
                total,
                timestamp_ms,
            } => {
                // Per-turn usage directly, else the delta of the cumulative total.
                let tokens = match (last, &total) {
                    (Some(per_turn), _) => per_turn,
                    (None, Some(t)) => match &self.last_total {
                        Some(prev) => t.delta_from(prev),
                        None => t.clone(),
                    },
                    (None, None) => return None,
                };
                if let Some(t) = total {
                    self.last_total = Some(t);
                }
                self.build_turn(&tokens, timestamp_ms, observed_ms, bound_norm)
            }
            CodexEvent::Other => None,
        }
    }

    fn build_turn(
        &self,
        tokens: &TokenTotals,
        timestamp_ms: Option<i64>,
        observed_ms: i64,
        bound_norm: &str,
    ) -> Option<RawTurn> {
        // Scope: a rollout whose cwd isn't the bound workspace isn't ours.
        if let Some(cwd) = &self.cwd {
            if !cwd_matches(cwd, bound_norm) {
                return None;
            }
        }
        // Match the Claude convention: input excludes the cached subset, output
        // includes reasoning, and reasoning is also reported separately.
        let tokens_input = tokens.input.saturating_sub(tokens.cached);
        if tokens_input
            .saturating_add(tokens.output)
            .saturating_add(tokens.reasoning)
            .saturating_add(tokens.cached)
            == 0
        {
            return None; // a no-op usage event
        }
        Some(RawTurn {
            source: Source::Codex,
            model: self.model.clone(),
            harness: HARNESS_CODEX_CLI.to_string(),
            tokens_input,
            tokens_output: tokens.output,
            tokens_thinking: tokens.reasoning,
            tokens_cache: tokens.cached,
            prompt_id: None,
            timestamp_ms: timestamp_ms.unwrap_or(observed_ms),
            cost_usd: None,
            duration_ms: None,
            session_id: self.session_id.clone(),
            workspace: self.cwd.clone(),
            // Codex reports real token counts.
            counts_estimated: false,
            // A rollout's `id` is session-scoped, not per-turn — keying dedup on
            // it would collapse every turn of a session into one. `token_count`
            // events carry no per-turn id, so dedup stays on the content hash.
            event_id: None,
        })
    }
}

fn is_rollout_jsonl(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some("jsonl")
        && path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|n| n.starts_with("rollout"))
}

/// Find rollout transcripts under the `YYYY/MM/DD` tree (bounded recursion).
fn find_rollouts(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, depth + 1, out);
            } else if is_rollout_jsonl(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

/// Split the complete (`\n`-terminated) lines out of `buf`, returning them and
/// the number of bytes consumed (an unterminated trailing line is left).
fn complete_lines(buf: &[u8]) -> (Vec<&str>, usize) {
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), 0);
    };
    let consumed = last_nl + 1;
    let lines = buf[..consumed]
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            std::str::from_utf8(l)
                .ok()
                .map(|s| s.trim_end_matches('\r'))
        })
        .collect();
    (lines, consumed)
}

/// Tails Codex rollout transcripts, scoped to the bound workspace's cwd.
pub struct CodexSource {
    sessions_dir: PathBuf,
    bound_norm: String,
    registry: AdapterRegistry,
    files: HashMap<PathBuf, FileState>,
    poll: Duration,
}

impl CodexSource {
    pub fn new(sessions_dir: &Path, workspace: &Path, registry: AdapterRegistry) -> Self {
        Self {
            sessions_dir: sessions_dir.to_path_buf(),
            bound_norm: normalize_for_compare(&workspace.to_string_lossy()),
            registry,
            files: HashMap::new(),
            poll: DEFAULT_POLL,
        }
    }

    /// Prepare each pre-existing rollout so its history is not re-emitted but its
    /// session context *is* recovered: fold the file to recover the model, cwd,
    /// and last cumulative total, then tail only bytes appended from now on.
    ///
    /// This matters when the daemon (re)starts mid-session. Without the recovered
    /// `last_total`, the next cumulative `token_count` event would be deltaed
    /// against `None` and emit the entire running total as one turn — a massive
    /// silent over-count. Without the recovered `cwd`, a still-running session for
    /// *another* directory would pass the (skipped) scope check and be attributed
    /// here. A session freshly started for the attempt is a brand-new file with no
    /// history to recover, so it is still read from its start.
    pub async fn prime(&mut self) {
        let bound = self.bound_norm.clone();
        for path in find_rollouts(&self.sessions_dir) {
            let content = tokio::fs::read(&path).await.ok();
            // If the file can't be read right now, fall back to skipping to its end.
            let fallback = match &content {
                Some(_) => None,
                None => tokio::fs::metadata(&path).await.map(|m| m.len()).ok(),
            };
            let state = self.files.entry(path).or_default();
            match content {
                Some(bytes) => {
                    let (lines, _) = complete_lines(&bytes);
                    for line in lines {
                        // Fold state only — primed history is never emitted as turns.
                        let _ = state.observe(line, 0, &bound);
                    }
                    state.offset = bytes.len() as u64;
                }
                None => {
                    state.offset = fallback.unwrap_or(0);
                }
            }
        }
        self.publish(0);
    }

    /// One scan across the rollout tree; returns the newly-observed turns.
    pub async fn poll_once(&mut self, observed_ms: i64) -> Vec<RawTurn> {
        // The rollout-tree walk is blocking std::fs; run it off the async worker so
        // it can't stall the runtime (tailing each file below is already async).
        let dir = self.sessions_dir.clone();
        let rollouts = tokio::task::spawn_blocking(move || find_rollouts(&dir))
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for path in rollouts {
            match self.tail_file(&path, observed_ms).await {
                Ok(mut turns) => out.append(&mut turns),
                Err(err) => tracing::warn!(file = %path.display(), %err, "codex: tail failed"),
            }
        }
        self.publish(out.len());
        out
    }

    fn publish(&self, just_emitted: usize) {
        if !self.sessions_dir.exists() {
            self.registry.set(
                NAME,
                AdapterState::NotFound,
                "no ~/.codex/sessions directory",
            );
            return;
        }
        let detail = if just_emitted > 0 {
            format!("captured {just_emitted} turn(s) this scan")
        } else {
            format!("watching {} rollout file(s)", self.files.len())
        };
        self.registry.set(NAME, AdapterState::Detected, detail);
    }

    async fn tail_file(&mut self, path: &Path, observed_ms: i64) -> std::io::Result<Vec<RawTurn>> {
        let len = tokio::fs::metadata(path).await?.len();
        let prev = self.files.get(path).map(|s| s.offset).unwrap_or(0);
        // A shorter file means truncation/rotation — restart this file's state.
        if len < prev {
            self.files.insert(path.to_path_buf(), FileState::default());
        }
        let start = self.files.get(path).map(|s| s.offset).unwrap_or(0);
        if len <= start {
            self.files.entry(path.to_path_buf()).or_default().offset = len;
            return Ok(Vec::new());
        }

        let mut file = tokio::fs::File::open(path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.read_to_end(&mut buf).await?;
        let (lines, consumed) = complete_lines(&buf);

        let bound = self.bound_norm.clone();
        let state = self.files.entry(path.to_path_buf()).or_default();
        let mut out = Vec::new();
        for line in lines {
            if let Some(turn) = state.observe(line, observed_ms, &bound) {
                out.push(turn);
            }
        }
        state.offset = start + consumed as u64;
        Ok(out)
    }
}

#[async_trait]
impl TelemetrySource for CodexSource {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn run(
        mut self: Box<Self>,
        sink: RawTurnSink,
        mut shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        self.prime().await;
        tracing::info!(dir = %self.sessions_dir.display(), "Codex adapter started");
        let mut ticker = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    for turn in self.poll_once(now_ms()).await {
                        if sink.send(turn).await.is_err() {
                            return Ok(()); // engine gone
                        }
                    }
                }
                () = wait_for_shutdown(&mut shutdown) => break,
            }
        }
        tracing::info!("Codex adapter stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/work/repo";

    fn bound() -> String {
        normalize_for_compare(WS)
    }

    #[test]
    fn parses_meta_and_token_count_lines() {
        let meta = parse_line(
            r#"{"type":"session_meta","payload":{"id":"sess-1","cwd":"/work/repo","model":"gpt-5.3-codex"}}"#,
        );
        assert_eq!(
            meta,
            CodexEvent::Meta {
                model: Some("gpt-5.3-codex".into()),
                cwd: Some("/work/repo".into()),
                session_id: Some("sess-1".into()),
            }
        );

        let usage = parse_line(
            r#"{"timestamp":"2026-06-17T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":80,"reasoning_output_tokens":30}}}}"#,
        );
        match usage {
            CodexEvent::Usage {
                total: Some(t),
                timestamp_ms: Some(ts),
                ..
            } => {
                assert_eq!(t.input, 100);
                assert_eq!(t.reasoning, 30);
                assert_eq!(ts, 1_781_697_600_000); // 2026-06-17T12:00:00Z
            }
            other => panic!("expected usage, got {other:?}"),
        }
    }

    #[test]
    fn cumulative_totals_are_deltaed_into_per_turn_records() {
        let mut state = FileState::default();
        // Meta first sets the model + cwd.
        assert!(state
            .observe(
                r#"{"type":"session_meta","payload":{"cwd":"/work/repo","model":"gpt-5.3-codex"}}"#,
                1_000,
                &bound(),
            )
            .is_none());

        // First cumulative total → the first turn's full usage.
        let t1 = state
            .observe(
                r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":10}}}}"#,
                2_000,
                &bound(),
            )
            .expect("first turn");
        assert_eq!(t1.model.as_deref(), Some("gpt-5-3-codex")); // mapped
        assert_eq!(t1.tokens_input, 100);
        assert_eq!(t1.tokens_output, 50);
        assert_eq!(t1.tokens_thinking, 10, "reasoning → thinking");

        // Next cumulative total → the delta is the second turn only.
        let t2 = state
            .observe(
                r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":250,"cached_input_tokens":0,"output_tokens":120,"reasoning_output_tokens":40}}}}"#,
                3_000,
                &bound(),
            )
            .expect("second turn");
        assert_eq!(t2.tokens_input, 150, "250 − 100");
        assert_eq!(t2.tokens_output, 70, "120 − 50");
        assert_eq!(t2.tokens_thinking, 30, "40 − 10");
        assert_eq!(t2.source, Source::Codex);
        assert_eq!(t2.harness, "codex_cli");
    }

    #[test]
    fn prefers_last_token_usage_when_present() {
        let mut state = FileState {
            cwd: Some(WS.into()),
            ..Default::default()
        };
        let turn = state
            .observe(
                r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":999,"output_tokens":999},"last_token_usage":{"input_tokens":12,"cached_input_tokens":2,"output_tokens":8,"reasoning_output_tokens":3}}}}"#,
                1_000,
                &bound(),
            )
            .expect("turn");
        // The per-turn last_token_usage wins over the cumulative total.
        assert_eq!(turn.tokens_input, 10, "12 input − 2 cached");
        assert_eq!(turn.tokens_cache, 2);
        assert_eq!(turn.tokens_output, 8);
        assert_eq!(turn.tokens_thinking, 3);
    }

    #[test]
    fn turns_outside_the_bound_cwd_are_dropped() {
        let mut state = FileState {
            cwd: Some("/some/other/dir".into()),
            ..Default::default()
        };
        assert!(state
            .observe(
                r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"output_tokens":50}}}}"#,
                1_000,
                &bound(),
            )
            .is_none());
    }

    #[tokio::test]
    async fn primes_history_then_tails_a_new_session_file() {
        let base = std::env::temp_dir().join(format!("promptlyd-codex-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let sessions = base.join("sessions");
        let day = sessions.join("2026").join("06").join("17");
        tokio::fs::create_dir_all(&day).await.unwrap();
        let workspace = PathBuf::from(WS);

        // A pre-existing rollout (history) present before the daemon starts.
        let old = day.join("rollout-old.jsonl");
        tokio::fs::write(
            &old,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/work/repo\",\"model\":\"gpt-5.3-codex\"}}\n{\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":10,\"output_tokens\":10}}}}\n",
        )
        .await
        .unwrap();

        let mut src = CodexSource::new(&sessions, &workspace, AdapterRegistry::new());
        src.prime().await;
        assert!(src.poll_once(1_000).await.is_empty(), "history is skipped");

        // A new session file appears and produces two turns.
        let new = day.join("rollout-new.jsonl");
        tokio::fs::write(
            &new,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/work/repo\",\"model\":\"gpt-5.3-codex\"}}\n{\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":100,\"output_tokens\":40,\"reasoning_output_tokens\":5}}}}\n",
        )
        .await
        .unwrap();
        let turns = src.poll_once(2_000).await;
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].model.as_deref(), Some("gpt-5-3-codex"));
        assert_eq!(turns[0].tokens_output, 40);
        assert_eq!(turns[0].workspace.as_deref(), Some("/work/repo"));
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Detected);

        // No new bytes → nothing more (dedup via the byte offset).
        assert!(src.poll_once(3_000).await.is_empty());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn priming_recovers_the_baseline_so_a_resumed_session_deltas_not_overcounts() {
        use std::io::Write as _;
        let base =
            std::env::temp_dir().join(format!("promptlyd-codex-resume-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let day = base.join("sessions").join("2026").join("06").join("17");
        tokio::fs::create_dir_all(&day).await.unwrap();
        let roll = day.join("rollout-active.jsonl");
        // A session already in progress when the daemon (re)starts: its cumulative
        // total has reached 200/100.
        tokio::fs::write(
            &roll,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/work/repo\",\"model\":\"gpt-5.3-codex\"}}\n{\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":200,\"output_tokens\":100}}}}\n",
        )
        .await
        .unwrap();

        let mut src = CodexSource::new(
            &base.join("sessions"),
            &PathBuf::from(WS),
            AdapterRegistry::new(),
        );
        src.prime().await;
        assert!(
            src.poll_once(1_000).await.is_empty(),
            "primed history is not re-emitted"
        );

        // The still-running session logs its next turn (cumulative now 230/120).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&roll)
            .unwrap();
        f.write_all(b"{\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":230,\"output_tokens\":120}}}}\n").unwrap();
        drop(f);

        let turns = src.poll_once(2_000).await;
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tokens_input, 30, "230 - 200, not the whole 230");
        assert_eq!(turns[0].tokens_output, 20, "120 - 100");
        // cwd recovered at prime, so the resumed turn is correctly attributed.
        assert_eq!(turns[0].workspace.as_deref(), Some("/work/repo"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn priming_recovers_cwd_so_a_foreign_session_stays_out_of_scope() {
        use std::io::Write as _;
        let base =
            std::env::temp_dir().join(format!("promptlyd-codex-foreign-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let day = base.join("sessions").join("2026").join("06").join("17");
        tokio::fs::create_dir_all(&day).await.unwrap();
        let roll = day.join("rollout-foreign.jsonl");
        // A pre-existing session bound to a DIFFERENT directory.
        tokio::fs::write(
            &roll,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/some/other/dir\",\"model\":\"gpt-5.3-codex\"}}\n{\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":50,\"output_tokens\":50}}}}\n",
        )
        .await
        .unwrap();

        let mut src = CodexSource::new(
            &base.join("sessions"),
            &PathBuf::from(WS),
            AdapterRegistry::new(),
        );
        src.prime().await;
        // The foreign session logs another turn after the daemon started.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&roll)
            .unwrap();
        f.write_all(b"{\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":120,\"output_tokens\":110}}}}\n").unwrap();
        drop(f);

        // cwd was recovered at prime, so the foreign-directory turns are dropped.
        assert!(
            src.poll_once(2_000).await.is_empty(),
            "a session for another workspace must not be attributed here"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn missing_sessions_dir_reports_not_found() {
        let base = std::env::temp_dir().join(format!("promptlyd-codex-nf-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let mut src = CodexSource::new(
            &base.join("sessions"),
            &PathBuf::from(WS),
            AdapterRegistry::new(),
        );
        assert!(src.poll_once(1_000).await.is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::NotFound);
    }
}
