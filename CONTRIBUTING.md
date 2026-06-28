# Contributing

Thanks for your interest in Promptly's local capture stack.

## Prerequisites

- A Rust toolchain via [rustup](https://rustup.rs). The repo pins `stable` in
  `rust-toolchain.toml`, so the right channel is selected automatically.
- A C toolchain (for the bundled SQLite in `rusqlite`): `build-essential` on
  Linux, the Xcode Command Line Tools on macOS, the MSVC build tools on Windows.

## Develop

```sh
git clone https://github.com/AidanHT/promptly-daemon
cd promptly-daemon

cargo build
cargo test
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

CI runs exactly `cargo fmt --all --check`, `cargo clippy --all-targets -- -D
warnings`, and `cargo test` on every push to `main` and every PR. Keep each commit
green against all three.

## Layout

- `crates/promptlyd/` — the daemon: capture (OTLP receiver + JSONL watcher + the
  best-effort adapters), normalize, correlate, and the localhost-only HTTP API.
- `crates/promptly/` — the player CLI, driven over the daemon's control API. It
  scores locally with parity to the server and packages device-signed submissions.
- `vendor/` — fixtures vendored from the Promptly web app (see below).

## The vendored fixtures (`vendor/`)

Three modules embed two JSON files from `vendor/` at build time via `include_str!`:

- `vendor/parity-fixture.json` — scoring constants, token weights, and the model
  economics matrix. `crates/promptly/src/scoring.rs` and
  `crates/promptlyd/src/model_map.rs` embed it. The CLI's scoring is a
  byte-for-byte port of the web app's; the parity test fails if they diverge.
- `vendor/turn-chain-vectors.json` — the Ed25519 turn-chain signing vectors.
  `crates/promptly/src/signing.rs` embeds it to pin signing to the server's
  verifier.

These are mirrored from the Promptly web app — **do not hand-edit them**. When the
upstream contract changes, re-copy the file and bump the workspace version; the
parity tests are what enforce the sync.

## Commits

Short, imperative subject lines (e.g. "Add the Codex adapter"). One logical change
per commit.
