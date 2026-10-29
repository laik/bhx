#!/usr/bin/env python3
"""A deterministic stand-in for a candidate-choosing model (`Capability::Locate`).

A Locate request lists the elements that matched one locator and asks which one
the step means. This process answers from a mode, so the harness side can be
exercised without a real model: request shape, id validation, the confidence
floor, whether the chosen element is the one acted on, and what gets recorded.

    python3 testkit/chooser.py --port 8794
    BH_LLM_URL=http://127.0.0.1:8794/v1/chat/completions

Modes (`POST /__mode {"mode": ...}`):

    first        pick the first candidate the harness offered (default)
    last         pick the last candidate offered
    flip         first option while the page clock is under 3000 ms, then switch
                 to the last one with only 0.6 confidence — the shape a real
                 local model produced: a right first answer, then a late,
                 weakly-held change of mind
    second       pick the second candidate
    semantic     pick the candidate whose label matches a word in the task goal
    confident    pick the second candidate with confidence 0.9
    shy          pick the second candidate with confidence 0.2 (below any sane floor)
    invented     answer with an id that was never offered
    empty        answer with no choice at all
    error        return 500, so the call fails

`GET /__last` returns the call count and the last request text, so a suite can
assert what was actually offered to the model — including that the candidates
came from resolution and were not invented here.
"""

from __future__ import annotations

import argparse
import json
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Candidate ids arrive in two shapes: the ambiguity arbiter offers c1..cN, and a
# controller's `decide` block offers the author's own option ids. Match either.
CANDIDATES = re.compile(r"^- ([A-Za-z0-9_-]+): (.*)$", re.M)
TASK = re.compile(r"^Task: (.*)$", re.M)
ELAPSED = re.compile(r'"elapsedMs"\s*:\s*(\d+)')


def pick(text: str, mode: str) -> dict:
    ids = [m.group(1) for m in CANDIDATES.finditer(text)]
    if mode == "invented":
        return {"choice": "c99", "confidence": 0.95, "reason": "invented id"}
    if mode == "empty":
        return {"confidence": 0.95, "reason": "no choice field"}
    if not ids:
        return {"confidence": 0.0, "reason": "the request listed no candidates"}
    if mode == "semantic":
        # Deliberately reads only the candidate labels and the task line, so a
        # suite can prove the harness delivered enough information to choose
        # with: this stub knows nothing about the page it is looking at.
        found = TASK.search(text)
        goal_text = found.group(1) if found else ""
        described = {m.group(1): m.group(2) for m in CANDIDATES.finditer(text)}
        words = [w for w in re.split(r"\s+", goal_text) if len(w) >= 2]
        for cid in ids:
            label = described[cid]
            if any(w.lower() in label.lower() for w in words):
                return {"choice": cid, "confidence": 0.9,
                        "reason": f"label matches the task: {label}"}
        return {"confidence": 0.1, "reason": "no label matched the task"}
    if mode == "flip":
        # The page clock is in the state text the harness sent (observe script
        # output), so the flip is driven by the game, not by call order.
        clock = ELAPSED.search(text)
        t = int(clock.group(1)) if clock else 0
        if t < 3000:
            return {"choice": ids[0], "confidence": 0.9, "reason": "covering the first option"}
        return {"choice": ids[-1], "confidence": 0.6,
                "reason": "changed my mind after the page clock passed 3s"}
    which = ids[0] if mode == "first" else (ids[1] if len(ids) > 1 else ids[0])
    described = next(m.group(2) for m in CANDIDATES.finditer(text) if m.group(1) == which)
    if mode == "last":
        which = ids[-1]
        described = next(m.group(2) for m in CANDIDATES.finditer(text) if m.group(1) == which)
    conf = 0.2 if mode == "shy" else 0.9
    return {"choice": which, "confidence": conf, "reason": f"picked {described}"}


class Handler(BaseHTTPRequestHandler):
    mode = "first"
    calls = 0
    last = None

    def log_message(self, fmt, *args):
        pass

    def _send(self, code: int, value) -> None:
        body = json.dumps(value).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            return self._send(200, {"ok": True, "mode": self.mode})
        if self.path == "/__last":
            return self._send(200, {"calls": self.calls, "text": self.last})
        self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path == "/__mode":
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
            Handler.mode = body.get("mode", "first")
            Handler.calls = 0
            Handler.last = None
            return self._send(200, {"mode": self.mode})
        if self.path != "/v1/chat/completions":
            return self._send(404, {"error": "not found"})

        length = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(length) or b"{}")
        Handler.calls += 1
        parts = []
        for msg in req.get("messages", []):
            content = msg.get("content")
            if isinstance(content, str):
                parts.append(content)
            elif isinstance(content, list):
                for c in content:
                    if isinstance(c, dict) and c.get("type") == "text":
                        parts.append(c["text"])
        text = "\n".join(parts)
        Handler.last = text

        if Handler.mode == "error":
            return self._send(500, {"error": "stub is down"})
        result = pick(text, Handler.mode)
        return self._send(200, {
            "choices": [{
                "message": {"role": "assistant", "content": json.dumps(result)},
                "finish_reason": "stop",
            }]
        })


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8794)
    a = ap.parse_args()
    print(f"chooser stub on :{a.port}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
