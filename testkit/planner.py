#!/usr/bin/env python3
"""A deterministic stand-in for a text planning model.

`bhx plan` sends a goal plus the observed page state to an OpenAI-compatible
endpoint and expects workflow steps back. This process is the model end of that
path, implemented as a rule instead of a neural network: it reads the URL out of
the state text and returns the testkit admin workflow (log in, filter failed
orders, export CSV).

That tests everything the harness owns — the request shape, step schema
validation, id normalization, credential placeholders, and running the planned
task through the engine. It says nothing about a real model's planning quality;
the docs say so.

`POST /__mode` makes the stub return a deliberately bad plan; `GET /__last`
returns the call count and last request text. Modes:

    good           the admin workflow (default)
    empty          zero steps
    bad_action     an unknown action variant
    no_actions     a step with an empty action list
    bad_timeout    timeout_ms = 0
    external_nav   navigates outside allowed_hosts
    unknown_cred   references a credential the task does not hold

    python3 testkit/planner.py --port 8791
    BH_LLM_URL=http://127.0.0.1:8791/v1/chat/completions \
      bhx plan --goal "登录后台，筛选失败订单，导出 CSV" --url http://127.0.0.1:8766/ --run
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

URL_LINE = re.compile(r"^URL: (\S+)", re.MULTILINE)
GOAL_LINE = re.compile(r"^Goal: (.*)$", re.MULTILINE)


def origin_of(url: str) -> str:
    m = re.match(r"^(https?://[^/?#]+)", url)
    return m.group(1) if m else url


def plan(state: str, goal: str, mode: str = "good") -> dict:
    m = URL_LINE.search(state)
    if not m:
        return {"error": "state has no URL line"}
    base = origin_of(m.group(1)) + "/"

    # The login page is the first thing the planner sees. Credentials are
    # placeholders: the planner never sees, invents or returns a secret.
    steps = [
        {
            "id": "login",
            "goal": "登录运营后台",
            "actions": [
                {"type": "navigate", "url": base},
                {"type": "type", "target": {"role": "textbox", "name": "用户名"},
                 "text": "${cred:username}", "clear_first": True},
                {"type": "type", "target": {"role": "textbox", "name": "密码"},
                 "text": "${cred:password}", "clear_first": True},
                {"type": "click", "target": {"role": "button", "name": "登录"}},
            ],
            "success": {"type": "element_visible",
                        "locator": {"role": "button", "name": "导出 CSV"}},
            "timeout_ms": 20000,
        },
        {
            "id": "filter",
            "goal": "筛选失败订单",
            "preconditions": [{"type": "element_visible",
                               "locator": {"role": "button", "name": "导出 CSV"}}],
            "actions": [
                {"type": "click", "target": {"role": "button", "name": "失败"}},
                {"type": "type", "target": {"placeholder": "订单号或客户"},
                 "text": "100", "clear_first": True},
            ],
            "success": {"type": "js", "script":
                        "document.querySelectorAll('#rows tr').length === 2"},
            "timeout_ms": 15000,
        },
        {
            "id": "export",
            "goal": "导出筛选结果为 CSV",
            "actions": [
                {"type": "click", "target": {"role": "button", "name": "导出 CSV"}},
            ],
            "success": {"type": "download_completed",
                        "filename_regex": "^orders-.*\\.csv$", "min_bytes": 20},
            "timeout_ms": 20000,
        },
    ]

    # Modes exist only so the suite can prove the harness rejects a bad plan
    # before any step runs. They are not model behavior.
    if mode == "empty":
        steps = []
    elif mode == "bad_action":
        steps[0]["actions"] = [{"type": "teleport"}]
    elif mode == "no_actions":
        steps[0]["actions"] = []
    elif mode == "bad_timeout":
        steps[0]["timeout_ms"] = 0
    elif mode == "external_nav":
        steps[0]["actions"].insert(0, {"type": "navigate", "url": "http://evil.example/"})
    elif mode == "unknown_cred":
        steps[0]["actions"].insert(
            0, {"type": "type", "target": {"role": "textbox", "name": "用户名"},
                "text": "${cred:not_supplied}", "clear_first": True})

    return {
        "steps": steps,
        "confidence": 0.8,
        "reason": f"testkit admin workflow for goal: {goal} ({mode})",
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "bhx-planner-stub/1"

    def log_message(self, fmt: str, *args) -> None:
        if self.server.verbose:
            super().log_message(fmt, *args)

    def _json(self, code: int, value) -> None:
        body = json.dumps(value, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/health":
            return self._json(200, {"ok": True, "backend": "rule-planner",
                                    "mode": self.server.mode})
        if self.path == "/__last":
            return self._json(200, {"calls": self.server.calls,
                                    "text": self.server.last_text,
                                    "mode": self.server.mode})
        self._json(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path == "/__mode":
            length = int(self.headers.get("Content-Length") or 0)
            try:
                body = json.loads(self.rfile.read(length) or b"{}")
                self.server.mode = body.get("mode", "good")
                self.server.calls = 0
                self.server.last_text = ""
            except Exception as e:
                return self._json(400, {"error": str(e)})
            return self._json(200, {"mode": self.server.mode})
        if self.path != "/v1/chat/completions":
            return self._json(404, {"error": "not found"})
        length = int(self.headers.get("Content-Length") or 0)
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
            user = req["messages"][1]["content"]
            text = " ".join(part.get("text", "") for part in user
                            if part.get("type") == "text")
            self.server.calls += 1
            self.server.last_text = text
            g = GOAL_LINE.search(text)
            result = plan(text, g.group(1) if g else "", self.server.mode)
            print(f"planner: {self.server.mode} -> {len(result.get('steps', []))} step(s)",
                  file=sys.stderr, flush=True)
        except Exception as e:  # a test stub: report, do not crash the suite
            result = {"error": str(e)}
        return self._json(200, envelope(result))


def envelope(result: dict) -> dict:
    return {
        "id": "stub-plan-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": json.dumps(result)},
            "finish_reason": "stop",
        }],
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8791)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    httpd.verbose = args.verbose
    httpd.mode = "good"
    httpd.calls = 0
    httpd.last_text = ""
    print(f"planner stub on http://{args.host}:{args.port}/v1/chat/completions", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
