#!/usr/bin/env python3
"""End-to-end check for ambiguity arbitration (`policy.arbitrate_ambiguous`).

A locator that matches several elements fails by default, and that is the
behaviour this suite keeps proving. With the policy set, a model may choose —
but only confidently, only among what resolution produced, and only with the
decision recorded on the action.

Two pages, both with buttons a `{role: button, name: 导出}` locator matches:

  /arbiter/        both named exactly 导出 — nothing on the page tells them
                   apart, so a model has no information to use
  /arbiter/export  导出 CSV vs 导出 PDF — the labels answer the question

Server-side truth (`/arbiter/__state`) records which control was pressed, so
"it picked the right one" is a claim about the site's record, not about a
model's intent. testkit/chooser.py plays the model.

  off              no policy -> ambiguous error, the model is never called
  indistinguishable both names equal -> declined, and no call is spent
  confident        picks 导出 CSV, the step verifies, the action event carries
                   model / confidence / candidate count / reason
  shy              below the floor -> the original ambiguity error stands
  invented         an id that was never offered -> error, nothing dispatched
  first            picks 导出 PDF -> the wrong control is still the one acted
                   on, and the event says the choice was the model's
  error            model down -> ambiguity error, never a silent guess
  too_many         max_candidates below the match count -> no model call
  bad_policy       a floor outside 0..1 is refused before any work starts

Exit code 0 only if every check passed.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import shutil
import sys
import time
import urllib.request

PASS = 0
FAIL = 0


def check(cond, msg):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"ok    {msg}")
    else:
        FAIL += 1
        print(f"FAIL  {msg}")


def run(cmd, env=None, timeout=120):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=env)


def http_json(url, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


BASE_SPEC = {
    "protocol_version": "0.1",
    "kind": "workflow",
    "goal": "导出为 CSV",
    "policy": {"max_model_calls": 2, "max_wall_ms": 30000, "allowed_hosts": ["127.0.0.1"]},
    "steps": [
        {
            "id": "s1",
            "goal": "点击导出为 CSV 的那个按钮",
            "actions": [
                {"type": "navigate", "url": "http://127.0.0.1:{PORT}/arbiter/export/"},
                {"type": "click", "target": {"role": "button", "name": "导出"}},
            ],
            # The page title changes only when a button is pressed; which one is
            # proven by the site's record, not by this condition.
            "success": {"type": "js", "script": "document.title.startsWith('clicked')"},
            "timeout_ms": 10000,
        }
    ],
}


class Suite:
    def __init__(self, args):
        self.args = args
        self.site = args.site
        self.chooser = args.chooser
        self.tmp = args.tmp
        os.makedirs(self.tmp, exist_ok=True)

    def spec(self, name, arbiter=None, max_calls=2, page="/arbiter/export/"):
        raw = json.dumps(BASE_SPEC, ensure_ascii=False).replace("{PORT}", str(self.port))
        raw = raw.replace("/arbiter/export/", page)
        spec = json.loads(raw)
        spec["policy"]["max_model_calls"] = max_calls
        if arbiter is not None:
            spec["policy"]["arbitrate_ambiguous"] = arbiter
        path = os.path.join(self.tmp, f"{name}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(spec, f, ensure_ascii=False, indent=2)
        return path

    def env(self):
        env = os.environ.copy()
        env["BH_LLM_URL"] = self.chooser + "/v1/chat/completions"
        env.pop("BH_VLM_URL", None)
        return env

    def mode(self, m):
        return http_json(f"{self.chooser}/__mode", "POST", {"mode": m})

    def last_call(self):
        return http_json(f"{self.chooser}/__last")

    def clicks(self):
        return http_json(f"{self.site}/arbiter/__state")["clicks"]

    def reset_site(self):
        http_json(f"{self.site}/arbiter/__reset", "POST", {})

    def task_run(self, spec_path, home):
        shutil.rmtree(home, ignore_errors=True)
        os.makedirs(home, exist_ok=True)
        return run([self.args.bhx, "task", "run", spec_path, "--home", home], env=self.env())

    def events(self, home):
        """Read events.jsonl from the single task directory in this home."""
        tasks = os.path.join(home, "tasks")
        if not os.path.isdir(tasks):
            return []
        out = []
        for tid in os.listdir(tasks):
            f = os.path.join(tasks, tid, "events.jsonl")
            if os.path.isfile(f):
                for line in open(f, encoding="utf-8"):
                    line = line.strip()
                    if line:
                        out.append(json.loads(line))
        return out

    def action_events(self, home):
        return [e for e in self.events(home) if e.get("type") == "action.executed"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="./target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--chooser", default="http://127.0.0.1:8794")
    ap.add_argument("--tmp", default="/tmp/arbiter-smoke")
    args = ap.parse_args()

    s = Suite(args)
    s.port = int(args.site.rsplit(":", 1)[1])

    # The chooser must be up: the suite starts its own when it is not.
    proc = None
    try:
        http_json(f"{s.chooser}/health")
    except Exception:
        proc = subprocess.Popen(
            [sys.executable, os.path.join(os.path.dirname(__file__), "chooser.py"),
             "--port", s.chooser.rsplit(":", 1)[1]],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        for _ in range(20):
            try:
                http_json(f"{s.chooser}/health")
                break
            except Exception:
                time.sleep(0.3)

    # ---------------------------------------------------------------- off
    s.reset_site()
    s.mode("confident")
    r = s.task_run(s.spec("off"), f"{s.tmp}/home-off")
    check(r.returncode != 0, "without the policy an ambiguous locator still fails")
    check("ambiguous" in (r.stderr + r.stdout), "the error is the ordinary ambiguity error")
    check(s.last_call()["calls"] == 0, "the model was not consulted at all")
    check(s.clicks() == [], "nothing was clicked")
    check(all(e["data"].get("arbitration") is None
              for e in s.action_events(f"{s.tmp}/home-off")),
          "no arbitration on any action")

    # --------------------------------------------------- indistinguishable
    # /arbiter/ has two buttons both named 导出. Nothing on the page separates
    # them, so a confident model answer is a coin flip with good posture:
    # measured, a local choice model gave 0.67 and 0.83 across two orderings
    # and stuck to the same element. Decline, and do not spend the call.
    s.reset_site()
    s.mode("confident")
    r = s.task_run(s.spec("same", {"min_confidence": 0.7, "max_candidates": 8},
                          page="/arbiter/"), f"{s.tmp}/home-same")
    check(r.returncode != 0, "two identically-named controls are not arbitrated")
    out = r.stderr + r.stdout
    check("ambiguous" in out, "the ambiguity error still stands there")
    check("same role and name" in out and "more specific locator" in out,
          "and it says why and what to fix, not just 'ambiguous'")
    check(s.last_call()["calls"] == 0, "and no model call is spent on an unanswerable question")
    check(s.clicks() == [], "nothing was clicked")

    # ----------------------------------------------------------- confident
    # /arbiter/export has 导出 CSV and 导出 PDF: the labels answer the question.
    s.reset_site()
    s.mode("confident")
    home = f"{s.tmp}/home-conf"
    r = s.task_run(s.spec("conf", {"min_confidence": 0.7, "max_candidates": 8}), home)
    check(r.returncode == 0, f"with a confident pick the task succeeds ({r.stderr[-160:]})")
    clicks = s.clicks()
    # "confident" picks the second candidate, which is the CSV button.
    check(clicks == ["csv"], f"the button the model chose is the one pressed (got {clicks})")
    evs = [e for e in s.action_events(home) if e["data"].get("type") == "click"]
    check(len(evs) == 1, "exactly one click was recorded")
    if evs:
        a = evs[0]["data"].get("arbitration")
        check(a is not None, "the click carries the arbitration that made it possible")
        if a:
            check(a.get("model"), f"names the model that chose ({a.get('model')})")
            check(a.get("confidence", 0) >= 0.7, f"reports its confidence ({a.get('confidence')})")
            check(a.get("candidates") == 2, f"reports how many were offered ({a.get('candidates')})")
            check(a.get("reason"), f"keeps the stated reason ({str(a.get('reason'))[:60]})")
            check(a.get("strategy") == "role+name",
                  f"and which locator strategy was ambiguous ({a.get('strategy')})")
    last = s.last_call()
    check(last["calls"] == 1, f"exactly one model call was spent ({last['calls']})")
    text = last["text"] or ""
    check("c1" in text and "c2" in text, "the two matches were offered as c1/c2")
    check('导出 CSV' in text and '导出 PDF' in text,
          "with their real labels, not placeholders")
    check("Controls:" in text, "together with the page state they came from")
    check("导出为 CSV" in text, "and the task goal they are choosing for")

    # ------------------------------------------------------- read-the-labels
    # A stub that only reads the goal and the labels. Choosing right here means
    # the request carried enough information to choose with.
    s.reset_site()
    s.mode("semantic")
    home = f"{s.tmp}/home-sem"
    r = s.task_run(s.spec("sem", {"min_confidence": 0.7, "max_candidates": 8}), home)
    check(r.returncode == 0, "a label-reading model can carry the task")
    clicks = s.clicks()
    check(clicks == ["csv"], f"and picks the CSV button the goal asks for (got {clicks})")

    # ---------------------------------------------------------------- shy
    s.reset_site()
    s.mode("shy")
    r = s.task_run(s.spec("shy", {"min_confidence": 0.7, "max_candidates": 8}),
                  f"{s.tmp}/home-shy")
    check(r.returncode != 0, "below the floor the task fails")
    out = r.stderr + r.stdout
    check("ambiguous" in out, "with the original ambiguity error")
    check("only 0.2 confident" in out, "and it names the confidence that fell short")
    check(s.clicks() == [], "and nothing was clicked")

    # ------------------------------------------------------------- invented
    s.reset_site()
    s.mode("invented")
    r = s.task_run(s.spec("inv", {"min_confidence": 0.7, "max_candidates": 8}),
                  f"{s.tmp}/home-inv")
    check(r.returncode != 0, "an id that was never offered is refused")
    check(s.clicks() == [], "nothing was clicked on an invented id")

    # -------------------------------------------------------------- first
    s.reset_site()
    s.mode("first")
    home = f"{s.tmp}/home-first"
    r = s.task_run(s.spec("first", {"min_confidence": 0.7, "max_candidates": 8}), home)
    check(r.returncode == 0, "a confident pick of the *other* button is still honoured")
    clicks = s.clicks()
    check(clicks == ["pdf"], f"and it is the one pressed (got {clicks})")
    evs = [e for e in s.action_events(home) if e["data"].get("type") == "click"]
    check(evs and evs[0]["data"].get("arbitration"),
          "the trace can tell this click came from a guess")

    # --------------------------------------------------------- budget guard
    # Arbitration is a model call like any other: it must not be able to spend
    # past the task's own budget.
    s.reset_site()
    s.mode("confident")
    r = s.task_run(s.spec("budget", {"min_confidence": 0.7, "max_candidates": 8},
                          max_calls=0), f"{s.tmp}/home-budget")
    check(r.returncode != 0, "with max_model_calls=0 the step is not arbitrated")
    check(s.last_call()["calls"] == 0, "and no call was made")
    check(s.clicks() == [], "nothing was clicked")

    # -------------------------------------------------------------- error
    s.reset_site()
    s.mode("error")
    r = s.task_run(s.spec("err", {"min_confidence": 0.7, "max_candidates": 8}),
                  f"{s.tmp}/home-err")
    check(r.returncode != 0, "a model that fails does not become a licence to guess")
    out = r.stderr + r.stdout
    check("ambiguous" in out, "the ambiguity error still explains the failure")
    check("could not answer" in out, "and says the model was the part that failed")
    check(s.clicks() == [], "nothing was clicked")

    # ------------------------------------------------------------- too_many
    s.reset_site()
    s.mode("confident")
    r = s.task_run(s.spec("many", {"min_confidence": 0.7, "max_candidates": 1}),
                  f"{s.tmp}/home-many")
    check(r.returncode != 0, "more matches than max_candidates is refused")
    check("needs narrowing" in (r.stderr + r.stdout), "and says the locator is the problem")
    check(s.last_call()["calls"] == 0, "without spending a model call")

    # ----------------------------------------------------------- bad policy
    s.mode("confident")
    r = s.task_run(s.spec("bad", {"min_confidence": 1.7, "max_candidates": 8}),
                  f"{s.tmp}/home-bad")
    check(r.returncode != 0, "a confidence floor outside 0..1 is rejected")
    check("min_confidence" in (r.stderr + r.stdout), "and says which field was wrong")
    r = s.task_run(s.spec("bad2", {"min_confidence": 0.7, "max_candidates": 0}),
                  f"{s.tmp}/home-bad2")
    check(r.returncode != 0, "max_candidates 0 is rejected too")

    if proc:
        proc.terminate()
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
