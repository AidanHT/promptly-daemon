# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/AidanHT/promptly-daemon/releases/tag/v0.1.0
