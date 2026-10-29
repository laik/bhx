#!/usr/bin/env python3
"""Local test sites plus the server-side truth the suites assert on.

The rule from docs/testing.md: a test never asserts "the agent thinks it
worked". It reads `/__state` and compares. The pages here are ordinary,
slightly hostile web pages; this process is the source of truth.

    python3 testkit/server.py --port 8766

Sites
  /            admin console (login, order table, filter, CSV export)
  /form/       multi-page form wizard submitted to the server
  /game/       canvas game; the page posts telemetry so the suite can check
               what the controller actually did to it
  /canvas/     canvas console: buttons and a drag target with no DOM semantics
Endpoints per site
  GET  /<site>/__state      server truth
  POST /<site>/__reset      back to a clean slate
  POST /<site>/__chaos      inject faults {seed, rules:[...]}
  GET  /admin/__downloads   files the server produced, with sha256
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote, unquote, urlparse

ROOT = Path(__file__).resolve().parent / "sites"

ORDERS = [
    {"id": "SO-1001", "customer": "杭州蓝鲸", "amount": "1,280.00", "status": "失败"},
    {"id": "SO-1002", "customer": "上海云栖", "amount": "640.00", "status": "已支付"},
    {"id": "SO-1003", "customer": "深圳前海", "amount": "2,300.00", "status": "失败"},
    {"id": "SO-1004", "customer": "北京望京", "amount": "98.00", "status": "已支付"},
    {"id": "SO-1005", "customer": "成都天府", "amount": "512.00", "status": "待支付"},
    {"id": "SO-1006", "customer": "广州珠江", "amount": "1,024.00", "status": "已支付"},
]

ROLES = {"engineer": "工程师", "manager": "经理", "other": "其他"}

LOCK = threading.Lock()


def fresh_state() -> dict:
    return {
        "admin": {"downloads": [], "chaos": [], "applied": []},
        "form": {"submissions": [], "chaos": [], "applied": []},
        "game": {"reports": 0, "last_report": None, "reports_log": []},
        "canvas": {"clicks": 0, "confirmed": 0, "events": [], "actions": []},
        # Which of two identically-named controls was acted on. The page has no
        # other way to tell the harness it picked the right one.
        "arbiter": {"clicks": []},
        "plane": {"reports": 0, "last_report": None, "reports_log": []},
    }


STATE = fresh_state()


# --------------------------------------------------------------- admin data

def orders_for(status: str, q: str) -> list[dict]:
    rows = ORDERS
    if status and status != "all":
        rows = [o for o in rows if o["status"] == status]
    q = (q or "").strip().lower()
    if q:
        rows = [o for o in rows if q in o["id"].lower() or q in o["customer"].lower()]
    return rows


def csv_for(status: str, q: str) -> tuple[str, bytes]:
    """The exported file, byte for byte what the page hands to the browser."""
    rows = orders_for(status, q)
    name = f"orders-{'all' if not status or status == 'all' else status}.csv"
    lines = ["订单号,客户,金额,状态"]
    lines += [",".join([o["id"], o["customer"], o["amount"], o["status"]]) for o in rows]
    body = ("﻿" + "\n".join(lines) + "\n").encode("utf-8")
    return name, body


def take_request_rule(site: str, path: str) -> dict | None:
    """Consume one matching `on: request` rule. Unmatched rules stay unconsumed
    so the suite can fail on 'injected but never fired'."""
    with LOCK:
        for rule in STATE[site]["chaos"]:
            if rule["on"] == "request" and rule.get("path") == path and rule.get("times", 1) > 0:
                rule["times"] -= 1
                STATE[site]["applied"].append(
                    {"id": rule["id"], "on": "request", "path": path, "status": rule.get("status")}
                )
                return rule
    return None


def take_delay(site: str, path: str) -> int:
    with LOCK:
        for rule in STATE[site]["chaos"]:
            if rule["on"] == "delay" and rule.get("path") == path and rule.get("times", 1) > 0:
                rule["times"] -= 1
                STATE[site]["applied"].append({"id": rule["id"], "on": "delay", "path": path})
                return int(rule.get("ms", 0))
    return 0


# ------------------------------------------------------------------- form

EMAIL = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
PHONE = re.compile(r"^1\d{10}$")


def validate_form(body: dict) -> dict[str, str]:
    errors: dict[str, str] = {}
    if not str(body.get("name", "")).strip():
        errors["name"] = "请输入姓名"
    if not EMAIL.match(str(body.get("email", ""))):
        errors["email"] = "邮箱格式不正确"
    if not PHONE.match(str(body.get("phone", ""))):
        errors["phone"] = "手机号必须是 11 位，以 1 开头"
    if not str(body.get("company", "")).strip():
        errors["company"] = "请输入公司名称"
    if body.get("role") not in ROLES:
        errors["role"] = "请选择角色"
    if body.get("agree") is not True:
        errors["agree"] = "请先同意条款"
    return errors


# ------------------------------------------------------------------ server

class Handler(BaseHTTPRequestHandler):
    server_version = "bhx-testkit/1"

    def log_message(self, fmt: str, *args) -> None:  # quiet by default
        if self.server.verbose:
            super().log_message(fmt, *args)

    # ------------------------------------------------------------ plumbing

    def _send(self, code: int, body: bytes, ctype: str, headers: dict | None = None) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _json(self, code: int, value) -> None:
        self._send(code, json.dumps(value, ensure_ascii=False).encode(), "application/json; charset=utf-8")

    def _body(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            return json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return {}

    # -------------------------------------------------------------- routes

    def do_GET(self) -> None:
        url = urlparse(self.path)
        path = unquote(url.path)
        query = parse_qs(url.query)

        if path == "/":
            return self._static("admin", "index.html")

        if path == "/admin/api/orders":
            delay = take_delay("admin", path)
            if delay:
                time.sleep(delay / 1000)
            rule = take_request_rule("admin", path)
            if rule and rule.get("status", 200) >= 400:
                return self._json(rule["status"], {"error": "injected"})
            status = (query.get("status") or ["all"])[0]
            q = (query.get("q") or [""])[0]
            return self._json(200, {"orders": orders_for(status, q)})

        if path == "/admin/api/export":
            delay = take_delay("admin", path)
            if delay:
                time.sleep(delay / 1000)
            rule = take_request_rule("admin", path)
            if rule and rule.get("status", 200) >= 400:
                return self._send(rule["status"], b"export failed\n", "text/plain; charset=utf-8")
            status = (query.get("status") or ["all"])[0]
            q = (query.get("q") or [""])[0]
            name, body = csv_for(status, q)
            with LOCK:
                STATE["admin"]["downloads"].append(
                    {
                        "name": name,
                        "bytes": len(body),
                        "sha256": hashlib.sha256(body).hexdigest(),
                        "status": status,
                        "q": q,
                        "at": time.time(),
                    }
                )
            disposition = f"attachment; filename*=UTF-8''{quote(name)}"
            return self._send(
                200, body, "text/csv; charset=utf-8", {"Content-Disposition": disposition}
            )

        if path == "/admin/__state":
            with LOCK:
                return self._json(
                    200,
                    {
                        "orders": ORDERS,
                        "downloads": STATE["admin"]["downloads"],
                        "chaos_applied": STATE["admin"]["applied"],
                        "chaos": STATE["admin"]["chaos"],
                    },
                )

        if path == "/admin/__downloads":
            with LOCK:
                return self._json(200, {"downloads": STATE["admin"]["downloads"]})

        if path == "/admin/__chaos":
            with LOCK:
                return self._json(200, {"rules": STATE["admin"]["chaos"]})

        if path == "/plane/__state":
            with LOCK:
                return self._json(200, STATE["plane"])

        if path == "/arbiter/__state":
            with LOCK:
                return self._json(200, STATE["arbiter"])

        if path == "/game/__state":
            with LOCK:
                return self._json(200, STATE["game"])

        if path == "/canvas/__state":
            with LOCK:
                return self._json(200, STATE["canvas"])

        if path == "/form/__state":
            with LOCK:
                return self._json(
                    200,
                    {
                        "submissions": STATE["form"]["submissions"],
                        "chaos_applied": STATE["form"]["applied"],
                        "roles": ROLES,
                    },
                )

        # An order row, for pages that link to a detail view.
        if path.startswith("/admin/order/"):
            oid = path.rsplit("/", 1)[-1]
            row = next((o for o in ORDERS if o["id"] == oid), None)
            if row is None:
                return self._json(404, {"error": "no such order"})
            return self._json(200, row)

        if path.startswith("/admin/"):
            return self._static("admin", path[len("/admin/") :])
        if path.startswith("/form/"):
            return self._static("form", path[len("/form/") :])
        if path.startswith("/game/"):
            return self._static("game", path[len("/game/") :])
        if path.startswith("/canvas/"):
            return self._static("canvas", path[len("/canvas/") :])
        if path.startswith("/plane/"):
            return self._static("plane", path[len("/plane/") :])
        if path.startswith("/arbiter/"):
            return self._static("arbiter", path[len("/arbiter/") :])

        self._json(404, {"error": "not found", "path": path})

    def do_POST(self) -> None:
        path = unquote(urlparse(self.path).path)

        if path == "/admin/__reset":
            with LOCK:
                STATE["admin"] = fresh_state()["admin"]
            return self._json(200, {"reset": "admin"})

        if path == "/plane/api/report":
            body = self._body()
            with LOCK:
                STATE["plane"]["reports"] += 1
                STATE["plane"]["last_report"] = body
                log = STATE["plane"]["reports_log"]
                log.append(body)
                del log[:-80]  # the tail is enough to check a strategy worked
            return self._json(200, {"ok": True})

        if path == "/arbiter/__click":
            # POST from the page: which of the two same-named buttons was used.
            body = self._body()
            with LOCK:
                STATE["arbiter"]["clicks"].append(str(body.get("id", "")))
            return self._json(200, {"ok": True})

        if path == "/plane/__reset":
            with LOCK:
                STATE["plane"] = fresh_state()["plane"]
            return self._json(200, {"reset": "plane"})

        if path == "/arbiter/__reset":
            with LOCK:
                STATE["arbiter"] = fresh_state()["arbiter"]
            return self._json(200, {"reset": "arbiter"})

        if path == "/form/__reset":
            with LOCK:
                STATE["form"] = fresh_state()["form"]
            return self._json(200, {"reset": "form"})

        if path == "/game/__reset":
            with LOCK:
                STATE["game"] = fresh_state()["game"]
            return self._json(200, {"reset": "game"})

        if path == "/canvas/__reset":
            with LOCK:
                STATE["canvas"] = fresh_state()["canvas"]
            return self._json(200, {"reset": "canvas"})

        if path == "/canvas/api/event":
            body = self._body()
            with LOCK:
                STATE["canvas"]["events"].append(body)
                del STATE["canvas"]["events"][:-50]
                if body.get("type") == "click":
                    STATE["canvas"]["clicks"] += 1
            return self._json(200, {"ok": True})

        if path == "/canvas/api/action":
            body = self._body()
            # Deliberately slow: the button does not change appearance, so a
            # success verdict based on pixels would be a guess. Only page state
            # (confirmed) knows the server accepted it.
            time.sleep(0.3)
            with LOCK:
                STATE["canvas"]["actions"].append(body)
                STATE["canvas"]["confirmed"] += 1
            return self._json(200, {"ok": True})

        if path == "/game/api/report":
            body = self._body()
            with LOCK:
                STATE["game"]["reports"] += 1
                STATE["game"]["last_report"] = body
                log = STATE["game"]["reports_log"]
                log.append(body)
                del log[:-50]  # keep the tail; this is diagnostic, not storage
            return self._json(200, {"ok": True})

        if path == "/admin/__chaos":
            body = self._body()
            rules = []
            for i, r in enumerate(body.get("rules") or []):
                rule = dict(r)
                rule.setdefault("id", f"r{i + 1}")
                rule.setdefault("times", 1)
                rules.append(rule)
            with LOCK:
                STATE["admin"]["chaos"] = rules
                STATE["admin"]["applied"] = []
                STATE["admin"]["seed"] = body.get("seed")
            return self._json(200, {"rules": rules, "seed": body.get("seed")})

        if path == "/admin/__chaos/fire":
            body = self._body()
            rid = body.get("id")
            with LOCK:
                for rule in STATE["admin"]["chaos"]:
                    if rule["id"] == rid and rule.get("times", 1) > 0:
                        rule["times"] -= 1
                        STATE["admin"]["applied"].append(
                            {"id": rid, "on": rule["on"], "selector": rule.get("selector")}
                        )
                        return self._json(200, {"fired": True, "id": rid})
            return self._json(200, {"fired": False, "id": rid})

        if path == "/form/api/submit":
            body = self._body()
            errors = validate_form(body)
            if errors:
                return self._json(422, {"errors": errors})
            submission = {
                "id": f"F-{len(STATE['form']['submissions']) + 1001}",
                "name": str(body["name"]).strip(),
                "email": str(body["email"]),
                "phone": str(body["phone"]),
                "company": str(body["company"]).strip(),
                "role": body["role"],
                "at": time.time(),
            }
            with LOCK:
                STATE["form"]["submissions"].append(submission)
            return self._json(200, {"accepted": True, "id": submission["id"]})

        self._json(404, {"error": "not found", "path": path})

    # ------------------------------------------------------------- statics

    def _static(self, site: str, rel: str) -> None:
        rel = rel or "index.html"
        if rel.endswith("/"):
            rel += "index.html"
        base = (ROOT / site).resolve()
        target = (base / rel).resolve()
        if not str(target).startswith(str(base)) or not target.is_file():
            return self._json(404, {"error": "not found", "path": self.path})
        ctype = {
            ".html": "text/html; charset=utf-8",
            ".js": "text/javascript; charset=utf-8",
            ".css": "text/css; charset=utf-8",
            ".json": "application/json; charset=utf-8",
            ".png": "image/png",
        }.get(target.suffix, "application/octet-stream")
        self._send(200, target.read_bytes(), ctype)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8766)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    httpd.verbose = args.verbose
    print(f"testkit serving {ROOT} on http://{args.host}:{args.port}/", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
