# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
