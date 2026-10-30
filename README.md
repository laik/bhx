# my-browser-harness (`bhx`)

**A browser harness for AI agents.** It does not think for the agent — it makes
"observe → locate → act → verify → recover" boring and reliable. Games are one
workload it happens to survive, not its definition.

```
AI agent   ──►   bhx harness   ──►   Chromium (CDP)
 (decides)        (gets it done)      (real browser)
```

English · **[中文](README.zh-CN.md)** · [Docs](docs/README.md) ·
[Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Changelog](CHANGELOG.md)

---

A browser *agent* asks a model what to do at every step. A browser *harness*
does the opposite: the plan is compiled once into a declarative task, execution
takes zero model calls, and every action carries
its own success condition. The result is something you can schedule, diff,
replay and audit — not something you hope worked.

```bash
python3 testkit/server.py --port 8766 &        # the local test site
./target/release/bhx task run testkit/sites/admin/task.json \
    --cred username=admin --cred password=admin123     # login → filter → export CSV
# SUCCEEDED in 706 ms  (task 01M65XT9X1…)
#   s1         succeeded  verified=true 113 ms
#   s2         succeeded  verified=true  40 ms
#   s3         succeeded  verified=true  35 ms
# $ bhx task show <task-id> --json | jq .model_calls
# 0
```

That last line is the point of the project: a three-step job with a login, a
server-side filter and a CSV download, verified step by step, with **zero model
calls**. The transcript above is real output, not a sketch
([docs/benchmarks.md](docs/benchmarks.md) has the measurements).

## What it gives you

| Capability | What it means | Component |
|---|---|---|
| Work | Finish a multi-step web job and produce files/data | Task Engine + DOM Executor |
| Execution | Complete the same job through DOM, pixels, or both | Task Router + Vision Executor |
| Realtime | Keep acting at 30 Hz while the model is offline | Interactive Controller |

Everything else follows from those three:

- **Durable tasks.** SQLite state machine, per-step checkpoints, approvals for
  irreversible actions, artifact index, crash recovery (`kill -9` → `task resume`
  reruns only the interrupted step).
- **Three front doors, one engine.** CLI (`bhx`), MCP stdio server for agents
  (Claude Code, Pi, Codex, …), REST + WebSocket with resumable event streams.
- **A model layer with contracts, not prompts.** `Plan` / `Locate` /
  `VisualLocate` / `Verify` / `Repair` are typed capabilities behind a router with
  per-task call budgets, proposal validation and fallback chains. Any
  OpenAI-compatible endpoint works; an optional local decision model runs as a
  sidecar. No adapter configured is a loud failure, never a silent guess.
- **Proof, not vibes.** `bhx stats` recomputes success rate, latency percentiles
  and failed-action rate from the store; `bhx trace` writes a self-contained
  package with a single-file HTML replay of what actually happened.
- **Scheduled work.** `bhx schedule add task.json --every 30m` keeps credentials
  as names in the DB and reads their values from the environment at run time.

## Numbers

Measured on an Apple M5 laptop against the local test sites
([full log](docs/benchmarks.md)):

| | |
|---|---|
| CDP round trip | 0.1–0.5 ms |
| Single action p95 | 17.2 ms (97% of it is one `mouseMoved`) |
| Real workflow, 3 consecutive runs (`bhx run`, debug build) | 202 / 271 / 218 ms, **0 model calls** |
| Same job as a persisted task (`bhx task run`, release build) | 591–774 ms warm, ~1.4 s cold, **0 model calls** |
| Cancel effective in | 85 ms, no stuck keys |
| Interactive controller 30 s soak | 909 ticks, tick p95 34 ms, CPU 3.6% |
| Drag landing error (vision path) | 0.57–0.70 px (budget: ≤ 4 px) |
| Cross-agent acceptance | Claude Code 10 s, Pi + local Qwen 16–22 s, identical CSV sha256 |

## Requirements

- Rust 1.89+ (edition 2024; developed on 1.93) — `rusqlite` needs 1.89, so older
currently fails to resolve dependencies
- Google Chrome or Chromium — `BH_CHROME=/path/to/chrome` if it is not
  `/Applications/Google Chrome.app/…`
- Python 3 (stdlib only) for the test sites and acceptance suites
- No cloud service: the harness launches a local Chrome, or attaches to one that
  is already running (`BH_CDP_URL`)

## Build and run

```bash
cargo build --release
./target/release/bhx --help

# Look at a page: element indexes (e1, e2, …) plus state text
./target/release/bhx observe --url https://example.com

# Run a step script: no task state, gone when it finishes
./target/release/bhx run testkit/sites/admin/task.json

# Run it as a task: SQLite state, checkpoints, artifacts, approvals, resume
./target/release/bhx task run testkit/sites/admin/task.json

# History, metrics and a shareable replay
./target/release/bhx task ls
./target/release/bhx stats
./target/release/bhx trace <task-id> --out ./trace-<task-id>   # open trace.html

# Talk to it as an MCP server
./target/release/bhx mcp --home ./.bhx
```

Task state lives in `./.bhx/tasks/<id>/{task.json,events.jsonl,artifacts/,obs/,shots/}`;
credentials are stripped before anything touches disk.

Register it with an agent (project `.mcp.json` or user config):

```json
{ "mcpServers": { "bhx": { "command": "/abs/path/to/bhx", "args": ["mcp"] } } }
```

A ready-made agent skill (how to drive `bhx` from a CLI agent) is in
[.claude/skills/bhx/SKILL.md](.claude/skills/bhx/SKILL.md) — copy it into your project.

New to the project? **[docs/getting-started.md](docs/getting-started.md)** walks the
first task end to end (in Chinese; the commands run as written).

## Test it yourself

The whole suite runs offline against local sites, with deterministic stand-ins in
place of the models — so it tests the harness, not a model's day.

```bash
cargo test --workspace        # unit tests, no browser
./testkit/run-phase1.sh       # browser runtime + minimal MCP
./testkit/run-phase2.sh       # task engine, crash recovery
./testkit/run-phase3.sh       # interactive controller + vision path
./testkit/run-phase4.sh       # REST/WS, planning, verify, repair, scheduling, trace
```

## Documentation

Deep docs are currently **Chinese**; English translations are a good first issue.

| Doc | Content |
|---|---|
| [docs/README.md](docs/README.md) | Index and reading order |
| [docs/getting-started.md](docs/getting-started.md) | First task, end to end, copy-pasteable |
| [docs/requirements.md](docs/requirements.md) | Product definition, capability model, non-goals, safety, acceptance |
| [docs/architecture.md](docs/architecture.md) | Layers, executors, task engine, technology choices, risks |
| [docs/protocol.md](docs/protocol.md) | Task / action / observation / event / model-call protocol (normative) |
| [docs/interfaces.md](docs/interfaces.md) | Traits and invariants between crates |
| [docs/data-model.md](docs/data-model.md) | SQLite schema, directory layout, configuration, recovery |
| [docs/walkthrough.md](docs/walkthrough.md) | One real task followed from entry to artifact |
| [docs/testing.md](docs/testing.md) | Test sites, chaos injection, suites, metrics |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured latencies and a local decision-model baseline |
| [docs/roadmap.md](docs/roadmap.md) | Phases with exit criteria and what actually landed |

![architecture](docs/assets/architecture.svg)

## Status

Design is at v0.2; the wire protocol is `0.1` and still evolving — expect breaking
changes before 1.0, with migrations noted in the [changelog](CHANGELOG.md).

- **Phase 0 — feasibility** ✅ a custom thin CDP client (`bh-cdp`, 206 lines)
  instead of Playwright or chromiumoxide
- **Phase 1 — browser runtime + minimal MCP** ✅
- **Phase 2 — task engine** ✅ (checkpoints, approvals, recovery, artifact hashing)
- **Phase 3 — hybrid execution** ✅ (30 Hz rules loop, vision path with bbox
  validation, DOM→vision reroute)
- **Phase 4 — model and interface integration** ⏳ in progress: REST/WS, explicit
  and engine-side planning, semantic verify, auto-repair, Laya sidecar,
  session-level page control, MCP interactive jobs all landed; recording/replay of
  real model traffic and the third cross-agent acceptance are open
- **Phase 5/6 early slices** predicate batching, locator arbitration with a model,
  run history + trace player, scheduling, attaching to a remote browser

Open gaps worth knowing about: Linux/CI browser matrix is not exercised yet,
there is no packaging (no `cargo install` crate, no brew formula), the protocol has
no version negotiation, and only one human-in-the-loop approval flow exists.
Help wanted on all four — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Repo layout

| Path | Contents |
|---|---|
| `crates/bh-core` | Protocol types: tasks, steps, actions, locators, conditions, observations, errors |
| `crates/bh-cdp` | Thin CDP client: WebSocket, request/response, event broadcast |
| `crates/bh-browser` | Browser lifecycle, sessions, navigation, input, downloads, dialogs |
| `crates/bh-observe` | Accessibility tree → element index (budgets, visibility, redaction, hashing) |
| `crates/bh-action` | Locator resolution, actionability checks, dispatch, conditions, cancellation |
| `crates/bh-exec` | Step executor, vision executor, interactive controller, locator arbitration |
| `crates/bh-model` | Capability contracts, call budgets, proposal validation, OpenAI-compatible adapter, optional local sidecar |
| `crates/bh-task` | Task engine: SQLite state machine, checkpoints, approvals, artifacts, recovery, scheduling |
| `crates/bh-server` | MCP stdio server + REST/WS front door |
| `crates/bhx` | CLI binary: `observe` / `run` / `task` / `plan` / `stats` / `trace` / `schedule` / `serve` / `mcp` |
| `testkit/` | Local sites, test server, deterministic model stand-ins, acceptance suites |
| `sidecars/laya/` | Optional local decision-model sidecar (torch/MPS) |
| `spike/` | Throwaway Phase 0 code that produced the latency baseline; not part of the product |

## Naming

The project is **my-browser-harness**; the binary, the crate names and this
repository are **`bhx`**. [browser-use](https://github.com/browser-use)'s
`browser-harness` CLI is a different, unrelated project — the short name avoids
PATH collisions and confusion. See
[docs/architecture.md §8](docs/architecture.md#8-命名).

## The one-line acceptance test

> Given a real back-office job (login → filter → export CSV), it completes
> independently three times in a row with ≤ 2 model calls, and still recovers when
> a popup and an API timeout are injected mid-run.

That is what the suites in `testkit/` measure, against what the test server
recorded — not against what the page claims happened.

## License

MIT — see [LICENSE](LICENSE).
