# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AidanHT/promptly-daemon/releases/tag/v0.1.0
