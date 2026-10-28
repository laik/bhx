#!/usr/bin/env python3
"""REST session smoke test for `bhx serve`.

Drives a browser the way an HTTP-only agent would: create a session, navigate,
observe, dispatch actions, wait for a condition, evaluate JS in both worlds,
take a screenshot, and close the page. Asserts on site state (`/admin/__state`,
`/admin/__downloads`) and on the files the session actually downloaded, never
on what the harness says about itself.

Usage:
    python3 testkit/session-smoke.py --api http://127.0.0.1:8771 \
        --site http://127.0.0.1:8770 --home /tmp/bhx-home

Exit code 0 only if every check passed.
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
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


def request(base, method, path, body=None, raw=False, timeout=20):
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


def poll(fn, timeout=8.0, interval=0.1):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    return last


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--api", default="http://127.0.0.1:8765")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--home", default="", help="serve --home, to check download placement")
    args = ap.parse_args()

    status, health, _ = request(args.api, "GET", "/v1/health")
    check(status == 200 and health.get("sessions") is True,
          "health advertises the session API")

    request(args.site, "POST", "/admin/__reset")

    status, created, _ = request(
        args.api,
        "POST",
        "/v1/sessions",
        {"allowed_hosts": ["127.0.0.1"]},
        timeout=60,
    )
    check(status == 201, f"session created with 201 (got {status})")
    sid = created.get("session_id") if isinstance(created, dict) else None
    if not sid:
        return finish()
    check(created.get("url") == "about:blank",
          f"new session starts on about:blank (got {created.get('url')!r})")

    status, sessions, _ = request(args.api, "GET", "/v1/sessions")
    ids = [s.get("session_id") for s in sessions] if isinstance(sessions, list) else []
    check(status == 200 and sid in ids, "session appears in the session list")

    status, info, _ = request(args.api, "GET", f"/v1/sessions/{sid}")
    check(status == 200 and info.get("allowed_hosts") == ["127.0.0.1"],
          "session info reports the allow-list")

    # ------------------------------------------------------------ navigate
    status, nav, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/navigate", {"url": args.site + "/admin/"}
    )
    check(status == 200 and nav.get("url", "").endswith("/admin/"),
          f"navigate lands on /admin/ ({status}, {nav.get('url') if isinstance(nav, dict) else nav})")
    check(isinstance(nav, dict) and "运营后台" in nav.get("title", ""),
          f"navigate returns the page title ({nav.get('title') if isinstance(nav, dict) else nav!r})")

    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/navigate", {"url": "http://127.0.0.2:9/x"}
    )
    check(status == 403, f"navigation outside allowed_hosts is refused with 403 (got {status})")
    status, info, _ = request(args.api, "GET", f"/v1/sessions/{sid}")
    check(status == 200 and info.get("url", "").endswith("/admin/"),
          "a refused navigation does not move the page")

    # ------------------------------------------------------------- observe
    status, obs, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/observe",
        {"goal": "登录后台", "max_elements": 40},
    )
    text = obs.get("text", "") if isinstance(obs, dict) else ""
    check(status == 200 and "Controls:" in text, "observe returns an element index")
    check(isinstance(obs, dict) and obs.get("elements", 0) >= 3,
          f"observe counts elements ({obs.get('elements') if isinstance(obs, dict) else None})")
    check("登录" in text, "the login button is in the observation")

    status, obs_shot, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/observe", {"screenshot": True}
    )
    shot = obs_shot.get("screenshot") if isinstance(obs_shot, dict) else None
    img = base64.b64decode(shot) if shot else b""
    check(status == 200 and img[:3] == b"\xff\xd8\xff",
          "observe can attach a JPEG screenshot")
    check(isinstance(obs_shot, dict) and obs_shot.get("screenshot_mime") == "image/jpeg",
          "the attached screenshot names its media type")
    check(isinstance(obs_shot, dict) and obs_shot.get("screenshot_error") is None,
          "the attached screenshot reported no error")

    # ------------------------------------------------------------ evaluate
    status, ev, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/evaluate",
        {"script": "document.querySelector('#user') !== null"},
    )
    check(status == 200 and ev.get("result") is True,
          "isolated-world evaluate can read the DOM")

    status, ev, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/evaluate",
        {"script": "window.__probe = 42; window.__probe", "world": "main"},
    )
    check(status == 200 and ev.get("result") == 42, "main-world evaluate sets a page global")

    status, ev, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/evaluate",
        {"script": "typeof window.__probe"},
    )
    check(status == 200 and ev.get("result") == "undefined",
          "isolated-world evaluate cannot see main-world globals")

    status, _, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/evaluate",
        {"script": "1", "world": "shadow"},
    )
    check(status == 422, f"an unknown JS world is refused with 422 (got {status})")

    # ------------------------------------------------------------- actions
    status, _, _ = request(args.api, "POST", f"/v1/sessions/{sid}/actions", {"actions": []})
    check(status == 422, f"an empty action batch is refused with 422 (got {status})")

    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/actions",
        {"actions": [{"type": "click", "target": {"css": "#definitely-not-here"}}]},
    )
    check(status == 422, f"an action that cannot resolve fails the batch with 422 (got {status})")
    check(isinstance(body, dict) and "[0] failed" in body.get("log", ""),
          "the failed batch log names the action index")

    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/actions",
        {"actions": [
            {"type": "type", "target": {"css": "#user"}, "text": "admin", "clear_first": True},
            {"type": "type", "target": {"css": "#pass"}, "text": "admin123", "clear_first": True},
        ]},
    )
    check(status == 200 and body.get("ok") is True, "typing batch succeeds")

    status, obs, _ = request(args.api, "POST", f"/v1/sessions/{sid}/observe", {})
    text = obs.get("text", "") if isinstance(obs, dict) else ""
    check('value="***"' in text, "the password value is masked in the observation")
    check("admin123" not in text, "the password never appears in the observation text")

    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/actions",
        {"actions": [{"type": "click", "target": {"css": "#login-btn"}}]},
    )
    check(status == 200 and body.get("ok") is True, "the login click succeeds")

    # ----------------------------------------------------------- condition
    status, cond, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/condition",
        {"condition": {"type": "js", "script": "!document.querySelector('#app').hidden"},
         "timeout_ms": 5000},
    )
    check(status == 200 and cond.get("holds") is True,
          f"condition wait sees the app (got {cond})")

    status, cond, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/condition",
        {"condition": {"type": "js", "script": "false"}},
    )
    check(status == 200 and cond.get("holds") is False,
          "a false condition is an answer, not an error")

    # A batch may contain navigation; the allow-list must stop it before the
    # first action runs.
    status, _, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/actions",
        {"actions": [
            {"type": "click", "target": {"css": "#export"}},
            {"type": "navigate", "url": "http://127.0.0.2:9/x"},
        ]},
    )
    check(status == 403, f"a batch navigating outside allowed_hosts is refused (got {status})")
    status, info, _ = request(args.api, "GET", f"/v1/sessions/{sid}")
    check(status == 200 and info.get("url", "").endswith("/admin/"),
          "the refused batch did not navigate")
    _, downloads, _ = request(args.site, "GET", "/admin/__downloads")
    check(len(downloads.get("downloads", [])) == 0,
          "the refused batch did not run its first action either")

    # --------------------------------------------------------- download
    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/actions",
        {"actions": [{"type": "click", "target": {"css": "#export"}}]},
    )
    check(status == 200 and body.get("ok") is True, "the export click succeeds")

    def latest_download():
        body = request(args.site, "GET", "/admin/__downloads")[1]
        rows = body.get("downloads", []) if isinstance(body, dict) else []
        return rows[-1] if rows else None

    record = poll(latest_download)
    check(record is not None, "the site recorded the download")
    if record:
        path = os.path.join(
            args.home, "sessions", sid, "downloads", record.get("name", "")
        ) if args.home else ""
        if args.home:
            # The site records the download before Chrome has necessarily
            # finished writing it, so give the file a moment to appear.
            check(poll(lambda: os.path.exists(path), timeout=5) is True,
                  f"the download landed in the session directory ({path})")
            if os.path.exists(path):
                with open(path, "rb") as f:
                    digest = hashlib.sha256(f.read()).hexdigest()
                check(digest == record.get("sha256"),
                      "the downloaded bytes match the site's record")
        # Screenshot endpoint returns the bytes directly, not JSON.
        status, blob, ctype = request(
            args.api, "GET", f"/v1/sessions/{sid}/screenshot", raw=True
        )
        check(status == 200 and ctype.startswith("image/png") and blob[:8] == b"\x89PNG\r\n\x1a\n",
              "the screenshot endpoint returns PNG bytes")

    # ------------------------------------------------------------ verify
    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/{sid}/verify",
        {"goal": "已登录后台", "threshold": 0.6},
    )
    check(status == 503, f"verify without a model adapter answers 503 (got {status})")

    # ---------------------------------------------------- second session
    status, created_b, _ = request(args.api, "POST", "/v1/sessions", {}, timeout=60)
    check(status == 201, f"a second session can be created (got {status})")
    sid_b = created_b.get("session_id") if isinstance(created_b, dict) else None
    if sid_b:
        status, nav_b, _ = request(
            args.api, "POST", f"/v1/sessions/{sid_b}/navigate", {"url": args.site + "/game/"}
        )
        check(status == 200 and nav_b.get("url", "").endswith("/game/"),
              "the second session navigates independently")
        _, info_a, _ = request(args.api, "GET", f"/v1/sessions/{sid}")
        check(info_a.get("url", "").endswith("/admin/"),
              "navigating the second session leaves the first on its page")

        status, closed, _ = request(args.api, "DELETE", f"/v1/sessions/{sid_b}")
        check(status == 200 and closed.get("closed") is True,
              "closing the second session succeeds")
        status, _, _ = request(args.api, "GET", f"/v1/sessions/{sid_b}")
        check(status == 404, f"a closed session is gone (got {status})")

        status, cond, _ = request(
            args.api, "POST", f"/v1/sessions/{sid}/condition",
            {"condition": {"type": "js", "script": "document.title.includes('运营后台')"}},
        )
        check(status == 200 and cond.get("holds") is True,
              "the first session still works after the second one closed")

    # -------------------------------------------------------------- close
    status, body, _ = request(
        args.api, "POST", f"/v1/sessions/nope/observe", {}
    )
    check(status == 404, f"an unknown session is 404 (got {status})")

    status, closed, _ = request(args.api, "DELETE", f"/v1/sessions/{sid}")
    check(status == 200 and closed.get("closed") is True, "closing the session succeeds")
    status, _, _ = request(args.api, "POST", f"/v1/sessions/{sid}/observe", {})
    check(status == 404, f"a closed session no longer answers (got {status})")
    status, sessions, _ = request(args.api, "GET", "/v1/sessions")
    ids = [s.get("session_id") for s in sessions] if isinstance(sessions, list) else []
    check(sid not in ids, "the closed session is removed from the list")

    return finish()


def finish():
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
