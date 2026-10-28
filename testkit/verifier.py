#!/usr/bin/env python3
"""A deterministic stand-in for a verification model.

`browser_verify` asks a text model whether the current page state shows the
goal achieved, and expects one of two fixed choices back. This process answers
that with a rule: the testkit admin page shows `导出 CSV` only after a
successful login, so its presence means "achieved".

That exercises the harness side — the state sent, the fixed choices, proposal
validation, the confidence threshold, and the uncertain verdict — and says
nothing about a real model's judgement. When the task asked for a screenshot,
the image part is recorded so a test can prove exactly what the model was shown.

    python3 testkit/verifier.py --port 8793
    BH_LLM_URL=http://127.0.0.1:8793/v1/chat/completions bhx mcp

`POST /__mode {"choice": "achieved"|"not_achieved", "confidence": 0.4}` forces the
next answers; `{"choice": "auto"}` restores the rule. `GET /__last` returns the
last request's text and image summary (media type, byte count, sha256). Only the
test suites use either.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MARKER = "导出 CSV"


def judge(text: str, mode: dict) -> dict:
    choice = mode.get("choice", "auto")
    if choice in ("achieved", "not_achieved"):
        return {
            "choice": choice,
            "confidence": float(mode.get("confidence", 0.9)),
            "reason": f"forced by test mode {choice}",
        }
    if MARKER in text:
        return {
            "choice": "achieved",
            "confidence": 0.9,
            "reason": "the export control is visible, which the admin page only shows after login",
        }
    return {
        "choice": "not_achieved",
        "confidence": 0.88,
        "reason": "the export control is absent; the page is still the login form",
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "bhx-verifier-stub/1"

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
            return self._json(200, {"ok": True, "backend": "rule-verifier", "mode": self.server.mode})
        if self.path == "/__last":
            return self._json(200, self.server.last)
        self._json(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path == "/__mode":
            length = int(self.headers.get("Content-Length") or 0)
            try:
                self.server.mode = json.loads(self.rfile.read(length) or b"{}")
            except Exception as e:
                return self._json(400, {"error": str(e)})
            return self._json(200, self.server.mode)
        if self.path != "/v1/chat/completions":
            return self._json(404, {"error": "not found"})
        length = int(self.headers.get("Content-Length") or 0)
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
            user = req["messages"][1]["content"]
            text = " ".join(part.get("text", "") for part in user
                            if part.get("type") == "text")
            image = None
            for part in user:
                if part.get("type") != "image_url":
                    continue
                head, _, b64 = part["image_url"]["url"].partition(",")
                data = base64.b64decode(b64)
                image = {
                    "media_type": head.removeprefix("data:").split(";")[0],
                    "bytes": len(data),
                    "sha256": hashlib.sha256(data).hexdigest(),
                }
            self.server.last = {"text": text, "image": image}
            self.server.last_image = image
            result = judge(text, self.server.mode)
            print(f"verifier: {result['choice']}", file=sys.stderr, flush=True)
        except Exception as e:  # a test stub: report, do not crash the suite
            result = {"error": str(e)}
        return self._json(200, envelope(result))


def envelope(result: dict) -> dict:
    return {
        "id": "stub-verify-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": json.dumps(result)},
            "finish_reason": "stop",
        }],
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8793)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    httpd.verbose = args.verbose
    httpd.mode = {"choice": "auto"}
    httpd.last = {"text": None, "image": None}
    print(f"verifier stub on http://{args.host}:{args.port}/v1/chat/completions", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
