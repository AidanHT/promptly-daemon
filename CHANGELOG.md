# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-07-15

The first stable release. Everything a player touches was audited end-to-end
for production — the six capture paths (Claude Code CLI + IDE panel, Codex CLI
+ IDE extension, Cursor, Copilot Chat), every CLI command, scoring parity with
the server, the installers, and the docs — and the sharp edges found were
fixed. No protocol change: a v0.4.x install upgrades cleanly with
`promptly update`.

### Fixed

- **`promptly init --force` (and `play --force`) empties the folder before
  unpacking** — keeping `.git`, and the solve clock survives the wipe.
  Unpacking over leftover files failed the post-unpack baseline check with a
  misleading "corrupt download" error that no retry could ever fix.
- **A no-arg `promptly play` outside a level workspace fails cleanly** instead
  of first ending the active capture session and rescoping the daemon to the
  wrong folder — the same wrong-directory guard `start` already had.
- **`promptly start <level>` accepts the short level names every other command
  takes** (`lru`, `7`, `stage-1-01`). It falsely refused them in the correct
  workspace — including the exact form `submit`'s own hint tells you to type.
- **The signed prompt count `P` no longer inflates on agentic runs.**
  Task-subagent transcript lines (`isSidechain`), meta records (`isMeta`), and
  slash-command lines in the Claude Code JSONL no longer count as user prompts
  or misgroup the turns that follow them.
- **Copilot thinking text is counted once, in the thinking bucket.** It was
  estimated into the output count *and* (when rounds record real counts) summed
  again as thinking; without recorded counts it now estimates thinking instead
  of inflating output.
- **An unresolvable Cursor per-prompt model pick no longer inherits the
  previous prompt's** — turns fall to the composer's configured model or
  degrade to `estimated`, never to a stale price.
- **`promptlyd status` reports a stopped session as idle**, matching what
  `promptly status` already said.
- **Pairing no longer times out after a single poll** if the server omits the
  device-flow window — a missing `expires_in` defaults to the usual 15 minutes.
- **`promptlyd install` on macOS no longer fights `promptly down` and
  `promptly update`**: the launchd agent relaunches only after a crash
  (`KeepAlive.SuccessfulExit=false`), the same policy as systemd's
  `Restart=on-failure`.
- **`promptly doctor`'s closing verdict counts warnings honestly** ("4 of 7
  checks need attention" instead of calling warnings failures), and the live
  score context line shows the run time it actually assumed instead of a
  hardcoded "2s".
- Internal build-plan numbers no longer leak into `promptly --help`, and the
  `--api-url` help names the real default (the production site, not localhost).

### Changed

- **Local projections score estimated captures exactly as grading does.**
  Editor-adapter captures (Cursor / Codex / Copilot) floor the effort base at
  anchor parity — the server's fairness rule since 2026-07-15 — and the scored
  harness is the weight-dominant one over the signed sources, so `watch`,
  `score`, and the submit preview no longer project higher than the graded
  result on estimated runs. The breakdown says when the floor applied.
- **`claude-fable-5` spellings resolve like the other Anthropic tiers** —
  datestamped or reordered forms find the priced row instead of the floor.
- **A Codex session can never be re-scoped by a later metadata line**: the
  session's original cwd stays authoritative, so a future Codex version
  persisting cwd-carrying events mid-session can't silently drop turns.
- **A crashed adapter scan reports itself** on `/health` / `promptly doctor`
  (`unsupported`, with a restart hint) instead of dying silently behind a stale
  `detected` status.
- **The CLI notes a version-mismatched daemon** when it reuses one that
  survived an upgrade (a service-managed or non-default-port daemon), with the
  `promptly down` hint to relaunch it on the new build.
- **The installers upgrade a live install safely.** Both stop or step around a
  running daemon (no more half-replaced installs), `install.ps1` runs in its
  own scope (no variables leak into your shell via `irm | iex`), preserves a
  `REG_EXPAND_SZ` user PATH instead of flattening `%VAR%` entries, broadcasts
  the PATH change, and works on Windows PowerShell 5.1.
- **Every release asset ships with a `.sha256` checksum file.**
- **README correctness pass**: the signed chain is v4 (not v3), all six capture
  paths are named — including the Claude Code IDE panel's JSONL-only caveat —
  and the install, idle-shutdown, and command-list copy matches the code.

## [0.4.8] - 2026-07-15

### Changed

- **The workspace folder is now the level's short keyword, not the full slug.**
  `promptly play lru` (and `promptly init lru`) unpack into `./lru` instead of
  `./stage-1-01-lru-eviction-debug`, so the `cd` after fetching is as short as
  the name you just typed — whichever accepted form (keyword, number,
  `stage-N-NN` prefix, or full slug) you used. `init`'s and `play`'s closing
  hints print the folder exactly as created, and `--dir` still overrides the
  default. A slug outside the catalog keeps naming its folder after itself.
  Existing full-slug workspaces are unaffected: the daemon identifies a
  workspace by its `.promptly/manifest.json`, never by the folder name, so
  `start`/`restart`/`submit` in an old folder keep working.

## [0.4.7] - 2026-07-15

Harness-capture overhaul: Cursor, Codex (CLI + IDE), and Copilot Chat capture
were audited against the storage formats those tools actually write in mid-2026
and rebuilt where they had silently drifted. A Cursor agent run that previously
captured **zero** turns now captures; Copilot's current session format is
readable again.

### Fixed

- **Cursor capture worked against a storage model Cursor no longer uses** —
  a real `cursor-agent` session captured 0 turns end-to-end. Three independent
  breaks, all fixed:
  - *Workspace scoping:* Cursor records the **project root** it was opened in,
    which is routinely the *parent* of the bound level folder; the old
    exact-equality `workspaceStorage` match could never see it. Conversations
    now scope by the global store's `composerData.workspaceIdentifier`
    (`uri.fsPath` **or** storage-hash id, ancestor/descendant-aware), unioned
    with the composer ids the workspace's own store names
    (`selectedComposerIds`/`lastFocusedComposerIds` — the migrated stub — plus
    the legacy `allComposers`).
  - *Schema drift:* per-bubble `tokenCount` is `{0,0}` on current agent
    sessions and `modelInfo` is null, so turns were dropped as empty. Usage is
    now estimated from the bubble's real content (`text`, `thinking.text`,
    `codeBlocks`, tool args/results → marked `estimated`), the model resolves
    from the user bubble's per-prompt pick or the composer's `modelConfig`
    (`composer-2.5`, `composer-2.5-fast`, and `-fast` variants generally now
    resolve to their priced row), bubble `createdAt` parses in its new
    RFC3339-string form, and conversation order follows
    `fullConversationHeadersOnly` so prompt grouping survives equal timestamps.
  - *WAL blindness:* the store is now opened read-only **without**
    `immutable=1` when possible, so bubbles a running Cursor hasn't
    checkpointed yet are visible live (`immutable=1` remains the fallback).
- **Copilot Chat capture skipped every current session.** VS Code stores chats
  as `.jsonl` mutation logs now (snapshot + set/append deltas); the adapter
  only read whole-file `.json` and reported the result as "no sessions yet".
  The log is now replayed into the session object (the per-request schema is
  unchanged), `.jsonl`-only workspaces report `Unsupported` when genuinely
  unreadable instead of masking breakage, real per-round
  `toolCallRounds[].thinking.tokens` feed the thinking count, and Auto-mode
  requests resolve their model through the rounds' `phaseModelId`.
- **Codex parsing matched current rollouts only by accident.** The model never
  rides on `session_meta` (it carries only `model_provider`) — it arrives on
  per-turn `turn_context` lines, which the generic metadata fall-through both
  consumed for the model *and* let re-write the session's scoping cwd on every
  turn. `turn_context` is now a first-class event: it updates the model, fills
  a still-unknown cwd, and can never re-scope a bound session. `token_count`
  heartbeats (`info: null`) are classified explicitly. Codex IDE (VS Code's
  `openai.chatgpt`) shares the same rollout store and needs no separate source.
- **Windows extended-length paths (`\\?\C:\…`) normalize away** in workspace
  matching, so an extended-length cwd still scopes to its plain-form workspace.

### Changed

- Session-start copy is harness-neutral: it now names Cursor/Codex/Copilot
  alongside Claude Code instead of saying "run Claude Code here", and the
  capture line lists the editor adapters. Note: Claude Code driven from the
  IDE **panel** exports no OTEL (upstream anthropics/claude-code#35105), so it
  captures via JSONL only and ranks `unverified`; run `claude` in a terminal
  (or set `claudeCode.useTerminal: true`) for verified-eligible runs.

## [0.4.6] - 2026-07-14

Daemon lifecycle quality-of-life — no protocol, capture, or scoring change. The
daemon now gets out of the way once it has nothing to capture: it stops itself
after a successful submit and after 15 idle minutes, and it no longer pins the
level folder against deletion while it runs.

### Fixed

- **A level folder can be deleted again.** The background daemon inherited the
  CLI's working directory — usually the level folder — and a Windows process's
  cwd locks that directory against deletion (`EBUSY: resource busy or locked`)
  for as long as the process lives, which for a detached daemon meant
  indefinitely. The CLI now spawns `promptlyd` with its cwd anchored in the data
  dir (`~/.promptly`), and `promptlyd run` also moves itself there after
  resolving its paths, so even a manually-started daemon releases the folder.
  Scoping is unaffected — the workspace was already passed explicitly.

### Added

- **`promptly submit` stops the daemon after a successful upload.** The capture
  is signed and submitted — nothing is left to observe — so the finish line now
  returns the machine to a clean state (and releases the level folder), printing
  `daemon stopped — 'promptly play <level>' starts your next run`. Best-effort:
  a stop failure never fails the already-durable submit, and a declined or
  failed submit leaves the daemon running. `play`/`up` relaunch it on demand.
- **The daemon shuts itself down after 15 idle minutes.** With no active capture
  session and no CLI control activity for the window, `promptlyd` stops
  gracefully through the same path as `promptly down`, logging
  `idle for 15 minutes with no active capture session; shutting down` — to the
  terminal when foregrounded, to `~/.promptly/promptlyd.log` when detached. An
  active session is never idle (a player mid-think keeps their capture no matter
  how quiet the harness is), and passive reads (`/health`, the web HUD's
  `/stream`) deliberately don't reset the clock, so a forgotten browser tab
  can't pin the daemon alive. `--idle-timeout-secs <n>` tunes the window; `0`
  disables it.

## [0.4.5] - 2026-07-14

Submit-gate parity with the server's new cross-source tolerance — no protocol or
capture change. Honest Claude Code captures routinely show token-count drift
between OTEL and JSONL on a handful of turns (OTLP exporters batch 1–5s behind
the transcript around fast bursts; a cache-writing turn splits its accounting),
and the server now verifies a capture whose disagreements are token-only and
touch at most half the turns. The CLI's fail-closed submit gate and tier preview
apply the same rule instead of demanding `--force` for a capture the server
would verify.

### Changed

- **`promptly submit` no longer gates on benign cross-source drift.** The
  integrity fail-safe requires the confirm prompt / `--force` only for
  non-benign disagreement — a `model`-field mismatch at any count, token
  disagreement on more than half the turns, or a disagreement with no named
  fields — alongside the existing implausible-turn and pacing signals. Benign
  token-only drift is still surfaced for transparency, as a one-line note
  instead of an integrity warning.
- **The pre-submit tier preview now mirrors the server's cross-source gate.**
  `projected_tier` previously ignored cross-source agreement entirely, so it
  could print `verified-eligible` for a capture the server would downgrade. It
  now caps at `unverified` (naming the disagreeing turns and fields) exactly
  when the server's trust policy would.

## [0.4.4] - 2026-07-13

Scoring-display parity with the web app's readable score rescale — no protocol or
capture change. Every competitive score (and so `promptly score` and the live
projection) now reads on the same range the site shows instead of a number 1000×
larger. Ranking is unchanged: dividing every score by the same positive constant
is order-preserving.

### Changed

- **`promptly score` and the live projection now use the readable score scale.**
  The embedded scoring-parity fixture (`vendor/parity-fixture.json`, vendored from
  the web app's `lib/scoring/parity-fixture.json`) drops `score_token_scale`
  1_000_000 → 1_000, dividing the token-efficiency numerator — and thus every
  projected score — by 1000 to match the value the server records and the
  leaderboard ranks on. The anchor parity vector is now 183.82 (was 183,823.53).

## [0.4.3] - 2026-07-13

First-run guidance polish — no protocol or scoring change. The commands a new
player meets first now name the next step at the moment it matters, so the
pair → play → submit loop is self-guiding.

### Added

- **`promptly doctor` now checks device pairing.** The setup diagnostic reports
  whether this device is paired — the one account step a ranked, verified submit
  needs — alongside the existing daemon / OTEL / manifest / runtime / web app /
  Judge0 / version checks, and points an unpaired (or corrupt-credential) device
  at `promptly pair`. Offline play still works, so it warns rather than fails.

### Changed

- **A local-nonce start names `promptly pair` instead of a stale note.** Starting
  a scored session unpaired capped the run at `unverified` but only printed a
  passive "pairing reaches verified once the cloud release lands" — a reference to
  a release that has since shipped. It now reads "run `promptly pair`; your next
  run can then rank verified", so the player learns the fix where it applies.
- **A successful `promptly pair` points at `promptly play`.** Pairing ended on
  "device paired" with no next step; it now names the command that starts a ranked
  solve, closing the first-run loop.

## [0.4.2] - 2026-07-13

### Fixed

- **Codex and Cursor captures now score `P` by prompts, not turns.** v0.4.1
  fixed the Claude Code path; the same over-count remained on the two secondary
  agentic harnesses. The Codex adapter tagged every `token_count` turn as its own
  prompt (it read only the usage lines), and the Cursor adapter gave every
  assistant bubble a unique id — so an agentic run on either signed `P = turns`.
  Both now group the turns one user prompt drives: Codex reads the rollout's
  user-message lines as prompt boundaries, and Cursor orders each conversation's
  bubbles and stamps every assistant bubble with the user bubble that preceded it
  (that bubble's unique id moves to the dedup key, so distinct bubbles still never
  collapse). Both stay best-effort/version-fragile — an unrecognized user-message
  shape simply falls back to one prompt per turn, never a miscount. The GitHub
  Copilot and OTEL paths were already correct (one Copilot request = one exchange;
  OTEL carries a real shared `prompt.id`).

## [0.4.1] - 2026-07-13

### Fixed

- **JSONL-only captures now score `P` by prompts, not turns.** v0.4.0 signs the
  session prompt count, but the Claude Code JSONL watcher had tagged every
  assistant turn as its own prompt — it never read the user-prompt lines — so a
  JSONL-only run (the common case when OTEL consent is declined or unavailable)
  still signed `P = turns`. An agentic session that drives nine turns off one
  prompt was over-penalized 9× on the multi-turn factor. The watcher now reads
  the user-prompt lines that open each prompt — distinguishing a real prompt from
  a tool-result line by its content shape — and groups the turns between them, so
  that session signs `P = 1`. Verified end-to-end against the real captured
  session that first exhibited the bug. OTEL-backed captures were already correct;
  the Codex adapter, which likewise carries no per-turn prompt id, is a known
  remaining gap tracked separately. The web scorer needs no change — it already
  clamps the signed count to `[1, turns]` and scores it verbatim.

## [0.4.0] - 2026-07-13

Scoring-fidelity and session-lifecycle fixes surfaced by a full end-to-end play
test. The headline change signs the real prompt count into the capture chain so
grading divides by prompts, not turns — an agentic run that drives many turns
off a single prompt is no longer over-penalized. **Update with `promptly
update`.**

### Added

- **The signed capture chain is now v4 and carries the session prompt count.**
  Grading's multi-turn penalty `P` is the number of *user prompts*, but the
  server used to approximate it with the turn count. The daemon now signs its
  real prompt tally into the terminal capture summary, so an agentic session
  (one prompt, many turns) scores against `P = 1` instead of `P = turns`. The
  server verifies v1–v4 and clamps the signed count to `[1, turns]`; a web
  deploy that accepts v4 precedes this release, and an older server simply falls
  back to the turn count — a graceful `verified`-badge downgrade, never a
  rejected submit.
- **`promptly test` falls back to a remote run when local tooling is missing.**
  With no local language runtime installed, `test` now packages the workspace
  and runs the public tests on the server (a paired device is required) and
  renders per-case results just like a local run. When the server does not offer
  remote testing yet, it says so plainly instead of pretending a runtime is
  needed.

### Changed

- **`promptly watch` attaches read-only instead of re-scoping the daemon.**
  Running `watch` from a folder other than the active session's used to stop the
  session, shut the daemon down, and relaunch it bound to the new folder —
  silently ending a scored run and discarding its in-memory turns. `watch` now
  only observes the running session (noting when it belongs to a different
  folder) and reports cleanly when no daemon is running.
- **The projection matches the web HUD's execution assumption (2 s).** `watch`
  and `score` previously projected the best case at the 1 s floor — exactly half
  the web HUD's number for the same run. Both now assume the HUD's 2 s default
  and label the projection as the ceiling it is. (The submit-time parity
  upper-bound is unchanged.)
- **`promptly play` prints one coherent next step** ("now: `cd` … and run
  `promptly start`") instead of the standalone `init` epilogue.

### Fixed

- **Burst turns no longer trip false cross-source disagreements.** In a
  same-model burst the correlator paired each OTEL event with the wrong JSONL
  turn, flagging spurious token mismatches that could demote a run below
  `verified` and raise an admin flag. Pairing now prefers the token-closest
  candidate before falling back to model equality.
- **Cache tokens are netted out of the cross-source input comparison**, so an
  OTEL turn that folds cache-creation into its input no longer "disagrees" with
  the JSONL split of the same turn.
- **A version-mismatched crash checkpoint is archived, so its warning fires
  once** instead of on every daemon start.
- **Arrival-order timestamp regressions no longer downgrade a capture at
  submit.** Turns can be observed slightly out of order across sources; that
  ordering jitter is no longer mistaken for tampering.
- **The per-session OTLP ingest token is no longer served over the session
  API.** It is required only inside the daemon (and to resume from
  `session.json`), and the browser never reads it, so it is stripped at the API
  boundary.

## [0.3.0] - 2026-07-12

### Added

- **Short level names for `init`, `play`, and `restart`.** Instead of spelling out
  the full slug (`promptly play stage-1-01-lru-eviction-debug`) you can now name a
  level by a one-word alias (`promptly play lru`), its number 1-20 (`promptly play
  1`), or a unique `stage-N-NN` prefix (`promptly play stage-1-01`). Resolution is
  an offline lookup against the frozen catalog, so it adds no network round-trip;
  any unrecognized name still passes straight through to the server unchanged, and
  the full slug keeps working everywhere. `promptly help` and `--help` list the
  accepted forms.

## [0.2.0] - 2026-07-09

A field audit of real play sessions found the capture pipeline wedging and
miscounting in ways that broke the first-run experience. This release overhauls
the session lifecycle, turn ingestion, and model pricing end-to-end. **Update
with `promptly update`** — the first daemon start after updating also cleans up
any session an older version left stranded.

### Added

- **Stale sessions are archived, never lost.** A session that is superseded (or
  found stranded at daemon startup) is stamped stopped, has its harness telemetry
  settings reverted in *its own* workspace, and is preserved under
  `~/.promptly/archive/<session_id>.json`.
- `GET /session/preflight` now reports a `blocking_session` (slug, workspace,
  started-at) when a previous session is still open elsewhere, and `promptly
  start` prints `closing the previous session (<slug>) — it was left open`
  before proceeding.

### Changed

- **Switching levels now ends the previous session cleanly.** `promptly start`
  in a new level (and `promptly down` / `promptly update`) stops the open
  capture session first — reverting the previous workspace's OTEL settings —
  instead of leaving it active forever. The daemon also refuses to adopt a
  marker bound to a different workspace at startup: it archives it and starts
  idle, self-healing state left behind by older versions.
- **`promptly start` refuses a folder that isn't a level workspace** ("run
  `promptly init <level>` or cd into one") *before* re-scoping the daemon to it.
  `promptly up` and `promptly watch` print a dim note but proceed.
- **`promptly watch` and `promptly score` now tell you what you're looking
  at:** a `session started <age> ago` header (flagged `(resumed)` when history
  was restored), a warning when the session is bound to a different level than
  the current folder, cache tokens on the token line, and the projection
  labeled for what it is — a ceiling that assumes a clear at floored run time.
  `watch` also de-duplicates its seed against the live stream, so a turn can
  never be counted twice on screen.
- The crash-recovery checkpoint format is now v3. The first start after
  updating discards an old checkpoint (its de-duplication keys use the old
  scheme): the session itself still stops/submits normally, but captured-turn
  history from before the update is not restored.

### Fixed

- **`promptly start` no longer fails with `daemon response wasn't understood:
  missing field 'status'`.** Two bugs compounded: a session left open on another
  level made the daemon refuse the start with a plain-error 409 the CLI couldn't
  parse, and nothing ever closed that session. The daemon now supersedes the
  stale session and starts fresh (all baseline attestation and reset
  confirmation protections unchanged), and the CLI surfaces any structured
  daemon error legibly instead of a decode failure.
- **Turns and tokens are no longer double-counted.** Claude Code writes one
  transcript line per content block, so a single assistant turn appears as 2–3
  lines with the same `message.id` and identical usage. The daemon recorded
  each line as its own turn (a real 8-turn session showed 24 turns and ~3× the
  tokens) and stray single-source entries could demote a run below the
  `verified` tier and even risk tripping the anti-fabrication pacing check.
  Turns now de-duplicate on the transcript's stable message id, and unmatched
  turns wait out the OTEL batch-export delay (~20 s pairing horizon; none for
  JSONL-only sessions) so telemetry merges into one turn instead of splitting.
- **Datestamped model ids now price at their real tier.** Claude Code reports
  ids like `claude-haiku-4-5-20251001`; the exact-match economics lookup missed
  the matrix row and floored the run at anchor parity — halving a Haiku run's
  local projected score. Ids are now canonicalized (lowercased, separators
  normalized, one trailing `-YYYYMMDD` stripped) before lookup, mirroring the
  same fix in the web app's grader, and the adapter model map resolves
  datestamped Anthropic ids too.
- The Copilot adapter no longer double-counts turns after a restart re-scan
  (its de-duplication now keys on the chat request id).
- `promptly stop` run from a different folder now says which session it ended
  and where (`stopped the session for <slug> (in <workspace>)`), instead of
  appearing to stop the current level.

## [0.1.9] - 2026-07-08

Promptly moved from its Vercel-assigned hostname to the custom domain
**`xpromptly.com`**. This release points the daemon and CLI at it. **Update with
`promptly update`** — an older binary cannot talk to the new site's HUD.

### Changed

- **The daemon's browser CORS allowlist is now `https://xpromptly.com`** (was
  `https://trypromptly.vercel.app`). The web workspace HUD reads the daemon from
  the browser, and the allowlist is an exact-origin match, so a pre-0.1.9 daemon
  is blocked from the new site and its live HUD stays dark. Loopback dev origins
  are unaffected, and `PROMPTLY_WEB_ORIGIN` / `--web-origin` still extend the
  list (use `PROMPTLY_WEB_ORIGIN=https://xpromptly.com` as a stopgap if you can't
  update immediately).
- **The CLI's default web-app URL is now `https://xpromptly.com`** (was
  `http://localhost:3000`). `pair`, `init`, `submit`, and remote `test` therefore
  need **no configuration at all** to play against production.
  - **Breaking for local development:** working against a local `npm run dev` now
    requires `PROMPTLY_API_URL=http://localhost:3000` (or `--api-url`). It used to
    be the default. `promptly doctor` still reports which app it resolved and
    whether that's local-dev or production.
- `promptly doctor`'s unreachable-local-server hint now points back at the flag /
  env var instead of naming a production URL to opt into.

### Fixed

- **Local scores now match the server again for models added since June.** The
  vendored scoring fixture had been left at its 2026-06-29 state, so it predated
  the 46-model economics matrix. `promptly score` and `promptly watch` priced any
  newer model against the baseline floor instead of its own row. The fixture is
  resynced and the model map regenerated from it. Three resolutions change:
  - `gpt-5` and `grok-4` are priced rows now, so they match exactly instead of
    being rejected as ambiguous prefixes.
  - `kimi-k2` became ambiguous (`kimi-k2-6` vs `kimi-k2-7-code`) and stays
    unresolved rather than guessing.
  - `claude-haiku-3-5` was delisted from the matrix, so it resolves to nothing and
    its turns are marked `estimated` rather than sliding onto a differently-priced
    row.

  Codex spellings the matrix prices individually (`gpt-5-codex`, `gpt-5-1-codex`,
  `gpt-5-2-codex`) now keep their own rows instead of collapsing onto one.

## [0.1.8] - 2026-07-07

### Fixed

- `promptly update` aborted with `checking whether the daemon is running: daemon
  error: HTTP 404` when another process was already listening on the daemon's
  control port (default 8765). The "stop the daemon before swapping its binary"
  step treated *any* answer on the port as a daemon, so a non-Promptly server's
  HTTP error was fatal. The port is now classified three ways — a healthy daemon
  (stopped), a free port (nothing to do), or a foreign process (noted, and the
  update proceeds: our daemon isn't running, so its binary is free to replace).
  `promptly down` and `promptly restart` share the fix.
- `promptly start` / `play` / `up` now fail with a clear message when the control
  port is held by another process — naming the port and the `--api-port <port>`
  escape hatch — instead of a raw `HTTP 404`.

### Changed

- The CLI grew a set of terminal visuals, shared across commands and honoring
  `NO_COLOR`/`--no-color`/non-TTY output as before:
  - `promptly score` renders as a card under a section rule, with the projected
    score on its own line, a colored **correctness meter**, and a **token
    composition bar** (input `█` / output `▓` / thinking `▒`) under the token
    breakdown.
  - `promptly watch` opens with a section rule and its in-place scoreboard is
    now two lines: the running totals, then a **per-turn burn sparkline**
    (`▁▂▃▅▇`, last 24 turns) beside the live projected score.
  - `promptly status` shows the captured token mix as the same composition bar
    under the totals.
  - `promptly test` closes with a **pass-rate meter** next to `N/M passed`,
    green/yellow/red by how close the suite is to passing.
  - `promptly doctor` opens with a section rule and its verdict line is fronted
    by a compact per-check mark strip (`✓!✗` in each check's color).
  - `promptly submit` renders the parity comparison as two aligned bars —
    local best-case projection vs the server's grade — scaled to the larger.
  - `promptly help` gains a brand mark, numbered quick-start steps, and section
    headings extended by dim rules.

## [0.1.7] - 2026-07-07

### Changed

- **Terminal visual polish across the CLI.** New shared visual primitives — a
  meter, sparkline, token-mix bar, and section rule — applied in a sweep across
  `help`, `score`, `watch`, `status`, `test`, `doctor`, and `submit`.

## [0.1.6] - 2026-07-04

### Fixed

- `promptly submit` and `promptly score` failed with **"no active capture
  session"** after `promptly stop`, breaking the documented finish line
  (`stop` → `submit`) that the `stop` command itself points you to. Stopping a
  session only closes its capture window — the signed turns and the bound attempt
  nonce stay on the marker — so both commands now read a stopped session, not only
  an open one. They still fail on a truly idle daemon (nothing started), and
  `promptly watch` still requires an open session (it is a live follow).

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

## [0.1.4] - 2026-07-03

The first anti-cheat hardening pass: authenticated ingest, server-attested
baselines, and the device-signed turn chain later versions extend.

### Added

- **Authenticated OTLP ingest.** The harness bootstrap mints a per-session
  ingest token and the receiver rejects any post that doesn't present it.
- **Server-attested kit baseline** verified before a fresh start.
- **The device-signed (v3) turn chain and capture summary** are signed and
  uploaded with every ranked submit.
- **Implausible turn pacing is flagged** before a ranked submit, and the
  capture's projected trust tier is shown before you confirm.
- The anti-cheat layers and the verified-eligibility matrix are documented in
  the README.

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

[Unreleased]: https://github.com/AidanHT/promptly-daemon/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.8...v1.0.0
[0.4.8]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.7...v0.4.8
[0.4.7]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.6...v0.4.7
[0.4.6]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.5...v0.4.6
[0.4.5]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/AidanHT/promptly-daemon/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/AidanHT/promptly-daemon/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/AidanHT/promptly-daemon/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.9...v0.2.0
[0.1.9]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/AidanHT/promptly-daemon/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AidanHT/promptly-daemon/releases/tag/v0.1.0
