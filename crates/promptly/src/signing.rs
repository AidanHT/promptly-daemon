//! Ed25519 turn-chain signing (`20`) — the CLI half of the anti-replay anchor.
//!
//! At submit the CLI signs the captured turn chain with the device's Ed25519 key
//! (established at pairing). Each turn is signed over the canonical JSON of
//! `(chain_version, attempt_nonce, turn_index, model, token_counts,
//! prev_signature)` and — from `chain_version` 3 — the turn's `confidence`,
//! `sources`, and `timestamp_ms`, linking turn N to turn N-1's signature. A
//! terminal entry binds the chain to `final_code_hash`, the v2 OTEL↔JSONL
//! `cross_source` corroboration summary, and — from v3 — the `capture_summary`
//! (pause accounting, paste/edit provenance, baseline attestation, nonce origin).
//! Signing all of it means the server can decide the trust tier from what was
//! signed, and none of it can be stripped or forged without breaking a signature.
//! The server (`lib/devices/turn-chain.ts`) verifies the whole chain against the
//! device's stored public key and scores **exactly what was signed**.
//!
//! This is a pinned cross-language contract: the canonical message bytes here MUST
//! match `lib/devices/turn-chain.ts` byte-for-byte. Both sides are cross-checked
//! against the shared `lib/devices/turn-chain-vectors.json` — the test below drives
//! the Rust side, `turn-chain.test.ts` the TypeScript side. Bump
//! [`CHAIN_VERSION`] on any format change so old chains stay verifiable.

use std::collections::BTreeSet;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

/// Canonical chain format version. v3 signs each turn's confidence, source set,
/// and timestamp plus a terminal `capture_summary`, so the server's trust-tier
/// policy reads only signed evidence. v4 adds the session `prompt_count` to that
/// summary — grading's `P` — so the server scores the daemon's real prompt tally
/// instead of approximating it with the turn count (an agentic run drives many
/// turns off one prompt). The server still verifies v1–v3 chains, so a web
/// redeploy that accepts v4 must precede this daemon release. Bump on any further
/// serialization change.
pub const CHAIN_VERSION: u32 = 4;

/// Per-turn token counts (the signed quantities `13` scores).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub thinking: u64,
    pub cache: u64,
}

/// The OTEL↔JSONL cross-source corroboration summary signed into a v2+ terminal
/// entry (`17`/`25`): how many captured turns disagreed across the two independent
/// telemetry sources, and the union of fields they disagreed on. Mirrors
/// `ChainCrossSource` in `turn-chain.ts`; serializes snake_case (`disagree_turns`,
/// `disagree_fields`) — the keys the server's `parseSignedChain` reads. Signing it
/// into the chain means a stripped or zeroed summary breaks the terminal signature
/// (→ `suspect`) rather than silently hiding disagreements.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CrossSource {
    pub disagree_turns: u32,
    pub disagree_fields: Vec<String>,
}

/// The session-scoped capture summary signed into a v3 terminal entry (`20`): the
/// integrity signals the server's trust policy reads to decide the verified tier.
/// Mirrors `CaptureSummary` in `turn-chain.ts`; serializes snake_case. Signing it
/// means a forked client can't lie about pauses, provenance, the baseline
/// attestation, or the nonce origin without breaking the terminal signature.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CaptureSummary {
    pub baseline_attested: bool,
    pub baseline_reset_count: u32,
    pub bulk_paste_events: u32,
    pub ignore_changed: bool,
    /// "server" (server-issued nonce) or "local" (offline start).
    pub nonce_origin: String,
    pub pause_count: u32,
    pub paused_ms_total: i64,
    /// Distinct user prompts this session — grading's `P`. Signed from v4 (before
    /// then the server approximated `P` with the turn count, penalizing agentic
    /// runs that drive many turns off a single prompt). The server clamps it to
    /// `[1, turns]`, so a forked client can't sign its way below one prompt.
    pub prompt_count: u32,
    pub signed_at_ms: i64,
    pub started_at_ms: i64,
    pub untracked_edit_windows: u32,
}

/// The v3-only signed fields for a single turn: its confidence tier, source set,
/// and capture-clock timestamp. Borrowed so the canonical builder allocates nothing.
#[derive(Debug, Clone, Copy)]
pub struct TurnV3<'a> {
    pub confidence: &'a str,
    pub sources: &'a [String],
    pub timestamp_ms: i64,
}

/// One turn to sign: its index, the resolved model, the token counts, and (from
/// v3) its confidence, source set, and timestamp. The `model` is resolved at
/// capture time — resolution happens before signing so the signed value is
/// canonical and later resolution changes never retroactively invalidate a chain.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub turn_index: u32,
    pub model: String,
    pub token_counts: TokenCounts,
    pub confidence: String,
    pub sources: Vec<String>,
    pub timestamp_ms: i64,
}

/// One signed turn on the wire (snake_case, matching `parseSignedChain`).
#[derive(Debug, Clone, Serialize)]
pub struct SignedTurnWire {
    pub turn_index: u32,
    pub model: String,
    pub token_counts: TokenCounts,
    pub confidence: String,
    /// Sorted, de-duplicated source names (matches the signed canonical order).
    pub sources: Vec<String>,
    pub timestamp_ms: i64,
    /// Base64 Ed25519 signature over this turn's canonical message.
    pub signature: String,
}

/// The terminal entry binding the chain to the submitted artifact.
#[derive(Debug, Clone, Serialize)]
pub struct SignedFinalWire {
    pub final_code_hash: String,
    /// The signed corroboration summary (v2+). Hashed into the canonical final
    /// message, so it can't be stripped or altered without breaking `signature`.
    pub cross_source: CrossSource,
    /// The signed capture summary (v3+). Hashed into the canonical final message.
    pub capture_summary: CaptureSummary,
    pub signature: String,
}

/// The signed turn chain the daemon submission uploads — serializes to the exact
/// snake_case JSON the server's `parseSignedChain` consumes (with the `final` key).
#[derive(Debug, Clone, Serialize)]
pub struct SignedChainWire {
    pub chain_version: u32,
    pub attempt_nonce: String,
    pub turns: Vec<SignedTurnWire>,
    #[serde(rename = "final")]
    pub final_entry: SignedFinalWire,
}

/// JSON-encode a string exactly as `JSON.stringify` does (the values we sign —
/// uuids, model ids, hex, base64 — need no special escaping, but encode generally
/// so the contract holds for any input). `serde_json` matches `JSON.stringify`'s
/// escaping (both leave `/` and base64 `+`/`=` literal).
fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes")
}

/// Canonical token-counts object — keys sorted `cache, input, output, thinking`,
/// no whitespace.
fn canonical_counts(c: &TokenCounts) -> String {
    format!(
        "{{\"cache\":{},\"input\":{},\"output\":{},\"thinking\":{}}}",
        c.cache, c.input, c.output, c.thinking
    )
}

/// Canonical source array — sorted and de-duplicated, JSON string array, no
/// whitespace. A `BTreeSet` gives the same sorted-unique order `turn-chain.ts`
/// produces with `[...new Set(sources)].sort()` over the ASCII source names.
fn canonical_sources(sources: &[String]) -> String {
    let sorted: BTreeSet<&str> = sources.iter().map(String::as_str).collect();
    let joined = sorted
        .iter()
        .map(|s| json_string(s))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// The sorted, de-duplicated source names emitted on the wire (matches the signed
/// canonical order; the server re-sorts anyway, so this is for tidiness).
fn sorted_sources(sources: &[String]) -> Vec<String> {
    sources
        .iter()
        .cloned()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// Canonical cross-source object — keys sorted `disagree_fields, disagree_turns`,
/// the field list in capture order, no whitespace. Mirrors `canonicalCrossSource`
/// in `turn-chain.ts` (which treats a null summary as empty, identical bytes).
fn canonical_cross_source(cs: &CrossSource) -> String {
    let fields = cs
        .disagree_fields
        .iter()
        .map(|f| json_string(f))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"disagree_fields\":[{}],\"disagree_turns\":{}}}",
        fields, cs.disagree_turns
    )
}

/// Canonical capture-summary object — keys sorted, booleans lowercase, no
/// whitespace. Mirrors `canonicalCaptureSummary` in `turn-chain.ts`. `prompt_count`
/// joins the signed keys from v4 (in sorted position, between `paused_ms_total`
/// and `signed_at_ms`); a v3 summary omits it, byte-identical to its legacy format.
fn canonical_capture_summary(version: u32, s: &CaptureSummary) -> String {
    let prompt_count_field = if version >= 4 {
        format!("\"prompt_count\":{},", s.prompt_count)
    } else {
        String::new()
    };
    format!(
        "{{\"baseline_attested\":{},\"baseline_reset_count\":{},\"bulk_paste_events\":{},\"ignore_changed\":{},\"nonce_origin\":{},\"pause_count\":{},\"paused_ms_total\":{},{}\"signed_at_ms\":{},\"started_at_ms\":{},\"untracked_edit_windows\":{}}}",
        s.baseline_attested,
        s.baseline_reset_count,
        s.bulk_paste_events,
        s.ignore_changed,
        json_string(&s.nonce_origin),
        s.pause_count,
        s.paused_ms_total,
        prompt_count_field,
        s.signed_at_ms,
        s.started_at_ms,
        s.untracked_edit_windows,
    )
}

/// The exact bytes signed for a turn — sorted keys, no whitespace. `version` is
/// threaded in (rather than read from [`CHAIN_VERSION`]) so the verifier and the
/// vector tests can reconstruct any format. From v3 the message also carries
/// `confidence`, `sources`, and `timestamp_ms`; older versions omit them,
/// byte-identical to their legacy format. `v3` is required when `version >= 3`.
pub fn canonical_turn_message(
    version: u32,
    attempt_nonce: &str,
    turn_index: u32,
    model: &str,
    counts: &TokenCounts,
    prev_signature: Option<&str>,
    v3: Option<TurnV3>,
) -> String {
    let prev = match prev_signature {
        Some(sig) => json_string(sig),
        None => "null".to_string(),
    };
    if version >= 3 {
        let v3 = v3.expect("a v3 turn message requires confidence, sources, and timestamp_ms");
        return format!(
            "{{\"attempt_nonce\":{},\"chain_version\":{},\"confidence\":{},\"model\":{},\"prev_signature\":{},\"sources\":{},\"timestamp_ms\":{},\"token_counts\":{},\"turn_index\":{}}}",
            json_string(attempt_nonce),
            version,
            json_string(v3.confidence),
            json_string(model),
            prev,
            canonical_sources(v3.sources),
            v3.timestamp_ms,
            canonical_counts(counts),
            turn_index,
        );
    }
    format!(
        "{{\"attempt_nonce\":{},\"chain_version\":{},\"model\":{},\"prev_signature\":{},\"token_counts\":{},\"turn_index\":{}}}",
        json_string(attempt_nonce),
        version,
        json_string(model),
        prev,
        canonical_counts(counts),
        turn_index,
    )
}

/// The exact bytes signed for the terminal entry binding `final_code_hash`. From
/// v2 it carries the `cross_source` summary; from v3 it also carries the
/// `capture_summary` (sorted before `chain_version`). Each field is version-gated
/// so an older message omits the newer keys, byte-identical to its legacy format.
/// `capture_summary` is required when `version >= 3`.
pub fn canonical_final_message(
    version: u32,
    attempt_nonce: &str,
    final_code_hash: &str,
    cross_source: &CrossSource,
    capture_summary: Option<&CaptureSummary>,
    prev_signature: Option<&str>,
    turn_count: usize,
) -> String {
    let prev = match prev_signature {
        Some(sig) => json_string(sig),
        None => "null".to_string(),
    };
    let capture_summary_field = if version >= 3 {
        let s = capture_summary.expect("a v3 final message requires a capture_summary");
        format!(
            "\"capture_summary\":{},",
            canonical_capture_summary(version, s)
        )
    } else {
        String::new()
    };
    let cross_source_field = if version >= 2 {
        format!("\"cross_source\":{},", canonical_cross_source(cross_source))
    } else {
        String::new()
    };
    format!(
        "{{\"attempt_nonce\":{},{}\"chain_version\":{},{}\"final_code_hash\":{},\"prev_signature\":{},\"turn_count\":{}}}",
        json_string(attempt_nonce),
        capture_summary_field,
        version,
        cross_source_field,
        json_string(final_code_hash),
        prev,
        turn_count,
    )
}

/// Rebuild the device signing key from its 32-byte Ed25519 seed (stored in the
/// credential store; generated at pairing).
pub fn signing_key_from_seed(seed: &[u8; 32]) -> SigningKey {
    SigningKey::from_bytes(seed)
}

/// The base64 raw 32-byte public key uploaded to `devices.public_key` at pairing.
pub fn public_key_base64(key: &SigningKey) -> String {
    STANDARD.encode(key.verifying_key().to_bytes())
}

/// Sign a message and return its base64 Ed25519 signature.
fn sign_base64(key: &SigningKey, message: &str) -> String {
    STANDARD.encode(key.sign(message.as_bytes()).to_bytes())
}

/// Sign the full turn chain at [`CHAIN_VERSION`]: each turn over its canonical
/// message (chained to the previous signature), then the terminal entry binding
/// `final_code_hash`, the `cross_source` summary, and the `capture_summary`.
/// Returns the wire chain ready to upload.
pub fn sign_chain(
    key: &SigningKey,
    attempt_nonce: &str,
    turns: &[TurnInput],
    cross_source: &CrossSource,
    capture_summary: &CaptureSummary,
    final_code_hash: &str,
) -> SignedChainWire {
    let mut signed_turns = Vec::with_capacity(turns.len());
    let mut prev: Option<String> = None;
    for turn in turns {
        let v3 = TurnV3 {
            confidence: &turn.confidence,
            sources: &turn.sources,
            timestamp_ms: turn.timestamp_ms,
        };
        let message = canonical_turn_message(
            CHAIN_VERSION,
            attempt_nonce,
            turn.turn_index,
            &turn.model,
            &turn.token_counts,
            prev.as_deref(),
            Some(v3),
        );
        let signature = sign_base64(key, &message);
        signed_turns.push(SignedTurnWire {
            turn_index: turn.turn_index,
            model: turn.model.clone(),
            token_counts: turn.token_counts,
            confidence: turn.confidence.clone(),
            sources: sorted_sources(&turn.sources),
            timestamp_ms: turn.timestamp_ms,
            signature: signature.clone(),
        });
        prev = Some(signature);
    }
    let final_message = canonical_final_message(
        CHAIN_VERSION,
        attempt_nonce,
        final_code_hash,
        cross_source,
        Some(capture_summary),
        prev.as_deref(),
        turns.len(),
    );
    SignedChainWire {
        chain_version: CHAIN_VERSION,
        attempt_nonce: attempt_nonce.to_string(),
        turns: signed_turns,
        final_entry: SignedFinalWire {
            final_code_hash: final_code_hash.to_string(),
            cross_source: cross_source.clone(),
            capture_summary: capture_summary.clone(),
            signature: sign_base64(key, &final_message),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // The shared cross-language vectors; `turn-chain.ts` reproduces the same
    // canonical strings + signatures (the contract this test pins on the Rust side).
    const VECTORS: &str = include_str!("../../../vendor/turn-chain-vectors.json");

    fn seed_from(v: &Value) -> [u8; 32] {
        hex::decode(v["seed_hex"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn counts_from(c: &Value) -> TokenCounts {
        TokenCounts {
            input: c["input"].as_u64().unwrap(),
            output: c["output"].as_u64().unwrap(),
            thinking: c["thinking"].as_u64().unwrap(),
            cache: c["cache"].as_u64().unwrap(),
        }
    }

    fn cross_source_from(c: &Value) -> CrossSource {
        CrossSource {
            disagree_turns: c["disagree_turns"].as_u64().unwrap() as u32,
            disagree_fields: c["disagree_fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_string())
                .collect(),
        }
    }

    fn strings_from(v: &Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    }

    fn capture_summary_from(c: &Value) -> CaptureSummary {
        CaptureSummary {
            baseline_attested: c["baseline_attested"].as_bool().unwrap(),
            baseline_reset_count: c["baseline_reset_count"].as_u64().unwrap() as u32,
            bulk_paste_events: c["bulk_paste_events"].as_u64().unwrap() as u32,
            ignore_changed: c["ignore_changed"].as_bool().unwrap(),
            nonce_origin: c["nonce_origin"].as_str().unwrap().to_string(),
            pause_count: c["pause_count"].as_u64().unwrap() as u32,
            paused_ms_total: c["paused_ms_total"].as_i64().unwrap(),
            // Absent on the v1/v2/v3 vectors (unsigned there); present from v4.
            prompt_count: c["prompt_count"].as_u64().unwrap_or(0) as u32,
            signed_at_ms: c["signed_at_ms"].as_i64().unwrap(),
            started_at_ms: c["started_at_ms"].as_i64().unwrap(),
            untracked_edit_windows: c["untracked_edit_windows"].as_u64().unwrap() as u32,
        }
    }

    /// Drive every canonical string + signature in a vector object (top-level v1,
    /// or the nested `v2`/`v3`) against the shared fixture — the cross-language
    /// contract. `capture_summary` is required for v3.
    fn assert_vectors(
        v: &Value,
        version: u32,
        cross_source: &CrossSource,
        capture_summary: Option<&CaptureSummary>,
    ) {
        let key = signing_key_from_seed(&seed_from(v));
        assert_eq!(
            public_key_base64(&key),
            v["public_key_b64"].as_str().unwrap()
        );

        let nonce = v["attempt_nonce"].as_str().unwrap();
        let mut prev: Option<String> = None;
        // Keep the parsed source vectors alive for the borrowed TurnV3.
        let mut source_store: Vec<Vec<String>> = Vec::new();
        for tv in v["turns"].as_array().unwrap() {
            if version >= 3 {
                source_store.push(strings_from(&tv["sources"]));
            }
        }
        for (i, tv) in v["turns"].as_array().unwrap().iter().enumerate() {
            let idx = tv["turn_index"].as_u64().unwrap() as u32;
            let model = tv["model"].as_str().unwrap();
            let counts = counts_from(&tv["token_counts"]);
            let v3 = if version >= 3 {
                Some(TurnV3 {
                    confidence: tv["confidence"].as_str().unwrap(),
                    sources: &source_store[i],
                    timestamp_ms: tv["timestamp_ms"].as_i64().unwrap(),
                })
            } else {
                None
            };
            let message =
                canonical_turn_message(version, nonce, idx, model, &counts, prev.as_deref(), v3);
            assert_eq!(
                message,
                tv["canonical"].as_str().unwrap(),
                "v{version} turn {idx} canonical bytes"
            );
            let signature = sign_base64(&key, &message);
            assert_eq!(
                signature,
                tv["signature"].as_str().unwrap(),
                "v{version} turn {idx} signature"
            );
            prev = Some(signature);
        }

        let turn_count = v["turns"].as_array().unwrap().len();
        let final_message = canonical_final_message(
            version,
            nonce,
            v["final_code_hash"].as_str().unwrap(),
            cross_source,
            capture_summary,
            prev.as_deref(),
            turn_count,
        );
        assert_eq!(
            final_message,
            v["final"]["canonical"].as_str().unwrap(),
            "v{version} final canonical bytes"
        );
        assert_eq!(
            sign_base64(&key, &final_message),
            v["final"]["signature"].as_str().unwrap(),
            "v{version} final signature",
        );
    }

    #[test]
    fn v1_canonical_messages_and_signatures_match_the_shared_vectors() {
        // The top-level vectors are the v1 contract (still verified for legacy
        // daemons mid-rollout); cross_source + capture_summary are ignored at v1.
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        assert_vectors(&v, 1, &CrossSource::default(), None);
    }

    #[test]
    fn v2_canonical_messages_and_signatures_match_the_shared_vectors() {
        // The nested `v2` object signs the cross_source summary into the terminal
        // entry; capture_summary is ignored at v2.
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let v2 = &v["v2"];
        assert_vectors(v2, 2, &cross_source_from(&v2["cross_source"]), None);
    }

    #[test]
    fn v3_canonical_messages_and_signatures_match_the_shared_vectors() {
        // The nested `v3` object signs the per-turn confidence/sources/timestamp
        // and the terminal capture_summary — the format this daemon now produces.
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let v3 = &v["v3"];
        let summary = capture_summary_from(&v3["capture_summary"]);
        assert_vectors(
            v3,
            3,
            &cross_source_from(&v3["cross_source"]),
            Some(&summary),
        );
    }

    #[test]
    fn v4_canonical_messages_and_signatures_match_the_shared_vectors() {
        // The nested `v4` object adds the signed session prompt_count to the
        // capture_summary — the format this daemon now produces.
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let v4 = &v["v4"];
        let summary = capture_summary_from(&v4["capture_summary"]);
        assert_eq!(summary.prompt_count, 2);
        assert_vectors(
            v4,
            4,
            &cross_source_from(&v4["cross_source"]),
            Some(&summary),
        );
    }

    #[test]
    fn sign_chain_reproduces_the_v4_vector_chain_and_wire_shape() {
        // sign_chain always signs at CHAIN_VERSION (now 4), so it must reproduce the
        // v4 vectors and emit the signed provenance + capture_summary (incl. the
        // prompt_count) on the wire.
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let v4 = &v["v4"];
        let key = signing_key_from_seed(&seed_from(v4));
        let nonce = v4["attempt_nonce"].as_str().unwrap();
        let cross = cross_source_from(&v4["cross_source"]);
        let summary = capture_summary_from(&v4["capture_summary"]);
        let turns: Vec<TurnInput> = v4["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tv| TurnInput {
                turn_index: tv["turn_index"].as_u64().unwrap() as u32,
                model: tv["model"].as_str().unwrap().to_string(),
                token_counts: counts_from(&tv["token_counts"]),
                confidence: tv["confidence"].as_str().unwrap().to_string(),
                sources: strings_from(&tv["sources"]),
                timestamp_ms: tv["timestamp_ms"].as_i64().unwrap(),
            })
            .collect();

        let chain = sign_chain(
            &key,
            nonce,
            &turns,
            &cross,
            &summary,
            v4["final_code_hash"].as_str().unwrap(),
        );
        assert_eq!(
            chain.turns[0].signature,
            v4["turns"][0]["signature"].as_str().unwrap()
        );
        assert_eq!(
            chain.turns[1].signature,
            v4["turns"][1]["signature"].as_str().unwrap()
        );
        assert_eq!(
            chain.final_entry.signature,
            v4["final"]["signature"].as_str().unwrap()
        );

        // It serializes to the snake_case wire JSON the server's parseSignedChain
        // reads — the `final` key (not Rust's `final_entry`), the signed per-turn
        // provenance, and the signed cross_source + capture_summary (with the
        // prompt_count) alongside the terminal signature.
        let wire = serde_json::to_value(&chain).unwrap();
        assert_eq!(wire["chain_version"], 4);
        assert_eq!(wire["attempt_nonce"].as_str().unwrap(), nonce);
        assert_eq!(wire["turns"][0]["turn_index"], 0);
        assert_eq!(wire["turns"][0]["token_counts"]["input"], 1200);
        assert_eq!(wire["turns"][0]["confidence"], "otel");
        assert_eq!(wire["turns"][0]["sources"][0], "jsonl");
        assert_eq!(wire["turns"][0]["sources"][1], "otel");
        assert!(wire["turns"][0]["timestamp_ms"].is_number());
        assert!(wire["final"]["final_code_hash"].is_string());
        assert_eq!(wire["final"]["cross_source"]["disagree_turns"], 1);
        assert_eq!(wire["final"]["capture_summary"]["nonce_origin"], "server");
        assert_eq!(wire["final"]["capture_summary"]["baseline_attested"], true);
        assert_eq!(wire["final"]["capture_summary"]["prompt_count"], 2);
        assert!(wire.get("final_entry").is_none(), "the wire key is `final`");
    }

    #[test]
    fn an_empty_chain_still_binds_the_final_code_hash() {
        // A session with no captured turns still produces a verifiable terminal
        // entry (prev_signature null, turn_count 0) at the current chain version.
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = signing_key_from_seed(&seed_from(&v));
        let cross = CrossSource::default();
        let summary = CaptureSummary {
            nonce_origin: "server".to_string(),
            ..CaptureSummary::default()
        };
        let chain = sign_chain(&key, "nonce-0", &[], &cross, &summary, "deadbeef");
        assert!(chain.turns.is_empty());
        let expected = canonical_final_message(
            CHAIN_VERSION,
            "nonce-0",
            "deadbeef",
            &cross,
            Some(&summary),
            None,
            0,
        );
        assert_eq!(chain.final_entry.signature, sign_base64(&key, &expected));
    }
}
