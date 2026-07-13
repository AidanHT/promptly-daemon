//! Fixture replay of Claude Code's real projects-JSONL block-line shape.
//!
//! Claude Code writes ONE LINE PER CONTENT BLOCK: a single physical assistant
//! turn lands as 2-3 `type:"assistant"` lines seconds apart, each with its own
//! `timestamp` but the SAME `message.id`/`requestId` and the IDENTICAL
//! whole-message `usage` repeated on every line. This fixture is a sanitized
//! replica of a real captured session (structure, ids-per-line pattern, usage
//! numbers, and timestamps preserved; content rewritten): 20 assistant lines
//! spanning 8 physical turns. Ground truth for that session: **8 turns,
//! 4 241 output tokens** — daemon v0.1.9 counted every block-line as a turn and
//! reported ~3× that. This test replays the fixture through the parse → dedup →
//! correlate → attribute pipeline and pins the real totals.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use promptlyd::engine::{Engine, EngineInit};
use promptlyd::scoping::{NonceOrigin, SessionMarker, SESSION_MARKER_VERSION};
use promptlyd::sources::jsonl::parse_complete_lines;
use tokio::sync::mpsc;

const WORKSPACE: &str = "/replay/ws";
const SESSION_ID: &str = "88095499-0000-4000-8000-fixture00001";

/// One physical assistant turn of the fixture: its stable ids, the
/// whole-message usage every block-line repeats, and one (block, timestamp)
/// pair per JSONL line.
struct Message {
    id: &'static str,
    request_id: &'static str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    /// `(content-block type, line timestamp)` — one JSONL line each.
    lines: &'static [(&'static str, &'static str)],
}

/// The 8-message / 20-line skeleton of the real session (usage numbers and
/// timestamps as captured; ids and content sanitized).
const MESSAGES: &[Message] = &[
    Message {
        id: "msg_fixture01",
        request_id: "req_fixture01",
        input: 10,
        output: 252,
        cache_read: 23_813,
        cache_creation: 8_381,
        lines: &[
            ("thinking", "2026-07-04T21:04:16.622Z"),
            ("text", "2026-07-04T21:04:16.832Z"),
            ("tool_use", "2026-07-04T21:04:17.074Z"),
        ],
    },
    Message {
        id: "msg_fixture02",
        request_id: "req_fixture02",
        input: 8,
        output: 136,
        cache_read: 32_194,
        cache_creation: 677,
        lines: &[
            ("thinking", "2026-07-04T21:04:18.784Z"),
            ("tool_use", "2026-07-04T21:04:18.994Z"),
        ],
    },
    Message {
        id: "msg_fixture03",
        request_id: "req_fixture03",
        input: 8,
        output: 833,
        cache_read: 32_871,
        cache_creation: 963,
        lines: &[
            ("thinking", "2026-07-04T21:04:22.749Z"),
            ("text", "2026-07-04T21:04:24.203Z"),
            ("tool_use", "2026-07-04T21:04:25.469Z"),
        ],
    },
    Message {
        id: "msg_fixture04",
        request_id: "req_fixture04",
        input: 8,
        output: 162,
        cache_read: 33_834,
        cache_creation: 961,
        lines: &[
            ("thinking", "2026-07-04T21:04:26.878Z"),
            ("text", "2026-07-04T21:04:27.042Z"),
            ("tool_use", "2026-07-04T21:04:27.445Z"),
        ],
    },
    Message {
        id: "msg_fixture05",
        request_id: "req_fixture05",
        input: 8,
        output: 306,
        cache_read: 34_795,
        cache_creation: 265,
        lines: &[
            ("thinking", "2026-07-04T21:04:34.436Z"),
            ("text", "2026-07-04T21:04:34.633Z"),
            ("tool_use", "2026-07-04T21:04:34.946Z"),
        ],
    },
    Message {
        id: "msg_fixture06",
        request_id: "req_fixture06",
        input: 8,
        output: 122,
        cache_read: 35_060,
        cache_creation: 356,
        lines: &[
            ("thinking", "2026-07-04T21:04:36.410Z"),
            ("tool_use", "2026-07-04T21:04:36.484Z"),
        ],
    },
    Message {
        id: "msg_fixture07",
        request_id: "req_fixture07",
        input: 8,
        output: 118,
        cache_read: 35_416,
        cache_creation: 275,
        lines: &[
            ("thinking", "2026-07-04T21:04:38.019Z"),
            ("tool_use", "2026-07-04T21:04:38.063Z"),
        ],
    },
    Message {
        id: "msg_fixture08",
        request_id: "req_fixture08",
        input: 8,
        output: 2_312,
        cache_read: 35_691,
        cache_creation: 731,
        lines: &[
            ("thinking", "2026-07-04T21:04:52.401Z"),
            ("text", "2026-07-04T21:04:54.908Z"),
        ],
    },
];

fn content_block(kind: &str) -> String {
    match kind {
        "thinking" => r#"{"type":"thinking","thinking":"weigh the eviction options"}"#.to_string(),
        "text" => r#"{"type":"text","text":"Applying the fix now."}"#.to_string(),
        _ => r#"{"type":"tool_use","id":"toolu_fixture","name":"Edit","input":{}}"#.to_string(),
    }
}

/// Render the fixture exactly as Claude Code writes it: one JSON object per
/// line, interleaved with the non-assistant record types a real session file
/// carries (which the parser must ignore).
pub fn fixture_jsonl(cwd: &str, session_id: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{{\"type\":\"file-history-snapshot\",\"sessionId\":\"{session_id}\"}}\n"
    ));
    out.push_str(&format!(
        "{{\"type\":\"user\",\"sessionId\":\"{session_id}\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"fix the eviction bug\"}}}}\n"
    ));
    for message in MESSAGES {
        for (kind, timestamp) in message.lines {
            out.push_str(&format!(
                "{{\"type\":\"assistant\",\"sessionId\":\"{session_id}\",\"cwd\":\"{cwd}\",\"requestId\":\"{req}\",\"timestamp\":\"{timestamp}\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-haiku-4-5\",\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\"cache_read_input_tokens\":{cr},\"cache_creation_input_tokens\":{cc}}},\"content\":[{block}]}}}}\n",
                req = message.request_id,
                id = message.id,
                input = message.input,
                output = message.output,
                cr = message.cache_read,
                cc = message.cache_creation,
                block = content_block(kind),
            ));
        }
        out.push_str(&format!(
            "{{\"type\":\"attachment\",\"sessionId\":\"{session_id}\"}}\n"
        ));
    }
    out
}

fn replay_marker() -> SessionMarker {
    SessionMarker {
        version: SESSION_MARKER_VERSION,
        session_id: "sess-replay".into(),
        workspace: PathBuf::from(WORKSPACE),
        level_id: "lvl-replay".into(),
        slug: "stage-1-01".into(),
        started_at_ms: 0,
        stopped_at_ms: None,
        attempt_nonce: "nonce-replay".into(),
        nonce_origin: NonceOrigin::Local,
        file_allowlist: Vec::new(),
        code_reset_count: 0,
        bootstrap: None,
        // JSONL-only: no OTEL counterpart is expected during the replay.
        otlp_token: None,
        baseline_attested: false,
    }
}

#[test]
fn replaying_the_20_line_fixture_stores_8_turns_and_4241_output_tokens() {
    let jsonl = fixture_jsonl(WORKSPACE, SESSION_ID);
    let (raws, consumed) = parse_complete_lines(jsonl.as_bytes(), 0, &mut None);
    assert_eq!(consumed, jsonl.len(), "every line is newline-terminated");
    assert_eq!(raws.len(), 20, "one raw observation per assistant line");

    let (_tx, rx) = mpsc::channel(64);
    let (mut engine, shared) = Engine::new(
        rx,
        EngineInit {
            binding: Some(replay_marker()),
            checkpoint_path: std::env::temp_dir().join(format!(
                "promptlyd-replay-{}-checkpoint.json",
                std::process::id()
            )),
            jsonl_offsets: Arc::new(Mutex::new(HashMap::new())),
            restored_turns: Vec::new(),
            restored_seen: Vec::new(),
        },
    );

    let mut now = 0;
    for raw in raws {
        assert_eq!(raw.session_id.as_deref(), Some(SESSION_ID));
        engine.process_raw(raw, now);
        now += 10;
    }
    engine.flush(now);

    // Ground truth from the real session this fixture replicates: 8 physical
    // turns and 4 241 output tokens. v0.1.9 stored 20+ turns / ~3× the tokens.
    let totals = shared.totals();
    assert_eq!(totals.turns, 8, "8 messages, not 20 block-lines");
    assert_eq!(
        totals.tokens_output, 4_241,
        "usage counted once per message"
    );
    assert_eq!(totals.tokens_input, 66);
    assert_eq!(totals.tokens_cache, 276_283, "cache_read + cache_creation");
    assert!(
        totals.tokens_thinking > 0,
        "the first (thinking) block-line of each message is the one kept"
    );

    // The stored turns carry the distinct per-message identities.
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.len(), 8);
    let distinct: std::collections::HashSet<_> =
        snapshot.iter().map(|t| t.turn_id.as_str()).collect();
    assert_eq!(distinct.len(), 8, "eight distinct turn ids");

    // Every turn groups under the single user prompt that opened the session, so
    // grading's P is 1 (not 8). The fixture's user line carries no uuid, so the id
    // is a deterministic fingerprint of that line.
    let prompts: std::collections::HashSet<_> =
        snapshot.iter().map(|t| t.prompt_id.clone()).collect();
    assert_eq!(prompts.len(), 1, "all eight turns share one prompt");
    assert!(
        snapshot[0].prompt_id.is_some(),
        "the prompt id is stamped from the opening user-prompt line"
    );
}
