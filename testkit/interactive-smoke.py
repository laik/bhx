#!/usr/bin/env python3
"""MCP smoke test for background interactive jobs (`interactive_*` tools).

Usage: python3 testkit/interactive-smoke.py [--bin ./target/debug/bhx]
                                            [--site http://127.0.0.1:8768]

The point is not that a key can be pressed. It is that an agent can start a
controller loop, follow it, replace its rules mid-run, and stop it — and that
the page, not the harness's own report, confirms what happened (keys released,
position reached). The suite starts its own testkit server when the site is not
already up, so it works on its own.

Exits non-zero on the first failed assertion.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse

here = Path(__file__).resolve().parent

ap = argparse.ArgumentParser()
ap.add_argument("--bin", default="./target/debug/bhx")
ap.add_argument("--site", default="http://127.0.0.1:8768")
args = ap.parse_args()

site = urlparse(args.site)
host = site.hostname or "127.0.0.1"
origin = f"{site.scheme}://{site.netloc}"
game_url = f"{origin}/game/"

CHECKS = 0


def check(cond: bool, what: str) -> None:
    global CHECKS
    if not cond:
        print(f"FAIL  {what}")
        sys.exit(1)
    CHECKS += 1
    print(f"ok    {what}")


def game_state() -> dict:
    with urllib.request.urlopen(f"{origin}/game/__state", timeout=5) as r:
        return json.load(r)


def reset_game() -> None:
    req = urllib.request.Request(f"{origin}/game/__reset", method="POST")
    urllib.request.urlopen(req, timeout=5).read()


def up() -> bool:
    try:
        game_state()
        return True
    except (urllib.error.URLError, OSError):
        return False


# ------------------------------------------------------------------ fixtures


def policy(*, version=1, goal="() => false", rules, deadline_ms=8000, max_wall_ms=30_000):
    return {
        "policy_version": version,
        "tick_hz": 30,
        "start_url": game_url,
        "observe": {
            "source": "js",
            "script": "() => window.__gameState()",
            "max_hz": 10,
        },
        "goal": {"script": goal, "deadline_ms": deadline_ms},
        "rules": rules,
        "safety": {
            "max_actions_per_sec": 30,
            "max_duration_ms": deadline_ms,
            "release_all_on_exit": True,
        },
    }


def hold_right(rule_id="hold"):
    return {
        "id": rule_id,
        "when": "s.x < 10000",
        "do": [{"type": "key_down", "key": "ArrowRight"}],
        "priority": 10,
    }


def task(pol, *, allowed=None):
    return {
        "protocol_version": "0.1",
        "kind": "interactive",
        "goal": "interactive smoke",
        "policy": {"max_wall_ms": 30_000, "allowed_hosts": allowed or [host]},
        "controller": pol,
    }


# --------------------------------------------------------------- test server

server = None
if not up():
    server = subprocess.Popen(
        [sys.executable, str(here / "server.py"),
         "--host", host, "--port", str(site.port or 80)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(100):
        if up():
            break
        time.sleep(0.05)
    else:
        server.kill()
        sys.exit(f"could not start testkit server on {origin}")
reset_game()

# --------------------------------------------------------------- MCP client

home = tempfile.mkdtemp(prefix="bhx-interactive-home-")
artifacts = tempfile.mkdtemp(prefix="bhx-interactive-artifacts-")
proc = subprocess.Popen(
    [args.bin, "mcp", "--artifacts", artifacts, "--home", home],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
)

next_id = 0
pending = {}


def send(method, params=None, notify=False):
    global next_id
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not notify:
        next_id += 1
        msg["id"] = next_id
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    return None if notify else next_id


def read_until(want_id, timeout=90.0):
    if want_id in pending:
        return pending.pop(want_id)
    deadline = time.time() + timeout
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("server closed stdout")
        msg = json.loads(line)
        if "id" in msg and msg["id"] != want_id:
            pending[msg["id"]] = msg
            continue
        if "id" in msg:
            return msg
    raise TimeoutError(f"no response for id {want_id}")


def call(method, params=None):
    return read_until(send(method, params))


def tool(name, arguments=None):
    r = call("tools/call", {"name": name, "arguments": arguments or {}})
    if "error" in r:
        raise AssertionError(f"{name}: rpc error {r['error']}")
    res = r["result"]
    text = "".join(c.get("text", "") for c in res["content"] if c["type"] == "text")
    return res.get("isError", False), text


def start(pol, **kw):
    is_err, text = tool("interactive_start", {"task": task(pol, **kw)})
    check(not is_err, f"interactive_start accepted the spec ({text.splitlines()[0] if text else ''})")
    task_id = [l.split(":", 1)[1].strip() for l in text.splitlines() if l.startswith("task_id:")][0]
    return task_id, text


def status(task_id):
    is_err, text = tool("interactive_status", {"task_id": task_id})
    return is_err, text


def stopped_event(text):
    """The last `interactive.stopped` JSON line in a status answer."""
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("interactive.stopped "):
            return json.loads(line[len("interactive.stopped "):])
    return None


def wait_for_motion(timeout=8.0) -> dict:
    """Wait until the page itself reports that the controller moved it.

    The browser takes a moment to launch; a stop sent before the first sample
    would test nothing but teardown.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = game_state()
        got = s.get("last_report")
        if got and got["x"] > 45:
            return got
        time.sleep(0.05)
    raise AssertionError(f"the controller never moved the page: {game_state()}")


def wait_state(task_id, want, timeout=10.0):
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        _, last = status(task_id)
        for line in last.splitlines():
            if line.startswith("task: ") and line.endswith(f" {want}"):
                return last
        time.sleep(0.05)
    raise AssertionError(f"{task_id} never reached {want}; last status:\n{last}")


def cleanup():
    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()
    if server is not None:
        server.kill()


# -------------------------------------------------------------------- checks


def main():
    r = call("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "interactive-smoke", "version": "0"},
    })
    check(r["result"]["serverInfo"]["name"] == "bhx", "initialize answered by bhx")
    send("notifications/initialized", notify=True)

    names = {t["name"] for t in call("tools/list")["result"]["tools"]}
    check(
        {"interactive_start", "interactive_status",
         "interactive_update_policy", "interactive_stop"} <= names,
        "interactive_* tools are listed",
    )

    # ---------------------------------------------------------------- A: goal
    reset_game()
    task_id, text = start(policy(goal="(s) => s.x >= 170", rules=[hold_right()]))
    check("policy_version: 1" in text, "start reports the initial policy version")

    st = status(task_id)[1]
    check(st.splitlines()[0].startswith("job: "), f"status answers while the job runs ({st.splitlines()[0]})")

    st = wait_state(task_id, "succeeded")
    check("goal_met" in st, "the controller stopped on its goal, not a deadline")

    # Costs are reported, not inferred: predicates share one JS round trip per
    # tick, and the tick interval is published so a slow loop is visible.
    stopped = stopped_event(st)
    check(stopped is not None, "the stop event is in the status output")
    if stopped:
        check(
            stopped["evals"] <= stopped["ticks"] + 1,
            f"goal and rule predicates share one round trip per tick "
            f"(evals={stopped['evals']} ticks={stopped['ticks']})",
        )
        check(
            0 < stopped["tick_ms_p50"] <= stopped["tick_ms_p95"] <= stopped["tick_ms_max"],
            f"tick intervals are ordered "
            f"(p50={stopped['tick_ms_p50']} p95={stopped['tick_ms_p95']} "
            f"max={stopped['tick_ms_max']})",
        )
        # Per-rule activity: an aggregate action count cannot distinguish 20
        # jumps from 19 jumps plus a crash-and-restart, so the loop reports
        # which rules actually fired and how often.
        per_rule = stopped.get("rules") or []
        ids = [r.get("id") for r in per_rule]
        check(
            ids == ["hold"],
            f"only the rule that fired is listed, once, in fire order (got {ids})",
        )
        if per_rule:
            r = per_rule[0]
            check(
                r.get("entries", 0) >= 1 and r.get("actions", 0) == stopped["actions"],
                f"rule entries and actions add up "
                f"(entries={r.get('entries')} rule_actions={r.get('actions')} "
                f"total_actions={stopped['actions']})",
            )

    s = game_state()["last_report"]
    check(s["x"] >= 170, f"the page moved to x={s['x']} (goal was the state, not the harness's word)")
    check(s["keys"] == [], "no key was left down after the goal")

    # ------------------------------------------------------- B: hot swap rules
    reset_game()
    started = time.time()
    task_id, _ = start(policy(rules=[hold_right()]))
    held = wait_for_motion()
    check(held["keys"] == ["ArrowRight"], "the page sees the key held before the update")
    st = status(task_id)[1]
    check("job: running" in st, "the job is still running before the update")
    check("policy_version: 1" in st, "the running policy version is visible")

    is_err, text = tool("interactive_update_policy", {
        "task_id": task_id,
        "policy": policy(version=2, goal="(s) => s.x >= 150", rules=[hold_right()]),
    })
    check(not is_err and "policy_version 2 queued" in text, "an update is queued for the next tick")

    st = wait_state(task_id, "succeeded")
    check('"policy_version":2' in st, "the stop event names the version that actually ran")
    check(time.time() - started < 6, "the new goal took effect without waiting for the old one")
    s = game_state()["last_report"]
    check(s["x"] >= 150, f"the page reached x={s['x']} under the updated goal")
    check(s["keys"] == [], "no key was left down after the updated goal")

    # The stop event is written just before the background job marks itself
    # finished; wait for the job itself, not for the state it produced.
    for _ in range(60):
        if status(task_id)[1].splitlines()[0] == "job: finished":
            break
        time.sleep(0.05)
    is_err, text = tool("interactive_update_policy", {
        "task_id": task_id,
        "policy": policy(version=2, goal="(s) => true", rules=[]),
    })
    check(is_err and "finished" in text, "an update to a finished job is refused")

    # ------------------------------------------ C: stale versions and stopping
    reset_game()
    task_id, _ = start(policy(rules=[hold_right()]))
    wait_for_motion()

    is_err, text = tool("interactive_update_policy", {
        "task_id": task_id,
        "policy": policy(version=1, rules=[hold_right()]),
    })
    check(is_err and "not newer" in text, "a late (not newer) policy is refused")

    is_err, text = tool("interactive_update_policy", {
        "task_id": task_id,
        "policy": policy(version=2, rules=[hold_right()]),
    })
    check(not is_err, "a newer policy is accepted while running")

    is_err, text = tool("interactive_stop", {"task_id": task_id})
    check(not is_err and "canceled" in text, f"stop reports the final state ({text.strip()})")
    s = game_state()["last_report"]
    check(s["keys"] == [], "stop released the held key")
    check(s["x"] > 40, f"the controller had actually moved the page (x={s['x']})")

    # ------------------------------------------------ D: 30 s soak under load
    # The exit condition is not "it moved once": hold a key for 30 s at 30 Hz,
    # then check the page saw the release and the process did not spin.
    reset_game()
    started = time.time()
    task_id, _ = start(policy(
        goal="() => false", rules=[hold_right()], deadline_ms=30_000, max_wall_ms=31_000
    ))
    wait_for_motion()
    st = wait_state(task_id, "failed", timeout=45)
    elapsed = time.time() - started
    check(elapsed >= 29, f"the soak ran its full deadline ({elapsed:.1f}s)")

    stopped = stopped_event(st)
    check(stopped is not None and stopped["why"] == "deadline", "the soak ended on its deadline")
    if stopped:
        check(
            stopped["ticks"] >= 700,
            f"a 30 Hz loop completed most of 30 s of ticks (ticks={stopped['ticks']})",
        )
        check(
            stopped["evals"] <= stopped["ticks"] + 1,
            f"one predicate round trip per tick over the whole soak "
            f"(evals={stopped['evals']} ticks={stopped['ticks']})",
        )
        check(
            0 < stopped["tick_ms_p50"] <= stopped["tick_ms_p95"] <= stopped["tick_ms_max"],
            f"tick intervals stayed in budget (p50={stopped['tick_ms_p50']} "
            f"p95={stopped['tick_ms_p95']} max={stopped['tick_ms_max']} ms)",
        )
    s = game_state()["last_report"]
    check(s["keys"] == [], "30 s of held input left no key down")

    cpu = subprocess.run(
        ["ps", "-o", "%cpu=", "-p", str(proc.pid)],
        capture_output=True, text=True,
    ).stdout.strip()
    cpu = float(cpu or 0)
    check(cpu < 100, f"the server process stayed under one core (avg {cpu:.1f}%)")

    # ------------------------------------------------------- E: bad requests
    is_err, text = tool("interactive_start", {
        "task": {"protocol_version": "0.1", "kind": "workflow",
                 "goal": "not interactive", "steps": []},
    })
    check(is_err and "kind=interactive" in text, "interactive_start refuses a workflow spec")

    bad = task(policy(rules=[hold_right()]))
    bad["controller"]["observe"]["script"] = "  "
    is_err, text = tool("interactive_start", {"task": bad})
    check(is_err and "observe.script" in text, "a missing observe script is rejected synchronously")

    bad = task(policy(rules=[hold_right()]), allowed=["example.com"])
    is_err, text = tool("interactive_start", {"task": bad})
    check(is_err and "allowed_hosts" in text, "a start_url outside allowed_hosts is rejected")

    is_err, text = tool("interactive_status", {"task_id": "no-such-task"})
    check(is_err, "status of an unknown job is an error")

    is_err, text = tool("interactive_stop", {"task_id": "no-such-task"})
    check(is_err, "stop of an unknown job is an error")

    print(f"\npassed: {CHECKS}, failed: 0")
    return 0


try:
    code = main()
finally:
    cleanup()
sys.exit(code)
