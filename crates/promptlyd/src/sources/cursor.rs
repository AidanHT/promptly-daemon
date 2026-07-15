//! Cursor capture adapter (`21`) — **best-effort, version-fragile**.
//!
//! Cursor stores conversation turns ("bubbles") in a SQLite database,
//! `…/User/globalStorage/state.vscdb`, in a `cursorDiskKV` table keyed
//! `bubbleId:<composerId>:<bubbleId>`, with a per-conversation
//! `composerData:<composerId>` row beside them. Both the Cursor IDE and the
//! `cursor-agent` CLI write this one global store, so this adapter covers both.
//!
//! Which conversations belong to the bound workspace is decided three ways,
//! because Cursor has moved the membership record across versions:
//!   1. `composerData.workspaceIdentifier` — carries the project root's
//!      `uri.fsPath` and the `workspaceStorage/<hash>` id (current versions;
//!      always present for `cursor-agent` sessions).
//!   2. The workspace's own `…/User/workspaceStorage/<hash>/state.vscdb`
//!      `ItemTable` key `composer.composerData`: current versions keep a
//!      migrated stub listing `selectedComposerIds`/`lastFocusedComposerIds`
//!      (some IDE conversations carry no `workspaceIdentifier` at all); legacy
//!      versions list `allComposers`.
//!   3. The `<hash>` dirs themselves are matched to the bound workspace by
//!      `workspace.json` — **including ancestors/descendants**, because Cursor
//!      records the project root it was opened in, which is routinely the
//!      *parent* of a bound level workspace ([`vscode::paths_related`]).
//!
//! Bubble schema drift handled here: `createdAt` may be epoch ms **or** an
//! RFC3339 string; per-bubble `tokenCount` is `{0,0}` on current agent sessions
//! (real counts stopped ~Cursor 3.9), so token usage is *estimated* from the
//! bubble's content (`text`, `thinking.text`, `codeBlocks`, tool args/results)
//! and marked `estimated`; the model comes from the bubble's `modelInfo`, else
//! the prompt's user-bubble `modelInfo`, else the composer's `modelConfig`.
//! Conversation order comes from `composerData.fullConversationHeadersOnly`
//! when present (timestamp order as fallback) so prompt grouping — grading's
//! `P` — survives equal timestamps.
//!
//! SQLite is opened **read-only** without `immutable` when possible, so rows
//! still in the WAL (Cursor checkpoints lazily) are visible live; a stale WAL
//! with no writer can refuse read-only recovery, so `immutable=1` remains the
//! fallback. When the schema isn't recognized the adapter degrades and reports
//! via the registry for `promptly doctor` (`19`) rather than crashing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::jsonl::{normalize_for_compare, parse_rfc3339_millis};
use super::registry::{AdapterRegistry, AdapterState};
use super::{vscode, wait_for_shutdown, RawTurnSink, Shutdown, TelemetrySource};
use crate::clock::now_ms;
use crate::model::{RawTurn, Source, HARNESS_CURSOR};
use crate::model_map;

/// Registry/adapter name (also the `harness` string scoring keys on, `13`).
pub const NAME: &str = "cursor";
/// Chars-per-token for the token estimation fallback (matches the
/// JSONL watcher's thinking estimate, `17`).
const CHARS_PER_TOKEN: u64 = 4;
/// Poll interval. Slower than the JSONL watcher: Cursor is a secondary source and
/// each scan opens SQLite, so there's no need to hammer it.
const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// Extract the composer ids a workspace's `composer.composerData` names. Legacy
/// versions list full records under `allComposers`; current versions keep a
/// migrated stub whose `selectedComposerIds`/`lastFocusedComposerIds` are the
/// only membership trace left in the workspace store.
pub fn parse_composer_ids(composer_data: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if let Some(arr) = composer_data.get("allComposers").and_then(Value::as_array) {
        ids.extend(
            arr.iter()
                .filter_map(|c| c.get("composerId").and_then(Value::as_str))
                .map(str::to_string),
        );
    }
    for key in ["selectedComposerIds", "lastFocusedComposerIds"] {
        if let Some(arr) = composer_data.get(key).and_then(Value::as_array) {
            ids.extend(arr.iter().filter_map(|c| {
                c.as_str().map(str::to_string).or_else(|| {
                    c.get("composerId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            }));
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// The bubble's own timestamp — epoch-ms integer or RFC3339 string, both of
/// which Cursor has used for `createdAt` — or `observed_ms` when it carries
/// none. Used both to stamp the turn and to order a composer's bubbles when no
/// conversation headers exist.
fn bubble_timestamp(value: &Value, observed_ms: i64) -> i64 {
    ["createdAt", "cTime", "timestamp"]
        .iter()
        .find_map(|k| {
            let v = value.get(*k)?;
            v.as_i64()
                .or_else(|| v.as_str().and_then(parse_rfc3339_millis))
        })
        .unwrap_or(observed_ms)
}

/// Sum the characters of every string reachable through content-bearing keys
/// (`value`/`text`/`content`/`code`), bare strings, and arrays — the shape
/// drift-tolerant estimator input for `codeBlocks` and similar nests.
fn content_chars(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.chars().count() as u64,
        Value::Array(a) => a.iter().map(content_chars).sum(),
        Value::Object(o) => ["value", "text", "content", "code"]
            .iter()
            .filter_map(|k| o.get(*k))
            .map(content_chars)
            .sum(),
        _ => 0,
    }
}

/// The model a bubble reports (`modelInfo.modelName`), resolved — `None` when
/// absent or unrecognized.
fn bubble_model(value: &Value) -> Option<String> {
    value
        .pointer("/modelInfo/modelName")
        .and_then(Value::as_str)
        .and_then(model_map::resolve)
        .map(str::to_string)
}

/// Parse one Cursor bubble into a [`RawTurn`], or `None` if it isn't a usable
/// assistant turn. Lenient: unknown fields and minor drift are tolerated.
/// `prompt_group` is the id of the user bubble that opened the current prompt
/// (so all the turns of one prompt group together); `fallback_model` is the
/// prompt/composer-level model used when the bubble carries none; `observed_ms`
/// is the capture time, used when the bubble carries no timestamp of its own.
pub fn parse_bubble(
    value: &Value,
    composer_id: &str,
    bubble_id: &str,
    prompt_group: Option<&str>,
    fallback_model: Option<&str>,
    observed_ms: i64,
) -> Option<RawTurn> {
    // Cursor bubble `type`: 1 = user, 2 = assistant. Only assistant turns carry
    // usage/content; when the field is absent, require an assistant signal.
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

    let model = bubble_model(value).or_else(|| fallback_model.map(str::to_string));

    let count = |ptr: &str| value.pointer(ptr).and_then(Value::as_u64).unwrap_or(0);
    let mut tokens_input = count("/tokenCount/inputTokens");
    let mut tokens_output = count("/tokenCount/outputTokens");
    let mut tokens_thinking = 0;
    let mut counts_estimated = false;

    if tokens_input == 0 && tokens_output == 0 {
        // Current Cursor records a zero tokenCount on every agent bubble, so
        // estimate from the bubble's content and mark the turn estimated (it
        // scores at the floor). Output-shaped content: the response text, code
        // blocks, and the tool-call arguments the model wrote. Input-shaped
        // content: the tool results fed back to it. Thinking text estimates
        // separately, mirroring the JSONL watcher.
        let out_chars = value
            .get("text")
            .map(content_chars)
            .unwrap_or(0)
            .saturating_add(value.get("codeBlocks").map(content_chars).unwrap_or(0))
            .saturating_add(
                value
                    .pointer("/toolFormerData/rawArgs")
                    .map(content_chars)
                    .unwrap_or(0),
            );
        let in_chars = value
            .pointer("/toolFormerData/result")
            .map(content_chars)
            .unwrap_or(0);
        let think_chars = value
            .get("thinking")
            .map(content_chars)
            .unwrap_or(0)
            .saturating_add(
                value
                    .get("allThinkingBlocks")
                    .map(content_chars)
                    .unwrap_or(0),
            );
        if out_chars == 0 && in_chars == 0 && think_chars == 0 {
            return None; // nothing reported and nothing to estimate from
        }
        tokens_input = in_chars.div_ceil(CHARS_PER_TOKEN);
        tokens_output = out_chars.div_ceil(CHARS_PER_TOKEN);
        tokens_thinking = think_chars.div_ceil(CHARS_PER_TOKEN);
        counts_estimated = true;
    }

    let timestamp_ms = bubble_timestamp(value, observed_ms);

    Some(RawTurn {
        source: Source::Cursor,
        model,
        harness: HARNESS_CURSOR.to_string(),
        tokens_input,
        tokens_output,
        tokens_thinking,
        // Cursor doesn't break out cache tokens.
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
        // decoupled from `prompt_id`, which carries the shared prompt group.
        event_id: Some(format!("{composer_id}:{bubble_id}")),
    })
}

/// Build a SQLite `file:` URI for `path` with the given query string. `?` and
/// `#` are URI-significant; the rest of a filesystem path is fine.
fn sqlite_uri(path: &Path, query: &str) -> String {
    let slashed = path.to_string_lossy().replace('\\', "/");
    let abs = if slashed.starts_with('/') {
        slashed
    } else {
        // Windows drive path (`C:/…`) needs the authority-less leading slash.
        format!("/{slashed}")
    };
    let encoded = abs.replace('?', "%3f").replace('#', "%23");
    format!("file://{encoded}?{query}")
}

/// Open the db read-only + immutable: no locks taken, safe against a running
/// Cursor, but blind to rows still in the WAL.
fn open_immutable(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        sqlite_uri(path, "immutable=1"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

/// Open the db for reading, preferring a live WAL-aware read-only connection
/// (sees the rows a running Cursor hasn't checkpointed yet) and falling back to
/// `immutable=1` (a stale WAL with no live writer can refuse read-only
/// recovery). A short busy timeout rides out Cursor's checkpoint moments.
fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    match Connection::open_with_flags(
        sqlite_uri(path, "mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(conn) => {
            let _ = conn.busy_timeout(Duration::from_millis(250));
            Ok(conn)
        }
        Err(_) => open_immutable(path),
    }
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

/// All `(key, value-bytes)` rows whose key starts with `prefix`, via an
/// index-friendly range scan (`LIKE` loses the primary-key index under
/// SQLite's default case-insensitive collation; this store is ~10⁵ rows).
fn prefix_rows(conn: &Connection, prefix: &str) -> rusqlite::Result<Vec<(String, Vec<u8>)>> {
    let mut stmt =
        conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    // '\u{10FFFF}' sorts after every continuation of the prefix.
    let upper = format!("{prefix}\u{10FFFF}");
    let rows = stmt.query_map([prefix, upper.as_str()], |r| {
        Ok((r.get::<_, String>(0)?, column_bytes(r, 1)?))
    })?;
    rows.collect()
}

/// What one `composerData:<id>` row tells us: where the conversation is rooted
/// and how to read it.
#[derive(Debug, Default)]
struct ComposerMeta {
    /// The project root recorded on the conversation (normalized), if any.
    root_norm: Option<String>,
    /// The `workspaceStorage` hash id recorded on the conversation, if any.
    storage_id: Option<String>,
    /// Conversation order: bubble ids from `fullConversationHeadersOnly`.
    header_order: Vec<String>,
    /// The composer-level model (the user's pick in `selectedModels`, else the
    /// routed `modelName`), resolved.
    model: Option<String>,
}

fn parse_composer_meta(value: &Value) -> ComposerMeta {
    let root_norm = value
        .pointer("/workspaceIdentifier/uri/fsPath")
        .and_then(Value::as_str)
        .map(normalize_for_compare);
    let storage_id = value
        .pointer("/workspaceIdentifier/id")
        .and_then(Value::as_str)
        .filter(|id| *id != "empty-window")
        .map(str::to_string);
    let header_order = value
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|h| h.get("bubbleId").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // `selectedModels[0].modelId` is the user's actual pick; the routed
    // `modelName` can disagree with it, so it is only the fallback.
    let model = value
        .pointer("/modelConfig/selectedModels/0/modelId")
        .and_then(Value::as_str)
        .and_then(model_map::resolve)
        .or_else(|| {
            value
                .pointer("/modelConfig/modelName")
                .and_then(Value::as_str)
                .and_then(model_map::resolve)
        })
        .map(str::to_string);
    ComposerMeta {
        root_norm,
        storage_id,
        header_order,
        model,
    }
}

/// All bubbles for one composer, parsed and grouped by user prompt; returns
/// `(bubble_key, turn)` pairs. Conversation order comes from the composer's
/// header list when present (bubbles share timestamps routinely), else
/// timestamp-then-id; each assistant bubble is stamped with the most recent
/// `type:1` user bubble that precedes it — that shared id is the prompt group
/// grading's `P` counts. The user bubble's own `modelInfo` (where current
/// Cursor records the per-prompt pick) becomes the fallback model for the
/// turns it drives, ahead of the composer-level model.
fn read_bubbles(
    conn: &Connection,
    composer: &str,
    meta: &ComposerMeta,
    observed_ms: i64,
) -> rusqlite::Result<Vec<(String, RawTurn)>> {
    struct Bubble {
        key: String,
        bubble_id: String,
        value: Value,
        ts: i64,
        is_user: bool,
    }
    let mut bubbles = Vec::new();
    for (key, bytes) in prefix_rows(conn, &format!("bubbleId:{composer}:"))? {
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
    // Conversation order: the headers list when the composer has one, else by
    // timestamp with the bubble id as a stable tiebreaker.
    let order: HashMap<&str, usize> = meta
        .header_order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    bubbles.sort_by(|a, b| {
        let pos = |x: &Bubble| order.get(x.bubble_id.as_str()).copied();
        match (pos(a), pos(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            // Headers order wins when both are listed; anything unlisted sorts
            // after by time.
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.ts.cmp(&b.ts).then_with(|| a.bubble_id.cmp(&b.bubble_id)),
        }
    });

    let mut current_user: Option<String> = None;
    let mut prompt_model: Option<String> = None;
    let mut out = Vec::new();
    for bubble in &bubbles {
        if bubble.is_user {
            // A user bubble opens a prompt; it is not itself a turn. Its
            // modelInfo (when present) names the pick for the turns it drives.
            // A pick that's present but UNRESOLVABLE (a model name the matrix
            // doesn't know yet) clears the carryover instead of silently keeping
            // the previous prompt's pick — the turns fall to the composer's
            // modelConfig or degrade to estimated, never to a stale price.
            current_user = Some(bubble.bubble_id.clone());
            let named = bubble
                .value
                .pointer("/modelInfo/modelName")
                .and_then(Value::as_str);
            if named.is_some() {
                prompt_model = bubble_model(&bubble.value);
            }
            continue;
        }
        let fallback = prompt_model.as_deref().or(meta.model.as_deref());
        if let Some(turn) = parse_bubble(
            &bubble.value,
            composer,
            &bubble.bubble_id,
            current_user.as_deref(),
            fallback,
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
    workspace_str: String,
    workspace_norm: String,
    registry: AdapterRegistry,
    seen: HashSet<String>,
    /// composer id → in-scope verdict from its `composerData` row, cached so a
    /// poll only JSON-parses rows it hasn't classified yet (the global store
    /// holds hundreds of conversations; the workspace-stub membership below is
    /// re-read fresh every poll and unions in regardless of this cache).
    scope_cache: HashMap<String, bool>,
    poll: Duration,
}

impl CursorSource {
    /// Build an adapter reading Cursor under `user_dir` for `workspace`.
    pub fn new(user_dir: &Path, workspace: &Path, registry: AdapterRegistry) -> Self {
        Self {
            user_dir: user_dir.to_path_buf(),
            workspace_str: workspace.to_string_lossy().into_owned(),
            workspace_norm: normalize_for_compare(&workspace.to_string_lossy()),
            registry,
            seen: HashSet::new(),
            scope_cache: HashMap::new(),
            poll: DEFAULT_POLL,
        }
    }

    /// Locate, scope, and read the bound workspace's bubbles, classifying the
    /// source's state. Never panics: any I/O or schema problem maps to a state.
    fn collect(&mut self, observed_ms: i64) -> Scan {
        let global_db = self.user_dir.join("globalStorage").join("state.vscdb");
        if !global_db.exists() {
            return Scan::empty(
                AdapterState::NotFound,
                format!("Cursor global storage not found ({})", global_db.display()),
            );
        }
        let conn = match open_reader(&global_db) {
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

        // The workspace's hash dirs across this install — the bound folder and
        // any ancestor/descendant project root Cursor was actually opened in.
        let storages = vscode::related_workspace_storages(
            &self.user_dir.join("workspaceStorage"),
            &self.workspace_norm,
        );
        let storage_ids: HashSet<&str> = storages.iter().map(|(h, _)| h.as_str()).collect();

        // Membership route 2: composer ids the workspace stores themselves name.
        let mut in_scope: HashSet<String> = storages
            .iter()
            .flat_map(|(_, dir)| self.stub_composer_ids(dir))
            .collect();

        // Membership route 1: composerData rows whose recorded project root (or
        // storage id) is the bound workspace / related to it.
        match prefix_rows(&conn, "composerData:") {
            Ok(rows) => {
                for (key, bytes) in rows {
                    let id = key
                        .strip_prefix("composerData:")
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let cached = self.scope_cache.get(&id).copied();
                    let verdict = match cached {
                        Some(v) => v,
                        None => {
                            let verdict = serde_json::from_slice::<Value>(&bytes)
                                .map(|v| {
                                    let meta = parse_composer_meta(&v);
                                    meta.root_norm.as_deref().is_some_and(|root| {
                                        vscode::paths_related(root, &self.workspace_norm)
                                    }) || meta
                                        .storage_id
                                        .as_deref()
                                        .is_some_and(|sid| storage_ids.contains(sid))
                                })
                                .unwrap_or(false);
                            self.scope_cache.insert(id.clone(), verdict);
                            verdict
                        }
                    };
                    if verdict {
                        in_scope.insert(id);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "cursor: composer enumeration failed"),
        }

        if in_scope.is_empty() {
            return Scan::empty(
                AdapterState::Detected,
                "no Cursor conversations for this workspace yet",
            );
        }

        let mut composers: Vec<String> = in_scope.into_iter().collect();
        composers.sort();
        let mut bubbles = Vec::new();
        for composer in &composers {
            // Fresh meta each poll: the headers list and model pick evolve as
            // the conversation grows.
            let meta = read_kv(&conn, "cursorDiskKV", &format!("composerData:{composer}"))
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .map(|v| parse_composer_meta(&v))
                .unwrap_or_default();
            match read_bubbles(&conn, composer, &meta, observed_ms) {
                Ok(mut found) => bubbles.append(&mut found),
                Err(e) => tracing::warn!(composer, error = %e, "cursor: bubble read failed"),
            }
        }
        let detail = format!(
            "{} turn(s) across {} conversation(s)",
            bubbles.len(),
            composers.len()
        );
        Scan {
            state: AdapterState::Detected,
            detail,
            bubbles,
        }
    }

    /// Composer ids named by one workspace hash dir's `composer.composerData`
    /// (legacy `allComposers` or the current migrated stub). Best-effort:
    /// unreadable/missing stores contribute nothing.
    fn stub_composer_ids(&self, ws_dir: &Path) -> Vec<String> {
        let ws_db = ws_dir.join("state.vscdb");
        if !ws_db.exists() {
            return Vec::new();
        }
        let Ok(conn) = open_reader(&ws_db) else {
            return Vec::new();
        };
        if !table_exists(&conn, "ItemTable") {
            return Vec::new();
        }
        read_kv(&conn, "ItemTable", "composer.composerData")
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .map(|v| parse_composer_ids(&v))
            .unwrap_or_default()
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
                    let registry = self.registry.clone();
                    let (returned, turns) = match tokio::task::spawn_blocking(move || {
                        let turns = self.poll_once(observed);
                        (self, turns)
                    })
                    .await
                    {
                        Ok(pair) => pair,
                        Err(_) => {
                            // The blocking scan panicked. Say so on /health —
                            // exiting with the last (likely `detected`) status
                            // would read as capture silently working.
                            tracing::error!("Cursor scan panicked — adapter stopped");
                            registry.set(
                                NAME,
                                AdapterState::Unsupported,
                                "the Cursor scan crashed — capture from Cursor stopped; \
                                 restart the daemon (`promptly down`, then `up`)",
                            );
                            return Ok(());
                        }
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
    fn parses_a_legacy_assistant_bubble_with_real_counts() {
        let v: Value =
            serde_json::from_str(&assistant_bubble("claude-opus-4.8", 100, 200)).unwrap();
        let turn = parse_bubble(&v, "comp-A", "b1", Some("u1"), None, TS)
            .expect("assistant bubble parses");
        assert_eq!(turn.source, Source::Cursor);
        assert_eq!(turn.harness, "cursor");
        assert_eq!(turn.model.as_deref(), Some("claude-opus-4-8")); // mapped
        assert_eq!(turn.tokens_input, 100);
        assert_eq!(turn.tokens_output, 200);
        assert!(!turn.counts_estimated, "real counts stay real");
        // prompt_id carries the shared prompt group; event_id the unique bubble.
        assert_eq!(turn.prompt_id.as_deref(), Some("comp-A:u1"));
        assert_eq!(turn.event_id.as_deref(), Some("comp-A:b1"));
        assert_eq!(turn.session_id.as_deref(), Some("comp-A"));
    }

    #[test]
    fn current_agent_bubble_estimates_from_content_and_iso_timestamp() {
        // The real v3.10 agent shape: zero tokenCount, null modelInfo, ISO
        // createdAt, content spread across thinking/toolFormerData.
        let v: Value = serde_json::from_str(
            r#"{"type":2,"modelInfo":null,"tokenCount":{"inputTokens":0,"outputTokens":0},
                "createdAt":"2026-07-15T03:06:50.399Z","text":"",
                "thinking":{"text":"abcdefgh","signature":""},
                "toolFormerData":{"name":"grep","rawArgs":"{\"q\":1}","result":"abcdefghijkl"},
                "codeBlocks":[]}"#,
        )
        .unwrap();
        let turn =
            parse_bubble(&v, "c", "b", Some("u1"), Some("composer-2-5"), TS).expect("estimable");
        assert!(turn.counts_estimated);
        assert_eq!(turn.tokens_thinking, 2, "8 thinking chars / 4");
        assert_eq!(turn.tokens_output, 2, "8 rawArgs chars / 4");
        assert_eq!(turn.tokens_input, 3, "12 tool-result chars / 4");
        assert_eq!(
            turn.model.as_deref(),
            Some("composer-2-5"),
            "the composer-level model stands in for the null modelInfo"
        );
        // The ISO createdAt string is parsed, not discarded for observed_ms.
        assert_eq!(
            turn.timestamp_ms,
            parse_rfc3339_millis("2026-07-15T03:06:50.399Z").unwrap(),
        );
        assert_ne!(
            turn.timestamp_ms, TS,
            "the observed-time fallback wasn't used"
        );
    }

    #[test]
    fn zero_token_bubble_with_text_estimates_like_before() {
        let v: Value = serde_json::from_str(
            r#"{"type":2,"modelInfo":{"modelName":"gpt-5.5"},"tokenCount":{"inputTokens":0,"outputTokens":0},"text":"abcdefgh"}"#,
        )
        .unwrap();
        let turn = parse_bubble(&v, "c", "b", None, None, TS).expect("estimable");
        assert!(turn.counts_estimated);
        assert_eq!(turn.tokens_output, 2, "8 chars / 4 per token");
        assert_eq!(turn.model.as_deref(), Some("gpt-5-5"));
    }

    #[test]
    fn user_bubbles_and_contentless_zero_token_bubbles_are_skipped() {
        let user: Value = serde_json::from_str(r#"{"type":1,"text":"hello"}"#).unwrap();
        assert!(parse_bubble(&user, "c", "b", None, None, TS).is_none());
        // Assistant, but zero tokens and nothing to estimate from.
        let empty: Value =
            serde_json::from_str(r#"{"type":2,"tokenCount":{"inputTokens":0,"outputTokens":0}}"#)
                .unwrap();
        assert!(parse_bubble(&empty, "c", "b", None, None, TS).is_none());
    }

    #[test]
    fn unknown_model_falls_back_to_unresolved() {
        let v: Value =
            serde_json::from_str(&assistant_bubble("some-future-model", 10, 10)).unwrap();
        let turn = parse_bubble(&v, "c", "b", None, None, TS).unwrap();
        // Unmappable model → None (→ estimated/baseline-floor downstream), counts
        // themselves are still real.
        assert!(turn.model.is_none());
        assert!(!turn.counts_estimated);
    }

    #[test]
    fn parses_composer_ids_from_legacy_and_migrated_stubs() {
        // Legacy: full records under allComposers.
        let legacy: Value = serde_json::from_str(
            r#"{"allComposers":[{"composerId":"a"},{"composerId":"b"}],"selectedComposerId":"a"}"#,
        )
        .unwrap();
        assert_eq!(parse_composer_ids(&legacy), vec!["a", "b"]);
        // Current: the migrated stub keeps only id lists.
        let stub: Value = serde_json::from_str(
            r#"{"selectedComposerIds":["c","a"],"lastFocusedComposerIds":["d"],
                "hasMigratedComposerData":true,"hasMigratedMultipleComposers":true}"#,
        )
        .unwrap();
        assert_eq!(parse_composer_ids(&stub), vec!["a", "c", "d"]);
        assert!(parse_composer_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn composer_meta_reads_root_headers_and_model_pick() {
        // The real v3.10 shape (workspaceIdentifier.uri.fsPath, ISO header
        // createdAt, selectedModels naming the user's actual pick).
        let v: Value = serde_json::from_str(
            r#"{"createdAt":1784084459613,
                "workspaceIdentifier":{"id":"ffc8b554","uri":{"fsPath":"c:\\work\\Challenge","path":"/c:/work/Challenge","scheme":"file"}},
                "fullConversationHeadersOnly":[
                  {"bubbleId":"u1","type":1,"createdAt":"2026-07-15T03:06:48.131Z"},
                  {"bubbleId":"a1","type":2,"createdAt":"2026-07-15T03:06:50.399Z"}],
                "modelConfig":{"modelName":"composer-2.5-fast",
                  "selectedModels":[{"modelId":"claude-4.5-sonnet","parameters":[]}]}}"#,
        )
        .unwrap();
        let meta = parse_composer_meta(&v);
        assert_eq!(
            meta.root_norm.as_deref(),
            Some(&*normalize_for_compare("c:\\work\\Challenge"))
        );
        assert_eq!(meta.storage_id.as_deref(), Some("ffc8b554"));
        assert_eq!(meta.header_order, vec!["u1", "a1"]);
        assert_eq!(
            meta.model.as_deref(),
            Some("claude-sonnet-4-5"),
            "selectedModels[0].modelId (the user's pick) outranks modelName"
        );

        // An empty-window draft: no usable root, no storage id.
        let draft: Value =
            serde_json::from_str(r#"{"workspaceIdentifier":{"id":"empty-window"}}"#).unwrap();
        let meta = parse_composer_meta(&draft);
        assert!(meta.root_norm.is_none() && meta.storage_id.is_none());
    }

    #[test]
    fn sqlite_uri_is_absolute() {
        assert_eq!(
            sqlite_uri(Path::new("/home/me/state.vscdb"), "immutable=1"),
            "file:///home/me/state.vscdb?immutable=1"
        );
        // A Windows drive path gains the authority-less leading slash.
        assert_eq!(
            sqlite_uri(Path::new(r"C:\Users\me\state.vscdb"), "mode=ro"),
            "file:///C:/Users/me/state.vscdb?mode=ro"
        );
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
    fn reader_open_reads_back_committed_rows() {
        let base = std::env::temp_dir().join(format!("promptlyd-cur-imm-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("state.vscdb");
        make_kv_db(&db, "cursorDiskKV", &[("bubbleId:c:1", "{}")]);

        for conn in [open_reader(&db).unwrap(), open_immutable(&db).unwrap()] {
            assert!(table_exists(&conn, "cursorDiskKV"), "table visible");
            let n: i64 = conn
                .query_row("SELECT count(*) FROM cursorDiskKV", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "committed row is visible to the reader");
        }

        std::fs::remove_dir_all(&base).ok();
    }

    /// A composerData row binding `comp` to `root` (the fsPath form the real
    /// store uses), with optional headers.
    fn composer_data(root: &Path, headers: &[(&str, i64)]) -> String {
        let fs_path = root.to_string_lossy().replace('\\', "\\\\");
        let headers_json: Vec<String> = headers
            .iter()
            .map(|(id, ty)| format!(r#"{{"bubbleId":"{id}","type":{ty}}}"#))
            .collect();
        format!(
            r#"{{"workspaceIdentifier":{{"id":"hash-x","uri":{{"fsPath":"{fs_path}"}}}},
                "fullConversationHeadersOnly":[{}],
                "modelConfig":{{"modelName":"composer-2.5"}}}}"#,
            headers_json.join(",")
        )
    }

    #[test]
    fn scopes_by_composer_workspace_identifier_including_a_parent_root() {
        let base = std::env::temp_dir().join(format!("promptlyd-cursor-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Cursor").join("User");
        // The agent recorded the PARENT project root; the daemon binds the
        // level subfolder — the exact shape of the failed real-world run.
        let parent = base.join("challenge");
        let workspace = parent.join("stage-1-02");
        std::fs::create_dir_all(&workspace).unwrap();

        let global = user.join("globalStorage");
        std::fs::create_dir_all(&global).unwrap();
        make_kv_db(
            &global.join("state.vscdb"),
            "cursorDiskKV",
            &[
                ("composerData:comp-A", &composer_data(&parent, &[])),
                (
                    "bubbleId:comp-A:b1",
                    &assistant_bubble("claude-opus-4.8", 100, 50),
                ),
                ("bubbleId:comp-A:b2", r#"{"type":1,"text":"my prompt"}"#),
                // A conversation rooted somewhere unrelated stays out.
                (
                    "composerData:comp-B",
                    &composer_data(&base.join("elsewhere"), &[]),
                ),
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
    fn scopes_by_the_workspace_stub_when_composer_data_lacks_an_identifier() {
        // The June-2026 IDE shape: the conversation's composerData has NO
        // workspaceIdentifier; the only membership trace is the migrated stub
        // in the workspace's own store.
        let base =
            std::env::temp_dir().join(format!("promptlyd-cursor-stub-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let user = base.join("Cursor").join("User");
        let workspace = base.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();

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
                r#"{"selectedComposerIds":["comp-C"],"hasMigratedComposerData":true}"#,
            )],
        );

        let global = user.join("globalStorage");
        std::fs::create_dir_all(&global).unwrap();
        make_kv_db(
            &global.join("state.vscdb"),
            "cursorDiskKV",
            &[
                (
                    "composerData:comp-C",
                    r#"{"modelConfig":{"modelName":"composer-2.5"}}"#,
                ),
                (
                    "bubbleId:comp-C:b1",
                    r#"{"type":2,"tokenCount":{"inputTokens":0,"outputTokens":0},"text":"abcdefgh","createdAt":"2026-07-15T03:06:50.399Z"}"#,
                ),
            ],
        );

        let mut src = CursorSource::new(&user, &workspace, AdapterRegistry::new());
        let turns = src.poll_once(TS);
        assert_eq!(turns.len(), 1, "stub membership brings comp-C in scope");
        assert!(turns[0].counts_estimated);
        assert_eq!(
            turns[0].model.as_deref(),
            Some("composer-2-5"),
            "composer-level model backfills the missing modelInfo"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn headers_order_drives_prompt_grouping_over_shared_timestamps() {
        let base =
            std::env::temp_dir().join(format!("promptlyd-cursor-grp-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("state.vscdb");
        // One user bubble then two assistant bubbles it drove. No timestamps at
        // all — only the headers list carries the conversation order.
        make_kv_db(
            &db,
            "cursorDiskKV",
            &[
                ("bubbleId:comp-A:u1", r#"{"type":1,"text":"solve it"}"#),
                (
                    "bubbleId:comp-A:a1",
                    r#"{"type":2,"modelInfo":{"modelName":"claude-opus-4.8"},"tokenCount":{"inputTokens":10,"outputTokens":20}}"#,
                ),
                (
                    "bubbleId:comp-A:a2",
                    r#"{"type":2,"modelInfo":{"modelName":"claude-opus-4.8"},"tokenCount":{"inputTokens":5,"outputTokens":8}}"#,
                ),
            ],
        );

        let conn = open_reader(&db).unwrap();
        let meta = ComposerMeta {
            header_order: vec!["u1".into(), "a1".into(), "a2".into()],
            ..Default::default()
        };
        let turns = read_bubbles(&conn, "comp-A", &meta, TS).unwrap();
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
    fn user_bubble_model_info_names_the_prompts_model() {
        let base =
            std::env::temp_dir().join(format!("promptlyd-cursor-umod-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("state.vscdb");
        // Current Cursor puts the per-prompt pick on the USER bubble; the
        // assistant bubble's modelInfo is null.
        make_kv_db(
            &db,
            "cursorDiskKV",
            &[
                (
                    "bubbleId:comp-A:u1",
                    r#"{"type":1,"text":"go","modelInfo":{"modelName":"claude-4.5-sonnet"}}"#,
                ),
                (
                    "bubbleId:comp-A:a1",
                    r#"{"type":2,"modelInfo":null,"tokenCount":{"inputTokens":0,"outputTokens":0},"text":"done abcd"}"#,
                ),
            ],
        );

        let conn = open_reader(&db).unwrap();
        let meta = ComposerMeta {
            header_order: vec!["u1".into(), "a1".into()],
            model: Some("composer-2-5".into()), // composer-level pick loses…
            ..Default::default()
        };
        let turns = read_bubbles(&conn, "comp-A", &meta, TS).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].1.model.as_deref(),
            Some("claude-sonnet-4-5"),
            "…to the prompt-level pick on the user bubble"
        );
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
        let mut src = CursorSource::new(&user, &workspace, AdapterRegistry::new());
        assert!(src.poll_once(TS).is_empty());
        assert_eq!(src.registry.snapshot()[0].state, AdapterState::Unsupported);

        std::fs::remove_dir_all(&base).ok();
    }
}
