//! GitHub Copilot Chat capture adapter (`21`) — **best-effort, low confidence**.
//!
//! Copilot Chat rides VS Code's native chat store under
//! `…/User/workspaceStorage/<hash>/chatSessions/`. Two container formats
//! coexist there:
//!   - legacy (≤2025) `<id>.json` — one whole JSON document with a `requests`
//!     array, one entry per user→assistant exchange;
//!   - current `<id>.jsonl` — a **mutation log**: line 0 (`kind: 0`) is the
//!     initial snapshot under `v` (its `requests` starts empty), and each later
//!     line sets a value at a path (`kind: 1`, path in `k`) or appends to an
//!     array at a path (`kind: 2`) — the session object must be rebuilt by
//!     replaying the log ([`replay_session_log`]). The per-request schema is
//!     the same in both containers.
//!
//! The `<hash>` dir is matched to the bound workspace by its `workspace.json`
//! (`crate::sources::vscode`) — including ancestor/descendant project roots,
//! since players routinely open the parent folder — and that match *is* the
//! scoping (`18`): every session file under it belongs to that workspace. Both
//! stable VS Code and Insiders are searched (`paths::vscode_user_dirs`).
//!
//! Unlike Claude Code and Codex, these logs carry **no I/O token counts** — they
//! are a UI transcript, not an API-usage record. So every turn's tokens are
//! *estimated* from the message/response text (chars/4) and the turn is marked
//! `estimated` (`counts_estimated`); it always scores against the baseline-floor
//! tier (`13a`). Thinking tokens are the exception: current sessions record real
//! per-round `thinking.tokens` in `result.metadata.toolCallRounds`, which are
//! summed. The model name, when the request records one, is resolved
//! best-effort (`copilot/<model>` prefixed; Auto-mode requests may only name a
//! usable model in their tool-call rounds' `phaseModelId`).
//!
//! A session file is rewritten/appended in place as its chat grows, so — like
//! the Cursor adapter — we re-read it each poll and dedup by a stable
//! per-request key rather than tailing a byte offset. When the store is absent
//! or its shape isn't recognized the adapter degrades and reports via the
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

/// Sum the real per-round thinking token counts a current session records
/// (`result.metadata.toolCallRounds[].thinking.tokens`); 0 when absent.
fn thinking_tokens(req: &Value) -> u64 {
    req.pointer("/result/metadata/toolCallRounds")
        .and_then(Value::as_array)
        .map(|rounds| {
            rounds
                .iter()
                .filter_map(|r| r.pointer("/thinking/tokens").and_then(Value::as_u64))
                .sum()
        })
        .unwrap_or(0)
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

    // Best-effort model: the first candidate that actually RESOLVES wins, so an
    // Auto-mode `copilot/auto` on `modelId` falls through to the round-level
    // `phaseModelId` that names the model Auto actually routed to.
    let named = [
        "/modelId",
        "/result/metadata/modelId",
        "/result/metadata/model",
        "/model",
    ]
    .iter()
    .filter_map(|p| req.pointer(p).and_then(Value::as_str));
    let routed = req
        .pointer("/result/metadata/toolCallRounds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("phaseModelId").and_then(Value::as_str));
    let model = named
        .chain(routed)
        .find_map(model_map::resolve)
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
        // Real per-round counts when the session records them; cache tokens are
        // never broken out.
        tokens_thinking: thinking_tokens(req),
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

/// Both chat-session containers: legacy whole-file `.json` and the current
/// `.jsonl` mutation log.
fn is_session_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("json") | Some("jsonl")
    )
}

/// One step of a mutation-log path (`k`): an object key or an array index.
fn descend<'a>(value: &'a mut Value, seg: &Value) -> Option<&'a mut Value> {
    match seg {
        Value::String(key) => {
            // Create missing object keys so a `set` can build nested state the
            // snapshot didn't carry yet.
            if value.is_null() {
                *value = Value::Object(serde_json::Map::new());
            }
            let obj = value.as_object_mut()?;
            Some(obj.entry(key.clone()).or_insert(Value::Null))
        }
        Value::Number(n) => {
            let idx = n.as_u64()? as usize;
            if value.is_null() {
                *value = Value::Array(Vec::new());
            }
            let arr = value.as_array_mut()?;
            if idx == arr.len() {
                arr.push(Value::Null);
            }
            arr.get_mut(idx)
        }
        _ => None,
    }
}

/// Navigate `root` along the whole `k` path, creating missing intermediates.
fn resolve_path<'a>(root: &'a mut Value, path: &[Value]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = descend(cur, seg)?;
    }
    Some(cur)
}

/// Rebuild a chat-session object from VS Code's `.jsonl` mutation log: line 0
/// (`kind: 0`) is the initial snapshot under `v`; later lines set a value at
/// path `k` (`kind: 1`) or append `v`'s elements to the array at `k`
/// (`kind: 2`). Unknown kinds and malformed lines are skipped (drift-lenient);
/// `None` when no snapshot line is found — an unrecognized container.
pub fn replay_session_log(text: &str) -> Option<Value> {
    let mut base: Option<Value> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry.get("kind").and_then(Value::as_i64);
        match (kind, &mut base) {
            (Some(0), _) => {
                if let Some(v) = entry.get("v") {
                    base = Some(v.clone());
                }
            }
            (Some(1), Some(root)) => {
                let (Some(path), Some(v)) =
                    (entry.get("k").and_then(Value::as_array), entry.get("v"))
                else {
                    continue;
                };
                if let Some(slot) = resolve_path(root, path) {
                    *slot = v.clone();
                }
            }
            (Some(2), Some(root)) => {
                let (Some(path), Some(v)) =
                    (entry.get("k").and_then(Value::as_array), entry.get("v"))
                else {
                    continue;
                };
                let Some(slot) = resolve_path(root, path) else {
                    continue;
                };
                if slot.is_null() {
                    *slot = Value::Array(Vec::new());
                }
                if let Some(arr) = slot.as_array_mut() {
                    match v {
                        Value::Array(items) => arr.extend(items.iter().cloned()),
                        other => arr.push(other.clone()),
                    }
                }
            }
            _ => continue,
        }
    }
    base
}

/// Parse a session file's text by its container: a `.jsonl` mutation log is
/// replayed into the session object; a `.json` is the object itself.
fn parse_container(path: &Path, text: &str) -> Option<Value> {
    if path.extension().and_then(OsStr::to_str) == Some("jsonl") {
        replay_session_log(text)
    } else {
        serde_json::from_str(text).ok()
    }
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
    workspace_str: String,
    workspace_norm: String,
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
            workspace_str: workspace.to_string_lossy().into_owned(),
            workspace_norm: super::jsonl::normalize_for_compare(&workspace.to_string_lossy()),
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

        // The workspace's `<hash>` dirs in each install where it — or a related
        // project root (players routinely open the parent folder) — was opened.
        let hash_dirs: Vec<PathBuf> = roots
            .iter()
            .flat_map(|root| {
                vscode::related_workspace_storages(
                    &root.join("workspaceStorage"),
                    &self.workspace_norm,
                )
            })
            .map(|(_, dir)| dir)
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
                if !is_session_file(&path) {
                    continue;
                }
                // Counted BEFORE parsing, so a container we fail to read still
                // registers — a jsonl-only workspace must report `Unsupported`
                // on breakage, never a silent "no sessions yet".
                files_total += 1;
                let stem = path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or("session");
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(value) = parse_container(&path, &text) else {
                    continue; // unreadable container — counted as unrecognized
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

    /// A current-format `.jsonl` mutation log modeled line-for-line on a real
    /// VS Code 1.124 session: snapshot, title set, request append, response
    /// appends, result set.
    fn mutation_log() -> String {
        [
            r#"{"kind":0,"v":{"version":3,"creationDate":1773400465000,"sessionId":"s-1","requests":[],"inputState":{"selectedModel":{"identifier":"copilot/gpt-5.3-codex"}}}}"#,
            r#"{"kind":1,"k":["customTitle"],"v":"Removing a Git repository"}"#,
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"request_1","timestamp":1773400465296,"modelId":"copilot/gpt-5.3-codex","message":{"text":"hi there","parts":[{"text":""}]}}]}"#,
            r#"{"kind":2,"k":["requests",0,"response"],"v":[{"kind":"thinking","value":"plan"}]}"#,
            r#"{"kind":2,"k":["requests",0,"response"],"v":[{"value":"done abc"}]}"#,
            r#"{"kind":1,"k":["requests",0,"result"],"v":{"metadata":{"toolCallRounds":[{"thinking":{"tokens":177},"phaseModelId":"gpt-5.3-codex"},{"thinking":{"tokens":36}}]}}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn replays_a_current_jsonl_mutation_log_into_turns() {
        let session = replay_session_log(&mutation_log()).expect("snapshot line found");
        // The replayed object carries the same request schema as legacy .json…
        let turns = parse_session(&session, "s-1", TS).expect("recognized");
        assert_eq!(turns.len(), 1);
        let (key, turn) = &turns[0];
        assert_eq!(key, "s-1:request_1");
        assert_eq!(turn.model.as_deref(), Some("gpt-5-3-codex"));
        assert_eq!(turn.timestamp_ms, 1_773_400_465_296);
        assert!(turn.counts_estimated, "I/O is still estimated from text");
        assert_eq!(turn.tokens_input, 2, "\"hi there\" = 8 chars / 4");
        assert_eq!(
            turn.tokens_output, 3,
            "\"plan\" + \"done abc\" = 12 chars / 4 across appended parts"
        );
        assert_eq!(
            turn.tokens_thinking, 213,
            "real per-round thinking counts (177 + 36) are summed, not estimated"
        );
    }

    #[test]
    fn replay_skips_malformed_lines_and_needs_a_snapshot() {
        // Deltas without a snapshot rebuild nothing.
        assert!(replay_session_log(r#"{"kind":1,"k":["a"],"v":1}"#).is_none());
        assert!(replay_session_log("not json at all").is_none());
        // Garbage between valid lines is skipped, not fatal.
        let log = format!(
            "{}\nnot json\n{}",
            r#"{"kind":0,"v":{"requests":[]}}"#,
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r1"}]}"#
        );
        let session = replay_session_log(&log).expect("snapshot found");
        assert_eq!(
            session
                .pointer("/requests/0/requestId")
                .and_then(Value::as_str),
            Some("r1"),
        );
    }

    #[test]
    fn auto_mode_resolves_via_the_rounds_phase_model() {
        // Auto mode records `copilot/auto` as the modelId — unresolvable — but
        // each round names the model Auto actually routed to.
        let v: Value = serde_json::from_str(
            r#"{"requests":[{"requestId":"r1","modelId":"copilot/auto","message":{"text":"hi"},"response":[{"value":"abcd"}],"result":{"metadata":{"toolCallRounds":[{"phaseModelId":"gpt-5-mini"}]}}}]}"#,
        )
        .unwrap();
        let turns = parse_session(&v, "s", TS).expect("recognized");
        assert_eq!(turns[0].1.model.as_deref(), Some("gpt-5-mini"));
    }

    #[test]
    fn jsonl_sessions_under_a_parent_project_root_are_captured() {
        // The real-world shape: VS Code was opened at the PARENT project root;
        // the daemon binds a level subfolder; the session is a .jsonl log.
        let base =
            std::env::temp_dir().join(format!("promptlyd-copilot-jl-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Code").join("User");
        let parent = base.join("challenge");
        let workspace = parent.join("stage-1-02");
        std::fs::create_dir_all(&workspace).unwrap();

        let hash = user.join("workspaceStorage").join("hash-1");
        let sessions = hash.join(CHAT_SESSIONS_DIR);
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            hash.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&parent)),
        )
        .unwrap();
        std::fs::write(sessions.join("s-1.jsonl"), mutation_log()).unwrap();

        let mut src = CopilotSource::new(
            std::slice::from_ref(&user),
            &workspace,
            AdapterRegistry::new(),
        );
        let turns = src.poll_once(TS);
        assert_eq!(
            turns.len(),
            1,
            "the parent-rooted jsonl session is in scope"
        );
        assert_eq!(turns[0].model.as_deref(), Some("gpt-5-3-codex"));
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Detected);
        // Dedup holds across polls.
        assert!(src.poll_once(TS + 1).is_empty());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_jsonl_only_workspace_with_unreadable_logs_reports_unsupported() {
        // The silent-masking regression: a workspace holding ONLY .jsonl files
        // the adapter can't parse must say "format changed", not "no sessions".
        let base =
            std::env::temp_dir().join(format!("promptlyd-copilot-uj-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Code").join("User");
        let workspace = base.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let hash = user.join("workspaceStorage").join("hash-1");
        let sessions = hash.join(CHAT_SESSIONS_DIR);
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            hash.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&workspace)),
        )
        .unwrap();
        std::fs::write(sessions.join("s-1.jsonl"), "no snapshot line here").unwrap();

        let mut src = CopilotSource::new(&[user], &workspace, AdapterRegistry::new());
        assert!(src.poll_once(TS).is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Unsupported);

        std::fs::remove_dir_all(&base).ok();
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
