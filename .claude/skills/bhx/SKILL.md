---
name: bhx
description: "Drive a real browser through the my-browser-harness engine (bhx): login, fill forms, filter tables, export/download files, verify results, resume after crashes, run on a schedule. Use for repeatable browser work that must be checked against the page, not for one-off public page reads (use curl) or ad-hoc poking (use the browser-harness skill)."
---

# bhx — task-level browser automation

`bhx` runs **declared workflows** with success conditions, checkpoints, artifacts and an
audit trail. Prefer it when the same browser job will run more than once or its result must
be provable. For exploratory clicking, the `browser-harness` skill is the right tool.

Repo: `/Users/dxp/workspace/go/src/github.com/laik/bhx`

## Locate the binary

```bash
command -v bhx || ls ./target/release/bhx
```

If missing, build: `cargo build --release` (Rust workspace, ~1 min).
Otherwise use `BHX=./target/release/bhx` below.

## The loop

1. **Observe** the page to get real element names — never guess locators.

```bash
$BHX observe --url http://127.0.0.1:8766/admin/ --max-elements 40
# e1  textbox "用户名" / e3  button "登录"
```

2. **Write a task JSON** with a success condition per step (mandatory). Start from
   `testkit/sites/admin/task.json`; minimal shape:

```json
{
  "protocol_version": "0.1",
  "kind": "workflow",
  "goal": "登录后台，导出失败订单",
  "policy": { "max_model_calls": 0, "max_wall_ms": 60000, "allowed_hosts": ["127.0.0.1"] },
  "steps": [
    {
      "id": "s1",
      "goal": "登录",
      "actions": [
        { "type": "navigate", "url": "http://127.0.0.1:8766/" },
        { "type": "type", "target": { "role": "textbox", "name": "用户名" }, "text": "admin", "clear_first": true },
        { "type": "type", "target": { "role": "textbox", "name": "密码" }, "text": "admin123", "clear_first": true },
        { "type": "click", "target": { "role": "button", "name": "登录" } }
      ],
      "success": { "type": "element_visible", "locator": { "role": "button", "name": "导出 CSV" } },
      "timeout_ms": 20000
    }
  ]
}
```

Locators: `role`+`name` > `placeholder` > `text` > `css` (semantic first, resilient to redesign).
Success types: `element_visible`, `js`, `download_completed`, `url_matches`, `text_present`, …
Download step: `"success": {"type":"download_completed","filename_regex":"^orders-.*\\.csv$"}` —
the file lands in the task's sandboxed `artifacts/` with a sanitized name.

3. **Run, then read the result** — exit code plus the JSON summary:

```bash
$BHX task run task.json --home ./.bhx          # persisted: checkpoints, artifacts, approvals
$BHX task show <id> --json                     # per-step status, attempts, errors
$BHX task artifacts <id>
```

4. **On failure**, do not just rerun: `task show` names the failing step and error.
   `$BHX task resume <id>` continues from the checkpoint. A crash mid-run does not retry
   blindly — irreversible actions stay `waiting_human` until `task approve <id>`.

## Credentials and safety (non-negotiable)

- Never write a real password into task JSON. Use `"text": "${cred:password}"`; the store
  strips secrets before writing, and password inputs are recorded as `"***"`.
- Scheduled runs store only credential *names*; values come from `BH_CRED_<NAME>` at run
  time. A missing value refuses the whole run — it is never typed literally.
- Keep `allowed_hosts` narrow (the sites the task actually needs).
- Irreversible actions (submit/delete/pay) must reach `waiting_human`; get explicit user
  approval before `task approve`.
- Screenshots (trace bundles, `include_screenshot` verification) contain whatever was on
  screen, possibly personal data. Don't publish or upload them without asking.
- Login walls, MFA and captchas are never auto-bypassed; they surface as `waiting_human`.

## Models (optional)

Deterministic workflows need zero model calls. Add a model only for planning, repair, or
semantic verification — any OpenAI-compatible endpoint:

```bash
BH_LLM_URL=http://127.0.0.1:8791 $BHX plan --goal "…" --url … --out plan.json --run
# policy.semantic_verify: {goal, threshold, on_uncertain, include_screenshot}
# retry.on_exhausted = "ask_model" repairs a failing step
```

`include_screenshot: true` sends a viewport PNG to the verifier (same bytes are stored and
sha256-logged). Text-only backends (Laya) must refuse it — never fake a visual verdict.

## Scheduling and remote browsers

```bash
$BHX schedule add task.json --every 30m
BH_CRED_PASSWORD=… $BHX schedule run --concurrency 4      # 1..8, default 1
$BHX schedule run --once                                  # CI
```

Remote Chrome: launch a **dedicated profile on a free port** (Chrome 136+ 404s on the
default profile), check the endpoint first, then attach:

```bash
curl -s http://127.0.0.1:9333/json/version >/dev/null || \
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    --remote-debugging-port=9333 --user-data-dir=/tmp/bhx-profile-9333 about:blank &
BH_CDP_URL=http://127.0.0.1:9333 $BHX task run task.json
```

Only tabs opened by bhx are closed; the browser process is left alone.

## Prefer MCP when available

If `task_run`, `task_status`, `browser_observe` tools are registered, call them instead of
the CLI — same engine, structured results. Register with:

```json
{ "mcpServers": { "bhx": { "command": "/abs/path/bhx", "args": ["mcp", "--home", "/abs/path/.bhx"] } } }
```

Task layer (`task_run`/`task_status`/`task_resume`/`task_approve`) for declared work;
low-level `browser_*` only while exploring; `interactive_*` for continuous control.

## Evidence and self-test

```bash
$BHX stats --json                 # success rate, p50/p95, model calls, failed-action rate
$BHX trace <id> --out ./trace-<id>   # self-contained replay package (trace.html)
./testkit/run-phase4.sh           # 11 groups, real site truth, injected failures
```

Never claim a workflow works without a successful `task run` and, for changes to the
harness itself, without the relevant smoke group passing. Docs: `docs/protocol.md`
(schema), `docs/interfaces.md` (invariants), `docs/walkthrough.md`.
