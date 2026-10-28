#!/usr/bin/env python3
"""Smoke test for attaching to an already-running Chrome (`BH_CDP_URL`).

Usage: python3 testkit/remote-smoke.py [--bin ./target/debug/bhx]
                                       [--port 8775] [--cdp-port 9333]

The point is not "a task passed". It is that the harness can use a browser it
did not start: it drives it, closes only the tab it opened, leaves the browser
running, and refuses to launch a local one when the endpoint is wrong. Exits
non-zero on the first failed assertion.
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

here = Path(__file__).resolve().parent

ap = argparse.ArgumentParser()
ap.add_argument("--bin", default="./target/debug/bhx")
ap.add_argument("--port", type=int, default=8775)
ap.add_argument("--cdp-port", type=int, default=9333)
args = ap.parse_args()

site = f"http://127.0.0.1:{args.port}"
# The second run needs a fresh origin: the page keeps the login in the
# browser profile's localStorage, so reusing one server would start the
# ws:// run already logged in, with no 用户名 field to type into.
site2 = f"http://127.0.0.1:{args.port + 1}"
cdp = f"http://127.0.0.1:{args.cdp_port}"
CHECKS = 0
FAILED = 0


def check(cond: bool, what: str) -> None:
    global CHECKS, FAILED
    CHECKS += 1
    if cond:
        print(f"ok    {what}")
    else:
        FAILED += 1
        print(f"FAIL  {what}")
        sys.exit(1)  # later assertions build on earlier ones


def up(url: str) -> bool:
    try:
        urllib.request.urlopen(url, timeout=0.5).read()
        return True
    except Exception:
        return False


def get(url: str):
    return json.loads(urllib.request.urlopen(url, timeout=2).read())


def downloads(base: str | None = None) -> list:
    return get(f"{base or site}/admin/__downloads")["downloads"]


def targets() -> int:
    # Only page targets: Chrome's extension background pages and service
    # workers come and go on their own and would make the count flaky.
    return sum(1 for t in get(f"{cdp}/json/list") if t["type"] == "page")


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def chrome_binary() -> str | None:
    for c in [
        os.environ.get("BH_CHROME"),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ]:
        if not c:
            continue
        if os.path.sep in c and os.path.exists(c):
            return c
        found = shutil.which(c)
        if found:
            return found
    return None


def run(*argv: str, env: dict | None = None) -> subprocess.CompletedProcess:
    base = {k: v for k, v in os.environ.items() if k != "BH_CDP_URL"}
    if env:
        base.update(env)
    return subprocess.run([args.bin, *argv], capture_output=True, text=True, env=base)


exe = chrome_binary()
if not exe:
    sys.exit("no Chrome found; set BH_CHROME")

server = None
if not up(f"{site}/admin/__state"):
    server = subprocess.Popen(
        [sys.executable, "-I", str(here / "server.py"), "--port", str(args.port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(100):
        if up(f"{site}/admin/__state"):
            break
        time.sleep(0.05)

server2 = subprocess.Popen(
    [sys.executable, "-I", str(here / "server.py"), "--port", str(args.port + 1)],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
for _ in range(100):
    if up(f"{site2}/admin/__state"):
        break
    time.sleep(0.05)
else:
    server2.kill()
    sys.exit(f"the second test server did not start on {site2}")

work = tempfile.mkdtemp(prefix="bhx-remote-")
profile = Path(work) / "profile"
chrome = subprocess.Popen(
    [
        exe,
        f"--remote-debugging-port={args.cdp_port}",
        f"--user-data-dir={profile}",
        "--headless=new",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "about:blank",
    ],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
for _ in range(100):
    if up(f"{cdp}/json/version"):
        break
    time.sleep(0.1)
else:
    chrome.kill()
    server2.kill()
    if server is not None:
        server.kill()
    sys.exit(f"chrome did not expose CDP on {cdp}")

spec_path = Path(work) / "task.json"
spec = json.loads((here / "sites" / "admin" / "task.json").read_text().replace("8766", str(args.port)))
spec_path.write_text(json.dumps(spec, ensure_ascii=False))

spec2_path = Path(work) / "task2.json"
spec2 = json.loads((here / "sites" / "admin" / "task.json").read_text().replace("8766", str(args.port + 1)))
spec2_path.write_text(json.dumps(spec2, ensure_ascii=False))

try:
    # ------------------------------------------------- http:// endpoint form
    home = str(Path(work) / "home-http")
    before_targets = targets()
    before_dl = len(downloads())
    r = run("task", "run", str(spec_path), "--home", home, env={"BH_CDP_URL": cdp})
    check(r.returncode == 0, f"the task ran against the remote browser ({r.stderr.strip()[:160]})")
    check(json.loads(r.stdout)["state"] == "succeeded", "it succeeded")
    check(len(downloads()) == before_dl + 1, "the site recorded the export")
    check(chrome.poll() is None, "the remote browser is still running after bhx exits")
    check(targets() == before_targets, "the tab bhx opened was closed again")
    check(Path(home, "tasks").is_dir(), "the task used the given home, not a throwaway")

    # ---------------------------------------------- ws:// endpoint form
    ws = get(f"{cdp}/json/version")["webSocketDebuggerUrl"]
    home2 = str(Path(work) / "home-ws")
    before_dl = len(downloads(site2))
    r = run("task", "run", str(spec2_path), "--home", home2, env={"BH_CDP_URL": ws})
    check(r.returncode == 0, f"a ws:// endpoint works too ({r.stderr.strip()[:160]})")
    check(json.loads(r.stdout)["state"] == "succeeded", "the ws:// run succeeded")
    check(len(downloads(site2)) == before_dl + 1, "the second run also exported")
    check(targets() == before_targets, "the second run left no tab behind")
    check(chrome.poll() is None, "the remote browser survived both runs")

    # ------------------------------------- a dead endpoint must not fall back
    dead = f"http://127.0.0.1:{free_port()}"
    home3 = str(Path(work) / "home-dead")
    before_dead = len(downloads())
    started = time.time()
    r = run("task", "run", str(spec_path), "--home", home3, env={"BH_CDP_URL": dead})
    elapsed = time.time() - started
    check(r.returncode != 0, "a dead endpoint fails the run")
    check("CDP" in r.stderr and "remote-debugging-port" in r.stderr,
          f"the failure says what to start ({r.stderr.strip()[:160]})")
    check(elapsed < 20, f"it gives up instead of hanging ({elapsed:.1f}s)")
    st = json.loads(run("stats", "--json", "--home", home3).stdout)
    check(not st["by_state"].get("pending"),
          f"a failed launch leaves no dangling pending task ({st['by_state']})")
    check(st["tasks"] == 0 or st["by_state"].get("failed") == st["tasks"],
          "any task it recorded is failed")
    check(len(downloads()) == before_dead, "the dead endpoint downloaded nothing")
finally:
    chrome.kill()
    chrome.wait(timeout=10)
    server2.kill()
    if server is not None:
        server.kill()

print(f"\npassed: {CHECKS}, failed: {FAILED}")
sys.exit(1 if FAILED else 0)
