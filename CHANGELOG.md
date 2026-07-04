# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.5] - 2026-07-03

### Changed

- `promptly help` opens with a **QUICK START** section — the three commands that
  take a new player from nothing to a ranked solve (`pair` → `play` → `submit`).
- `promptly doctor` now aligns every check's detail into one column and closes
  with a one-line verdict (`all N checks passed`, or the warning/failure counts
  in the worst level's color), so the report ends on an unambiguous answer.
- `promptly status` shows the captured token totals (`in · out · think`) while a
  session is capturing — the numbers the attempt is scored on.
- `promptly watch` redraws its running-totals line in place on a TTY (a live
  scoreboard under the newest turn) instead of duplicating it down the
  scrollback. Piped output stays append-only.

### Fixed

- The `promptly score` breakdown columns misaligned when colors were enabled:
  the label padding was applied after ANSI styling, so the (zero-width) escape
  codes consumed the pad. Labels are now padded before styling, and the
  breakdown aligns identically with and without color.

### Security

- **The `verified` badge is now gated on authenticated, OTEL-backed Claude Code —
  every other capture ranks as `unverified`.** Scoring rewards efficiency, so the
  incentive is to under-report or fabricate a clean capture; this release makes the
  badge a server decision over *signed* evidence rather than a client claim. A
  capture reaches `verified` only with a v3 device-signed turn chain that carries a
  server-issued nonce, an attested kit baseline, at least one OTEL-backed turn, all
  turns OTEL/JSONL-sourced and non-estimated, no cross-source disagreement, and
  coherent timing. A relabelled Copilot/Cursor/Codex capture, a JSONL-only run, an
  offline (local-nonce) start, or estimated counts all rank `unverified`; a
  broken/replayed chain is `suspect`. Because the server reads only what was signed,
  editing the unsigned `telemetry_confidence` wire field no longer earns the badge.
- **Authenticated OTLP ingest.** A consented session mints a fresh per-session
  ingest token, writes it into the harness settings
  (`OTEL_EXPORTER_OTLP_HEADERS=X-Promptly-Otlp-Token=…`), and the receiver rejects
  any `/v1/logs`/`/v1/metrics` post that doesn't present it — and *all* posts while
  idle or JSONL-only — **before parsing the body**. This closes the biggest gap in
  the verified path: any loopback process could previously POST fabricated
  `api_request` turns to inflate or forge a run. The token rotates per session and
  is re-asserted on resume.
- **Attested kit baseline.** A fresh `start` now verifies the local (player-editable)
  `manifest.json` `baseline_hash` against the server's authoritative value (returned
  with the attempt nonce) and refuses a stale or tampered kit
  (`re-run promptly init`), so a pre-solved workspace can't be anchored to a forged
  starter. Offline starts are unattested and cap at `unverified`.
- **Turn chain v3 — signed provenance.** Each turn now signs its confidence tier,
  source set, and timestamp, and the terminal entry signs a **capture summary**
  (nonce origin, baseline attestation and reset count, bulk-paste count) alongside
  the v2 cross-source agreement. All of it is tamper-evident: the server scores
  exactly what was signed. The canonical message format is pinned byte-for-byte to
  the web app and the shared `vendor/turn-chain-vectors.json`; v1/v2 chains still
  verify for a staged rollout.
- **Pacing plausibility.** `promptly submit` now flags an implausibly paced capture
  (timestamps that jump backwards, or a burst of turns tighter than any interactive
  session) before upload — the fingerprint of a fabricated or replayed chain — and
  the server re-checks pacing over the signed timestamps.

### Added

- `promptly submit` prints the **projected trust tier** (verified-eligible vs.
  `unverified`, with the reason) before the irreversible ranked confirmation, so you
  know whether a capture earns the badge — and why not — before submitting.

## [0.1.3] - 2026-06-30

### Security

- The daemon's control routes (`POST /session/start|stop|reset`, `/shutdown`) now
  require a per-process **capability token**, not just the presence of the
  `X-Promptly-Control` header. The daemon mints a fresh random token at startup and
  writes it owner-only (`0600` on Unix; the user-profile ACL on Windows) to
  `~/.promptly/control.json`; the `promptly` CLI reads it to authenticate. This
  shuts out a local non-browser process that could otherwise stop a rival's
  session, force a shutdown, or inject a session start by setting the old constant
  header. The token rotates every start, so a stale file can't drive a new daemon.
- Every API route now rejects a request whose `Host` header names a non-loopback
  authority, closing DNS rebinding as a third layer over the loopback-only bind and
  the CORS origin lock.
- The crash-recovery checkpoint is now **sealed** with a tamper-evident hash chain
  over the captured turns (`ledger`), committing to the integrity signals (the
  contributing sources, cross-source agreement, and plausibility) as well as the
  token counts. On load the daemon recomputes the seal and starts fresh if it no
  longer matches, so an offline edit of the persisted capture (lowering a turn's
  tokens, deleting a turn, hiding a disagreement) is denied rather than resumed and
  counted toward an attempt. The checkpoint format is now v2.
- The embedded OTLP receiver now also rejects a non-loopback `Host`, closing
  DNS-rebind telemetry injection — a rebound page's same-origin POST would
  otherwise sidestep the receiver's no-CORS posture and feed fabricated turns into
  the capture stream.
- `promptly submit` now reads the capture's integrity signals (cross-source
  agreement and plausibility) before uploading and **fails closed** on a tampering
  fingerprint: a capture carrying cross-source disagreements or implausible turns is
  not pushed by the routine `--yes` — it requires an explicit `--force` (or an
  interactive acknowledgement), so a flagged capture can't be recorded as a ranked
  attempt silently. The server still re-derives the authoritative verdict.
- The signed turn chain is now **`chain_version` 2**: the OTEL↔JSONL cross-source
  corroboration summary (how many turns disagreed, and on which fields) is signed
  into the terminal entry alongside `final_code_hash`. Previously this summary was
  never uploaded, so the server's cross-source integrity check had nothing to act
  on; now it rides inside the device-signed chain, so a forked daemon can't strip or
  zero it to hide fabricated turns without breaking the terminal signature (the
  server then grades the run `suspect`). The server verifies v1 and v2 chains, so
  it can be redeployed ahead of this release; older daemons keep submitting v1.

## [0.1.2] - 2026-06-30

### Added

- `promptly restart [<level>]` — discard the current attempt and re-fetch the
  level fresh in the same folder: it stops the daemon, clears the bound attempt
  and the solve clock, wipes the workspace (keeping `.git`), and re-downloads the
  pristine kit, so the next `promptly start` is a brand-new attempt. The kit is
  downloaded before anything is deleted, so a failed (e.g. offline) restart
  changes nothing.

### Changed

- `promptly submit` now confirms before the ranked upload — it records an attempt
  and can't be undone. Pass `--yes` to skip the prompt; a non-interactive shell
  must pass `--yes` to submit.

## [0.1.1] - 2026-06-30

### Added

- `promptly play <level>` — fetch a level, launch the daemon, and start capturing
  in a single command.
- `promptly up` / `promptly down` — start and stop the background daemon explicitly.
- The daemon's `GET /health` now reports its scoped `workspace`, and a guarded
  `POST /shutdown` route stops it gracefully.
- `promptly help` — a grouped, styled overview of every command.
- `promptly update` — upgrade the installed `promptly` + `promptlyd` binaries to
  the latest GitHub release in place (`--check` reports availability without
  installing). Downloads the prebuilt archive for your platform, stops the
  running daemon, and swaps both binaries.
- An automatic update check: after a command, `promptly` prints a one-line notice
  when a newer release is available (cached to once a day, interactive terminals
  only; `promptly doctor` reports the same status). Opt out with
  `PROMPTLY_NO_UPDATE_CHECK`.

### Changed

- The CLI now auto-manages the `promptlyd` daemon: `promptly start`, `watch`, and
  `play` launch it in the background scoped to your level (relaunching it when you
  switch levels), so you no longer run `promptlyd run` in a separate terminal. The
  background daemon detaches fully from the CLI's streams, so piping or capturing a
  command's output never blocks, and a relaunch (or `promptly down`) waits for the
  previous daemon to exit cleanly before continuing.

### Removed

- The redundant `promptly login` command. It never signed you in — device auth is
  the device-authorization flow (`promptly pair`) — it only reported whether a
  credential already existed, which `promptly doctor` already covers.

## [0.1.0] - 2026-06-28

### Added

- `promptlyd` local telemetry daemon: an embedded OTLP/HTTP receiver and a Claude
  Code JSONL session-log watcher, cross-source correlation and de-duplication, and
  a localhost-only HTTP API (`/health`, `/session`, `/stream`, plus CLI-only
  control routes).
- Best-effort Cursor / OpenAI Codex CLI / GitHub Copilot Chat adapters: read-only,
  inferred counts marked `estimated`, detection state surfaced via `promptly
  doctor`.
- Session scoping: workspace binding to a level manifest, a baseline integrity
  check with backup-and-reset, the attempt nonce, and an OTEL harness bootstrap
  gated on explicit consent.
- `promptly` CLI: `pair`, `init`, `start` / `stop` / `reset`, `test`, `watch`,
  `score`, `doctor`, and `submit` — local scoring with parity to the server and a
  redacted, device-signed ranked upload.
- One-line install scripts (`install.sh` / `install.ps1`) and cross-platform
  release binaries (Linux, macOS arm64/x86_64, Windows) published on `v*` tags.

[Unreleased]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AidanHT/promptly-daemon/releases/tag/v0.1.0
