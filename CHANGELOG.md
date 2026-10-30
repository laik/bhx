# Changelog

Notable changes to the harness, the task protocol and the front doors.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
crates follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**The wire protocol is `0.1` and is not stable.** A task JSON written today may
need edits before `1.0`; every breaking change is listed below with the
migration. Protocol history itself is documented in
[docs/protocol.md](docs/protocol.md) and per-phase evidence in
[docs/roadmap.md](docs/roadmap.md).

## [Unreleased]

### Changed
- **Documented minimum Rust is now 1.89, not 1.85.** Nothing in the workspace
  requires it, but the pinned dependencies do: `rusqlite` 0.41 needs 1.89,
  `libsqlite3-sys` 0.39 and `sha2` 0.11 need 1.88. `cargo build --workspace`
  fails at dependency resolution on 1.85. Measured and recorded in
  [docs/benchmarks.md](docs/benchmarks.md) (2026-10-30 section).

### Fixed
- Docs corrected against the code: the task-directory layout (step and
  verification screenshots live in `tasks/<id>/artifacts/`, indexed in the
  `artifacts` table; `obs/` and `shots/` remain empty), the MCP tool list
  (`browser_create_session` was missing from the normative list in
  [docs/protocol.md](docs/protocol.md)), the `Locate` capability (now used by
  ambiguity arbitration, no longer "not on any path"), `bhx serve` (described as
  unimplemented in [docs/data-model.md](docs/data-model.md)), the `chaos/` test
  site (never built; injection is a `server.py` endpoint), the phase-4 suite
  count, and the subprocess claim in [SECURITY.md](SECURITY.md).
- README/上手示例 transcripts replaced with real output. The previous numbers were
  not any recorded run.

### Planned
- Protocol version negotiation between clients and `bhx serve`
- `GET /v1/events` global event stream
- Recording/replay for model traffic in the test suites
- `chaos` rules `on: layout` and `on: move`
- `config.toml` (field names are already specified in [docs/data-model.md §7](docs/data-model.md))
- Packaging: crates.io / Homebrew / prebuilt binaries
- Linux browser matrix in CI

## [0.1.0] - 2026-10-29

First public snapshot. Everything below is implemented and covered by the
acceptance suites in [testkit/](testkit) unless marked otherwise.

### Added — browser runtime (Phase 1, 2026-10-28)
- Custom thin CDP client over WebSocket (`bh-cdp`, 206 lines): request/response, event
  subscription, target/session routing. No Playwright, no chromiumoxide
  (rationale: [docs/architecture.md §4](docs/architecture.md)).
- Browser lifecycle: launch with an isolated `user-data-dir`, or **attach** to a
  running Chrome via `BH_CDP_URL` (`http://host:port` or `ws://…`) closing only
  the tabs it opened.
- Navigation, input (keyboard/mouse with press-release pairing), downloads to a
  task directory, dialog handling, JavaScript evaluation in isolated or main world.
- Observation engine: accessibility tree → element index (`e1`, `e2`, …) with
  element budgets, visibility filtering, password redaction and content hashing.
- Action runtime: locator resolution (`role`+`name`, CSS, text, coordinates),
  actionability checks, condition waiting, cancellation under 200 ms with no
  residual pressed keys.
- MCP stdio server with page-level `browser_*` tools; CLI `observe` / `run`.

### Added — task engine (Phase 2, 2026-10-28)
- Task JSON protocol `0.1`: steps with preconditions, success conditions,
  `fail_when`, timeouts, side-effect grades, per-task budgets
  (`max_model_calls`, `max_wall_ms`, `allowed_hosts`).
- SQLite-backed state machine with per-step checkpoints, approvals
  (`waiting_approval`), artifact index and crash recovery: `kill -9` mid-step →
  `task approve` → `task resume` reruns only the interrupted step and keeps prior
  artifacts.
- Credentials kept as `${cred:name}` everywhere on disk; values only in memory or
  `BH_CRED_<NAME>`.
- CLI `task run|ls|show|resume|approve|reject|cancel|artifacts|recover`; MCP
  `task_*` tools; chaos-tested recovery (popup + HTTP 500 mid-task).

### Added — hybrid execution (Phase 3, 2026-10-28)
- Interactive Controller: 30 Hz rule loop with the model out of the loop; policy
  hot-update at tick boundaries, input release on timeout/stop.
- Vision Executor: screenshot → model proposal → normalized bbox → CSS coordinates
  → action → state verification. Rejects coordinates whose bbox does not match
  `viewport × dpr` instead of guessing.
- Task Router with per-step reroute (`fallback_visual`) from a failed DOM locator
  to the vision path.
- Model layer: typed `Plan` / `Locate` / `VisualLocate` / `Verify` / `Repair`
  capabilities, call budgets, proposal validation, OpenAI-compatible adapters.

### Added — interfaces and model integration (Phase 4, in progress)
- `bhx serve`: REST front door (`/v1/tasks`, `/v1/sessions`, artifacts, health)
  plus a resumable WebSocket event stream (`?after=<seq>`, tail-read from storage).
- Session-level page control over HTTP — the mirror of `browser_*` for agents that
  cannot speak stdio.
- `bhx plan` (goal → observe → validated steps → run) and engine-side planning for
  specs with `steps: []` + `mode=auto` + `start_url`; planning failures dispatch
  zero steps.
- Optional `policy.semantic_verify` (second opinion after mechanical conditions
  pass; `not_achieved` fails the task while keeping step results, `uncertain`
  escalates to a human) with opt-in screenshot input.
- Step-level `retry.on_exhausted = "ask_model"` auto-repair, restricted to
  `read_only` steps that need no approval, with strict replacement validation.
- Optional local decision-model sidecar adapter (`sidecars/laya/decide.py`),
  ranked after remote models by default; measured zero-shot accuracy is documented
  as insufficient for these pages ([docs/benchmarks.md](docs/benchmarks.md)).
- MCP `interactive_*` jobs: start without waiting, status, policy update with
  strictly increasing version, stop that releases all inputs.

### Added — operations (Phase 5/6 slices, 2026-10-28/29)
- `bhx stats`: success rate, duration p50/p95, model calls per task,
  failed-action rate — all recomputed from the store, not from memory.
- `bhx trace`: event timeline plus a self-contained package (`trace.json`,
  artifacts, and a single-file `trace.html` player with real-time replay).
  Per-step screenshots are opt-in (`policy.trace_screenshots`); failed steps are
  always captured.
- `bhx schedule add|ls|rm|enable|disable|run`: task snapshots in SQLite, slots
  consumed before running (a failure never becomes a blind retry after restart),
  credentials by name only, serial by default with `--concurrency 1..=8`.
- Ambiguous-locator arbitration: identical labels are abandoned without a model
  call; distinguishable candidates may be chosen by a model above a threshold, and
  every arbitration is recorded in the action event.
- Model-selected strategy inside the interactive controller (decisions at 1 Hz,
  rules still executing at 30 Hz); predicate batching reports `evals` and tick
  percentiles.
- Cross-agent acceptance over MCP with Claude Code and Pi (documented, run by hand:
  `testkit/agent-acceptance.sh`).

### Known limitations in 0.1.0
- Built and measured on macOS only; the Linux/CI browser matrix has not been run.
- Protocol `0.1` has no version negotiation; `bhx serve` is single-tenant.
- No packaging: build from source (`cargo build --release`).
- Model quality is not measured by the suites — deterministic stand-ins test the
  harness contract, geometry and validation paths. Real-model accuracy numbers,
  where they exist, are in [docs/benchmarks.md](docs/benchmarks.md) and marked as
  zero-shot samples.
- `chaos` rules `on: layout` / `on: move` and the `config.toml` loader described
  in the docs are not implemented; configuration is flags + environment variables.

### Migration notes
- None — first release. To pin yourself: check `bhx --version` and
  `"protocol_version": "0.1"` in your task JSON.
