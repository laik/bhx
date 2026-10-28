#!/usr/bin/env python3
"""Assertions for the acceptance suites.

Every check compares what the harness produced against the server's own state,
never against the page's own claims. Run with `-I` so nothing in the current
directory can shadow an import.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import urllib.request


def state(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=5) as r:
        return json.load(r)


def fail(msg: str) -> None:
    print(f"FAIL  {msg}")
    sys.exit(1)


def ok(msg: str) -> None:
    print(f"ok    {msg}")


def check_extract(artifact: str, url: str) -> None:
    with open(artifact, encoding="utf-8") as f:
        got = json.load(f)
    want = state(url)["orders"]
    if not isinstance(got, list):
        fail(f"extract artifact is not a list: {type(got).__name__}")
    if len(got) != len(want):
        fail(f"extracted {len(got)} rows, server has {len(want)}")
    fields = ["order", "customer", "amount", "status"]
    for i, (row, truth) in enumerate(zip(got, want)):
        for field in fields:
            server_field = "id" if field == "order" else field
            if row.get(field) != truth[server_field]:
                fail(f"row {i} field {field}: {row.get(field)!r} != {truth[server_field]!r}")
    ok(f"extracted {len(got)} rows, all {len(fields)} fields match server state")


def check_form(url: str, expect: dict) -> None:
    submissions = state(url)["submissions"]
    if len(submissions) != 1:
        fail(f"server has {len(submissions)} submissions, expected exactly 1")
    got = submissions[0]
    for k, v in expect.items():
        if got.get(k) != v:
            fail(f"submission field {k}: {got.get(k)!r} != {v!r}")
    ok(f"server accepted submission {got['id']} with all fields matching")


def check_download(artifact: str, url: str) -> None:
    downloads = state(url)["downloads"]
    if len(downloads) != 1:
        fail(f"server produced {len(downloads)} downloads, expected exactly 1")
    want = downloads[0]
    with open(artifact, "rb") as f:
        body = f.read()
    digest = hashlib.sha256(body).hexdigest()
    if len(body) != want["bytes"]:
        fail(f"artifact is {len(body)} bytes, server says {want['bytes']}")
    if digest != want["sha256"]:
        fail(f"artifact sha256 {digest[:12]}… != server {want['sha256'][:12]}…")
    if not body.startswith(b"\xef\xbb\xbf"):
        fail("exported CSV has no UTF-8 BOM; spreadsheet tools will misread it")
    ok(f"artifact matches server download {want['name']} byte for byte ({digest[:12]}…)")


def check_rows(url: str, rows: int) -> None:
    want = state(url)["orders"]
    if len(want) != rows:
        fail(f"server has {len(want)} orders")
    ok(f"server has {rows} orders")


def check_chaos(url: str, min_fires: int) -> None:
    s = state(url)
    applied = s["applied"] if "applied" in s else s["chaos_applied"]
    kinds = [a["on"] for a in applied]
    if kinds.count("click") < 1:
        fail(f"click (popup) rule never fired; applied={applied}")
    if kinds.count("request") < 1:
        fail(f"request (500) rule never fired; applied={applied}")
    if len(applied) < min_fires:
        fail(f"{len(applied)} chaos rules fired, expected {min_fires}")
    ok(f"chaos fired as injected: {', '.join(kinds)}")
    downloads = s["downloads"]
    if len(downloads) != 1:
        fail(f"after retry the server has {len(downloads)} downloads, expected 1")
    ok("the retried export was accepted by the server")


def check_game(url: str, min_x: int | None, min_y: int | None, clicks: int | None) -> None:
    s = state(url)
    if s["reports"] == 0:
        fail("the page never reported; the browser was gone before telemetry arrived")
    got = s["last_report"]
    # Held keys are the target's own account of input state, not the harness's
    # claim: a stuck key is visible here even if the task reported success.
    if got["keys"]:
        fail(f"keys still held at exit: {got['keys']}")
    if min_x is not None and got["x"] < min_x:
        fail(f"x={got['x']}, expected >= {min_x}")
    if min_y is not None and got["y"] < min_y:
        fail(f"y={got['y']}, expected >= {min_y}")
    if clicks is not None:
        if got["clicks"] != clicks:
            fail(f"clicks={got['clicks']}, expected {clicks}")
        lc = got.get("lastClick")
        if not lc:
            fail("click was counted but no coordinates were recorded")
        if not (0 <= lc["x"] < 360 and 0 <= lc["y"] < 240):
            fail(f"click landed outside the canvas: {lc}")
        print(f"ok    click landed at ({lc['x']}, {lc['y']}) inside the canvas")
    ok(f"no keys held; last state x={got['x']} y={got['y']} ({s['reports']} reports)")


def _near(what: str, got: dict, x: float, y: float, tol: float) -> None:
    dx, dy = got["x"] - x, got["y"] - y
    dist = (dx * dx + dy * dy) ** 0.5
    if dist > tol:
        fail(f"{what} at ({got['x']:.1f}, {got['y']:.1f}), expected ({x}, {y}) ±{tol}px "
             f"(off by {dist:.1f}px)")
    print(f"ok    {what} at ({got['x']:.1f}, {got['y']:.1f}), within {dist:.2f}px of ({x}, {y})")


def check_canvas(url: str, min_clicks: int | None, source: str | None,
                 click_near: tuple[float, float] | None, tol: float,
                 drag_start: tuple[float, float] | None,
                 drag_end: tuple[float, float] | None,
                 confirmed: bool) -> None:
    s = state(url)
    if min_clicks is not None and s["clicks"] < min_clicks:
        fail(f"server recorded {s['clicks']} clicks, expected >= {min_clicks}")
    if source is not None:
        clicks = [e for e in s["events"] if e.get("type") == "click"]
        if not clicks:
            fail("no click event reached the server")
        got = clicks[-1]
        if got.get("source") != source:
            fail(f"last click source {got.get('source')!r}, expected {source!r}")
    if click_near is not None:
        clicks = [e for e in s["events"] if e.get("type") == "click"]
        if not clicks:
            fail("no click event reached the server")
        _near("click", clicks[-1], click_near[0], click_near[1], tol)
    if drag_start is not None or drag_end is not None:
        drags = [e for e in s["events"] if e.get("type") == "drag"]
        if not drags:
            fail("no drag event reached the server")
        got = drags[-1]
        if drag_start is not None:
            _near("drag start", got["start"], drag_start[0], drag_start[1], tol)
        if drag_end is not None:
            _near("drag end", got["end"], drag_end[0], drag_end[1], tol)
        if not got.get("ok"):
            fail(f"drag ended outside the drop zone: {got['end']}")
        print("ok    the page accepted the drop inside the zone")
    if confirmed:
        if s["confirmed"] < 1:
            fail("server never confirmed the action, yet the task reported success")
        print(f"ok    server confirmed the action {s['confirmed']} time(s)")
    if min_clicks is not None:
        print(f"ok    server recorded {s['clicks']} click(s), {len(s['events'])} event(s)")


def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("extract")
    p.add_argument("--artifact", required=True)
    p.add_argument("--state", required=True)

    p = sub.add_parser("form")
    p.add_argument("--state", required=True)
    p.add_argument("--expect", required=True, help="JSON object of expected fields")

    p = sub.add_parser("download")
    p.add_argument("--artifact", required=True)
    p.add_argument("--state", required=True)

    p = sub.add_parser("rows")
    p.add_argument("--state", required=True)
    p.add_argument("--count", type=int, required=True)

    p = sub.add_parser("chaos")
    p.add_argument("--state", required=True)
    p.add_argument("--min-fires", type=int, default=2)

    p = sub.add_parser("game")
    p.add_argument("--state", required=True)
    p.add_argument("--min-x", type=int)
    p.add_argument("--min-y", type=int)
    p.add_argument("--clicks", type=int)

    p = sub.add_parser("canvas")
    p.add_argument("--state", required=True)
    p.add_argument("--min-clicks", type=int)
    p.add_argument("--source", choices=["dom", "canvas"])
    p.add_argument("--click-near", type=float, nargs=2, metavar=("X", "Y"))
    p.add_argument("--tolerance", type=float, default=6.0)
    p.add_argument("--drag-start", type=float, nargs=2, metavar=("X", "Y"))
    p.add_argument("--drag-end", type=float, nargs=2, metavar=("X", "Y"))
    p.add_argument("--confirmed", action="store_true")

    a = ap.parse_args()
    if a.cmd == "extract":
        check_extract(a.artifact, a.state)
    elif a.cmd == "form":
        check_form(a.state, json.loads(a.expect))
    elif a.cmd == "download":
        check_download(a.artifact, a.state)
    elif a.cmd == "rows":
        check_rows(a.state, a.count)
    elif a.cmd == "chaos":
        check_chaos(a.state, a.min_fires)
    elif a.cmd == "game":
        check_game(a.state, a.min_x, a.min_y, a.clicks)
    elif a.cmd == "canvas":
        check_canvas(
            a.state, a.min_clicks, a.source,
            tuple(a.click_near) if a.click_near else None, a.tolerance,
            tuple(a.drag_start) if a.drag_start else None,
            tuple(a.drag_end) if a.drag_end else None,
            a.confirmed,
        )


if __name__ == "__main__":
    main()
