#!/usr/bin/env python3
"""MCP smoke test: drive `bhx mcp` over stdio and assert the browser obeys.

Usage: python3 testkit/mcp-smoke.py [--bin ./target/debug/bhx] [--url URL]

Exits non-zero on the first failed assertion, so it works as a CI gate.
Responses may arrive out of order (the server runs each call in a task), so
every read loops until the wanted id shows up.
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse

here = Path(__file__).resolve().parent
ap = argparse.ArgumentParser()
ap.add_argument("--bin", default="./target/debug/bhx")
ap.add_argument("--url", default="http://127.0.0.1:8766/")
ap.add_argument("--verifier-port", type=int, default=8793)
args = ap.parse_args()

site = urlparse(args.url)


def up() -> bool:
    try:
        with urllib.request.urlopen(f"{site.scheme}://{site.netloc}/admin/__state", timeout=1):
            return True
    except (urllib.error.URLError, OSError):
        return False


def reset() -> None:
    for path in ("/admin/__reset", "/form/__reset"):
        req = urllib.request.Request(f"{site.scheme}://{site.netloc}{path}", method="POST")
        urllib.request.urlopen(req, timeout=2).read()


# The smoke test must stand on its own: `python3 testkit/mcp-smoke.py` with no
# server running starts the same one the suites use.
srv = None
if not up():
    here = Path(__file__).resolve().parent
    srv = subprocess.Popen(
        [sys.executable, str(here / "server.py"), "--host", site.hostname or "127.0.0.1",
         "--port", str(site.port or 80)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(100):
        if up():
            break
        time.sleep(0.05)
    else:
        srv.kill()
        sys.exit(f"could not start testkit server on {site.netloc}")
if up():
    reset()

# A deterministic Verify model, so the smoke test does not need an API key.
verifier = subprocess.Popen(
    [sys.executable, "-I", str(here / "verifier.py"), "--port", str(args.verifier_port)],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
verifier_url = f"http://127.0.0.1:{args.verifier_port}/v1/chat/completions"
for _ in range(100):
    try:
        with urllib.request.urlopen(
            f"http://127.0.0.1:{args.verifier_port}/health", timeout=1
        ):
            break
    except (urllib.error.URLError, OSError):
        time.sleep(0.05)
else:
    verifier.kill()
    sys.exit(f"could not start verifier stub on port {args.verifier_port}")

proc = subprocess.Popen(
    [args.bin, "mcp", "--artifacts", "/tmp/bhx-mcp-artifacts", "--home", "/tmp/bhx-mcp-home"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
    env=dict(os.environ, BH_LLM_URL=verifier_url),
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


def main():
    r = call("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "smoke", "version": "0"},
    })
    assert r["result"]["serverInfo"]["name"] == "bhx", r
    send("notifications/initialized", notify=True)

    tools = call("tools/list")["result"]["tools"]
    names = {t["name"] for t in tools}
    assert {"browser_observe", "browser_execute_actions", "browser_evaluate",
            "browser_verify", "task_artifacts", "artifact_list",
            "artifact_metadata", "artifact_fetch"} <= names, names
    print(f"tools: {', '.join(sorted(names))}")

    t0 = time.time()
    is_err, text = tool("browser_navigate", {"url": args.url})
    assert not is_err, text
    print(f"navigate ok ({int((time.time() - t0) * 1000)} ms): {text.splitlines()[0]}")

    # Verify is a second opinion over page state with two fixed choices. On the
    # login page the goal is not achieved; the threshold turns a low-confidence
    # answer into "uncertain" rather than rounding it to a side.
    is_err, text = tool("browser_verify", {"goal": "已登录后台并看到订单列表"})
    assert not is_err and text.splitlines()[0] == "not_achieved", text
    print("verify ok: not_achieved before login")

    # Caller-supplied JS is isolated by default: DOM is visible, page globals
    # are not. The main world stays reachable, but only when asked for.
    is_err, text = tool("browser_evaluate", {
        "script": "window.__bhxProbe = 42; window.__bhxProbe", "world": "main",
    })
    assert not is_err and text.strip() == "42", text
    is_err, text = tool("browser_evaluate", {"script": "typeof window.__bhxProbe"})
    assert not is_err and text.strip() == '"undefined"', text
    is_err, text = tool("browser_evaluate", {"script": "document.title.length > 0"})
    assert not is_err and text.strip() == "true", text
    print("evaluate ok: isolated world sees DOM, not page globals")

    is_err, text = tool("browser_observe", {"goal": "登录"})
    assert not is_err, text
    assert 'button "登录"' in text, text
    print("observe ok:")
    for line in text.strip().splitlines()[-4:]:
        print("   ", line)

    is_err, text = tool("browser_execute_actions", {"actions": [
        {"type": "type", "target": {"role": "textbox", "name": "用户名"}, "text": "admin", "clear_first": True},
        {"type": "type", "target": {"role": "textbox", "name": "密码"}, "text": "admin123", "clear_first": True},
        {"type": "click", "target": {"role": "button", "name": "登录"}},
    ], "observe_after": True})
    assert not is_err, text
    assert 'button "导出 CSV"' in text, text
    print("login ok:")
    for line in text.strip().splitlines():
        print("   ", line)

    is_err, text = tool("browser_verify", {"goal": "已登录后台并看到订单列表"})
    assert not is_err and text.splitlines()[0] == "achieved", text
    print("verify ok: achieved after login")

    is_err, text = tool("browser_verify", {
        "goal": "已登录后台并看到订单列表", "threshold": 0.95,
    })
    assert not is_err and text.splitlines()[0] == "uncertain", text
    print("verify ok: below threshold is uncertain")

    is_err, text = tool("browser_check_condition", {
        "condition": {"type": "js", "script": "document.querySelectorAll('#rows tr').length >= 1"},
        "timeout_ms": 2000,
    })
    assert text.strip() == "true", text
    print("condition ok: true")

    # Cancellation must land inside 200 ms and leave no key held down.
    req = send("tools/call", {
        "name": "browser_check_condition",
        "arguments": {
            "condition": {"type": "js", "script": "false"},
            "timeout_ms": 15000,
        },
    })
    time.sleep(0.3)
    cancel_at = time.time()
    send("notifications/cancelled", {"requestId": req, "reason": "smoke"}, notify=True)
    res = read_until(req, timeout=10)["result"]
    took = (time.time() - cancel_at) * 1000
    text = "".join(c.get("text", "") for c in res["content"] if c["type"] == "text")
    assert took < 200, f"cancel took {took:.0f} ms"
    assert res.get("isError") and "canceled" in text.lower(), text
    print(f"cancel ok: {took:.0f} ms")

    is_err, text = tool("browser_check_condition", {
        "condition": {"type": "js", "script": "document.querySelectorAll('#rows tr').length >= 1"},
    })
    assert text.strip() == "true", text
    print("usable after cancel: true")

    is_err, text = tool("browser_screenshot")
    assert not is_err, text
    print(f"screenshot ok: {text}")

    is_err, text = tool("browser_close")
    assert not is_err, text
    print(text)

    # Task-level tools own their browser and their store: the whole point is
    # that the work is persisted, not that a page changed.
    task = {
        "protocol_version": "0.1",
        "goal": "MCP 冒烟：打开后台并确认页面可达",
        "policy": {"allowed_hosts": ["127.0.0.1"], "max_wall_ms": 30000},
        "steps": [{
            "id": "s1",
            "goal": "打开后台",
            "actions": [{"type": "navigate", "url": args.url}],
            "success": {"type": "js", "script": "document.readyState === 'complete'"},
            "timeout_ms": 15000,
        }],
    }
    is_err, text = tool("task_run", {"task": task})
    assert not is_err, text
    task_id = text.splitlines()[0].split(":", 1)[1].strip()
    assert "state: succeeded" in text, text
    print(f"task_run ok: {task_id}")

    is_err, text = tool("task_status", {"task_id": task_id})
    assert not is_err, text
    assert "succeeded" in text and "s1" in text, text
    print("task_status ok")

    is_err, text = tool("task_list", {"limit": 5})
    assert not is_err and task_id in text, text
    print("task_list ok")

    is_err, text = tool("task_artifacts", {"task_id": task_id})
    assert not is_err and "no artifacts" in text, text
    print("task_artifacts ok (empty)")

    # Artifact tools are task-scoped and read the store, not a URL. Run the
    # export workflow so there is a real file to fetch.
    export = json.loads((here / "sites/admin/task.json").read_text())
    export = json.loads(json.dumps(export).replace("8766", str(site.port or 80)))
    is_err, text = tool("task_run", {"task": export})
    assert not is_err and "state: succeeded" in text, text
    export_id = text.splitlines()[0].split(":", 1)[1].strip()
    print(f"export task ok: {export_id}")

    is_err, text = tool("artifact_list", {"task_id": export_id})
    assert not is_err and ".csv" in text and "sha256=" in text, text
    csv_line = next(line for line in text.splitlines() if ".csv" in line)
    csv_name = csv_line.split("name=", 1)[1].split("\t", 1)[0]
    print(f"artifact_list ok: {csv_name}")

    is_err, text = tool("artifact_metadata", {"task_id": export_id, "name": csv_name})
    assert not is_err and "sha256:" in text and "bytes:" in text, text
    print("artifact_metadata ok")

    is_err, text = tool("artifact_fetch", {"task_id": export_id, "name": csv_name})
    assert not is_err and "订单号" in text and "\n" in text, text[:300]
    print(f"artifact_fetch ok ({len(text)} chars of CSV)")

    is_err, text = tool("artifact_fetch", {"task_id": export_id, "name": "nope.csv"})
    assert is_err, text
    print("artifact_fetch rejects an unknown name")

    proc.stdin.close()
    proc.wait(timeout=10)
    verifier.terminate()
    verifier.wait(timeout=5)
    if srv is not None:
        srv.terminate()
        srv.wait(timeout=5)
    print("SMOKE PASSED")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001 - a smoke test wants the raw message
        proc.kill()
        verifier.kill()
        err = proc.stderr.read() if proc.stderr else ""
        if srv is not None:
            srv.kill()
        print(f"SMOKE FAILED: {e}\n--- server stderr ---\n{err[-3000:]}", file=sys.stderr)
        sys.exit(1)
