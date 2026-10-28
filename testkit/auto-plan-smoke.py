#!/usr/bin/env python3
"""End-to-end checks for engine-side auto planning.

A workflow with no steps and `mode=auto` is planned by the engine against the
real page at `start_url`, persisted to `task.json`, and only then executed. The
model end is testkit/planner.py, a deterministic rule; what is under test is the
harness contract:

- the planner sees the page the task will run against;
- the planned steps are written to disk (with credential placeholders) before
  the first step runs, so resume does not plan again;
- the planning call is charged to the task's model budget;
- bad proposals, unsafe navigation, unknown credentials, a missing adapter and
  an exhausted budget all fail the task before any step runs;
- neither `task.json`, the steps table nor the event log contains a secret.

Exit code 0 only if every check passed.
"""

import argparse
import hashlib
import http.client
import json
import os
import sqlite3
import subprocess
import sys
import urllib.parse

PASS = 0
FAIL = 0
GOAL = "登录后台，筛选失败订单，导出 CSV"


def ok(msg):
    global PASS
    PASS += 1
    print(f"ok    {msg}")


def bad(msg):
    global FAIL
    FAIL += 1
    print(f"FAIL  {msg}")


def check(cond, msg):
    ok(msg) if cond else bad(msg)


def request(base, method, path, body=None, timeout=15):
    u = urllib.parse.urlparse(base)
    conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=timeout)
    headers = {"Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    conn.request(method, u.path.rstrip("/") + path, body=data, headers=headers)
    r = conn.getresponse()
    payload = r.read()
    status = r.status
    conn.close()
    try:
        parsed = json.loads(payload) if payload else None
    except json.JSONDecodeError:
        parsed = payload.decode("utf-8", "replace")
    return status, parsed


def run(cmd, env=None, timeout=180):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=env)


def spec(start_url, *, allowed=None, max_calls=2, mode="auto", steps=None, goal=GOAL):
    return {
        "protocol_version": "0.1",
        "kind": "workflow",
        "goal": goal,
        "mode": mode,
        "start_url": start_url,
        "policy": {
            "max_model_calls": max_calls,
            "max_wall_ms": 60000,
            "allowed_hosts": allowed if allowed is not None else ["127.0.0.1"],
        },
        "steps": steps if steps is not None else [],
    }


def write_spec(path, value):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(value, f, ensure_ascii=False)


def task_id(home, bhx):
    ls = run([bhx, "task", "ls", "--home", home, "--limit", "5"])
    for line in ls.stdout.strip().splitlines():
        first = line.split()[0] if line.split() else ""
        # ULIDs are the only task ids the harness creates; this ignores the
        # "(no tasks in ...)" banner without inventing a parser for it.
        if len(first) == 26 and first.isalnum():
            return first
    return None


def show(home, bhx, tid):
    out = run([bhx, "task", "show", tid, "--json", "--home", home])
    return json.loads(out.stdout)


def step_rows(home, tid):
    path = os.path.join(home, "bhx.db")
    if not os.path.exists(path):
        return []
    conn = sqlite3.connect(path)
    try:
        return conn.execute(
            "select step_id, spec_json from steps where task_id = ?", (tid,)
        ).fetchall()
    finally:
        conn.close()


def events(home, tid):
    path = os.path.join(home, "tasks", tid, "events.jsonl")
    if not os.path.exists(path):
        return []
    return [json.loads(line) for line in open(path, encoding="utf-8") if line.strip()]


def task_json(home, tid):
    path = os.path.join(home, "tasks", tid, "task.json")
    return json.load(open(path, encoding="utf-8")) if os.path.exists(path) else {}


def artifact(home, bhx, tid):
    arts = run([bhx, "task", "artifacts", tid, "--home", home])
    rows = [line.split("\t") for line in arts.stdout.splitlines() if "\t" in line]
    if not rows:
        return None
    return rows[0]


def base_env(planner_url=None):
    env = {k: v for k, v in os.environ.items()
           if k not in ("BH_LLM_URL", "BH_VLM_URL", "BH_LLM_MODEL", "BH_VLM_MODEL")}
    if planner_url:
        env["BH_LLM_URL"] = planner_url + "/v1/chat/completions"
    return env


def reset_planner(planner, mode="good"):
    status, body = request(planner, "POST", "/__mode", {"mode": mode})
    return status == 200 and body.get("mode") == mode


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--planner", required=True)
    ap.add_argument("--tmp", required=True)
    args = ap.parse_args()
    os.makedirs(args.tmp, exist_ok=True)
    planner_url = args.planner.rstrip("/")

    # ---------------------------------------------------------------- success
    reset_planner(planner_url, "good")
    request(args.site, "POST", "/admin/__reset")
    home = os.path.join(args.tmp, "good-home")
    path = os.path.join(args.tmp, "good.json")
    write_spec(path, spec(args.site + "/"))
    r = run([args.bhx, "task", "run", path, "--cred", "username=admin",
             "--cred", "password=admin123", "--home", home],
            env=base_env(planner_url))
    check(r.returncode == 0, f"auto-plan task exited 0 (got {r.returncode})")
    if r.returncode != 0:
        print(r.stdout[-2000:], file=sys.stderr)
        print(r.stderr[-2000:], file=sys.stderr)

    tid = task_id(home, args.bhx)
    check(tid is not None, "auto-plan task left a task in the store")
    if not tid:
        return finish()

    st, last = request(planner_url, "GET", "/__last")
    check(st == 200 and last["calls"] == 1, f"planner was called once (got {last.get('calls')})")
    check(f"URL: {args.site}/" in last["text"], "planner saw the start_url page")
    check(f"Goal: {GOAL}" in last["text"], "planner saw the task goal")

    t = show(home, args.bhx, tid)
    check(t["state"] == "succeeded", f"task succeeded (got {t['state']})")
    check(t["model_calls"] == 1, f"planning call is charged to the task (got {t['model_calls']})")
    ids = [s["step_id"] for s in t["steps"]]
    check(ids == ["login", "filter", "export"], f"planned step ids ran in order ({ids})")
    check(all(s["verified"] for s in t["steps"]), "all planned steps verified")

    disk = task_json(home, tid)
    check([s.get("id") for s in disk.get("steps", [])] == ["login", "filter", "export"],
          "planned steps were persisted to task.json")
    check(disk.get("start_url") == args.site + "/", "task.json keeps start_url")
    raw = json.dumps(disk, ensure_ascii=False)
    check("admin123" not in raw, "task.json has no password value")
    check("${cred:username}" in raw and "${cred:password}" in raw,
          "task.json keeps credential placeholders")

    rows = step_rows(home, tid)
    check(len(rows) == 3, f"three steps rows (got {len(rows)})")
    joined = "\n".join(r[1] for r in rows)
    check("admin123" not in joined, "steps table has no password value")
    check("${cred:password}" in joined, "steps table keeps credential placeholders")

    ev = events(home, tid)
    kinds = [e["type"] for e in ev]
    created = next((e for e in ev if e["type"] == "task.created"), None)
    check(created is not None and created["data"]["steps"] == 0,
          "the task is created with zero steps, then planned")
    check("plan.requested" in kinds and "plan.created" in kinds,
          "plan.requested / plan.created events were written")
    planned = next((e for e in ev if e["type"] == "plan.created"), None)
    check(planned is not None and len(planned["data"]["steps"]) == 3,
          "plan.created records the three accepted steps")
    text_events = json.dumps(ev, ensure_ascii=False)
    check("admin123" not in text_events, "event log has no password value")

    art = artifact(home, args.bhx, tid)
    check(art is not None, "task lists the planned export artifact")
    if art:
        p, size, digest = art[0], int(art[1]), art[2]
        blob = open(p, "rb").read()
        check(len(blob) == size and hashlib.sha256(blob).hexdigest() == digest,
              "artifact bytes match the listed size and sha256")
        _, state = request(args.site, "GET", "/admin/__state")
        check(digest == state["downloads"][-1]["sha256"],
              "auto-planned export matches the site's server-side record")

    # ------------------------------------------------------------ bad plans
    def rejected(mode, needle, *, allowed=None, max_calls=2, start_url=None,
                 creds=("username=admin", "password=admin123"), expect_calls=1,
                 label=None):
        reset_planner(planner_url, mode)
        request(args.site, "POST", "/admin/__reset")
        h = os.path.join(args.tmp, f"{mode}-home")
        p = os.path.join(args.tmp, f"{mode}.json")
        write_spec(p, spec(start_url or args.site + "/", allowed=allowed,
                           max_calls=max_calls))
        cmd = [args.bhx, "task", "run", p, "--home", h]
        for c in creds:
            cmd += ["--cred", c]
        rr = run(cmd, env=base_env(planner_url))

        label = label or mode
        check(rr.returncode != 0, f"{label}: task run exits non-zero ({rr.returncode})")
        tid2 = task_id(h, args.bhx)
        check(tid2 is not None, f"{label}: a failed task is still recorded")
        if not tid2:
            return
        tt = show(h, args.bhx, tid2)
        check(tt["state"] == "failed", f"{label}: state failed (got {tt['state']})")
        err = (tt.get("error") or {}).get("message", "")
        check(needle in err, f"{label}: error names the reason ({err[:120]!r})")
        check(len(tt["steps"]) == 0, f"{label}: no step ran")
        check(len(step_rows(h, tid2)) == 0, f"{label}: no step row was written")
        _, l2 = request(planner_url, "GET", "/__last")
        check(l2["calls"] == expect_calls,
              f"{label}: planner calls == {expect_calls} (got {l2['calls']})")

    rejected("empty", "zero steps", label="empty plan")
    rejected("bad_action", "plan steps invalid", label="unknown action")
    rejected("no_actions", "no actions", label="empty step")
    rejected("bad_timeout", "timeout", label="bad timeout")
    rejected("external_nav", "outside allowed_hosts", label="external navigation")
    rejected("unknown_cred", "does not hold", label="unknown credential")
    rejected("good", "budget", max_calls=0, expect_calls=0,
             label="exhausted budget")
    rejected("good", "start_url", allowed=["example.com"], expect_calls=0,
             label="blocked start_url")

    # ----------------------------------------------------------- no adapter
    reset_planner(planner_url, "good")
    home = os.path.join(args.tmp, "noadapter-home")
    path = os.path.join(args.tmp, "noadapter.json")
    write_spec(path, spec(args.site + "/"))
    r = run([args.bhx, "task", "run", path, "--home", home], env=base_env(None))
    check(r.returncode != 0, f"no adapter: task run exits non-zero ({r.returncode})")
    tid = task_id(home, args.bhx)
    check(tid is not None, "no adapter: task is recorded")
    if tid:
        t = show(home, args.bhx, tid)
        err = (t.get("error") or {}).get("message", "")
        check(t["state"] == "failed", "no adapter: state failed")
        check("planning_unavailable" in err, f"no adapter: names planning_unavailable ({err[:120]!r})")
        check(t["model_calls"] == 0, f"no adapter: zero calls (got {t['model_calls']})")
        check(len(step_rows(home, tid)) == 0, "no adapter: no step row")

    # --------------------------------------- an explicit mode cannot be empty
    home = os.path.join(args.tmp, "scripted-home")
    path = os.path.join(args.tmp, "scripted.json")
    write_spec(path, spec(args.site + "/", mode="scripted"))
    r = run([args.bhx, "task", "run", path, "--home", home], env=base_env(planner_url))
    check(r.returncode == 2, f"empty scripted task exits 2 (got {r.returncode})")
    check("mode=auto" in r.stderr, "the error explains that mode=auto is required")
    check(task_id(home, args.bhx) is None, "empty scripted task did not create a task")

    return finish()


def finish():
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
