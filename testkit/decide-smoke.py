#!/usr/bin/env python3
"""End-to-end check for model-driven strategy inside a controller (`controller.decide`).

The claim being tested is narrow and specific: a model may pick **which of the
author's strategies to follow**, at a low rate, while the 30 Hz rules keep doing
the acting. The model is never in the tick.

The page is testkit/sites/plane/: two squadrons descend two lanes at different
speeds, so "cover which lane first" has a measurable payoff — covering the lane
that arrives later wins both squads, covering the fast one lets the other escape.
The score comes from the server's telemetry (`/plane/__state`), not from the
harness's own report.

testkit/chooser.py plays the model, so the choice is forced rather than sampled.

  first-listed  -> the left lane (slow) -> 3 hits
  last-listed   -> the right lane (fast) -> 0 hits, two squadrons escape
  ...which is the whole point: the *same* rules produced opposite outcomes, and
  the only thing that changed is what the model answered.

  off          no `decide` block -> no model call, the loop is pure rules
  no-adapter   `decide` with no model configured -> the loop runs, no strategy
               is applied, and the stop event says so (decisions: 0)
  shy          min_confidence above the answer -> the strategy is not changed
  invented     an id outside the option list -> ignored
  bad-spec     one option / empty question / an id that cannot appear in a
               predicate -> refused before the job starts
  budget       accepted decisions never exceed max_model_calls

Exit code 0 only if every check passed.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
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


def http_json(url, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.load(r)


HERE = os.path.dirname(os.path.abspath(__file__))


class Suite:
    def __init__(self, args):
        self.a = args
        self.site = args.site
        self.chooser = args.chooser
        self.tmp = args.tmp
        os.makedirs(self.tmp, exist_ok=True)
        base = json.load(open(os.path.join(HERE, "sites", "plane", "decide-left.json"),
                              encoding="utf-8"))
        self.base = base

    def spec(self, name, *, options=None, min_confidence=None, decide=True,
             max_calls=None, every_ms=None, switch=None, goal=None):
        spec = json.loads(json.dumps(self.base))
        c = spec["controller"]
        if not decide:
            c.pop("decide", None)
        else:
            if options is not None:
                c["decide"]["options"] = options
            if min_confidence is not None:
                c["decide"]["min_confidence"] = min_confidence
            if every_ms is not None:
                c["decide"]["every_ms"] = every_ms
            if switch is None:
                c["decide"].pop("switch_confidence", None)
            else:
                c["decide"]["switch_confidence"] = switch
        if goal is not None:
            # Deliberately disabling the goal is how the terminal-rule case
            # proves anything: with a working goal, the goal always wins the
            # race and the rule never gets to be the reason the loop ends.
            spec["controller"]["goal"]["script"] = goal
        if max_calls is not None:
            spec["policy"]["max_model_calls"] = max_calls
        port = self.site.rsplit(":", 1)[1]
        raw = json.dumps(spec, ensure_ascii=False).replace("{PORT}", port)
        path = os.path.join(self.tmp, f"{name}.json")
        open(path, "w", encoding="utf-8").write(raw)
        return path

    def env(self, with_model=True):
        env = os.environ.copy()
        if with_model:
            env["BH_LLM_URL"] = self.chooser + "/v1/chat/completions"
        else:
            env.pop("BH_LLM_URL", None)
            env.pop("BH_VLM_URL", None)
            env.pop("BH_LAYA_DECIDE", None)
        env["RUST_LOG"] = "warn"
        return env

    def calls(self):
        return http_json(f"{self.chooser}/__last")["calls"]

    def mode(self, m):
        return http_json(f"{self.chooser}/__mode", "POST", {"mode": m})

    def reset_game(self):
        http_json(f"{self.site}/plane/__reset", "POST", {})

    def telemetry(self):
        return http_json(f"{self.site}/plane/__state")["reports_log"]

    def run(self, spec_path, home, with_model=True):
        subprocess.run(["rm", "-rf", home])
        os.makedirs(home, exist_ok=True)
        r = subprocess.run([self.a.bhx, "task", "run", spec_path, "--home", home],
                           capture_output=True, text=True, env=self.env(with_model),
                           timeout=180)
        stop = None
        tasks = os.path.join(home, "tasks")
        if os.path.isdir(tasks):
            for tid in os.listdir(tasks):
                f = os.path.join(tasks, tid, "events.jsonl")
                if not os.path.isfile(f):
                    continue
                for line in open(f, encoding="utf-8"):
                    try:
                        e = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if e.get("type") == "interactive.stopped":
                        stop = e["data"]
        return r, stop


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="./target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--chooser", default="http://127.0.0.1:8794")
    ap.add_argument("--tmp", default="/tmp/decide-smoke")
    args = ap.parse_args()

    s = Suite(args)
    proc = None
    try:
        http_json(f"{s.chooser}/health")
    except Exception:
        proc = subprocess.Popen(
            [sys.executable, "-I", os.path.join(HERE, "chooser.py"),
             "--port", s.chooser.rsplit(":", 1)[1]],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        for _ in range(40):
            try:
                http_json(f"{s.chooser}/health")
                break
            except Exception:
                time.sleep(0.25)

    LEFT = {"id": "left", "description": "cover the LEFT lane, at x=120"}
    RIGHT = {"id": "right", "description": "cover the RIGHT lane, at x=360"}

    def play(name, **kw):
        s.mode(kw.pop("mode", "first"))
        s.reset_game()
        path = s.spec(name, **kw)
        home = f"{s.a.tmp}/home-{name}"
        r, stop = s.run(path, home)
        log = s.telemetry()
        final = log[-1] if log else {}
        return r, stop, final, log

    # ------------------------------------------------ the two strategies differ
    r, stop, final, log = play("left-first", options=[LEFT, RIGHT], mode="first")
    check(r.returncode == 0, f"model-driven strategy run completed ({r.stderr[-160:]})")
    check(stop is not None, "the controller reported a stop event")
    if stop:
        check(stop.get("decisions", 0) >= 1,
              f"the model's answer reached the loop (decisions={stop.get('decisions')})")
        fired = {x["id"] for x in stop.get("rules", [])}
        check({"go-left", "hold-left"} <= fired and not ({"go-right"} & fired),
              f"only the chosen strategy's rules ran ({sorted(fired)})")
    xs = [p["x"] for p in log]
    check(min(xs) <= 150, f"the plane actually steered to the left lane (min x={min(xs)})")
    left_score = final.get("score", -1)
    check(left_score >= 2, f"covering the slow lane first scored {left_score}")

    r, stop, final, log = play("right-first", options=[RIGHT, LEFT], mode="first")
    check(r.returncode == 0, "the same rules with the options re-ordered also ran")
    if stop:
        fired = {x["id"] for x in stop.get("rules", [])}
        check({"go-right", "hold-right"} <= fired and not ({"go-left"} & fired),
              f"the other option selects the other rules ({sorted(fired)})")
    xs = [p["x"] for p in log]
    check(max(xs) >= 330, f"and the plane steered to the right lane (max x={max(xs)})")
    right_score = final.get("score", -1)
    check(right_score < left_score,
          f"the decision has a measurable payoff: {left_score} vs {right_score} "
          f"(two squadrons escaped the un-covered lane: "
          f"alive L/R={final.get('leftAlive')}/{final.get('rightAlive')})")

    # ---------------------------------------------------------------- off
    # ---------------------------------------------------------------- off
    s.mode("first")
    before = s.calls()
    r, stop, final, log = play("no-decide", decide=False)
    check(r.returncode == 0, "without `decide` the interactive job runs unchanged")
    if stop:
        check(stop.get("decisions", 0) == 0, "and no decision is recorded")
        check(stop.get("rules", []) == [], "no strategy rule fires")
    check(s.calls() == before, "and not one model call is made")

    # ------------------------------------------------------------ no adapter
    s.mode("first")
    s.reset_game()
    path = s.spec("no-model", options=[LEFT, RIGHT])
    home = f"{s.a.tmp}/home-nomodel"
    subprocess.run(["rm", "-rf", home]); os.makedirs(home, exist_ok=True)
    r = subprocess.run([args.bhx, "task", "run", path, "--home", home],
                       capture_output=True, text=True, env=s.env(with_model=False),
                       timeout=180)
    check(r.returncode == 0,
          f"a `decide` block with no model configured does not stop the loop ({r.stderr[-140:]})")
    log = s.telemetry()
    check(len(log) > 0, "the game still ran and still reported telemetry")
    stop = None
    tasks = os.path.join(home, "tasks")
    if os.path.isdir(tasks):
        for tid in os.listdir(tasks):
            f = os.path.join(tasks, tid, "events.jsonl")
            if not os.path.isfile(f):
                continue
            for line in open(f, encoding="utf-8"):
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if e.get("type") == "interactive.stopped":
                    stop = e["data"]
    if stop:
        check(stop.get("decisions", 0) == 0, "the stop event says no decision was applied")
        check(stop.get("rules", []) == [], "and no strategy rule fired")

    # --------------------------------------------------------- shy / invented
    r, stop, final, log = play("shy", options=[LEFT, RIGHT], min_confidence=0.99)
    check(r.returncode == 0, "a decision below the floor still lets the job run")
    if stop:
        check(stop.get("decisions", 0) == 0,
              f"the under-confidence answer was not applied (decisions={stop.get('decisions')})")
        check(stop.get("rules", []) == [], "and no strategy rule fired")

    r, stop, final, log = play("invented", mode="invented")
    if stop:
        check(stop.get("decisions", 0) == 0, "an id outside the option list is ignored")
        check(stop.get("rules", []) == [], "nothing acted on an invented strategy")

    # ------------------------------------------------- hysteresis: no flip-flop
    # A stub that answers correctly first (left, 0.9) then changes its mind
    # after 3s to the wrong option with only 0.6. Without hysteresis that 0.6
    # clears a 0.5 floor and the ship abandons a lane it was covering — the
    # exact shape real Laya produced. With switch_confidence it must hold.
    def play_held(switch):
        kw = dict(options=[LEFT, RIGHT], min_confidence=0.5)
        if switch is not None:
            kw["switch"] = switch
        s.mode("flip")
        s.reset_game()
        path = s.spec(f"held-{switch}", **kw)
        home = f"{s.a.tmp}/home-held-{switch}"
        r, stop = s.run(path, home)
        log = s.telemetry()
        return stop, (log[-1] if log else {})

    stop_no, fin_no = play_held(None)
    stop_yes, fin_yes = play_held(0.85)
    if stop_no and stop_yes:
        refused = [d for d in stop_yes.get("decision_log", []) if not d.get("applied")]
        held = [d for d in refused if "switching away" in (d.get("reason") or "")]
        check(len(held) >= 1,
              f"hysteresis refused a mid-game change of mind ({len(held)} refusals)")
        check(all(d.get("previous") == "left" for d in held),
              "and says what it was holding onto")
        # Confirming the strategy already in force is not a switch, so counting
        # applied decisions measures nothing: a held strategy is applied once per
        # round. The claim is about *changes* — answers that differ from what is
        # being held.
        def changes(stop):
            return [d for d in stop.get("decision_log", [])
                    if d.get("applied") and d.get("previous") and d["previous"] != d["id"]]
        check(not changes(stop_yes) and changes(stop_no),
              f"without hysteresis {len(changes(stop_no))} changes of mind took effect; "
              f"with it, {len(changes(stop_yes))}")
        fired_no = {x["id"] for x in stop_no.get("rules", [])}
        fired_yes = {x["id"] for x in stop_yes.get("rules", [])}
        check("go-right" in fired_no and "go-right" not in fired_yes,
              f"the held run never steers away ({sorted(fired_yes)} vs {sorted(fired_no)})")
    check(fin_yes.get("score", -1) >= fin_no.get("score", -1),
          f"holding the strategy is not worse: {fin_yes.get('score')} vs {fin_no.get('score')}")

    # --------------------------------------------------- the game ends: stop
    # A terminal rule on s.over. The goal stays deliberately false here, so if
    # anything stops the loop it is that rule — not the ceiling, not luck.
    s.mode("first")
    s.reset_game()
    path = s.spec("terminal", options=[LEFT, RIGHT], goal="() => false")
    home = f"{s.a.tmp}/home-terminal"
    r, stop = s.run(path, home)
    if stop:
        check(stop.get("why") == "terminal_rule",
              f"a terminal rule ends the loop when the page says it is over ({stop.get('why')})")
        fired = {x["id"] for x in stop.get("rules", [])}
        check("game-over" in fired, "the terminal rule is the one that fired")
        log = stop.get("decision_log", [])
        late = [d for d in log if d.get("asked_at_ms", 0) >= 9000]
        check(not late,
              f"no model call after the game was over ({len(late)} of {len(log)} rounds were)")
        ticks = stop.get("ticks", 0)
        check(ticks > 0 and stop.get("elapsed_ms", 0) < 20000,
              f"the loop stopped with room left on the ceiling ({stop.get('elapsed_ms')} ms of 20000)")
    # Documented contract: terminal_rule with no goal script → succeeded; with a
    # goal present the goal still has to hold, so an unreachable goal → failed.
    check(r.returncode != 0,
          "a terminal stop does not count as success while an unreachable goal is declared")

    # ---------------------------------------------------------------- budget
    r, stop, final, log = play("budget", options=[LEFT, RIGHT], max_calls=2)
    if stop:
        check(stop.get("decisions", 0) <= 2,
              f"accepted decisions stay inside max_model_calls ({stop.get('decisions')})")

    # ------------------------------------------------------------- bad specs
    for label, kw in [
        ("one option", dict(options=[LEFT])),
        ("no question", dict()),
    ]:
        if label == "no question":
            p = s.spec("noq")
            raw = json.load(open(p, encoding="utf-8"))
            raw["controller"]["decide"]["question"] = ""
            json.dump(raw, open(p, "w", encoding="utf-8"), ensure_ascii=False)
        else:
            p = s.spec("oneopt", options=[LEFT])
        r = subprocess.run([args.bhx, "task", "run", p, "--home", f"{s.a.tmp}/home-bad"],
                           capture_output=True, text=True, env=s.env(), timeout=120)
        out = r.stderr + r.stdout
        check(r.returncode != 0, f"{label}: the job is refused")
        check("controller.decide" in out, f"and names the field ({out.strip()[:100]})")

    p = s.spec("badid", options=[{"id": "左 lane", "description": "x"}, RIGHT])
    r = subprocess.run([args.bhx, "task", "run", p, "--home", f"{s.a.tmp}/home-badid"],
                       capture_output=True, text=True, env=s.env(), timeout=120)
    check(r.returncode != 0 and "[A-Za-z0-9_-]" in (r.stderr + r.stdout),
          "an option id that cannot appear in a rule predicate is refused")

    if proc:
        proc.terminate()
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
