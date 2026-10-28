#!/usr/bin/env python3
"""Play chrome://dino with the interactive controller and check it survives.

This is a demo, not part of run-phase4.sh: it needs a real Chrome and a
rendering canvas (chrome://dino is Chrome's offline page). It drives the same
`interactive_start` MCP tool an agent would call and asserts the controller
reached its goal — 20 seconds of running — with real jumps.

    cargo build --release
    python3 testkit/dino-smoke.py            # launches its own Chrome (headless)
    python3 testkit/dino-smoke.py --headed   # watch it play
    python3 testkit/dino-smoke.py --cdp-url http://127.0.0.1:9333   # attach

The task lives in testkit/sites/dino/task.json. It reads the canvas (no
vision model, no game globals): an obstacle is a dark run of columns that
rises above the ground line, and the jump fires when it is ~0.22 s away.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TASK_PATH = ROOT / "testkit" / "sites" / "dino" / "task.json"


class Mcp:
    def __init__(self, bhx: str, home: Path, cdp_url: str | None):
        env = dict(os.environ)
        if cdp_url:
            env["BH_CDP_URL"] = cdp_url
        self.proc = subprocess.Popen(
            [bhx, "mcp", "--home", str(home)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1, cwd=str(ROOT), env=env,
        )
        self.next_id = 0
        self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "dino-smoke", "version": "0"}})

    def call(self, method: str, params: dict) -> dict:
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("mcp exited: " + self.proc.stderr.read())
            try:
                resp = json.loads(line)
            except json.JSONDecodeError:
                continue
            if resp.get("id") == self.next_id:
                if "error" in resp:
                    raise RuntimeError(resp["error"]["message"])
                return resp["result"]

    def tool(self, name: str, args: dict) -> tuple[bool, str]:
        result = self.call("tools/call", {"name": name, "arguments": args})
        text = "".join(c.get("text", "") for c in result.get("content", []) if c.get("type") == "text")
        return bool(result.get("isError")), text

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def stopped_payload(status: str) -> dict | None:
    for line in status.splitlines():
        line = line.strip()
        marker = "interactive.stopped "
        if line.startswith(marker):
            try:
                return json.loads(line[len(marker):])
            except json.JSONDecodeError:
                return None
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default=str(ROOT / "target" / "release" / "bhx"))
    ap.add_argument("--home", default=None, help="task store (default: a temp dir)")
    ap.add_argument("--cdp-url", default=None, help="attach to a running Chrome instead of launching one")
    ap.add_argument("--headed", action="store_true", help="launch a visible Chrome")
    ap.add_argument("--seconds", type=int, default=20, help="survival goal (default 20)")
    ap.add_argument("--keep-home", action="store_true", help="do not delete the temp store")
    args = ap.parse_args()

    if not Path(args.bhx).exists():
        print(f"bhx not found at {args.bhx}; run cargo build --release or pass --bhx")
        return 2

    spec = json.loads(TASK_PATH.read_text())
    if args.seconds != 20:
        spec["controller"]["goal"]["script"] = f"(s) => s.elapsed_ms >= {args.seconds * 1000}"
        spec["controller"]["goal"]["deadline_ms"] = args.seconds * 1000 + 5000
        spec["controller"]["safety"]["max_duration_ms"] = args.seconds * 1000 + 2000
        spec["policy"]["max_wall_ms"] = args.seconds * 1000 + 10000

    home = Path(args.home) if args.home else Path(tempfile.mkdtemp(prefix="bhx-dino-"))
    task_id = f"dino-{int(time.time())}"
    spec["id"] = task_id

    mcp = Mcp(args.bhx, home, args.cdp_url)
    checks: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = ""):
        checks.append((name, ok, detail))
        print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""))

    try:
        err, out = mcp.tool("interactive_start", {"task": spec, "headless": not args.headed})
        if err:
            print("interactive_start failed:", out)
            return 1
        print(f"home: {home}\ntask: {task_id}\n" + out.strip())

        deadline = time.time() + args.seconds + 15
        status = ""
        while time.time() < deadline:
            time.sleep(1)
            err, status = mcp.tool("interactive_status", {"task_id": task_id})
            if err:
                break
            if "job: finished" in status:
                break
        if "job: finished" not in status:
            mcp.tool("interactive_stop", {"task_id": task_id})
            print(status)
            check("controller finished", False, "timed out")
            return 1

        check("controller finished", True)
        state = re.search(r"^task: \S+ (\w+)", status, re.M)
        check("task succeeded", bool(state and state.group(1) == "succeeded"),
              state.group(1) if state else "no state")
        stopped = stopped_payload(status)
        check("stop reason is goal_met", bool(stopped and stopped.get("why") == "goal_met"),
              str(stopped and stopped.get("why")))
        if stopped:
            actions = int(stopped.get("actions", 0))
            ticks = int(stopped.get("ticks", 0))
            p95 = int(stopped.get("tick_ms_p95", 0))
            elapsed = int(stopped.get("elapsed_ms", 0))
            # One jump costs a key_down plus a key_up; the controller must have
            # jumped a real number of times, not just started and stood still.
            check("jumped repeatedly", actions >= max(2, args.seconds // 5), f"{actions} actions")
            check("ticked at ~30 Hz", ticks >= args.seconds * 20, f"{ticks} ticks")
            check("tick budget healthy", p95 <= 100, f"p95 {p95} ms")
            check("survived the goal duration", elapsed >= args.seconds * 1000 - 1000,
                  f"{elapsed} ms")
        failed = [c for c in checks if not c[1]]
        print(f"\n{len(checks) - len(failed)}/{len(checks)} checks passed")
        return 0 if not failed else 1
    finally:
        mcp.close()
        if not args.keep_home and not args.home:
            import shutil
            shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
