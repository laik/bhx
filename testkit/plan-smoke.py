#!/usr/bin/env python3
"""End-to-end check for `bhx plan`: goal -> model -> validated spec -> task run.

The model end is testkit/planner.py, a deterministic rule. What this proves is
the harness side: the request carries the observed state, the proposal is
validated and normalized, credentials stay as `${cred:...}` placeholders in the
spec written to disk, and the resulting task runs to a real CSV artifact that
matches the site's server-side record.

Exit code 0 only if every check passed.
"""

import argparse
import hashlib
import http.client
import json
import os
import re
import subprocess
import sys
import urllib.parse

PASS = 0
FAIL = 0


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


def run(cmd, timeout=180):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--home", required=True)
    ap.add_argument("--out", required=True, help="where the planned spec is written")
    args = ap.parse_args()

    request(args.site, "POST", "/admin/__reset")

    planned = run([
        args.bhx, "plan",
        "--goal", "登录后台，筛选失败订单，导出 CSV",
        "--url", args.site + "/",
        "--out", args.out,
        "--run",
        "--cred", "username=admin",
        "--cred", "password=admin123",
        "--home", args.home,
    ])
    check(planned.returncode == 0, f"bhx plan --run exited 0 (got {planned.returncode})")
    if planned.returncode != 0:
        print(planned.stderr.rstrip(), file=sys.stderr)
        return finish()

    check("planned 3 step(s)" in planned.stderr,
          f"planning reported three steps ({first_line(planned.stderr, 'planned')})")
    check(re.fullmatch(r"(?s).*1 call\(s\) of budget 2.*", planned.stderr) is not None,
          "planning used exactly one model call")

    raw = open(args.out, encoding="utf-8").read()
    spec = json.loads(raw)
    ids = [s.get("id") for s in spec.get("steps", [])]
    check(ids == ["login", "filter", "export"], f"planned step ids kept ({ids})")
    check(spec.get("credentials") == {}, "spec written to disk has no credentials map")
    check("admin123" not in raw, "spec written to disk has no password value")
    check("${cred:username}" in raw and "${cred:password}" in raw,
          "spec keeps credential placeholders")
    check(spec.get("goal") == "登录后台，筛选失败订单，导出 CSV",
          "spec carries the goal it planned for")

    ls = run([args.bhx, "task", "ls", "--home", args.home, "--limit", "5"])
    task_id = ls.stdout.split()[0] if ls.stdout.strip() else None
    check(task_id is not None, "planned run left a task in the store")
    if not task_id:
        return finish()

    show = run([args.bhx, "task", "show", task_id, "--json", "--home", args.home])
    task = json.loads(show.stdout)
    check(task["state"] == "succeeded", f"planned task succeeded (got {task['state']})")
    check(len(task["steps"]) == 3 and all(s["verified"] for s in task["steps"]),
          "all planned steps verified")
    check(task["model_calls"] == 0, "running the planned task used no model calls")

    # The engine writes task.json with credentials stripped; the password must
    # not leak into the task directory at all.
    task_json = os.path.join(args.home, "tasks", task_id, "task.json")
    disk = open(task_json, encoding="utf-8").read() if os.path.exists(task_json) else ""
    check(disk != "" and "admin123" not in disk,
          "task.json on disk contains no password")
    check("${cred:username}" in disk, "task.json on disk keeps credential placeholders")

    arts = run([args.bhx, "task", "artifacts", task_id, "--home", args.home])
    rows = [line.split("\t") for line in arts.stdout.splitlines() if "\t" in line]
    check(bool(rows), "task lists an artifact")
    if rows:
        path, size, digest = rows[0][0], int(rows[0][1]), rows[0][2]
        blob = open(path, "rb").read()
        check(len(blob) == size and hashlib.sha256(blob).hexdigest() == digest,
              "artifact bytes match the listed size and sha256")
        _, state = request(args.site, "GET", "/admin/__state")
        recorded = state["downloads"][-1]["sha256"]
        check(digest == recorded,
              "planned export matches the site's server-side download record")

    return finish()


def first_line(text, prefix):
    for line in text.splitlines():
        if prefix in line:
            return line.strip()
    return "(not found)"


def finish():
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
