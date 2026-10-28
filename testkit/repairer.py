#!/usr/bin/env python3
"""A deterministic stand-in for a repair model.

A step with `retry.on_exhausted=ask_model` sends its failed step, the error, and
the current page to a text model and expects one corrected replacement step
back. This process returns the known-good testkit filter step, so the harness
side can be exercised without a real model: request shape, schema validation,
the safety envelope, dispatch, and verification.

`POST /__mode` lets a suite exercise the rejection paths:

    {"mode": "good"}               corrected read-only step (default)
    {"mode": "fail_action"}        corrected step that still cannot run
    {"mode": "raise_side_effect"}  asks for irreversible
    {"mode": "change_id"}          renames the step
    {"mode": "change_executor"}    forces the vision executor
    {"mode": "change_success"}     weakens the success condition
    {"mode": "change_preconditions"} adds a JS precondition
    {"mode": "inject_js"}          adds an evaluate action
    {"mode": "inject_extract"}     adds an extract action
    {"mode": "external_nav"}       navigates to another host
    {"mode": "drop_success"}       removes the success condition

The stub echoes the failed step it received (id, goal, preconditions, success,
fail_when, executor, side_effect, retry) and only rewrites the actions. That is
what a real repair is supposed to do; the modes above deliberately violate one
part of the envelope at a time.

`GET /__last` returns the call count and the last request text, so a suite can
assert what the harness actually sent. `POST /__mode` resets both.

    python3 testkit/repairer.py --port 8792
"""

from __future__ import annotations

import argparse
import copy
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FAILED_STEP = re.compile(r"Failed step:\n(\{.*?\})\n\nFailure:", re.S)


def corrected(failed: dict, mode: str) -> dict:
    step = copy.deepcopy(failed)
    actions = [
        {"type": "click", "target": {"role": "button", "name": "失败"}},
        {"type": "type", "target": {"placeholder": "订单号或客户"},
         "text": "100", "clear_first": True},
    ]
    if mode == "external_nav":
        actions.insert(0, {"type": "navigate", "url": "http://evil.example/"})
    if mode == "fail_action":
        actions = [{"type": "click",
                    "target": {"role": "button", "name": "这个按钮不存在"}}]
    if mode == "inject_js":
        actions.append({"type": "evaluate", "script": "document.cookie"})
    if mode == "inject_extract":
        actions.append({"type": "extract", "name": "creds",
                        "script": "({cookie: document.cookie})"})
    step["actions"] = actions

    if mode == "change_id":
        step["id"] = str(step.get("id", "s?")) + "-renamed"
    if mode == "change_executor":
        step["executor"] = "vision"
    if mode == "change_success":
        step["success"] = {"type": "js", "script": "true"}
    if mode == "change_preconditions":
        step["preconditions"] = [{"type": "js", "script": "true"}]
    if mode == "drop_success":
        step.pop("success", None)
    if mode == "raise_side_effect":
        step["side_effect"] = "irreversible"

    return {
        "steps": [step],
        "confidence": 0.82,
        "reason": f"testkit repair stub ({mode})",
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "bhx-repairer-stub/1"

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
            return self._json(200, {"ok": True, "backend": "rule-repairer",
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
            m = FAILED_STEP.search(text)
            if not m:
                raise ValueError("request has no Failed step JSON block")
            failed = json.loads(m.group(1))
            step_id = str(failed.get("id", "s?"))
            result = corrected(failed, self.server.mode)
            print(f"repairer: {self.server.mode} -> {step_id}", file=sys.stderr, flush=True)
        except Exception as e:  # a test stub: report, do not crash the suite
            result = {"error": str(e)}
        return self._json(200, envelope(result))


def envelope(result: dict) -> dict:
    return {
        "id": "stub-repair-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": json.dumps(result)},
            "finish_reason": "stop",
        }],
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8792)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    httpd.verbose = args.verbose
    httpd.mode = "good"
    httpd.calls = 0
    httpd.last_text = ""
    print(f"repairer stub on http://{args.host}:{args.port}/v1/chat/completions", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
