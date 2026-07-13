//! Cursor capture adapter (`21`) — **best-effort, version-fragile**.
//!
//! Cursor stores conversation turns ("bubbles") in a SQLite database,
//! `…/User/globalStorage/state.vscdb`, in a `cursorDiskKV` table keyed
//! `bubbleId:<composerId>:<bubbleId>`, each value a JSON object carrying
//! `modelInfo.modelName` and `tokenCount.inputTokens`/`outputTokens`. Which
//! composers belong to a workspace is recorded in that workspace's own
//! `…/User/workspaceStorage/<hash>/state.vscdb` (`ItemTable` key
//! `composer.composerData`); the `<hash>` dir is matched to the bound workspace
//! by its `workspace.json` (`crate::sources::vscode`). So this adapter:
//!   1. finds the bound workspace's `<hash>` dir,
//!   2. reads its composer ids,
//!   3. reads those composers' bubbles from the global store.
//!
//! That composer-membership scoping is what keeps capture bound to the
//! workspace (`18`).
//!
//! SQLite is opened **read-only + `immutable`** so a running Cursor is never
//! disturbed and no lock is taken; the trade-off is that data still only in the
//! WAL isn't seen until Cursor checkpoints it (caught on a later poll). Cursor
//! occasionally records a zero `tokenCount`; we then estimate output tokens from
//! the response text (char/4) and mark the turn `estimated` (`17`). When the
//! schema isn't recognized the adapter degrades and reports via the registry for
//! `promptly doctor` (`19`) rather than crashing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::registry::{AdapterRegistry, AdapterState};
use super::{vscode, wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::model::{RawTurn, Source, HARNESS_CURSOR};
use crate::model_map;

/// Registry/adapter name (also the `harness` string scoring keys on, `13`).
pub const NAME: &str = "cursor";
/// Chars-per-token for the zero-`tokenCount` estimation fallback (matches the
/// JSONL watcher's thinking estimate, `17`).
const CHARS_PER_TOKEN: u64 = 4;
/// Poll interval. Slower than the JSONL watcher: Cursor is a secondary source and
/// each scan opens SQLite, so there's no need to hammer it.
const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// Extract the composer ids bound to a workspace from its `composer.composerData`.
pub fn parse_composer_ids(composer_data: &Value) -> Vec<String> {
    composer_data
        .get("allComposers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("composerId").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The bubble's own timestamp (`createdAt`/`cTime`/`timestamp`), or `observed_ms`
/// when it carries none. Used both to stamp the turn and to order a composer's
/// bubbles into conversation order for prompt grouping.
fn bubble_timestamp(value: &Value, observed_ms: i64) -> i64 {
    ["createdAt", "cTime", "timestamp"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_i64))
        .unwrap_or(observed_ms)
}

/// Parse one Cursor bubble into a [`RawTurn`], or `None` if it isn't a
/// token-bearing assistant turn. Lenient: unknown fields and minor drift are
/// tolerated. `prompt_group` is the id of the user bubble that opened the current
/// prompt (so all the turns of one prompt group together); `observed_ms` is the
/// capture time, used when the bubble carries no timestamp of its own.
pub fn parse_bubble(
    value: &Value,
    composer_id: &str,
    bubble_id: &str,
    prompt_group: Option<&str>,
    observed_ms: i64,
) -> Option<RawTurn> {
    // Cursor bubble `type`: 1 = user, 2 = assistant. Only assistant turns carry
    // model/token usage; when the field is absent, require an assistant signal.
    match value.get("type").and_then(Value::as_i64) {
        Some(2) => {}
        Some(_) => return None,
        None => {
            let assistant_like = value.get("modelInfo").is_some()
                || value.pointer("/tokenCount/outputTokens").is_some();
            if !assistant_like {
                return None;
            }
        }
    }

    let model = value
        .pointer("/modelInfo/modelName")
        .and_then(Value::as_str)
        .and_then(model_map::resolve)
        .map(str::to_string);

    let count = |ptr: &str| value.pointer(ptr).and_then(Value::as_u64).unwrap_or(0);
    let tokens_input = count("/tokenCount/inputTokens");
    let mut tokens_output = count("/tokenCount/outputTokens");
    let mut counts_estimated = false;

    if tokens_input == 0 && tokens_output == 0 {
        // Cursor sometimes records a zero tokenCount; estimate output from the
        // response text and mark the turn estimated (it scores at the floor).
        let chars = value
            .get("text")
            .and_then(Value::as_str)
            .map(|t| t.chars().count() as u64)
            .unwrap_or(0);
        if chars == 0 {
            return None; // nothing reported and nothing to estimate from
        }
        tokens_output = chars.div_ceil(CHARS_PER_TOKEN);
        counts_estimated = true;
    }

    let timestamp_ms = bubble_timestamp(value, observed_ms);

    Some(RawTurn {
        source: Source::Cursor,
        model,
        harness: HARNESS_CURSOR.to_string(),
        tokens_input,
        tokens_output,
        // Cursor doesn't break out thinking or cache tokens.
        tokens_thinking: 0,
        tokens_cache: 0,
        // The prompt group: every assistant bubble one user bubble drove shares
        // that user bubble's id, so grading's `P` counts prompts, not the many
        // bubbles an agentic prompt produces. Falls back to the bubble's own id
        // when no user bubble precedes it (its own prompt — no worse than before).
        prompt_id: Some(match prompt_group {
            Some(group) => format!("{composer_id}:{group}"),
            None => format!("{composer_id}:{bubble_id}"),
        }),
        timestamp_ms,
        cost_usd: None,
        duration_ms: None,
        session_id: Some(composer_id.to_string()),
        // Scoping is by composer membership (below); the I/O layer stamps the
        // bound workspace so the engine attributes the turn unambiguously.
        workspace: None,
        counts_estimated,
        // The unique composer:bubble pair keys the engine's dedup — so distinct
        // bubbles stay distinct and a re-read one after a restart is ignored —
        // now decoupled from `prompt_id`, which carries the shared prompt group.
        event_id: Some(format!("{composer_id}:{bubble_id}")),
    })
}

/// Build a SQLite `file:` URI opening the db read-only + immutable. `immutable=1`
/// promises SQLite the file won't change, so it takes no lock and ignores the
/// WAL — safe to read a database a running Cursor holds open.
fn sqlite_immutable_uri(path: &Path) -> String {
    let slashed = path.to_string_lossy().replace('\\', "/");
    let abs = if slashed.starts_with('/') {
        slashed
    } else {
        // Windows drive path (`C:/…`) needs the authority-less leading slash.
        format!("/{slashed}")
    };
    // `?` and `#` are URI-significant; the rest of a filesystem path is fine.
    let encoded = abs.replace('?', "%3f").replace('#', "%23");
    format!("file://{encoded}?immutable=1")
}

fn open_immutable(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        sqlite_immutable_uri(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// The raw bytes of a column, whether SQLite stored it as TEXT or BLOB (Cursor
/// uses both across versions); other types yield no bytes.
fn column_bytes(row: &rusqlite::Row, idx: usize) -> rusqlite::Result<Vec<u8>> {
    use rusqlite::types::ValueRef;
    match row.get_ref(idx)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => Ok(b.to_vec()),
        _ => Ok(Vec::new()),
    }
}

/// Read a single key's value (TEXT or BLOB) from a key/value table. The table
/// name is a fixed literal here, never user input.
fn read_kv(conn: &Connection, table: &str, key: &str) -> Option<Vec<u8>> {
    conn.query_row(
        &format!("SELECT value FROM {table} WHERE key = ?1"),
        [key],
        |r| column_bytes(r, 0),
    )
    .ok()
    .filter(|b| !b.is_empty())
}

/// All bubbles for one composer, parsed and grouped by user prompt; returns
/// `(bubble_key, turn)` pairs. Cursor records no per-turn prompt id, so we order
/// the bubbles into conversation order (by timestamp, then id) and stamp each
/// assistant bubble with the most-recent `type:1` user bubble that precedes it —
/// that shared id is the prompt group grading's `P` counts.
fn read_bubbles(
    conn: &Connection,
    composer: &str,
    observed_ms: i64,
) -> rusqlite::Result<Vec<(String, RawTurn)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE ?1")?;
    let like = format!("bubbleId:{composer}:%");
    let rows = stmt.query_map([like], |r| {
        Ok((r.get::<_, String>(0)?, column_bytes(r, 1)?))
    })?;

    // Collect every bubble (user + assistant) with its ordering key.
    struct Bubble {
        key: String,
        bubble_id: String,
        value: Value,
        ts: i64,
        is_user: bool,
    }
    let mut bubbles = Vec::new();
    for row in rows {
        let (key, bytes) = row?;
        let bubble_id = key.rsplit(':').next().unwrap_or("").to_string();
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue; // a non-JSON value isn't a bubble we understand
        };
        let ts = bubble_timestamp(&value, observed_ms);
        let is_user = value.get("type").and_then(Value::as_i64) == Some(1);
        bubbles.push(Bubble {
            key,
            bubble_id,
            value,
            ts,
            is_user,
        });
    }
    // Conversation order: by timestamp, then bubble id as a stable tiebreaker
    // (bubbles sharing a timestamp — or carrying none — stay deterministically
    // ordered).
    bubbles.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.bubble_id.cmp(&b.bubble_id)));

    let mut current_user: Option<String> = None;
    let mut out = Vec::new();
    for bubble in &bubbles {
        if bubble.is_user {
            current_user = Some(bubble.bubble_id.clone());
            continue; // a user bubble opens a prompt; it is not itself a turn
        }
        if let Some(turn) = parse_bubble(
            &bubble.value,
            composer,
            &bubble.bubble_id,
            current_user.as_deref(),
            observed_ms,
        ) {
            out.push((bubble.key.clone(), turn));
        }
    }
    Ok(out)
}

/// The result of one scan: the detection state to publish plus the bubbles found.
struct Scan {
    state: AdapterState,
    detail: String,
    bubbles: Vec<(String, RawTurn)>,
}

impl Scan {
    fn empty(state: AdapterState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            bubbles: Vec::new(),
        }
    }
}

/// Tails Cursor's bubble store, scoped to the bound workspace's composers.
pub struct CursorSource {
    user_dir: PathBuf,
    workspace: PathBuf,
    workspace_str: String,
    registry: AdapterRegistry,
    seen: HashSet<String>,
    poll: Duration,
}

impl CursorSource {
    /// Build an adapter reading Cursor under `user_dir` for `workspace`.
    pub fn new(user_dir: &Path, workspace: &Path, registry: AdapterRegistry) -> Self {
        Self {
            user_dir: user_dir.to_path_buf(),
            workspace: workspace.to_path_buf(),
            workspace_str: workspace.to_string_lossy().into_owned(),
            registry,
            seen: HashSet::new(),
            poll: DEFAULT_POLL,
        }
    }

    /// Locate, scope, and read the bound workspace's bubbles, classifying the
    /// source's state. Never panics: any I/O or schema problem maps to a state.
    fn collect(&self, observed_ms: i64) -> Scan {
        let global_db = self.user_dir.join("globalStorage").join("state.vscdb");
        if !global_db.exists() {
            return Scan::empty(
                AdapterState::NotFound,
                format!("Cursor global storage not found ({})", global_db.display()),
            );
        }
        let ws_storage = self.user_dir.join("workspaceStorage");
        let Some(ws_dir) = vscode::find_workspace_storage(&ws_storage, &self.workspace) else {
            return Scan::empty(
                AdapterState::NotFound,
                "this workspace hasn't been opened in Cursor",
            );
        };
        let composer_ids = match self.read_composer_ids(&ws_dir) {
            Ok(ids) => ids,
            Err(detail) => return Scan::empty(AdapterState::Unsupported, detail),
        };
        if composer_ids.is_empty() {
            return Scan::empty(
                AdapterState::Detected,
                "no Cursor conversations in this workspace yet",
            );
        }
        let conn = match open_immutable(&global_db) {
            Ok(c) => c,
            Err(e) => {
                return Scan::empty(
                    AdapterState::NotFound,
                    format!("couldn't open Cursor global storage: {e}"),
                )
            }
        };
        if !table_exists(&conn, "cursorDiskKV") {
            return Scan::empty(
                AdapterState::Unsupported,
                "cursorDiskKV table missing — Cursor storage format changed",
            );
        }
        let mut bubbles = Vec::new();
        for composer in &composer_ids {
            match read_bubbles(&conn, composer, observed_ms) {
                Ok(mut found) => bubbles.append(&mut found),
                Err(e) => tracing::warn!(composer, error = %e, "cursor: bubble read failed"),
            }
        }
        let detail = format!(
            "{} turn(s) across {} conversation(s)",
            bubbles.len(),
            composer_ids.len()
        );
        Scan {
            state: AdapterState::Detected,
            detail,
            bubbles,
        }
    }

    /// Composer ids bound to this workspace. `Err` means the schema wasn't
    /// recognized (→ `Unsupported`); an empty `Ok` means no conversations yet.
    fn read_composer_ids(&self, ws_dir: &Path) -> Result<Vec<String>, String> {
        let ws_db = ws_dir.join("state.vscdb");
        if !ws_db.exists() {
            return Ok(Vec::new());
        }
        let conn =
            open_immutable(&ws_db).map_err(|e| format!("couldn't open workspace state: {e}"))?;
        if !table_exists(&conn, "ItemTable") {
            return Err("ItemTable missing — Cursor storage format changed".into());
        }
        let Some(bytes) = read_kv(&conn, "ItemTable", "composer.composerData") else {
            return Ok(Vec::new());
        };
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("composer data not JSON: {e}"))?;
        Ok(parse_composer_ids(&value))
    }

    /// Skip whatever already exists so only conversations from now on are
    /// captured (mirrors the JSONL watcher's `prime`; on restart this avoids
    /// re-emitting bubbles already captured before the restart).
    pub fn prime(&mut self, observed_ms: i64) {
        let scan = self.collect(observed_ms);
        for (key, _) in scan.bubbles {
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
        for (key, mut turn) in scan.bubbles {
            if self.seen.insert(key) {
                turn.workspace = Some(self.workspace_str.clone());
                turns.push(turn);
            }
        }
        turns
    }
}

#[async_trait]
impl TelemetrySource for CursorSource {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn run(
        mut self: Box<Self>,
        sink: RawTurnSink,
        mut shutdown: Shutdown,
    ) -> anyhow::Result<()> {
        self.prime(now_ms());
        tracing::info!(user_dir = %self.user_dir.display(), "Cursor adapter started");
        let mut ticker = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // The scan opens SQLite (blocking); run it off the async worker
                    // so it can't stall the OTLP receiver, API, or engine sharing
                    // the runtime. `self` makes the round trip through the task.
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
        tracing::info!("Cursor adapter stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    const TS: i64 = 1_700_000_000_000;

    fn assistant_bubble(model: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":2,"modelInfo":{{"modelName":"{model}"}},"tokenCount":{{"inputTokens":{input},"outputTokens":{output}}}}}"#
        )
    }

    #[test]
    fn parses_an_assistant_bubble() {
        let v: Value =
            serde_json::from_str(&assistant_bubble("claude-opus-4.8", 100, 200)).unwrap();
        let turn =
            parse_bubble(&v, "comp-A", "b1", Some("u1"), TS).expect("assistant bubble parses");
        assert_eq!(turn.source, Source::Cursor);
        assert_eq!(turn.harness, "cursor");
        assert_eq!(turn.model.as_deref(), Some("claude-opus-4-8")); // mapped
        assert_eq!(turn.tokens_input, 100);
        assert_eq!(turn.tokens_output, 200);
        assert!(!turn.counts_estimated);
        // prompt_id carries the shared prompt group; event_id the unique bubble.
        assert_eq!(turn.prompt_id.as_deref(), Some("comp-A:u1"));
        assert_eq!(turn.event_id.as_deref(), Some("comp-A:b1"));
        assert_eq!(turn.session_id.as_deref(), Some("comp-A"));
    }

    #[test]
    fn zero_token_bubble_estimates_from_text_and_marks_estimated() {
        let v: Value = serde_json::from_str(
            r#"{"type":2,"modelInfo":{"modelName":"gpt-5.5"},"tokenCount":{"inputTokens":0,"outputTokens":0},"text":"abcdefgh"}"#,
        )
        .unwrap();
        let turn = parse_bubble(&v, "c", "b", None, TS).expect("estimable");
        assert!(turn.counts_estimated);
        assert_eq!(turn.tokens_output, 2, "8 chars / 4 per token");
        assert_eq!(turn.model.as_deref(), Some("gpt-5-5"));
    }

    #[test]
    fn user_bubbles_and_empty_zero_token_bubbles_are_skipped() {
        let user: Value = serde_json::from_str(r#"{"type":1,"text":"hello"}"#).unwrap();
        assert!(parse_bubble(&user, "c", "b", None, TS).is_none());
        // Assistant, but zero tokens and no text to estimate from.
        let empty: Value =
            serde_json::from_str(r#"{"type":2,"tokenCount":{"inputTokens":0,"outputTokens":0}}"#)
                .unwrap();
        assert!(parse_bubble(&empty, "c", "b", None, TS).is_none());
    }

    #[test]
    fn unknown_model_falls_back_to_unresolved() {
        let v: Value =
            serde_json::from_str(&assistant_bubble("some-future-model", 10, 10)).unwrap();
        let turn = parse_bubble(&v, "c", "b", None, TS).unwrap();
        // Unmappable model → None (→ estimated/baseline-floor downstream), counts
        // themselves are still real.
        assert!(turn.model.is_none());
        assert!(!turn.counts_estimated);
    }

    #[test]
    fn parses_composer_ids() {
        let v: Value = serde_json::from_str(
            r#"{"allComposers":[{"composerId":"a"},{"composerId":"b"}],"selectedComposerId":"a"}"#,
        )
        .unwrap();
        assert_eq!(parse_composer_ids(&v), vec!["a", "b"]);
        assert!(parse_composer_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn sqlite_uri_is_absolute_and_immutable() {
        let uri = sqlite_immutable_uri(Path::new("/home/me/state.vscdb"));
        assert_eq!(uri, "file:///home/me/state.vscdb?immutable=1");
        // A Windows drive path gains the authority-less leading slash.
        let win = sqlite_immutable_uri(Path::new(r"C:\Users\me\state.vscdb"));
        assert_eq!(win, "file:///C:/Users/me/state.vscdb?immutable=1");
    }

    fn make_kv_db(path: &Path, table: &str, rows: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            &format!("CREATE TABLE {table} (key TEXT PRIMARY KEY, value BLOB)"),
            [],
        )
        .unwrap();
        for (k, v) in rows {
            conn.execute(
                &format!("INSERT INTO {table} (key, value) VALUES (?1, ?2)"),
                params![k, v],
            )
            .unwrap();
        }
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
    fn immutable_open_reads_back_committed_rows() {
        let base = std::env::temp_dir().join(format!("promptlyd-cur-imm-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("state.vscdb");
        make_kv_db(&db, "cursorDiskKV", &[("bubbleId:c:1", "{}")]);

        let conn = open_immutable(&db).expect("immutable open");
        assert!(table_exists(&conn, "cursorDiskKV"), "table visible");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM cursorDiskKV", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "committed row is visible to an immutable reader");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reads_only_the_bound_workspaces_bubbles_and_dedups() {
        let base = std::env::temp_dir().join(format!("promptlyd-cursor-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Cursor").join("User");
        let workspace = base.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();

        // The workspace's <hash> dir, pointing at our workspace, with its
        // composer list naming comp-A (and NOT comp-B, another workspace's).
        let hash = user.join("workspaceStorage").join("hash-1");
        std::fs::create_dir_all(&hash).unwrap();
        std::fs::write(
            hash.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&workspace)),
        )
        .unwrap();
        make_kv_db(
            &hash.join("state.vscdb"),
            "ItemTable",
            &[(
                "composer.composerData",
                r#"{"allComposers":[{"composerId":"comp-A"}]}"#,
            )],
        );

        // The global bubble store: two comp-A turns (one assistant, one user) and
        // a comp-B turn that belongs to a different workspace.
        let global = user.join("globalStorage");
        std::fs::create_dir_all(&global).unwrap();
        make_kv_db(
            &global.join("state.vscdb"),
            "cursorDiskKV",
            &[
                (
                    "bubbleId:comp-A:b1",
                    &assistant_bubble("claude-opus-4.8", 100, 50),
                ),
                ("bubbleId:comp-A:b2", r#"{"type":1,"text":"my prompt"}"#),
                ("bubbleId:comp-B:b1", &assistant_bubble("gpt-5.5", 9, 9)),
            ],
        );

        let mut src = CursorSource::new(&user, &workspace, AdapterRegistry::new());
        let turns = src.poll_once(TS);
        assert_eq!(turns.len(), 1, "only comp-A's assistant turn is in scope");
        assert_eq!(turns[0].model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(turns[0].tokens_output, 50);
        // The bound workspace is stamped so the engine attributes it.
        assert_eq!(
            turns[0].workspace.as_deref(),
            Some(workspace.to_string_lossy().as_ref())
        );
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Detected);

        // A second poll finds nothing new (dedup by bubble key).
        assert!(src.poll_once(TS + 1).is_empty());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn assistant_bubbles_group_under_the_preceding_user_bubble() {
        let base =
            std::env::temp_dir().join(format!("promptlyd-cursor-grp-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("state.vscdb");
        // One user bubble, then two assistant bubbles it drove (ascending cTime so
        // conversation order is unambiguous).
        make_kv_db(
            &db,
            "cursorDiskKV",
            &[
                (
                    "bubbleId:comp-A:u1",
                    r#"{"type":1,"text":"solve it","cTime":100}"#,
                ),
                (
                    "bubbleId:comp-A:a1",
                    r#"{"type":2,"modelInfo":{"modelName":"claude-opus-4.8"},"tokenCount":{"inputTokens":10,"outputTokens":20},"cTime":200}"#,
                ),
                (
                    "bubbleId:comp-A:a2",
                    r#"{"type":2,"modelInfo":{"modelName":"claude-opus-4.8"},"tokenCount":{"inputTokens":5,"outputTokens":8},"cTime":300}"#,
                ),
            ],
        );

        let conn = open_immutable(&db).unwrap();
        let turns = read_bubbles(&conn, "comp-A", TS).unwrap();
        assert_eq!(
            turns.len(),
            2,
            "two assistant turns; the user bubble is not a turn"
        );
        // Both assistant turns share the user bubble's prompt group → P counts 1…
        assert_eq!(turns[0].1.prompt_id.as_deref(), Some("comp-A:u1"));
        assert_eq!(turns[1].1.prompt_id.as_deref(), Some("comp-A:u1"));
        // …while each keeps its own unique dedup id.
        assert_eq!(turns[0].1.event_id.as_deref(), Some("comp-A:a1"));
        assert_eq!(turns[1].1.event_id.as_deref(), Some("comp-A:a2"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reports_not_found_and_unsupported_states() {
        let base = std::env::temp_dir().join(format!("promptlyd-cursor-st-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Cursor").join("User");
        let workspace = base.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();

        // No global storage at all → NotFound, no panic.
        let mut src = CursorSource::new(&user, &workspace, AdapterRegistry::new());
        assert!(src.poll_once(TS).is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::NotFound);

        // Global store present but the expected table is gone → Unsupported.
        let global = user.join("globalStorage");
        std::fs::create_dir_all(&global).unwrap();
        make_kv_db(&global.join("state.vscdb"), "someOtherTable", &[]);
        let hash = user.join("workspaceStorage").join("hash-1");
        std::fs::create_dir_all(&hash).unwrap();
        std::fs::write(
            hash.join("workspace.json"),
            format!(r#"{{"folder":"{}"}}"#, file_uri(&workspace)),
        )
        .unwrap();
        make_kv_db(
            &hash.join("state.vscdb"),
            "ItemTable",
            &[(
                "composer.composerData",
                r#"{"allComposers":[{"composerId":"comp-A"}]}"#,
            )],
        );
        let mut src = CursorSource::new(&user, &workspace, AdapterRegistry::new());
        assert!(src.poll_once(TS).is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Unsupported);

        std::fs::remove_dir_all(&base).ok();
    }
}
