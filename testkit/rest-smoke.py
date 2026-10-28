#!/usr/bin/env python3
"""REST + WebSocket smoke test for `bhx serve`.

Talks to the harness the way a non-MCP agent would: start a task over HTTP,
follow its event stream over WebSocket, then download the artifact and compare
its hash with the site's server-side record.

Usage:
    python3 testkit/rest-smoke.py --api http://127.0.0.1:8765 \
        --site http://127.0.0.1:8766 --spec testkit/sites/admin/task.json

Exit code 0 only if every check passed.
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
import socket
import struct
import sys
import time
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


def base_parts(base):
    u = urllib.parse.urlparse(base)
    return u.hostname, u.port or 80, u.path.rstrip("/")


def request(base, method, path, body=None, raw=False, timeout=15):
    host, port, prefix = base_parts(base)
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    headers = {"Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    conn.request(method, prefix + path, body=data, headers=headers)
    r = conn.getresponse()
    payload = r.read()
    status = r.status
    ctype = r.getheader("Content-Type", "")
    conn.close()
    if raw:
        return status, payload, ctype
    try:
        parsed = json.loads(payload) if payload else None
    except json.JSONDecodeError:
        parsed = payload.decode("utf-8", "replace")
    return status, parsed, ctype


def wait_state(api, task_id, want, timeout=30.0):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        status, body, _ = request(api, "GET", f"/v1/tasks/{task_id}")
        if status == 200:
            last = body["task"]["state"]
            if last == want:
                return last
            if last in ("succeeded", "failed", "canceled") and want not in (
                last,
            ):
                return last
        time.sleep(0.1)
    return last


# ----------------------------------------------------------------- websocket

def ws_connect(host, port, path):
    sock = socket.create_connection((host, port), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("server closed during websocket handshake")
        buf += chunk
    head, buf = buf.split(b"\r\n\r\n", 1)
    status_line = head.split(b"\r\n", 1)[0]
    if b" 101 " not in status_line:
        raise RuntimeError(f"websocket upgrade failed: {status_line!r}")
    return sock, buf


def _read_exact(sock, buf, n):
    while len(buf) < n:
        chunk = sock.recv(65536)
        if not chunk:
            raise EOFError("websocket closed")
        buf += chunk
    return buf[:n], buf[n:]


def read_frame(sock, buf):
    hdr, buf = _read_exact(sock, buf, 2)
    opcode = hdr[0] & 0x0F
    masked = bool(hdr[1] & 0x80)
    length = hdr[1] & 0x7F
    if length == 126:
        ext, buf = _read_exact(sock, buf, 2)
        length = struct.unpack("!H", ext)[0]
    elif length == 127:
        ext, buf = _read_exact(sock, buf, 8)
        length = struct.unpack("!Q", ext)[0]
    mask = None
    if masked:
        mask, buf = _read_exact(sock, buf, 4)
    payload, buf = _read_exact(sock, buf, length)
    if mask:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, payload, buf


def tail_events(api, task_id, after=0):
    u = urllib.parse.urlparse(api)
    sock, buf = ws_connect(
        u.hostname, u.port or 80, f"/v1/tasks/{task_id}/events/ws?after={after}"
    )
    events, closed = [], False
    try:
        while not closed:
            opcode, payload, buf = read_frame(sock, buf)
            if opcode == 0x8:  # close
                break
            if opcode == 0x9:  # ping → pong
                sock.sendall(_client_frame(0xA, payload))
                continue
            if opcode not in (0x1, 0x0):
                continue
            msg = json.loads(payload)
            if msg.get("type") == "stream.closed":
                closed = True
            elif msg.get("type") == "stream.error":
                raise RuntimeError(msg.get("message", "stream error"))
            else:
                events.append(msg)
    finally:
        sock.close()
    return events, closed


def _client_frame(opcode, payload):
    mask = os.urandom(4)
    n = len(payload)
    if n < 126:
        hdr = bytes([0x80 | opcode, 0x80 | n])
    elif n < 65536:
        hdr = bytes([0x80 | opcode, 0x80 | 126]) + struct.pack("!H", n)
    else:
        hdr = bytes([0x80 | opcode, 0x80 | 127]) + struct.pack("!Q", n)
    return hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload))


# ---------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--api", default="http://127.0.0.1:8765")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--spec", default="testkit/sites/admin/task.json")
    args = ap.parse_args()

    status, body, _ = request(args.api, "GET", "/v1/health")
    check(status == 200 and body.get("ok") is True, "health endpoint answers")

    request(args.site, "POST", "/admin/__reset")

    spec = json.load(open(args.spec))
    site_port = urllib.parse.urlparse(args.site).port or 80
    raw = json.dumps(spec).replace("8766", str(site_port))
    spec = json.loads(raw)

    status, body, _ = request(args.api, "POST", "/v1/tasks", spec)
    check(status == 202 and body.get("task_id"), f"task accepted over REST ({status})")
    task_id = body.get("task_id") if isinstance(body, dict) else None
    if not task_id:
        return finish()

    state = wait_state(args.api, task_id, "succeeded")
    check(state == "succeeded", f"task reached succeeded (got {state})")

    status, body, _ = request(args.api, "GET", f"/v1/tasks/{task_id}")
    check(status == 200, "status endpoint answers")
    steps = body.get("steps", []) if isinstance(body, dict) else []
    verified = [s for s in steps if s.get("verified") is True]
    check(len(verified) >= 2, f"{len(verified)} steps verified")
    check(body["task"]["model_calls"] == 0, "no model calls for a scripted task")

    status, events, _ = request(args.api, "GET", f"/v1/tasks/{task_id}/events")
    seqs = [e["seq"] for e in events]
    check(status == 200 and seqs == sorted(seqs) and len(set(seqs)) == len(seqs),
          "event seq is strictly increasing")
    kinds = [e["type"] for e in events]
    check("task.created" in kinds and "task.finished" in kinds,
          "event log has creation and completion")

    ws_events, closed = tail_events(args.api, task_id)
    ws_kinds = [e["type"] for e in ws_events]
    check(len(ws_events) == len(events), f"websocket replayed all {len(events)} events")
    check("task.finished" in ws_kinds, "websocket carried task.finished")
    check(closed, "websocket closed the stream after the terminal event")

    status, arts, _ = request(args.api, "GET", f"/v1/tasks/{task_id}/artifacts")
    csv = next((a for a in arts if (a.get("name") or "").endswith(".csv")), None) if status == 200 else None
    check(csv is not None, "task lists a CSV artifact")
    if csv:
        status, blob, ctype = request(
            args.api, "GET", urllib.parse.quote(csv["url"], safe="/%"), raw=True
        )
        check(status == 200 and ctype.startswith("text/csv"), "artifact downloads as text/csv")
        digest = hashlib.sha256(blob).hexdigest()
        check(digest == csv["sha256"], "artifact bytes match the listed sha256")

        _, site_state, _ = request(args.site, "GET", "/admin/__state")
        recorded = site_state["downloads"][-1]["sha256"]
        check(digest == recorded, "artifact matches the site's server-side download record")

    # An empty workflow is a planning request only when mode=auto. A scripted
    # workflow with no steps is still a client error, not a task that sits
    # around forever.
    status, body, _ = request(
        args.api,
        "POST",
        "/v1/tasks",
        {"protocol_version": "0.1", "kind": "workflow", "goal": "x",
         "mode": "scripted", "policy": {"max_wall_ms": 1000}, "steps": []},
    )
    check(status == 422, f"empty scripted workflow refused with 422 (got {status})")

    # mode=auto accepts the empty workflow and plans inside the task. This
    # server has no model endpoint, so the task must fail with an honest
    # planning_unavailable rather than pretend it has a plan.
    status, body, _ = request(
        args.api,
        "POST",
        "/v1/tasks",
        {"protocol_version": "0.1", "kind": "workflow", "goal": "x",
         "mode": "auto", "policy": {"max_wall_ms": 5000}, "steps": []},
    )
    check(status == 202, f"empty auto workflow accepted with 202 (got {status})")
    if status == 202:
        aid = body["task_id"]
        state = wait_state(args.api, aid, "failed", timeout=15)
        check(state == "failed", f"auto workflow without a model fails (got {state})")
        status, body, _ = request(args.api, "GET", f"/v1/tasks/{aid}")
        err = (body.get("task", {}).get("error") or {}).get("message", "")
        check("planning_unavailable" in err,
              f"failure names planning_unavailable ({err[:100]!r})")

    status, body, _ = request(args.api, "POST", "/v1/tasks", dict(spec, id=task_id))
    check(status == 409, f"duplicate task id refused with 409 (got {status})")

    # Cancellation while a task is still working.
    crash = json.load(open("testkit/sites/admin/task-crash.json"))
    crash = json.loads(json.dumps(crash).replace("8766", str(site_port)))
    status, body, _ = request(args.api, "POST", "/v1/tasks", crash)
    if status == 202:
        cid = body["task_id"]
        time.sleep(0.8)
        status, body, _ = request(args.api, "POST", f"/v1/tasks/{cid}/cancel")
        check(status == 200, "cancel accepted")
        state = wait_state(args.api, cid, "canceled", timeout=15)
        check(state == "canceled", f"task stopped as canceled (got {state})")
    else:
        bad(f"cancel case could not start a task ({status})")

    return finish()


def finish():
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
