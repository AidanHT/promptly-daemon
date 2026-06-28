//! The Claude Code JSONL session-log watcher (the fallback/supplement source).
//!
//! Claude Code writes one JSON object per line to
//! `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. `assistant` lines
//! carry `message.usage` token counts and `content[].type == "thinking"` blocks.
//! This watcher tails those files from a saved byte offset (so a restart resumes
//! without re-reading), parses each turn, and scopes to the active workspace by
//! the `cwd` field inside each entry — never the folder name, which is a lossy
//! encoding that distinct paths can collide on.
//!
//! The parsing and tailing are pure and unit-tested; the async [`JsonlSource`]
//! is the thin polling shell over them.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::model::{RawTurn, Source, HARNESS_CLAUDE_CODE_CLI};
use crate::paths::encode_project_dir;

/// Rough chars-per-token used to estimate thinking tokens from thinking-block
/// text (Claude bills thinking inside `output_tokens` and the JSONL `usage`
/// never breaks it out).
const CHARS_PER_TOKEN: usize = 4;

/// Default poll interval for the project directory.
const DEFAULT_POLL: Duration = Duration::from_millis(500);

/// Estimate thinking tokens from the total character count of thinking blocks.
pub fn estimate_thinking_tokens(thinking_chars: usize) -> u64 {
    if thinking_chars == 0 {
        return 0;
    }
    thinking_chars.div_ceil(CHARS_PER_TOKEN) as u64
}

/// Parse an RFC3339 timestamp (Claude Code uses UTC `...Z`) to epoch millis.
/// Shared with the Codex adapter (`21`), whose rollout lines use the same format.
pub(crate) fn parse_rfc3339_millis(s: &str) -> Option<i64> {
    let dt = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    Some((dt.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Parse one JSONL line into a raw turn, or `None` if it is not a token-bearing
/// `assistant` line. Lenient (`serde_json::Value`) so unrelated fields and minor
/// schema drift don't break capture.
pub fn parse_line(line: &str, fallback_ts: i64) -> Option<RawTurn> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str)? != "assistant" {
        return None;
    }
    let message = v.get("message")?;
    let usage = message.get("usage")?;

    let token = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let tokens_cache =
        token("cache_read_input_tokens").saturating_add(token("cache_creation_input_tokens"));

    let thinking_chars: usize = message
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
                .filter_map(|b| b.get("thinking").and_then(Value::as_str))
                .map(|t| t.chars().count())
                .sum()
        })
        .unwrap_or(0);

    let timestamp_ms = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_millis)
        .unwrap_or(fallback_ts);

    Some(RawTurn {
        source: Source::Jsonl,
        model: message
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        harness: HARNESS_CLAUDE_CODE_CLI.to_string(),
        tokens_input: token("input_tokens"),
        tokens_output: token("output_tokens"),
        tokens_thinking: estimate_thinking_tokens(thinking_chars),
        tokens_cache,
        prompt_id: None,
        timestamp_ms,
        cost_usd: None,
        duration_ms: None,
        session_id: v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        workspace: v.get("cwd").and_then(Value::as_str).map(str::to_string),
        // The JSONL `usage` block reports real token counts.
        counts_estimated: false,
    })
}

/// Parse every line terminated by `\n` in `buf`, returning the turns and how many
/// bytes were consumed (through the final newline). Bytes after the last newline
/// are an incomplete line and left unconsumed so the next read completes them.
pub fn parse_complete_lines(buf: &[u8], fallback_ts: i64) -> (Vec<RawTurn>, usize) {
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), 0);
    };
    let consumed = last_nl + 1;
    let mut turns = Vec::new();
    for line in buf[..consumed].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(line) {
            if let Some(turn) = parse_line(s.trim_end_matches('\r'), fallback_ts) {
                turns.push(turn);
            }
        }
    }
    (turns, consumed)
}

/// Normalize a path string for equivalence comparison: unify separators, drop a
/// trailing separator, and lowercase on Windows (its filesystem is case-insensitive).
pub fn normalize_for_compare(path: &str) -> String {
    let mut s = path.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    s
}

/// Does a JSONL entry's `cwd` denote the active workspace? This is the
/// disambiguation the lossy folder encoding can't provide.
pub fn cwd_matches(entry_cwd: &str, workspace_norm: &str) -> bool {
    normalize_for_compare(entry_cwd) == workspace_norm
}

/// Per-file byte offsets, shared with the engine so it can persist them to the
/// crash checkpoint while the watcher keeps advancing them.
pub type SharedOffsets = Arc<Mutex<HashMap<PathBuf, u64>>>;

/// Tails the active workspace's Claude Code session logs.
pub struct JsonlSource {
    project_dir: PathBuf,
    workspace_norm: String,
    offsets: SharedOffsets,
    poll: Duration,
}

impl JsonlSource {
    /// Build a watcher for `workspace`, looking under `projects_dir` for the
    /// encoded project folder. `offsets` seeds per-file positions from the crash
    /// checkpoint (empty for a fresh start) and is shared with the engine.
    pub fn new(workspace: &Path, projects_dir: &Path, offsets: SharedOffsets) -> Self {
        let workspace_str = workspace.to_string_lossy();
        Self {
            project_dir: projects_dir.join(encode_project_dir(&workspace_str)),
            workspace_norm: normalize_for_compare(&workspace_str),
            offsets,
            poll: DEFAULT_POLL,
        }
    }

    /// A handle to the shared per-file offsets, for persisting to the checkpoint.
    pub fn offsets(&self) -> SharedOffsets {
        Arc::clone(&self.offsets)
    }

    /// Skip pre-existing history at startup: any session file not already tracked
    /// from the checkpoint starts at its current end, so only turns produced from
    /// now on are captured.
    pub async fn prime(&mut self) {
        let Ok(mut rd) = tokio::fs::read_dir(&self.project_dir).await else {
            return;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if !is_jsonl(&path) || self.offsets.lock().unwrap().contains_key(&path) {
                continue;
            }
            if let Ok(meta) = tokio::fs::metadata(&path).await {
                self.offsets.lock().unwrap().insert(path, meta.len());
            }
        }
    }

    /// One scan of the project directory: tail every session file from its offset
    /// and return the newly-observed, in-scope turns.
    pub async fn poll_once(&mut self) -> Vec<RawTurn> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(&self.project_dir).await else {
            return out;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if !is_jsonl(&path) {
                continue;
            }
            match self.tail_file(&path).await {
                Ok(turns) => out.extend(turns),
                Err(err) => {
                    tracing::warn!(file = %path.display(), %err, "JSONL tail failed");
                }
            }
        }
        // Scope to the active workspace using the in-entry cwd, never the folder
        // name (which distinct paths can collide on).
        out.retain(|turn| self.in_scope(turn));
        out
    }

    async fn tail_file(&mut self, path: &Path) -> std::io::Result<Vec<RawTurn>> {
        let len = tokio::fs::metadata(path).await?.len();
        let prev = self.offsets.lock().unwrap().get(path).copied().unwrap_or(0);
        // A shorter file means truncation/rotation — restart from the top.
        let start = if len < prev { 0 } else { prev };
        if len <= start {
            self.offsets.lock().unwrap().insert(path.to_path_buf(), len);
            return Ok(Vec::new());
        }

        let mut file = tokio::fs::File::open(path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.read_to_end(&mut buf).await?;

        let (turns, consumed) = parse_complete_lines(&buf, now_ms());
        self.offsets
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), start + consumed as u64);
        Ok(turns)
    }

    fn in_scope(&self, turn: &RawTurn) -> bool {
        match &turn.workspace {
            Some(cwd) => cwd_matches(cwd, &self.workspace_norm),
            // No cwd to check against — accept rather than silently drop.
            None => true,
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some("jsonl")
}

#[async_trait]
impl TelemetrySource for JsonlSource {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    async fn run(
        mut self: Box<Self>,
        sink: RawTurnSink,
        mut shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        self.prime().await;
        tracing::info!(dir = %self.project_dir.display(), "JSONL watcher started");
        let mut ticker = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    for turn in self.poll_once().await {
                        if sink.send(turn).await.is_err() {
                            return Ok(()); // engine gone
                        }
                    }
                }
                () = wait_for_shutdown(&mut shutdown) => break,
            }
        }
        tracing::info!("JSONL watcher stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: i64 = 1_700_000_000_000;

    fn assistant_line(model: &str, in_: u64, out: u64, thinking: &str, cwd: &str) -> String {
        let content = if thinking.is_empty() {
            String::from(r#"[{"type":"text","text":"hi"}]"#)
        } else {
            format!(
                r#"[{{"type":"thinking","thinking":"{thinking}"}},{{"type":"text","text":"hi"}}]"#
            )
        };
        format!(
            r#"{{"type":"assistant","sessionId":"s1","cwd":"{cwd}","timestamp":"2026-06-16T19:00:00.000Z","message":{{"model":"{model}","usage":{{"input_tokens":{in_},"output_tokens":{out},"cache_read_input_tokens":5,"cache_creation_input_tokens":2}},"content":{content}}}}}"#
        )
    }

    #[test]
    fn parses_assistant_usage_and_thinking() {
        let line = assistant_line("claude-opus-4-8", 100, 200, "abcdefgh", "/work/ws");
        let turn = parse_line(&line, TS).expect("assistant line parses");
        assert_eq!(turn.source, Source::Jsonl);
        assert_eq!(turn.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(turn.tokens_input, 100);
        assert_eq!(turn.tokens_output, 200);
        assert_eq!(turn.tokens_cache, 7, "cache_read + cache_creation");
        assert_eq!(turn.tokens_thinking, 2, "8 chars / 4 per token");
        assert_eq!(turn.workspace.as_deref(), Some("/work/ws"));
        assert_eq!(turn.timestamp_ms, 1_781_636_400_000); // 2026-06-16T19:00:00Z
    }

    #[test]
    fn ignores_non_assistant_and_usageless_lines() {
        assert!(parse_line(r#"{"type":"user","message":{"content":"hi"}}"#, TS).is_none());
        assert!(parse_line(r#"{"type":"assistant","message":{"model":"m"}}"#, TS).is_none());
        assert!(parse_line("not json", TS).is_none());
    }

    #[test]
    fn estimate_thinking_is_ceil_div_four() {
        assert_eq!(estimate_thinking_tokens(0), 0);
        assert_eq!(estimate_thinking_tokens(1), 1);
        assert_eq!(estimate_thinking_tokens(8), 2);
        assert_eq!(estimate_thinking_tokens(9), 3);
    }

    #[test]
    fn parse_complete_lines_leaves_partial_trailing_line() {
        let mut buf = assistant_line("m", 1, 1, "", "/ws").into_bytes();
        buf.push(b'\n');
        let partial = b"{\"type\":\"assist"; // no newline yet
        buf.extend_from_slice(partial);

        let (turns, consumed) = parse_complete_lines(&buf, TS);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            consumed,
            buf.len() - partial.len(),
            "stops at the last newline"
        );
    }

    #[test]
    fn cwd_matching_normalizes_separators_and_trailing_slash() {
        let ws = normalize_for_compare(r"C:\work\My Repo");
        assert!(cwd_matches(r"C:\work\My Repo", &ws));
        assert!(cwd_matches("C:/work/My Repo/", &ws));
        assert!(!cwd_matches("C:/work/Other", &ws));
    }

    #[tokio::test]
    async fn prime_skips_history_then_tails_appended_turns() {
        let tmp = std::env::temp_dir().join(format!("promptlyd-jsonl-{}", std::process::id()));
        let workspace = tmp.join("ws");
        let projects = tmp.join("projects");
        let project_dir = projects.join(encode_project_dir(&workspace.to_string_lossy()));
        tokio::fs::create_dir_all(&project_dir).await.unwrap();
        let cwd = workspace.to_string_lossy().replace('\\', "\\\\");
        let log = project_dir.join("session.jsonl");

        // Pre-existing history (one turn) present before the daemon starts.
        let mut history = assistant_line("m", 1, 1, "", &cwd);
        history.push('\n');
        tokio::fs::write(&log, &history).await.unwrap();

        let mut src = JsonlSource::new(&workspace, &projects, Arc::new(Mutex::new(HashMap::new())));
        src.prime().await;
        assert!(src.poll_once().await.is_empty(), "history is skipped");

        // A new turn is appended while capturing.
        let mut next = assistant_line("claude-opus-4-8", 50, 60, "", &cwd);
        next.push('\n');
        let existing = tokio::fs::read_to_string(&log).await.unwrap();
        tokio::fs::write(&log, format!("{existing}{next}"))
            .await
            .unwrap();

        let turns = src.poll_once().await;
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tokens_output, 60);

        // Out-of-scope cwd is dropped even inside the same (lossy) folder.
        let mut foreign = assistant_line("m", 9, 9, "", "/somewhere/else");
        foreign.push('\n');
        let existing = tokio::fs::read_to_string(&log).await.unwrap();
        tokio::fs::write(&log, format!("{existing}{foreign}"))
            .await
            .unwrap();
        assert!(
            src.poll_once().await.is_empty(),
            "foreign cwd is out of scope"
        );

        tokio::fs::remove_dir_all(&tmp).await.ok();
    }
}
