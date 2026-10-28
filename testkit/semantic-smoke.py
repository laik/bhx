#!/usr/bin/env python3
"""End-to-end check for `policy.semantic_verify`.

The mechanical success conditions in the admin workflow are all real, so every
step can be `verified` while the final result is still wrong. This suite makes
that gap visible: testkit/verifier.py is forced to answer achieved,
not_achieved, or with low confidence, and the task must react accordingly.

Checks, in order:
  achieved        -> task succeeds, one model call, task.verified event
  screenshot      -> the verifier receives the exact PNG recorded as an artifact
  no screenshot   -> without the policy flag the request carries no image
  not_achieved    -> all steps verified but task fails
  uncertain       -> waiting_approval; approve + resume finishes without redoing steps
  uncertain+fail  -> policy turns it into a task failure
  no adapter      -> configured check fails loudly instead of being skipped
  bad threshold   -> rejected before a browser or task is created

Exit code 0 only if every check passed.
"""

import argparse
import hashlib
import http.client
import json
import os
import re
import shutil
import subprocess
import sys
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


def request(base, method, path, body=None, timeout=15):
    u = urllib.parse.urlparse(base)
    conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=timeout)
    headers = {"Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    conn.request(method, u.path.rstrip("/") + path, body=data, headers=headers)
    r = conn.getresponse()
    payload = r.read()
    status = r.status
    conn.close()
    try:
        parsed = json.loads(payload) if payload else None
    except json.JSONDecodeError:
        parsed = payload.decode("utf-8", "replace")
    return status, parsed


def chrome_binary():
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


def render_dom(page):
    """Load the trace player in a real browser and return the post-script DOM."""
    exe = chrome_binary()
    if not exe:
        return None
    r = subprocess.run(
        [exe, "--headless=new", "--disable-gpu", "--no-first-run",
         "--no-default-browser-check", "--dump-dom", "file://" + os.path.abspath(page)],
        capture_output=True, text=True, timeout=30,
    )
    return r.stdout if r.returncode == 0 else None


def run(cmd, env=None, timeout=180):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=env)


class Suite:
    def __init__(self, args):
        self.args = args
        self.site = args.site.rstrip("/")
        self.verifier = args.verifier.rstrip("/")
        self.base = json.load(open(args.spec, encoding="utf-8"))
        self.tmp = args.tmp
        os.makedirs(self.tmp, exist_ok=True)

    # ------------------------------------------------------------- fixtures
    def reset_site(self):
        request(self.site, "POST", "/admin/__reset")

    def set_mode(self, **mode):
        status, value = request(self.verifier, "POST", "/__mode", mode)
        if status != 200:
            raise RuntimeError(f"could not set verifier mode: {status} {value}")

    def spec(self, name, semantic, max_calls=3):
        raw = json.dumps(self.base, ensure_ascii=False)
        raw = raw.replace("http://127.0.0.1:8766", self.site)
        spec = json.loads(raw)
        spec["policy"]["max_model_calls"] = max_calls
        spec["policy"]["semantic_verify"] = semantic
        path = os.path.join(self.tmp, f"{name}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(spec, f, ensure_ascii=False, indent=2)
        return path

    def env(self, with_model=True):
        env = os.environ.copy()
        if with_model:
            env["BH_LLM_URL"] = self.verifier + "/v1/chat/completions"
        else:
            env.pop("BH_LLM_URL", None)
            env.pop("BH_VLM_URL", None)
        return env

    def task_run(self, spec_path, home, with_model=True):
        return run(
            [self.args.bhx, "task", "run", spec_path, "--home", home],
            env=self.env(with_model),
        )

    def task_id(self, home):
        tasks = os.path.join(home, "tasks")
        ids = [n for n in os.listdir(tasks) if os.path.isdir(os.path.join(tasks, n))] \
            if os.path.isdir(tasks) else []
        return ids[0] if len(ids) == 1 else None

    def show(self, home, task_id):
        r = run([self.args.bhx, "task", "show", task_id, "--json", "--home", home])
        try:
            return json.loads(r.stdout)
        except json.JSONDecodeError:
            return {}

    def events(self, home, task_id):
        path = os.path.join(home, "tasks", task_id, "events.jsonl")
        if not os.path.exists(path):
            return []
        out = []
        for line in open(path, encoding="utf-8"):
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        return out

    # ------------------------------------------------------------- scenarios
    def achieved(self):
        print("\n== achieved")
        self.reset_site()
        self.set_mode(choice="achieved", confidence=0.9)
        home = os.path.join(self.tmp, "achieved")
        spec = self.spec("achieved", {"threshold": 0.6})
        r = self.task_run(spec, home)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode})")
        tid = self.task_id(home)
        check(tid is not None, "one task was created")
        if not tid:
            return
        t = self.show(home, tid)
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "succeeded", f"state succeeded (got {t.get('state')})")
        check(sem.get("verdict") == "achieved", f"semantic achieved (got {sem.get('verdict')})")
        check(sem.get("model") == "llm", f"verdict records the model ({sem.get('model')})")
        check(t.get("model_calls") == 1, f"one model call (got {t.get('model_calls')})")
        check(all(s["verified"] for s in t.get("steps", [])) and len(t.get("steps", [])) == 3,
              "all three steps are verified")
        verified_events = [e for e in self.events(home, tid) if e["type"] == "task.verified"]
        check(len(verified_events) == 1 and
              verified_events[0]["data"]["verdict"] == "achieved",
              "task.verified event carries the achieved verdict")

    def achieved_with_screenshot(self):
        print("\n== achieved with a screenshot")
        self.reset_site()
        self.set_mode(choice="achieved", confidence=0.9)
        home = os.path.join(self.tmp, "with-shot")
        spec = self.spec("with-shot", {"threshold": 0.6, "include_screenshot": True})
        r = self.task_run(spec, home)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "succeeded", f"state succeeded (got {t.get('state')})")
        check(sem.get("verdict") == "achieved", f"semantic achieved (got {sem.get('verdict')})")
        shot = sem.get("screenshot") or {}
        check(shot.get("width", 0) > 0 and shot.get("height", 0) > 0,
              f"the verdict records the image size ({shot.get('width')}x{shot.get('height')})")
        check(shot.get("bytes", 0) > 2000, f"the image is a real capture ({shot.get('bytes')} B)")
        status, last = request(self.verifier, "GET", "/__last")
        img = (last or {}).get("image") or {}
        check(status == 200 and img.get("sha256") == shot.get("sha256"),
              "the verifier received the image the task recorded")
        check(img.get("media_type") == "image/png" and img.get("bytes") == shot.get("bytes"),
              "it arrived as a PNG of the recorded size")
        check("导出 CSV" in ((last or {}).get("text") or ""),
              "the state text still accompanies the image")
        path = os.path.join(home, "tasks", tid, "artifacts", "verify-final.png")
        check(os.path.isfile(path), "the PNG is kept in the task home")
        if os.path.isfile(path):
            digest = hashlib.sha256(open(path, "rb").read()).hexdigest()
            check(digest == shot.get("sha256") == img.get("sha256"),
                  "the stored PNG is byte-identical to what the model saw")
        ev = self.events(home, tid)
        types = [e["type"] for e in ev]
        check("task.verify.screenshot" in types and "task.verified" in types,
              "the screenshot is logged alongside the verdict")
        if "task.verify.screenshot" in types and "task.verified" in types:
            check(types.index("task.verify.screenshot") < types.index("task.verified"),
                  "the screenshot is recorded before the verdict")
        out = os.path.join(self.tmp, "with-shot-trace")
        r = run([self.args.bhx, "trace", tid, "--out", out, "--home", home])
        check(r.returncode == 0, "trace --out bundled the run")
        copies = [n for n in os.listdir(os.path.join(out, "artifacts"))
                  if n.endswith("verify-final.png")]
        check(len(copies) == 1, "the bundle carries the verification screenshot")
        dom = render_dom(os.path.join(out, "trace.html"))
        check(dom is not None, "the trace player opens from file://")
        check(re.search(r'<img[^>]+src="artifacts/[^"]*verify-final\.png"', dom or "") is not None,
              "the player shows what the verifier actually saw")

    def achieved_without_screenshot(self):
        print("\n== achieved without a screenshot")
        self.reset_site()
        self.set_mode(choice="achieved", confidence=0.9)
        home = os.path.join(self.tmp, "no-shot")
        spec = self.spec("no-shot", {"threshold": 0.6})
        r = self.task_run(spec, home)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        sem = (t.get("result") or {}).get("semantic") or {}
        check("screenshot" not in sem, "the verdict carries no screenshot when not asked")
        status, last = request(self.verifier, "GET", "/__last")
        check(status == 200 and (last or {}).get("image") is None,
              "the verifier request carries no image by default")
        path = os.path.join(home, "tasks", tid, "artifacts", "verify-final.png")
        check(not os.path.exists(path), "no verification PNG is written by default")
        check(not any(e["type"] == "task.verify.screenshot" for e in self.events(home, tid)),
              "no screenshot event by default")

    def not_achieved(self):
        print("\n== not_achieved")
        self.reset_site()
        self.set_mode(choice="not_achieved", confidence=0.9)
        home = os.path.join(self.tmp, "not-achieved")
        spec = self.spec("not-achieved", {"threshold": 0.6})
        r = self.task_run(spec, home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        check(all(s["verified"] for s in t.get("steps", [])),
              "mechanical step checks had all passed")
        check(sem.get("verdict") == "not_achieved",
              f"semantic not_achieved (got {sem.get('verdict')})")
        check("semantic verification did not confirm" in
              ((t.get("error") or {}).get("message") or ""),
              "task error says semantic verification failed")
        check(t.get("model_calls") == 1, f"one model call (got {t.get('model_calls')})")

    def uncertain_wait_human(self):
        print("\n== uncertain -> wait for a human")
        self.reset_site()
        self.set_mode(choice="achieved", confidence=0.4)
        home = os.path.join(self.tmp, "uncertain")
        spec = self.spec("uncertain", {"threshold": 0.8})
        r = self.task_run(spec, home)
        check(r.returncode == 3, f"run stopped for approval (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        approvals = t.get("approvals") or []
        check(t.get("state") == "waiting_approval", f"state waiting_approval (got {t.get('state')})")
        check(len(approvals) == 1, f"one approval requested (got {len(approvals)})")
        if len(approvals) == 1:
            a = approvals[0]
            payload = a.get("payload") or {}
            sem = payload.get("semantic") or {}
            check(a.get("step_id") is None, "approval is task-level, not tied to a step")
            check("semantic verification is uncertain" in (a.get("reason") or ""),
                  "approval reason names semantic uncertainty")
            check(sem.get("verdict") == "uncertain",
                  f"payload records uncertain (got {sem.get('verdict')})")
            check(payload.get("next_index") == 3, "approval resumes after all three steps")

        approve = run([self.args.bhx, "task", "approve", tid, "--home", home])
        check(approve.returncode == 0, f"approval accepted (exit {approve.returncode})")
        resume = run([self.args.bhx, "task", "resume", tid, "--home", home],
                     env=self.env())
        check(resume.returncode == 0, f"resume finished (exit {resume.returncode})")
        t = self.show(home, tid)
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "succeeded", f"state succeeded after approval (got {t.get('state')})")
        check(sem.get("verdict") == "uncertain",
              f"approved uncertain verdict is kept (got {sem.get('verdict')})")
        check(t.get("model_calls") == 1,
              f"resume did not call the model again (got {t.get('model_calls')})")
        ev = self.events(home, tid)
        started = [e for e in ev if e["type"] == "step.started"]
        check(len(started) == 3, f"resume did not redo steps ({len(started)} step.started)")
        check(len([e for e in ev if e["type"] == "task.verified"]) == 1,
              "the semantic check ran once")
        check(len([e for e in ev if e["type"] == "task.finished"]) == 1,
              "task.finished emitted once")

    def uncertain_fail(self):
        print("\n== uncertain -> policy fails")
        self.reset_site()
        self.set_mode(choice="achieved", confidence=0.4)
        home = os.path.join(self.tmp, "uncertain-fail")
        spec = self.spec("uncertain-fail",
                         {"threshold": 0.8, "on_uncertain": "fail"})
        r = self.task_run(spec, home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        check(sem.get("verdict") == "uncertain", f"semantic uncertain (got {sem.get('verdict')})")
        check("policy says fail" in ((t.get("error") or {}).get("message") or ""),
              "error explains the policy decision")

    def no_adapter(self):
        print("\n== no model adapter")
        self.reset_site()
        home = os.path.join(self.tmp, "no-adapter")
        spec = self.spec("no-adapter", {"threshold": 0.6})
        r = self.task_run(spec, home, with_model=False)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        message = (t.get("error") or {}).get("message") or ""
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        check(all(s["verified"] for s in t.get("steps", [])),
              "mechanical step checks had all passed")
        check("semantic verification failed" in message and "no model adapter" in message,
              "error says the configured check could not run")
        check(t.get("model_calls") == 0, f"no calls were charged (got {t.get('model_calls')})")

    def bad_threshold(self):
        print("\n== bad threshold")
        home = os.path.join(self.tmp, "bad-threshold")
        spec = self.spec("bad-threshold", {"threshold": 1.5})
        r = self.task_run(spec, home)
        check(r.returncode == 1, f"run rejected (exit {r.returncode})")
        check("outside 0..1" in r.stderr, "error names the invalid threshold")
        tasks = os.path.join(home, "tasks")
        check(not os.path.isdir(tasks) or not os.listdir(tasks),
              "no task was created for an invalid policy")

    def run_all(self):
        self.achieved()
        self.achieved_with_screenshot()
        self.achieved_without_screenshot()
        self.not_achieved()
        self.uncertain_wait_human()
        self.uncertain_fail()
        self.no_adapter()
        self.bad_threshold()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--verifier", default="http://127.0.0.1:8793")
    ap.add_argument("--spec", default="testkit/sites/admin/task.json")
    ap.add_argument("--tmp", required=True)
    args = ap.parse_args()
    Suite(args).run_all()
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
