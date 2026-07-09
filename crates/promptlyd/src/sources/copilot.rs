//! GitHub Copilot Chat capture adapter (`21`) — **best-effort, low confidence**.
//!
//! Copilot Chat rides VS Code's native chat store: each conversation is a whole
//! JSON document at `…/User/workspaceStorage/<hash>/chatSessions/<id>.json` with a
//! `requests` array, one entry per user→assistant exchange. The `<hash>` dir is
//! matched to the bound workspace by its `workspace.json`
//! (`crate::sources::vscode`), and that match *is* the scoping (`18`) — every
//! session file under it belongs to that workspace. Both stable VS Code and
//! Insiders are searched (`paths::vscode_user_dirs`).
//!
//! Unlike Claude Code and Codex, these logs carry **no token counts** — they are a
//! UI transcript, not an API-usage record. So every turn's tokens are *estimated*
//! from the message/response text (chars/4) and the turn is marked `estimated`
//! (`counts_estimated`); it always scores against the baseline-floor tier (`13a`).
//! The model name, when the request records one, is still resolved best-effort.
//!
//! A session file is rewritten in place as its chat grows (not appended line by
//! line), so — like the Cursor adapter — we re-read it each poll and dedup by a
//! stable per-request key rather than tailing a byte offset. When the store is
//! absent or its shape isn't recognized the adapter degrades and reports via the
//! registry for `promptly doctor` (`19`) rather than crashing.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::registry::{AdapterRegistry, AdapterState};
use super::{vscode, wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::model::{RawTurn, Source, HARNESS_COPILOT_CHAT};
use crate::model_map;

/// Registry/adapter name.
const NAME: &str = "copilot";
/// The per-workspace subdirectory VS Code stores native chat sessions in.
const CHAT_SESSIONS_DIR: &str = "chatSessions";
/// Chars-per-token for the text-length estimate (matches the other adapters,
/// `17`/`21`). Copilot's logs never carry real counts, so this is always used.
const CHARS_PER_TOKEN: usize = 4;
/// Poll interval. Slower than the JSONL watcher: Copilot is a secondary source and
/// each scan re-reads whole session files, so there's no need to hammer it.
const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// Estimate the character length of a chat message/response value. VS Code stores
/// content as bare strings or under `value`/`text`/`parts` keys (nested for
/// markdown), varying across versions — we sum the strings reachable through those
/// known content keys and ignore everything else (type tags, ids, …).
fn content_chars(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.iter().map(content_chars).sum(),
        Value::Object(o) => ["value", "text", "parts"]
            .iter()
            .filter_map(|k| o.get(*k))
            .map(content_chars)
            .sum(),
        _ => 0,
    }
}

/// Parse one `requests[]` entry into an estimated [`RawTurn`], or `None` when it
/// isn't a completed assistant turn (no response text to estimate from). `key` is
/// `<session-file>:<request-id>`, the dedup/content key. `observed_ms` is the
/// capture time, used when the request carries no timestamp of its own.
fn parse_request(
    req: &Value,
    file_stem: &str,
    index: usize,
    observed_ms: i64,
) -> Option<(String, RawTurn)> {
    let response_chars = req.get("response").map(content_chars).unwrap_or(0);
    if response_chars == 0 {
        return None; // an in-flight or empty request — nothing to estimate from
    }
    let message_chars = req.get("message").map(content_chars).unwrap_or(0);

    // Best-effort model: the request records it under one of a few drifting keys.
    let model = [
        "/modelId",
        "/result/metadata/modelId",
        "/result/metadata/model",
        "/model",
    ]
    .iter()
    .find_map(|p| req.pointer(p).and_then(Value::as_str))
    .and_then(model_map::resolve)
    .map(str::to_string);

    let id = ["requestId", "id", "responseId"]
        .iter()
        .find_map(|k| req.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("idx-{index}"));
    let key = format!("{file_stem}:{id}");

    let timestamp_ms = ["timestamp", "requestTimestamp"]
        .iter()
        .find_map(|k| req.get(*k).and_then(Value::as_i64))
        .unwrap_or(observed_ms);

    let turn = RawTurn {
        source: Source::Copilot,
        model,
        harness: HARNESS_COPILOT_CHAT.to_string(),
        tokens_input: message_chars.div_ceil(CHARS_PER_TOKEN) as u64,
        tokens_output: response_chars.div_ceil(CHARS_PER_TOKEN) as u64,
        // Copilot's logs don't break out thinking or cache tokens.
        tokens_thinking: 0,
        tokens_cache: 0,
        // A stable per-request id so the engine's content-id dedup distinguishes
        // distinct requests and ignores a re-read one after a restart.
        prompt_id: Some(key.clone()),
        timestamp_ms,
        cost_usd: None,
        duration_ms: None,
        session_id: Some(file_stem.to_string()),
        // Scoping is structural (the file's `<hash>` dir); the I/O layer stamps the
        // bound workspace so the engine attributes the turn.
        workspace: None,
        // Copilot reports no token counts — these are always estimates.
        counts_estimated: true,
        // The same stable `<file>:<request-id>` key: unlike the content hash
        // (whose timestamp falls back to the observation time when the request
        // carries none), it survives a daemon restart's re-scan, so the engine's
        // dedup recognizes a re-read request even with the source's own `seen`
        // set gone.
        event_id: Some(key.clone()),
    };
    Some((key, turn))
}

/// Parse a chat-session document into its turns. `None` means it has no `requests`
/// array — an unrecognized shape (the adapter reports `Unsupported` when *every*
/// session file looks like this, i.e. the storage format changed).
fn parse_session(
    value: &Value,
    file_stem: &str,
    observed_ms: i64,
) -> Option<Vec<(String, RawTurn)>> {
    let requests = value.get("requests").and_then(Value::as_array)?;
    Some(
        requests
            .iter()
            .enumerate()
            .filter_map(|(i, req)| parse_request(req, file_stem, i, observed_ms))
            .collect(),
    )
}

fn is_json(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some("json")
}

/// The result of one scan: the detection state to publish plus the turns found
/// (each with its dedup key).
struct Scan {
    state: AdapterState,
    detail: String,
    turns: Vec<(String, RawTurn)>,
}

impl Scan {
    fn empty(state: AdapterState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            turns: Vec::new(),
        }
    }
}

/// Reads Copilot Chat's per-workspace session store, scoped to the bound workspace.
pub struct CopilotSource {
    user_dirs: Vec<PathBuf>,
    workspace: PathBuf,
    workspace_str: String,
    registry: AdapterRegistry,
    seen: HashSet<String>,
    poll: Duration,
}

impl CopilotSource {
    /// Build an adapter searching the given VS Code `User` dirs (stable + Insiders)
    /// for `workspace`'s chat sessions.
    pub fn new(user_dirs: &[PathBuf], workspace: &Path, registry: AdapterRegistry) -> Self {
        Self {
            user_dirs: user_dirs.to_vec(),
            workspace: workspace.to_path_buf(),
            workspace_str: workspace.to_string_lossy().into_owned(),
            registry,
            seen: HashSet::new(),
            poll: DEFAULT_POLL,
        }
    }

    /// Locate the bound workspace's chat sessions across every VS Code install and
    /// parse them, classifying the source's state. Never panics: any I/O or schema
    /// problem maps to a state rather than an error.
    fn collect(&self, observed_ms: i64) -> Scan {
        let roots: Vec<&PathBuf> = self.user_dirs.iter().filter(|d| d.exists()).collect();
        if roots.is_empty() {
            return Scan::empty(AdapterState::NotFound, "no VS Code user storage found");
        }

        // The workspace's `<hash>` dir in each install where it has been opened.
        let hash_dirs: Vec<PathBuf> = roots
            .iter()
            .filter_map(|root| {
                vscode::find_workspace_storage(&root.join("workspaceStorage"), &self.workspace)
            })
            .collect();
        if hash_dirs.is_empty() {
            return Scan::empty(
                AdapterState::NotFound,
                "this workspace hasn't been opened in VS Code",
            );
        }

        let mut files_total = 0usize;
        let mut recognized = 0usize;
        let mut turns = Vec::new();
        for hash in &hash_dirs {
            let Ok(rd) = std::fs::read_dir(hash.join(CHAT_SESSIONS_DIR)) else {
                continue; // no chatSessions dir under this hash — Copilot unused here
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if !is_json(&path) {
                    continue;
                }
                files_total += 1;
                let stem = path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or("session");
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue; // not JSON — leave it counted as unrecognized
                };
                if let Some(mut found) = parse_session(&value, stem, observed_ms) {
                    recognized += 1;
                    turns.append(&mut found);
                }
            }
        }

        if files_total == 0 {
            return Scan::empty(
                AdapterState::Detected,
                "no Copilot chat sessions in this workspace yet",
            );
        }
        if recognized == 0 {
            return Scan::empty(
                AdapterState::Unsupported,
                "chat session format not recognized — Copilot storage changed",
            );
        }
        let detail = format!("{} turn(s) across {} session(s)", turns.len(), files_total);
        Scan {
            state: AdapterState::Detected,
            detail,
            turns,
        }
    }

    /// Skip whatever already exists so only conversations from now on are captured
    /// (mirrors the JSONL watcher's `prime`; on restart this avoids re-emitting
    /// requests already captured before the restart).
    pub fn prime(&mut self, observed_ms: i64) {
        let scan = self.collect(observed_ms);
        for (key, _) in scan.turns {
            self.seen.insert(key);
        }
        self.registry.set(NAME, scan.state, scan.detail);
    }

    /// One scan: publish the state and return the newly-observed turns (stamped
    /// with the bound workspace so the engine attributes them).
    pub fn poll_once(&mut self, observed_ms: i64) -> Vec<RawTurn> {
        let scan = self.collect(observed_ms);
        self.registry.set(NAME, scan.state, scan.detail);
        let mut turns = Vec::new();
        for (key, mut turn) in scan.turns {
            if self.seen.insert(key) {
                turn.workspace = Some(self.workspace_str.clone());
                turns.push(turn);
            }
        }
        turns
    }
}

#[async_trait]
impl TelemetrySource for CopilotSource {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn run(
        mut self: Box<Self>,
        sink: RawTurnSink,
        mut shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        self.prime(now_ms());
        tracing::info!(installs = self.user_dirs.len(), "Copilot adapter started");
        let mut ticker = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // The scan walks dirs and reads whole JSON files (blocking); run
                    // it off the async worker so it can't stall the OTLP receiver,
                    // API, or engine sharing the runtime. `self` round-trips the task.
                    let observed = now_ms();
                    let (returned, turns) = match tokio::task::spawn_blocking(move || {
                        let turns = self.poll_once(observed);
                        (self, turns)
                    })
                    .await
                    {
                        Ok(pair) => pair,
                        Err(_) => return Ok(()), // the blocking scan panicked
                    };
                    self = returned;
                    for turn in turns {
                        if sink.send(turn).await.is_err() {
                            return Ok(()); // engine gone
                        }
                    }
                }
                () = wait_for_shutdown(&mut shutdown) => break,
            }
        }
        tracing::info!("Copilot adapter stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: i64 = 1_700_000_000_000;

    /// A single-request chat-session document (one user→assistant exchange).
    fn session_json(model: &str, message: &str, response: &str) -> String {
        format!(
            r#"{{"requests":[{{"requestId":"r1","timestamp":1700000000000,"modelId":"{model}","message":{{"text":"{message}"}},"response":[{{"value":"{response}"}}]}}]}}"#
        )
    }

    fn file_uri(p: &Path) -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        if s.starts_with('/') {
            format!("file://{s}")
        } else {
            format!("file:///{s}")
        }
    }

    #[test]
    fn content_chars_sums_nested_value_text_and_bare_strings() {
        assert_eq!(content_chars(&serde_json::json!("abcd")), 4);
        assert_eq!(content_chars(&serde_json::json!({ "value": "abc" })), 3);
        // Nested markdown content: {value:{value:"ab"}}.
        assert_eq!(
            content_chars(&serde_json::json!({ "value": { "value": "ab" } })),
            2
        );
        // An array of parts plus a bare string; non-content keys are ignored.
        let parts = serde_json::json!([{ "kind": "markdown", "value": "abc" }, "de"]);
        assert_eq!(content_chars(&parts), 5);
        // Message carried as parts/text.
        assert_eq!(
            content_chars(&serde_json::json!({ "parts": [{ "text": "abcd" }] })),
            4,
        );
    }

    #[test]
    fn parses_a_request_into_an_estimated_turn() {
        let v: Value =
            serde_json::from_str(&session_json("claude-opus-4.8", "hi there", "abcdefgh")).unwrap();
        let turns = parse_session(&v, "sess-1", TS).expect("recognized");
        assert_eq!(turns.len(), 1);
        let (key, turn) = &turns[0];
        assert_eq!(key, "sess-1:r1");
        assert_eq!(turn.source, Source::Copilot);
        assert_eq!(turn.harness, "copilot_chat");
        assert!(turn.counts_estimated, "Copilot logs carry no token counts");
        assert_eq!(turn.tokens_input, 2, "\"hi there\" = 8 chars / 4");
        assert_eq!(turn.tokens_output, 2, "\"abcdefgh\" = 8 chars / 4");
        assert_eq!(turn.model.as_deref(), Some("claude-opus-4-8")); // mapped
        assert_eq!(turn.timestamp_ms, 1_700_000_000_000);
        assert_eq!(turn.prompt_id.as_deref(), Some("sess-1:r1"));
        assert_eq!(turn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            turn.event_id.as_deref(),
            Some("sess-1:r1"),
            "the stable request key also drives the engine's dedup"
        );
    }

    #[test]
    fn requests_without_a_response_are_skipped() {
        // No response text → nothing to estimate, so not a usable turn.
        let v: Value =
            serde_json::from_str(r#"{"requests":[{"requestId":"r1","message":{"text":"hi"}}]}"#)
                .unwrap();
        assert!(parse_session(&v, "s", TS).expect("recognized").is_empty());
    }

    #[test]
    fn session_without_a_requests_array_is_unrecognized() {
        // A document with no `requests` array is an unrecognized shape (→ None).
        assert!(parse_session(&serde_json::json!({ "version": 3 }), "s", TS).is_none());
    }

    #[test]
    fn unknown_model_falls_back_to_unresolved_but_still_estimated() {
        let v: Value =
            serde_json::from_str(&session_json("some-future-model", "hi", "abcd")).unwrap();
        let turns = parse_session(&v, "s", TS).expect("recognized");
        let turn = &turns[0].1;
        assert!(turn.model.is_none(), "unmappable model → unresolved");
        assert!(turn.counts_estimated);
    }

    #[test]
    fn reads_only_the_bound_workspaces_sessions_and_dedups() {
        let base = std::env::temp_dir().join(format!("promptlyd-copilot-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Code").join("User");
        let workspace = base.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();

        // The bound workspace's <hash> dir, with a Copilot chat session under it.
        let hash = user.join("workspaceStorage").join("hash-1");
        let sessions = hash.join("chatSessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            hash.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&workspace)),
        )
        .unwrap();
        std::fs::write(
            sessions.join("c1.json"),
            session_json("claude-opus-4.8", "hi there", "abcdefgh"),
        )
        .unwrap();

        // Another workspace's <hash> dir — never returned by find_workspace_storage.
        let other = user.join("workspaceStorage").join("hash-2");
        let other_sessions = other.join("chatSessions");
        std::fs::create_dir_all(&other_sessions).unwrap();
        std::fs::write(
            other.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&base.join("elsewhere"))),
        )
        .unwrap();
        std::fs::write(
            other_sessions.join("c1.json"),
            session_json("gpt-5.5", "x", "yyyy"),
        )
        .unwrap();

        let mut src = CopilotSource::new(
            std::slice::from_ref(&user),
            &workspace,
            AdapterRegistry::new(),
        );
        let turns = src.poll_once(TS);
        assert_eq!(turns.len(), 1, "only the bound workspace's session");
        assert_eq!(turns[0].model.as_deref(), Some("claude-opus-4-8"));
        assert!(turns[0].counts_estimated);
        assert_eq!(
            turns[0].workspace.as_deref(),
            Some(workspace.to_string_lossy().as_ref()),
        );
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Detected);

        // A second poll finds nothing new (dedup by request key).
        assert!(src.poll_once(TS + 1).is_empty());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reports_not_found_and_unsupported_states() {
        let base =
            std::env::temp_dir().join(format!("promptlyd-copilot-st-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Code").join("User");
        let workspace = base.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();

        // Nothing on disk → NotFound, no panic.
        let mut src = CopilotSource::new(
            std::slice::from_ref(&user),
            &workspace,
            AdapterRegistry::new(),
        );
        assert!(src.poll_once(TS).is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::NotFound);

        // Workspace opened, but every session file is an unrecognized shape → Unsupported.
        let hash = user.join("workspaceStorage").join("hash-1");
        let sessions = hash.join("chatSessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            hash.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&workspace)),
        )
        .unwrap();
        std::fs::write(sessions.join("c1.json"), r#"{"version":3}"#).unwrap();
        let mut src = CopilotSource::new(&[user], &workspace, AdapterRegistry::new());
        assert!(src.poll_once(TS).is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Unsupported);

        std::fs::remove_dir_all(&base).ok();
    }
}
