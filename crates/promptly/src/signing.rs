//! Ed25519 turn-chain signing (`20`) — the CLI half of the anti-replay anchor.
//!
//! At submit the CLI signs the captured turn chain with the device's Ed25519 key
//! (established at pairing). Each turn is signed over the canonical JSON of
//! `(chain_version, attempt_nonce, turn_index, model, token_counts,
//! prev_signature)`, linking turn N to turn N-1's signature; a terminal entry
//! binds the chain to `final_code_hash`, tying the captured session to the exact
//! submitted artifact. The server (`lib/devices/turn-chain.ts`) verifies the whole
//! chain against the device's stored public key and scores **exactly what was
//! signed**.
//!
//! This is a pinned cross-language contract: the canonical message bytes here MUST
//! match `lib/devices/turn-chain.ts` byte-for-byte. Both sides are cross-checked
//! against the shared `lib/devices/turn-chain-vectors.json` — the test below drives
//! the Rust side, `turn-chain.test.ts` the TypeScript side. Bump
//! [`CHAIN_VERSION`] on any format change so old chains stay verifiable.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

/// Canonical chain format version. Bump on any serialization change.
pub const CHAIN_VERSION: u32 = 1;

/// Per-turn token counts (the signed quantities `13` scores).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub thinking: u64,
    pub cache: u64,
}

/// One turn to sign: its index, the resolved model, and the token counts. The
/// `model` is the model resolved at capture time — resolution happens before
/// signing so the signed value is canonical and later resolution changes never
/// retroactively invalidate a chain.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub turn_index: u32,
    pub model: String,
    pub token_counts: TokenCounts,
}

/// One signed turn on the wire (snake_case, matching `parseSignedChain`).
#[derive(Debug, Clone, Serialize)]
pub struct SignedTurnWire {
    pub turn_index: u32,
    pub model: String,
    pub token_counts: TokenCounts,
    /// Base64 Ed25519 signature over this turn's canonical message.
    pub signature: String,
}

/// The terminal entry binding the chain to the submitted artifact.
#[derive(Debug, Clone, Serialize)]
pub struct SignedFinalWire {
    pub final_code_hash: String,
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

/// The exact bytes signed for a turn — sorted keys, no whitespace.
pub fn canonical_turn_message(
    attempt_nonce: &str,
    turn_index: u32,
    model: &str,
    counts: &TokenCounts,
    prev_signature: Option<&str>,
) -> String {
    let prev = match prev_signature {
        Some(sig) => json_string(sig),
        None => "null".to_string(),
    };
    format!(
        "{{\"attempt_nonce\":{},\"chain_version\":{},\"model\":{},\"prev_signature\":{},\"token_counts\":{},\"turn_index\":{}}}",
        json_string(attempt_nonce),
        CHAIN_VERSION,
        json_string(model),
        prev,
        canonical_counts(counts),
        turn_index,
    )
}

/// The exact bytes signed for the terminal entry binding `final_code_hash`.
pub fn canonical_final_message(
    attempt_nonce: &str,
    final_code_hash: &str,
    prev_signature: Option<&str>,
    turn_count: usize,
) -> String {
    let prev = match prev_signature {
        Some(sig) => json_string(sig),
        None => "null".to_string(),
    };
    format!(
        "{{\"attempt_nonce\":{},\"chain_version\":{},\"final_code_hash\":{},\"prev_signature\":{},\"turn_count\":{}}}",
        json_string(attempt_nonce),
        CHAIN_VERSION,
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

/// Sign the full turn chain: each turn over its canonical message (chained to the
/// previous signature), then the terminal entry binding `final_code_hash`. Returns
/// the wire chain ready to upload.
pub fn sign_chain(
    key: &SigningKey,
    attempt_nonce: &str,
    turns: &[TurnInput],
    final_code_hash: &str,
) -> SignedChainWire {
    let mut signed_turns = Vec::with_capacity(turns.len());
    let mut prev: Option<String> = None;
    for turn in turns {
        let message = canonical_turn_message(
            attempt_nonce,
            turn.turn_index,
            &turn.model,
            &turn.token_counts,
            prev.as_deref(),
        );
        let signature = sign_base64(key, &message);
        signed_turns.push(SignedTurnWire {
            turn_index: turn.turn_index,
            model: turn.model.clone(),
            token_counts: turn.token_counts,
            signature: signature.clone(),
        });
        prev = Some(signature);
    }
    let final_message =
        canonical_final_message(attempt_nonce, final_code_hash, prev.as_deref(), turns.len());
    SignedChainWire {
        chain_version: CHAIN_VERSION,
        attempt_nonce: attempt_nonce.to_string(),
        turns: signed_turns,
        final_entry: SignedFinalWire {
            final_code_hash: final_code_hash.to_string(),
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

    #[test]
    fn canonical_messages_and_signatures_match_the_shared_vectors() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = signing_key_from_seed(&seed_from(&v));

        // The public key derived from the seed matches what pairing would upload.
        assert_eq!(
            public_key_base64(&key),
            v["public_key_b64"].as_str().unwrap()
        );

        let nonce = v["attempt_nonce"].as_str().unwrap();
        let mut prev: Option<String> = None;
        for tv in v["turns"].as_array().unwrap() {
            let idx = tv["turn_index"].as_u64().unwrap() as u32;
            let model = tv["model"].as_str().unwrap();
            let counts = counts_from(&tv["token_counts"]);
            let message = canonical_turn_message(nonce, idx, model, &counts, prev.as_deref());
            assert_eq!(
                message,
                tv["canonical"].as_str().unwrap(),
                "turn {idx} canonical bytes"
            );
            let signature = sign_base64(&key, &message);
            assert_eq!(
                signature,
                tv["signature"].as_str().unwrap(),
                "turn {idx} signature"
            );
            prev = Some(signature);
        }

        let turn_count = v["turns"].as_array().unwrap().len();
        let final_message = canonical_final_message(
            nonce,
            v["final_code_hash"].as_str().unwrap(),
            prev.as_deref(),
            turn_count,
        );
        assert_eq!(
            final_message,
            v["final"]["canonical"].as_str().unwrap(),
            "final canonical bytes"
        );
        assert_eq!(
            sign_base64(&key, &final_message),
            v["final"]["signature"].as_str().unwrap(),
            "final signature",
        );
    }

    #[test]
    fn sign_chain_reproduces_the_vector_chain_and_wire_shape() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = signing_key_from_seed(&seed_from(&v));
        let nonce = v["attempt_nonce"].as_str().unwrap();
        let turns: Vec<TurnInput> = v["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tv| TurnInput {
                turn_index: tv["turn_index"].as_u64().unwrap() as u32,
                model: tv["model"].as_str().unwrap().to_string(),
                token_counts: counts_from(&tv["token_counts"]),
            })
            .collect();

        let chain = sign_chain(&key, nonce, &turns, v["final_code_hash"].as_str().unwrap());
        assert_eq!(
            chain.turns[0].signature,
            v["turns"][0]["signature"].as_str().unwrap()
        );
        assert_eq!(
            chain.turns[1].signature,
            v["turns"][1]["signature"].as_str().unwrap()
        );
        assert_eq!(
            chain.final_entry.signature,
            v["final"]["signature"].as_str().unwrap()
        );

        // It serializes to the snake_case wire JSON the server's parseSignedChain
        // reads — including the `final` key (not Rust's `final_entry`).
        let wire = serde_json::to_value(&chain).unwrap();
        assert_eq!(wire["chain_version"], 1);
        assert_eq!(wire["attempt_nonce"].as_str().unwrap(), nonce);
        assert_eq!(wire["turns"][0]["turn_index"], 0);
        assert_eq!(wire["turns"][0]["token_counts"]["input"], 1200);
        assert!(wire["final"]["final_code_hash"].is_string());
        assert!(wire.get("final_entry").is_none(), "the wire key is `final`");
    }

    #[test]
    fn an_empty_chain_still_binds_the_final_code_hash() {
        // A session with no captured turns still produces a verifiable terminal
        // entry (prev_signature null, turn_count 0).
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = signing_key_from_seed(&seed_from(&v));
        let chain = sign_chain(&key, "nonce-0", &[], "deadbeef");
        assert!(chain.turns.is_empty());
        let expected = canonical_final_message("nonce-0", "deadbeef", None, 0);
        assert_eq!(chain.final_entry.signature, sign_base64(&key, &expected));
    }
}
