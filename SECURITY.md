# Security

`bhx` drives a **real, logged-in browser** on your machine. Treat it like a
credential-holding automation tool, not like a sandboxed library.

## Reporting a vulnerability

Email **laik.lj@me.com** (private report preferred for anything exploitable).
For anything that is a bug rather than a vulnerability, open a normal issue.
This is a pre-1.0, solo-maintained project: no SLA, but security reports get
priority, and only the latest release is supported.

## What the harness guarantees in code

| Area | Behaviour | Where |
|---|---|---|
| Credentials | Task specs, `task.json`, the `steps` table, events and traces contain only `${cred:name}`; values live in memory or in `BH_CRED_<NAME>` at run time. A scheduled task with a missing value refuses to run instead of typing the placeholder into a page. | [data-model.md §4](docs/data-model.md) |
| Password fields | Values are suppressed in the accessibility observation; forced display still yields `***`. | `crates/bh-observe` |
| Navigation scope | `allowed_hosts` is checked **before the first action** of a batch; an out-of-bounds navigation returns 403 / fails the task and the page does not move. | [protocol.md §10](docs/protocol.md) |
| Side effects | Actions are graded `read_only` / `reversible` / `irreversible`; irreversible steps stop at `waiting_approval` until a caller approves. Auto-repair only ever touches `read_only` steps. | [architecture.md §3.5](docs/architecture.md) |
| JS injection | `browser_evaluate` / `POST /v1/sessions/{id}/evaluate` run in an **isolated world** by default (DOM visible, page `window.*` not). Main world is opt-in. Task-declared `Condition::Js` runs in main world because it is part of the task author's spec, not caller-supplied code. | [protocol.md §10](docs/protocol.md) |
| Downloads | Written to a per-task or per-session directory, filename sanitized. Artifact reads (`artifact_fetch`, `GET /v1/tasks/{id}/artifacts/{name}`) canonicalize the path and refuse anything outside that task's `artifacts/`, and are capped by `max_bytes` (default 5 MiB). | `crates/bh-browser`, `crates/bh-server` |
| Browser state | Each task gets its own `user-data-dir` under `<home>/profiles/<task-id>`; cookies are not shared between tasks. Attaching to a remote browser (`BH_CDP_URL`) closes only the tabs the harness opened. | [data-model.md §2](docs/data-model.md) |
| Model egress | Model calls go only to endpoints you configure (`BH_LLM_URL`, `BH_VLM_URL`, or a local sidecar command). No telemetry, no third-party calls. | [architecture.md §3.6](docs/architecture.md) |
| No `unsafe`; every subprocess uses an argument vector, never a shell string | The workspace has no `unsafe` code. The only processes started are Chrome, the model sidecar (`python3 <script>`), and — for crash recovery — `pgrep`/`kill` against the harness's own profile path. No user-supplied string is ever interpolated into a command. | `crates/bh-browser`, `crates/bh-model/src/laya.rs`, `crates/bh-task/src/lib.rs` |

## What is *not* protected — read this before pointing it at production

- **Screenshots are the whole screen.** `policy.trace_screenshots`,
  `policy.semantic_verify.include_screenshot`, `browser_screenshot` and session
  `observe` capture whatever is visible — tokens, session cookies rendered in the
  UI, other tabs' content is not included but your logged-in identity is. Trace
  packages copy those bytes into a folder you may share. They are opt-in for
  full-sequence capture; failed steps are always screenshotted.
- **Observations go to the model when you enable a model step.** Element names,
  text blocks and (opt-in) screenshots leave your machine if the configured
  endpoint is remote. The local sidecar path exists for exactly this reason.
- **CDP is unauthenticated.** `BH_CDP_URL=http://host:9222` grants full browser
  control (all profiles, all cookies, arbitrary JS). Bind the debug port to
  localhost or a private network; never expose it to the public internet.
- **The agent writes the plan.** In `mode=auto` a model proposes the steps. The
  harness validates them (no new script actions, no out-of-scope hosts, ids and
  success conditions preserved, one dispatch only), but the *intent* — which form
  gets submitted — comes from the caller. Keep `allowed_hosts` tight and leave
  irreversible actions behind approval.
- **Task state is plaintext on disk.** `bhx.db`, `events.jsonl` and artifacts are
  not encrypted; protect `<home>` (default `./.bhx`) with filesystem permissions.
- **Chromium is the trusted computing base.** Browser CVEs apply to you; keep the
  browser updated.

## Known gaps (as of v0.1 protocol)

- No per-task network egress control beyond host allowlists (no scheme/port
  policy, no redirect re-check).
- No secrets manager integration: credentials come from flags or environment, so
  they can appear in your shell history or process list (`--cred name=value`).
- No audit signature or tamper-evidence for events and artifacts; the sha256 in
  the trace is a content hash, not an attestation.
- No multi-tenant isolation: one `bhx serve` process = one home directory = one
  trust domain. Do not share a server between untrusted callers.
