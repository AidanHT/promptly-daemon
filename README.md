# promptly + promptlyd

[![CI](https://github.com/AidanHT/promptly-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/AidanHT/promptly-daemon/actions/workflows/ci.yml)
[![Security audit](https://github.com/AidanHT/promptly-daemon/actions/workflows/security-audit.yml/badge.svg)](https://github.com/AidanHT/promptly-daemon/actions/workflows/security-audit.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

The local capture stack for **[Promptly](https://xpromptly.com)** — a
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
and installs them to a per-user bin directory — `~/.local/bin` on macOS/Linux
(the script tells you if that isn't on your PATH), `%LOCALAPPDATA%\Promptly\bin`
on Windows (added to your user PATH automatically).

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

Every archive is published alongside a `<archive>.sha256` checksum file
(`shasum -a 256 -c` / `Get-FileHash` to verify).

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
promptly play lru             # name a level by keyword, number, or stage prefix
cd lru                        # the workspace folder is that same short keyword
#   ...solve the challenge with your AI harness (Claude Code, etc.)...
promptly submit               # redact + package + device-signed ranked upload
```

Prefer it step by step? `promptly init <level>`, `cd` in, then `promptly start` —
the background daemon launches automatically (no second terminal). `promptly watch`
follows live token burn, `promptly stop` ends the session, and `promptly up` /
`promptly down` start and stop the daemon yourself if you'd rather manage it.

Running the web app locally? The browser HUD side just works; point the CLI at it
with one variable — see [Local vs production](#local-vs-production).

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
   thinking-token detail. Note: the Claude Code **IDE panel** (e.g. in VS Code)
   currently exports no OTEL upstream, so panel sessions capture JSONL-only and
   rank `unverified` — drive Claude Code from a terminal for a verified-eligible
   run.
3. **Best-effort adapters.** Cursor (the global + workspace `state.vscdb`
   composer stores, opened read-only), OpenAI Codex — the CLI **and** the VS Code
   IDE extension, which share the same `~/.codex/sessions` rollout store — and
   GitHub Copilot Chat (VS Code chat sessions, including the current `.jsonl`
   mutation-log format). These are reverse-engineered and version-fragile, so
   they degrade gracefully, mark inferred counts `estimated`, **never write to**
   the editor's files, and report their detection state on `/health` for
   `promptly doctor`.

When OTEL and JSONL observe the same turn they are **correlated, not just
de-duplicated**: the normalized turn carries an `agreement` marker (a tampering
signal), and OTEL values are authoritative — JSONL never silently overrides them.

### Verified-eligible captures

The ranked **verified** badge is reserved for the one capture path whose telemetry
is authenticated, cross-checked, and tamper-evident end to end. Every other capture
still ranks — it simply carries no badge (`unverified`), never a penalty.

| Capture | Local signal | Verified-eligible? |
| --- | --- | --- |
| Claude Code — native OTEL (consented, online) | `otel` | **Yes** — with a server-issued nonce + attested baseline |
| Claude Code — JSONL logs only (incl. the IDE panel) | `jsonl` | No — ranks `unverified` |
| Cursor / Codex (CLI + IDE) / Copilot adapters | `estimated` | No — reverse-engineered, ranks `unverified` |
| Any harness started offline (local nonce) | — | No — ranks `unverified` |
| Any capture with a tampering fingerprint | — | `suspect` (held for review) |

Only OTEL-backed Claude Code qualifies because it is the only source the daemon can
(1) **authenticate** (a per-session ingest token the receiver requires), (2)
**corroborate** (OTEL↔JSONL agreement), and (3) **bind** into a device-signed v4
turn chain the server verifies. Adapters read another tool's logs after the fact
with no such guarantees, so they can't earn the badge — by design, not omission.
`promptly submit` prints the projected tier before you confirm.

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
promptly restart [<level>]    # discard this attempt; re-fetch the level fresh in place
promptly up | down            # start / stop the background daemon explicitly
promptly test                 # run public tests (local-first; remote fallback)
promptly watch                # live per-turn token burn + projected score
promptly score                # projected score, parity with the server
promptly status               # is the daemon running / capturing?
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

The CLI manages the daemon for you: `promptly start` and `play` auto-launch
`promptlyd` in the background scoped to your level (and `promptly down` stops it),
so you never need a second terminal. An idle daemon — no capture session — also
shuts itself down after 15 minutes (`promptlyd run --idle-timeout-secs` tunes
this; `0` disables it). `promptlyd run` is still the
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

**Playing against the deployed app just works** — both halves are wired by default:

- **The web HUD** reads the daemon from the browser. The canonical production
  origin (`https://xpromptly.com`) is allowed by the daemon's CORS **by default**
  — no flag needed. A preview deploy is added with `PROMPTLY_WEB_ORIGIN`
  (comma/space-separated) or repeated `--web-origin`. It is always an **exact**
  origin, never a wildcard.
- **The CLI** (`pair`, `init`, `submit`, remote `test`) talks to the web app and
  defaults to `https://xpromptly.com`.

**Local dev needs one variable.** Loopback origins (`http://localhost:3000`,
`http://127.0.0.1:3000`) are always allowed by the daemon's CORS, so the HUD side
is free — but point the CLI away from production:

```sh
PROMPTLY_API_URL=http://localhost:3000 promptly doctor   # or --api-url
```

`promptly doctor` prints the resolved URL and whether it's local-dev or production.

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
  read-only (with an immutable snapshot as the fallback); they never write to
  your editor's files.
- **Device-signed runs.** A ranked submission is signed by a per-device Ed25519
  key created at pairing; the credential file is owner-only and the token expires.

### Anti-cheat: earning (and not faking) the verified badge

The scoring rewards efficiency (fewer tokens/turns), so the incentive is to
*under-report* work or *fabricate* a clean capture. These layers make the verified
badge unfakeable rather than trusting the client:

- **Verified is a server decision over signed evidence.** The badge requires a
  v4 device-signed turn chain with a server-issued nonce, an attested kit baseline,
  every turn OTEL/JSONL-sourced and non-estimated, at least one OTEL-backed turn,
  and coherent timing. The server reads only what was *signed*, so an edited wire
  field (e.g. a Copilot capture relabelled `otel`) can't earn it — it lands
  `unverified`, and a broken/replayed chain lands `suspect`.
- **Authenticated OTLP ingest.** The receiver mints a fresh per-session token,
  writes it into the harness settings, and rejects any post that doesn't present it
  — and *all* posts while idle or JSONL-only. No other loopback process can inject
  fabricated `api_request` turns to inflate or forge a run.
- **Attested kit baseline.** A fresh start verifies the local (player-editable)
  `manifest.json` baseline against the server's authoritative hash and refuses a
  stale or tampered kit, so a pre-solved workspace can't be anchored to a forged
  starter. Offline starts are unattested and cap at `unverified`.
- **Signed, tamper-evident provenance.** Each turn signs its confidence, source
  set, and timestamp; the terminal entry signs a capture summary (nonce origin,
  baseline attestation + reset count, bulk-paste count, prompt count) and the
  OTEL↔JSONL cross-source agreement. Implausible pacing (backwards timestamps, impossible
  bursts) is flagged before upload and re-checked server-side.

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
