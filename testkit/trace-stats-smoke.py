#!/usr/bin/env python3
"""Smoke test for run history and trace replay: `bhx stats` and `bhx trace`.

Usage: python3 testkit/trace-stats-smoke.py [--bin ./target/debug/bhx]
                                            [--port 8772]

The point is that the numbers come from what the store recorded, not from a
counter the harness kept in memory: actions are counted from `action.executed`
events, durations from task timestamps, and a trace bundles those events with
the artifacts they produced. Exits non-zero on the first failed assertion.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path
from urllib.parse import unquote

here = Path(__file__).resolve().parent

ap = argparse.ArgumentParser()
ap.add_argument("--bin", default="./target/debug/bhx")
ap.add_argument("--port", default="8772")
args = ap.parse_args()

port = args.port
site = f"http://127.0.0.1:{port}"
CHECKS = 0
FAILED = 0


def check(cond: bool, what: str) -> None:
    global CHECKS, FAILED
    CHECKS += 1
    if cond:
        print(f"ok    {what}")
    else:
        FAILED += 1
        print(f"FAIL  {what}")
        sys.exit(1)  # later assertions build on earlier ones


def up() -> bool:
    try:
        urllib.request.urlopen(f"{site}/admin/__state", timeout=0.5).read()
        return True
    except Exception:
        return False


def chrome_binary() -> str | None:
    for c in [
        os.environ.get("BH_CHROME"),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ]:
        if not c:
            continue
        if os.path.sep in c and os.path.exists(c):
            return c
        found = shutil.which(c)
        if found:
            return found
    return None


def render_dom(page: Path) -> str | None:
    """Load the player in a real browser and return the post-script DOM."""
    exe = chrome_binary()
    if not exe:
        return None
    r = subprocess.run(
        [exe, "--headless=new", "--disable-gpu", "--no-first-run",
         "--no-default-browser-check", "--dump-dom", page.resolve().as_uri()],
        capture_output=True, text=True, timeout=30,
    )
    return r.stdout if r.returncode == 0 else None


def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def run(*argv: str) -> subprocess.CompletedProcess:
    return subprocess.run([args.bin, *argv], capture_output=True, text=True)


def stats(home: str) -> dict:
    r = run("stats", "--json", "--home", home)
    check(r.returncode == 0, f"bhx stats exited 0 ({r.stderr.strip()[:120]})")
    return json.loads(r.stdout)


def trace(home: str, task_id: str) -> dict:
    r = run("trace", task_id, "--json", "--home", home)
    check(r.returncode == 0, f"bhx trace {task_id} exited 0 ({r.stderr.strip()[:120]})")
    return json.loads(r.stdout)


server = None
if not up():
    server = subprocess.Popen(
        [sys.executable, "-I", str(here / "server.py"), "--port", port],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(100):
        if up():
            break
        time.sleep(0.05)
    else:
        server.kill()
        sys.exit(f"could not start testkit server on {site}")

home = tempfile.mkdtemp(prefix="bhx-trace-home-")
spec_path = Path(home) / "task.json"
text = (here / "sites" / "admin" / "task.json").read_text()
spec = json.loads(text.replace("8766", port))
# Capture every step: this is what makes the trace a replay rather than a log.
spec["policy"]["trace_screenshots"] = True
spec_path.write_text(json.dumps(spec, ensure_ascii=False))

try:
    # ------------------------------------------------------- empty home
    r = run("stats", "--home", home)
    check(r.returncode == 0 and "no tasks" in r.stdout, "an empty home reports no tasks")

    # ------------------------------------------------------- a good run
    r = run("task", "run", str(spec_path), "--home", home)
    check(r.returncode == 0, f"the admin task exited 0 ({r.stderr.strip()[:120]})")
    summary = json.loads(r.stdout)  # non-TTY `task run` writes its report as JSON
    check(summary["state"] == "succeeded", "the admin task succeeded")
    task_id = summary["task_id"]

    st = stats(home)
    check(st["tasks"] == 1, "stats counts the task")
    check(st["by_state"].get("succeeded") == 1, "stats reports it succeeded")
    check(st["success_rate"] == 1.0, "success rate is 1.0 over one finished task")
    check(st["duration_ms"]["n"] == 1 and st["duration_ms"]["p50"] > 0, "duration comes from task timestamps")
    check(st["actions"] == 7, f"seven actions recorded ({st['actions']})")
    check(st["failed_actions"] == 0, "no failed actions")
    check(st["action_failure_rate"] == 0.0, "failed-action rate is 0%")
    check(st["model_calls"] == 0, "a scripted task used no model calls")

    t = trace(home, task_id)
    kinds = [e["type"] for e in t["events"]]
    check(kinds.count("action.executed") == 7, "the trace holds one event per dispatched action")
    check(kinds.index("step.started") < kinds.index("action.executed") < kinds.index("step.finished"),
          "action events sit between their step's start and finish")
    first = next(e for e in t["events"] if e["type"] == "action.executed")
    check({"step_id", "index", "type", "ok", "elapsed_ms"} <= first["data"].keys(),
          "an action event names its step, index, type and duration")
    check(len(t["steps"]) == 3 and all(s["verified"] for s in t["steps"]),
          "the trace carries the three verified steps")
    check("admin123" not in json.dumps(t), "the trace does not carry the typed password")

    shots = [a for a in t["artifacts"] if a["kind"] == "screenshot"]
    check(len(shots) == 3 and all(a["bytes"] > 1000 for a in shots),
          f"each step left a real screenshot ({len(shots)})")
    shot_events = [e for e in t["events"] if e["type"] == "step.screenshot"]
    check([e["data"]["step_id"] for e in shot_events] == ["s1", "s2", "s3"],
          "screenshot events follow the step order")
    order = [e["type"] for e in t["events"]]
    check(order.index("step.screenshot") < order.index("step.finished"),
          "a screenshot is recorded before its step finishes")

    out = tempfile.mkdtemp(prefix="bhx-trace-out-")
    r = run("trace", task_id, "--out", out, "--home", home)
    check(r.returncode == 0, "bhx trace --out wrote a bundle")
    bundle = Path(out) / "trace.json"
    check(bundle.exists(), "trace.json exists in the bundle directory")
    copies = list((Path(out) / "artifacts").glob("*.csv"))
    check(len(copies) == 1, f"the bundle holds the exported CSV ({len(copies)})")
    site_dl = json.loads(urllib.request.urlopen(f"{site}/admin/__downloads").read())["downloads"]
    check(sha256(str(copies[0])) == site_dl[-1]["sha256"],
          "the copied artifact matches the site's sha256")

    # ------------------------------------------------- the replay player
    html = Path(out) / "trace.html"
    check(html.exists() and html.stat().st_size > 4000, "trace.html ships with the bundle")
    source = html.read_text()
    check('src="http' not in source and 'href="http' not in source,
          "the player loads nothing from the network")
    dom = render_dom(html)
    check(dom is not None, "a browser can open trace.html from file://")
    check('class="ev"' in dom, "the timeline is rendered by the player's own script")
    check("s3" in dom and "orders-" in dom, "the rendered page shows the step and the artifact")
    check("admin123" not in dom, "the player does not show the typed password")
    links = re.findall(r'href="(artifacts/[^"]+)"', dom)
    check(len(links) == 4 and all((Path(out) / unquote(link)).is_file() for link in links),
          f"the player links to every copied artifact ({len(links)})")
    imgs = re.findall(r'<img[^>]+src="(artifacts/[^"]+)"', dom)
    check(len(imgs) == 6 and all((Path(out) / unquote(src)).is_file() for src in imgs),
          f"the player shows the step screenshots ({len(imgs)} images)")

    # ----------------------------------------------------- a failed run
    bad = json.loads(json.dumps(spec))
    bad["policy"].pop("trace_screenshots", None)
    bad["steps"][1]["actions"][0]["target"]["name"] = "不存在的按钮"
    bad_path = Path(home) / "bad.json"
    bad_path.write_text(json.dumps(bad, ensure_ascii=False))
    r = run("task", "run", str(bad_path), "--home", home)
    check(r.returncode != 0, "the failing task exits non-zero")

    st = stats(home)
    check(st["tasks"] == 2, "stats counts both tasks")
    check(st["by_state"].get("failed") == 1, "stats reports the failure")
    check(st["success_rate"] == 0.5, "success rate is 0.5 over two finished tasks")
    check(st["failed_actions"] >= 1, f"the failed action is counted ({st['failed_actions']})")
    check(0 < st["action_failure_rate"] < 1, "failed-action rate is a fraction")

    failed_id = [l.split()[0] for l in run("task", "ls", "--home", home).stdout.splitlines() if "failed" in l][0]
    t = trace(home, failed_id)
    bad_events = [
        e for e in t["events"]
        if e["type"] == "action.executed" and not e["data"]["ok"]
    ]
    check(len(bad_events) == 1, "the failing trace holds exactly one failed action")
    check(bool(bad_events[0]["data"].get("error")), "the failed action records why it failed")
    check(t["task"]["state"] == "failed", "the trace names the terminal state")
    failed_shots = [a for a in t["artifacts"] if a["kind"] == "screenshot"]
    check(len(failed_shots) == 1 and "step-02-s2" in (failed_shots[0]["name"] or ""),
          "a failed step gets a screenshot even without the policy flag")
finally:
    if server is not None:
        server.kill()

print(f"\npassed: {CHECKS}, failed: {FAILED}")
sys.exit(1 if FAILED else 0)
