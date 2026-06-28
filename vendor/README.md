# vendor/

Fixtures mirrored from the [Promptly web app](https://trypromptly.vercel.app),
embedded into the Rust binaries at build time via `include_str!`. They are the
cross-language contracts that keep the CLI in lockstep with the server — **do not
hand-edit them**.

- `parity-fixture.json` — scoring constants, token weights, and the model
  economics matrix. Embedded by `crates/promptly/src/scoring.rs` and
  `crates/promptlyd/src/model_map.rs`. The scoring parity test fails if the Rust
  port diverges from the web app's scoring.
- `turn-chain-vectors.json` — Ed25519 turn-chain signing vectors. Embedded by
  `crates/promptly/src/signing.rs` to pin signing to the server's verifier.

When the upstream contract changes, re-copy the file and bump the workspace
version; the parity tests are what enforce the sync.
