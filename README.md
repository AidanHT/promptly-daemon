# promptly + promptlyd

[![CI](https://github.com/AidanHT/promptly-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/AidanHT/promptly-daemon/actions/workflows/ci.yml)
[![Security audit](https://github.com/AidanHT/promptly-daemon/actions/workflows/security-audit.yml/badge.svg)](https://github.com/AidanHT/promptly-daemon/actions/workflows/security-audit.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

The local capture stack for **[Promptly](https://trypromptly.vercel.app)** — a
competitive prompt-engineering arena where engineers solve hard coding challenges
through an AI harness and are scored on prompt efficiency (tokens, turns, model
cost, execution speed).

This repository ships two Rust binaries that run on the player's machine:

- **`promptlyd`** — a local telemetry **daemon**. It auto-captures your AI coding
  usage, normalizes every source into one schema, and serves it over a
  **localhost-only** HTTP API.
- **`promptly`** — the player's terminal **CLI**. It fetches a challenge
  workspace, runs a scored capture session, tests locally, watches live token
  burn, scores with parity to the server, pairs your device, and submits a run for
  ranked grading.

Everything is captured **locally**. The only thing that ever leaves your machine
is a **redacted, device-signed** submission that you explicitly send.

---

## Install

### 1. One-line install script (no Rust toolchain needed)

Downloads the prebuilt `promptly` + `promptlyd` binaries for your platform from
the latest [GitHub release](https://github.com/AidanHT/promptly-daemon/releases)
and drops them on your PATH.

**macOS / Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/AidanHT/promptly-daemon/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/AidanHT/promptly-daemon/main/install.ps1 | iex
```

Override the version or location with `PROMPTLY_VERSION` / `PROMPTLY_INSTALL_DIR`.

### 2. With Cargo (the Rust package manager)

If you already have a [Rust toolchain](https://rustup.rs), install both binaries
straight from this repo — no release download, always builds from source:

```sh
cargo install --git https://github.com/AidanHT/promptly-daemon promptly promptlyd
```

(This is a Cargo **workspace** with two packages, so both names are listed
explicitly.)

### 3. Prebuilt binaries (manual)

Grab the archive for your platform from the
[releases page](https://github.com/AidanHT/promptly-daemon/releases/latest),
unpack it, and move `promptly` / `promptlyd` somewhere on your PATH. Prebuilt
targets:

| Platform | Target |
| --- | --- |
| Linux (x86-64) | `x86_64-unknown-linux-gnu` |
| macOS (Apple Silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Windows (x86-64) | `x86_64-pc-windows-msvc` |

> **Roadmap:** Homebrew tap, Scoop bucket, and a `crates.io` publish (so plain
> `cargo install promptly` works) are planned. Until then, options 1 and 2 above
> are the quickest paths.

### Updating

Already installed? Upgrade both binaries in place:

```sh
promptly update          # fetch the latest release and swap promptly + promptlyd
promptly update --check  # just report whether a newer version exists
```

It resolves the latest release for your platform, stops the daemon if it's
running, and replaces `promptly` + `promptlyd`. If you installed from source
instead, re-run `cargo install --git https://github.com/AidanHT/promptly-daemon promptly promptlyd --force`.

`promptly` also checks GitHub for a newer release about once a day and prints a
one-line notice when one is available (interactive terminals only — never in
scripts or CI; `promptly doctor` shows the same status). Set
`PROMPTLY_NO_UPDATE_CHECK=1` to disable it.

---

## Quick start

```sh
promptly pair                 # one-time: link this device to your Promptly account

# The fast path — fetch the level, launch the daemon, and begin capturing at once:
promptly play <level-slug>
cd <level-slug>
#   ...solve the challenge with your AI harness (Claude Code, etc.)...
promptly submit               # redact + package + device-signed ranked upload
```

Prefer it step by step? `promptly init <level>`, `cd` in, then `promptly start` —
the background daemon launches automatically (no second terminal). `promptly watch`
follows live token burn, `promptly stop` ends the session, and `promptly up` /
`promptly down` start and stop the daemon yourself if you'd rather manage it.

Playing on `localhost`? It just works — see [Local vs production](#local-vs-production).

From a source checkout you can also run the daemon directly with the bundled
launchers (`./run.sh` / `./run.ps1`), though you rarely need to — the CLI starts it
for you.

---

## What it captures

1. **Claude Code — native OpenTelemetry.** An embedded OTLP/HTTP receiver ingests
   Claude Code's `api_request` log events (model, token counts, `cost_usd`,
   `duration_ms`, `prompt.id`). High-confidence (`otel`). It speaks **OTLP/HTTP
   with JSON**, so the harness bootstrap sets
   `OTEL_EXPORTER_OTLP_PROTOCOL=http/json` and points
   `OTEL_EXPORTER_OTLP_ENDPOINT` at the daemon's loopback port.
2. **Claude Code — JSONL session logs.** A watcher tails
   `~/.claude/projects/<encoded-cwd>/*.jsonl`, parsing `assistant` usage and
   thinking blocks. Fallback/supplement (`jsonl`), and the source of
   thinking-token detail.
3. **Best-effort adapters.** Cursor (`state.vscdb`, read-only + immutable), OpenAI
   Codex CLI (`~/.codex/sessions` rollout JSONL), and GitHub Copilot Chat (VS Code
   `chatSessions/*.json`). These are reverse-engineered and version-fragile, so
   they degrade gracefully, mark inferred counts `estimated`, **never write to or
   lock** the editor's files, and report their detection state on `/health` for
   `promptly doctor`.

When OTEL and JSONL observe the same turn they are **correlated, not just
de-duplicated**: the normalized turn carries an `agreement` marker (a tampering
signal), and OTEL values are authoritative — JSONL never silently overrides them.

## Session scoping

Capture only counts toward a level **between an explicit start and stop**, and
only for the bound workspace — so unrelated AI usage never inflates an attempt. A
`promptly start` binds the session to the workspace's `.promptly/manifest.json`
level and, before capturing anything:

- runs the **baseline integrity check** (resets a tampered workspace to the
  canonical starter, after a backup),
- issues the **attempt nonce** (the anti-replay guard; offline caps at
  `unverified`, a server-issued nonce lifts the cap), and
- with explicit consent, bootstraps the OTEL env into the **project**
  `.claude/settings.json` (declining falls back to JSONL-only).

A `stop` reopens to a `start` (resume) without re-checking the baseline, so an
in-progress attempt is never reset out from under you.

## Commands

```
# CLI (promptly) — global: --api-url URL | --api-port 8765 | --no-color
promptly pair                 # device-authorization flow → 90-day device token
promptly play <level>         # fetch + launch the daemon + start capturing, in one step
promptly init <level>         # download the starter kit; start the solve clock
promptly start | stop | reset # bound capture session (auto-starts the daemon)
promptly up | down            # start / stop the background daemon explicitly
promptly test                 # run public tests (local-first; remote fallback)
promptly watch                # live per-turn token burn + projected score
promptly score                # projected score, parity with the server
promptly doctor               # diagnose daemon / OTEL / web app / manifest / runtime
promptly submit               # redact + package + device-signed ranked upload
promptly update               # upgrade promptly + promptlyd to the latest release
promptly help                 # grouped overview of every command

# Daemon (promptlyd) — auto-managed by the CLI; you rarely run it directly
promptlyd run [--workspace DIR] [--api-port 8765] [--otlp-port 4318] [--web-origin ORIGIN]…
promptlyd status [--api-port 8765]                 # connected / capturing / idle
promptlyd install [--workspace DIR] …              # register as a background OS service
promptlyd uninstall
```

The CLI manages the daemon for you: `promptly start`, `watch`, and `play`
auto-launch `promptlyd` in the background scoped to your level (and `promptly down`
stops it), so you never need a second terminal. `promptlyd run` is still the
foreground entrypoint a service manager invokes, and `promptlyd install` registers
it as a systemd **user** service (Linux), a launchd **agent** (macOS), or a logon
**scheduled task** (Windows) if you'd rather run it always-on.

## Local HTTP API (loopback only)

Read (consumed by the web HUD):

- `GET /health` — status, version, uptime, OTLP endpoint, adapter detection,
  recent errors.
- `GET /session` — the active session binding (bound level, nonce, window), token
  totals, captured turns, and provenance signals.
- `GET /stream` — Server-Sent Events, one event per normalized turn.
- `GET /session/preflight` — what a `start` would do, without side effects.

Control (driven by the `promptly` CLI):

- `POST /session/start` `{confirm_reset, consent_bootstrap}` — begin/resume.
- `POST /session/stop` — end the session and revert the harness settings.
- `POST /session/reset` — restore the workspace to the canonical starter.
- `POST /shutdown` — stop the daemon gracefully (the `promptly down` / level-switch path).

CORS only allows **GET** from loopback dev origins and the configured deployed
Promptly origin(s); the mutating routes additionally require the CLI's
`X-Promptly-Control` header. A public HTTPS Promptly page reaching `127.0.0.1`
also gets Chrome's Private Network Access preflight answered.

## Local vs production

**Local dev just works.** Loopback origins (`http://localhost:3000`,
`http://127.0.0.1:3000`) are always allowed, and the CLI defaults to
`http://localhost:3000`, so a local web app + `promptlyd run` + `promptly …` need
no configuration.

**Playing against the deployed app** needs two things, both wired by default:

- **The web HUD** reads the daemon from the browser. The canonical production
  origin (`https://trypromptly.vercel.app`) is allowed by the daemon's CORS **by
  default** — no flag needed. A custom domain or preview deploy is added with
  `PROMPTLY_WEB_ORIGIN` (comma/space-separated) or repeated `--web-origin`. It is
  always an **exact** origin, never a wildcard.
- **The CLI** (`pair`, `init`, `submit`, remote `test`) talks to the web app.
  Point it at production with `PROMPTLY_API_URL=https://trypromptly.vercel.app`
  (or `--api-url`). `promptly doctor` prints the resolved URL and whether it's
  local-dev or production.

## State

All under `~/.promptly/` (override the home dirs for testing with
`PROMPTLY_DATA_DIR` / `PROMPTLY_CLAUDE_HOME`):

- `session.json` — the session marker: bound level, workspace, attempt nonce,
  `code_reset_count`, and the bootstrap state needed to revert.
- `checkpoint.json` — crash-recovery checkpoint (turns, per-file JSONL offsets,
  dedup set), keyed by session id. A restart resumes without losing or
  double-counting turns. Machine-local; never synced.
- `credentials.json` — the paired device token + Ed25519 signing seed, `0600`
  (owner-only). 90-day expiry + one-command revocation bound the damage.
- `cache/<level>/v<n>/` — the pristine canonical starter, cached on a verified
  start so a later tampered start can be reset offline.
- `promptlyd.lock` / `session.lock` — single-instance and single-session guards.
- `<workspace>/.promptly/backup/<ts>/` — your files, backed up before any reset.

## Security

- **Loopback only.** Both servers bind `127.0.0.1`; the daemon is never exposed
  off-machine.
- **Local capture, explicit upload.** Nothing leaves your machine until you run
  `promptly submit`, and that payload is **redacted** (provider keys, bearer
  tokens, PEM blocks, `secret=`-style assignments) before it is signed and sent.
- **Read-only adapters.** The Cursor / Codex / Copilot adapters open editor state
  read-only and immutable; they never write to or lock your editor's files.
- **Device-signed runs.** A ranked submission is signed by a per-device Ed25519
  key created at pairing; the credential file is owner-only and the token expires.

## Build from source

```sh
cargo run -p promptlyd -- run        # foreground daemon
cargo run -p promptly  -- doctor     # CLI
cargo test                           # unit + integration tests (both crates)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Cross-platform release binaries are built and attached to a GitHub release on a
`v*` tag (`.github/workflows/release.yml`). See [CONTRIBUTING.md](./CONTRIBUTING.md)
for the `vendor/` parity fixtures and how scoring stays in lockstep with the web
app.

## License

[MIT](./LICENSE).
