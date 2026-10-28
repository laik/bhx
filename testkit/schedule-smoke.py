#!/usr/bin/env python3
"""Smoke test for scheduled execution (Phase 6): `bhx schedule`.

Usage: python3 testkit/schedule-smoke.py [--bin ./target/debug/bhx]
                                         [--port 8773]

What is actually being checked is not "a timer fired". It is that a schedule
stores a task and the *names* of its credentials, never the values; that a run
with the value missing refuses to start instead of typing `${cred:password}`
into the page; that a claimed slot is not retried blindly after a failure; and
that a completed run leaves a real CSV behind. Exits non-zero on the first
failed assertion.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

here = Path(__file__).resolve().parent

ap = argparse.ArgumentParser()
ap.add_argument("--bin", default="./target/debug/bhx")
ap.add_argument("--port", default="8773")
args = ap.parse_args()

port = args.port
site = f"http://127.0.0.1:{port}"
SECRET = "admin123"  # what the testkit site expects; never written to the task home
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


def downloads() -> list:
    return json.loads(urllib.request.urlopen(f"{site}/admin/__downloads").read())["downloads"]


def run(*argv: str, env: dict | None = None) -> subprocess.CompletedProcess:
    base = {k: v for k, v in os.environ.items() if k != "BH_CRED_PASSWORD"}
    if env:
        base.update(env)
    return subprocess.run([args.bin, *argv], capture_output=True, text=True, env=base)


def ls(home: str) -> list:
    # Polled from wait loops, so it must not add checks: the count would then
    # depend on timing instead of on what was verified.
    r = run("schedule", "ls", "--json", "--home", home)
    if r.returncode != 0:
        sys.exit(f"schedule ls failed: {r.stderr.strip()[:200]}")
    return json.loads(r.stdout)


def secret_on_disk(home: str) -> list:
    hits = []
    for p in Path(home).rglob("*"):
        if p.is_file() and SECRET.encode() in p.read_bytes():
            hits.append(str(p))
    return hits


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

home = tempfile.mkdtemp(prefix="bhx-schedule-home-")
spec_path = Path(home) / "task.json"
text = (here / "sites" / "admin" / "task.json").read_text()
spec = json.loads(text.replace("8766", port))
# The password becomes a credential reference; its value exists only in this
# process's environment at run time.
for action in spec["steps"][0]["actions"]:
    if action.get("target", {}).get("name") == "密码":
        action["text"] = "${cred:password}"
# A file may carry a planned task id; a schedule must not reuse it, or the
# second run would collide with the first.
spec["id"] = "admin-export"
spec_path.write_text(json.dumps(spec, ensure_ascii=False))

try:
    # --------------------------------------------------- bad concurrency
    r = run("schedule", "run", "--once", "--concurrency", "0", "--home", home)
    check(r.returncode != 0 and "1..=8" in r.stderr, "--concurrency 0 is refused")
    r = run("schedule", "run", "--once", "--concurrency", "9", "--home", home)
    check(r.returncode != 0 and "1..=8" in r.stderr, "--concurrency 9 is refused")

    # ------------------------------------------------------------ add + ls
    r = run("schedule", "add", str(spec_path), "--every", "1s", "--home", home)
    check(r.returncode == 0, f"schedule add exited 0 ({r.stderr.strip()[:160]})")
    check("BH_CRED_PASSWORD" in r.stdout, "add names the environment variable to provide")
    rows = ls(home)
    check(len(rows) == 1, "schedule ls lists the one schedule")
    sched = rows[0]
    check(sched["enabled"] and sched["interval_ms"] == 1000, "it is enabled every 1s")
    check(sched["cred_names"] == ["password"], f"it stored the name, not the value ({sched['cred_names']})")
    check(not sched["last_state"], "it has not run yet")
    check(SECRET not in json.dumps(rows), "listing carries no secret value")
    check(sched["spec"]["id"] is None, "the snapshot drops the file's task id")
    check(sched["spec"]["credentials"] == {}, "the snapshot holds no credential values")

    # ----------------------------- missing value: refuse, touch nothing
    before = len(downloads())
    r = run("schedule", "run", "--once", "--home", home)
    check(r.returncode != 0, "a run without the credential value exits non-zero")
    check("BH_CRED_PASSWORD" in r.stderr, f"the failure names the variable ({r.stderr.strip()[:160]})")
    check(len(downloads()) == before, "nothing was downloaded without credentials")
    sched = ls(home)[0]
    check(sched["last_state"] == "failed", "the schedule records the failure")
    check("BH_CRED_PASSWORD" in (sched["last_error"] or ""), "the failure is kept for inspection")
    check(not sched["last_task_id"], "no task was created for a run that never started")
    check(secret_on_disk(home) == [], "no credential value reached the task home")

    # ------------------------- the slot was consumed: no blind retry
    r = run("schedule", "run", "--once", "--home", home)
    check(r.returncode == 0 and "no schedule due" in r.stdout,
          "an attempted slot is not retried immediately")
    check(len(downloads()) == before, "the retry check downloaded nothing")

    # ------------------------------------------- wait for the next slot
    deadline = time.time() + 10
    while time.time() < deadline:
        sched = ls(home)[0]
        if sched["next_in_ms"] == 0:
            break
        time.sleep(0.1)
    check(sched["next_in_ms"] == 0, "the next slot becomes due")

    r = run("schedule", "run", "--once", "--home", home, env={"BH_CRED_PASSWORD": SECRET})
    check(r.returncode == 0, f"the scheduled run succeeded ({r.stderr.strip()[:160]})")
    check("succeeded" in r.stdout, "it reports the task state")
    after = len(downloads())
    check(after == before + 1, f"the site recorded exactly one new export ({before} -> {after})")
    csv = downloads()[-1]
    check(csv["bytes"] > 20 and csv["name"].startswith("orders-"), "the export is a real CSV")
    sched = ls(home)[0]
    check(sched["last_state"] == "succeeded", "the schedule records the success")
    check(bool(sched["last_task_id"]) and sched["last_task_id"] != "admin-export",
          "the schedule links to the fresh task it ran")
    check(secret_on_disk(home) == [], "a successful run still leaves no credential on disk")

    st = json.loads(run("stats", "--json", "--home", home).stdout)
    check(st["tasks"] == 1 and st["by_state"].get("succeeded") == 1,
          "the scheduled run is a normal task in the history")

    r = run("schedule", "rm", sched["id"], "--home", home)
    check(r.returncode == 0, "rm exits 0")
    check(ls(home) == [], "the schedule is gone")
    r = run("schedule", "rm", sched["id"], "--home", home)
    check(r.returncode != 0, "removing it again is an error, not a silent success")

    # A long interval removes timing races from these next assertions.
    r = run("schedule", "add", str(spec_path), "--every", "30m", "--home", home)
    check(r.returncode == 0, "add a 30m schedule")
    wide = ls(home)[0]
    r = run("schedule", "disable", wide["id"], "--home", home)
    check(r.returncode == 0, "disable exits 0")
    check(ls(home)[0]["enabled"] is False, "the schedule is disabled")
    r = run("schedule", "run", "--once", "--home", home)
    check(r.returncode == 0 and "no schedule due" in r.stdout, "a disabled schedule does not run")

    r = run("schedule", "enable", wide["id"], "--home", home)
    check(r.returncode == 0, "enable exits 0")
    r = run("schedule", "run", "--once", "--home", home)
    check(r.returncode == 0 and "no schedule due" in r.stdout,
          "re-enabling waits one interval instead of firing at once")
    run("schedule", "rm", wide["id"], "--home", home)

    r = run("schedule", "add", str(spec_path), "--every", "5 minutes", "--home", home)
    check(r.returncode != 0 and "unit" in r.stderr, "a bare duration without a unit is rejected")

    # ------------------------------------------------ daemon mode (no --once)
    r = run("schedule", "add", str(spec_path), "--every", "1h", "--home", home)
    check(r.returncode == 0, "add a long-interval schedule")
    before = len(downloads())
    denv = {k: v for k, v in os.environ.items() if k != "BH_CRED_PASSWORD"}
    denv["BH_CRED_PASSWORD"] = SECRET
    daemon = subprocess.Popen(
        [args.bin, "schedule", "run", "--home", home],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=denv,
    )
    try:
        deadline = time.time() + 30
        while time.time() < deadline:
            rows = ls(home)
            if rows and rows[-1]["last_state"] == "succeeded":
                break
            time.sleep(0.2)
        check(rows[-1]["last_state"] == "succeeded", "the daemon ran the due schedule")
        check(len(downloads()) == before + 1, "the daemon's run produced one export")
        check(daemon.poll() is None, "the daemon keeps polling after a run")
        time.sleep(0.6)
        check(len(downloads()) == before + 1, "a 1h schedule does not run again in the same pass")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
    # ------------------------------------------------ concurrency
    # The daemon above ran one schedule at a time. These check that the limit
    # is real: two due schedules overlap only when asked to, and one schedule
    # still never runs twice at once.
    def watch(proc: subprocess.Popen, h: str) -> int:
        """Highest number of schedules observed running at the same moment."""
        most = 0
        while proc.poll() is None:
            running = sum(1 for s in ls(h) if s["last_state"] == "running")
            most = max(most, running)
            time.sleep(0.05)
        return most

    def add_due(h: str) -> None:
        r = run("schedule", "add", str(spec_path), "--every", "1h", "--home", h)
        check(r.returncode == 0, f"add a schedule that is due now ({r.stderr.strip()[:120]})")

    parallel = tempfile.mkdtemp(prefix="bhx-concurrency-home-")
    add_due(parallel)
    add_due(parallel)
    before3 = len(downloads())
    penv = {k: v for k, v in os.environ.items() if k != "BH_CRED_PASSWORD"}
    penv["BH_CRED_PASSWORD"] = SECRET
    proc = subprocess.Popen(
        [args.bin, "schedule", "run", "--once", "--concurrency", "2", "--home", parallel],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=penv,
    )
    most = watch(proc, parallel)
    out, err = proc.communicate(timeout=60)
    check(most == 2, f"two schedules run at once with --concurrency 2 (saw {most})")
    check(proc.returncode == 0, f"the concurrent pass exited 0 ({err.strip()[:160]})")
    check(len(downloads()) == before3 + 2, "each concurrent run produced its own export")
    check(all(s["last_state"] == "succeeded" for s in ls(parallel)),
          "both concurrent runs are recorded")

    serial = tempfile.mkdtemp(prefix="bhx-concurrency-home-")
    add_due(serial)
    add_due(serial)
    before4 = len(downloads())
    proc = subprocess.Popen(
        [args.bin, "schedule", "run", "--once", "--home", serial],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=penv,
    )
    most = watch(proc, serial)
    out, err = proc.communicate(timeout=60)
    check(most == 1, f"the default runs one schedule at a time (saw {most})")
    check(proc.returncode == 0, "the serial pass exited 0")
    check(len(downloads()) == before4 + 2, "both serial runs still produced an export")
    check(all(s["last_state"] == "succeeded" for s in ls(serial)),
          "both serial runs are recorded")

    # A 1s interval with a ~2s run makes the next slot come due mid-run.
    overlap = tempfile.mkdtemp(prefix="bhx-concurrency-home-")
    r = run("schedule", "add", str(spec_path), "--every", "1s", "--home", overlap)
    check(r.returncode == 0, "add a 1s schedule for the overlap case")
    log = Path(overlap) / "daemon.log"
    with open(log, "w") as sink:
        daemon = subprocess.Popen(
            [args.bin, "schedule", "run", "--home", overlap],
            stdout=sink, stderr=subprocess.STDOUT, text=True, env=penv,
        )
    try:
        deadline = time.time() + 30
        text = ""
        while time.time() < deadline:
            text = log.read_text()
            if "skipped_overlap" in text:
                break
            time.sleep(0.1)
        check("skipped_overlap" in text, "a slot that comes due mid-run is skipped, not queued")
        check(daemon.poll() is None, "the daemon keeps running after skipping a slot")
        # Disabling first stops new claims, so the daemon can be stopped between
        # runs instead of killing a run (and the browser it started) halfway.
        r = run("schedule", "disable", ls(overlap)[0]["id"], "--home", overlap)
        check(r.returncode == 0, "disable stops the 1s schedule")
        deadline = time.time() + 30
        while time.time() < deadline:
            if ls(overlap)[0]["last_state"] in ("succeeded", "failed"):
                break
            time.sleep(0.1)
        check(ls(overlap)[0]["last_state"] == "succeeded",
              "the run that was in flight still finishes and is recorded")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
finally:
    if server is not None:
        server.kill()

print(f"\npassed: {CHECKS}, failed: {FAILED}")
sys.exit(1 if FAILED else 0)
