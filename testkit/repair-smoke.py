#!/usr/bin/env python3
"""End-to-end check for auto-repair (`retry.on_exhausted=ask_model`).

testkit/repairer.py plays the model. The suite proves the harness side:

  good             -> the replacement runs, the step verifies, the CSV is right
  fail_action      -> a replacement that still fails is reported, not retried
  reversible       -> only read-only steps are auto-repaired; the model is never asked
  raise_side_effect-> a proposed irreversible step is rejected before dispatch
  change_id        -> the replacement must keep the step id
  change_executor  -> a replacement cannot switch to the vision executor
  change_success   -> a replacement cannot rewrite (only preserve) the success condition
  change_preconditions -> a replacement cannot rewrite the step's gates
  inject_js        -> a replacement cannot add an evaluate action
  inject_extract   -> a replacement cannot add an extract action
  external_nav     -> a replacement cannot escape allowed_hosts
  drop_success     -> a replacement cannot remove the success condition

A rejected proposal is never dispatched: the task fails with the original error
plus the rejection reason, and no later step runs.

Exit code 0 only if every check passed.
"""

import argparse
import hashlib
import http.client
import json
import os
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


def run(cmd, env=None, timeout=180):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=env)


class Suite:
    def __init__(self, args):
        self.args = args
        self.site = args.site.rstrip("/")
        self.repairer = args.repairer.rstrip("/")
        self.base = json.load(open(args.spec, encoding="utf-8"))
        self.tmp = args.tmp
        os.makedirs(self.tmp, exist_ok=True)

    # ------------------------------------------------------------- fixtures
    def reset_site(self):
        request(self.site, "POST", "/admin/__reset")

    def set_mode(self, mode):
        status, value = request(self.repairer, "POST", "/__mode", {"mode": mode})
        if status != 200:
            raise RuntimeError(f"could not set repairer mode: {status} {value}")

    def last_call(self):
        _, value = request(self.repairer, "GET", "/__last")
        return value or {}

    def spec(self, name, side_effect="read_only", max_calls=2):
        raw = json.dumps(self.base, ensure_ascii=False)
        raw = raw.replace("http://127.0.0.1:8766", self.site)
        spec = json.loads(raw)
        spec["policy"]["max_model_calls"] = max_calls
        s2 = next(s for s in spec["steps"] if s["id"] == "s2")
        s2["retry"] = {"max_attempts": 1, "backoff_ms": 10,
                       "on_exhausted": "ask_model"}
        s2["side_effect"] = side_effect
        # A locator that cannot resolve: the step fails without changing state,
        # which is exactly the situation auto-repair is allowed to recover from.
        s2["actions"] = [{"type": "click",
                          "target": {"role": "button", "name": "这个按钮不存在"}}]
        path = os.path.join(self.tmp, f"{name}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(spec, f, ensure_ascii=False, indent=2)
        return path

    def env(self):
        env = os.environ.copy()
        env["BH_LLM_URL"] = self.repairer + "/v1/chat/completions"
        return env

    def task_run(self, spec_path, home):
        return run([self.args.bhx, "task", "run", spec_path, "--home", home],
                   env=self.env())

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

    def step_event(self, home, task_id, step_id):
        found = [e for e in self.events(home, task_id)
                 if e["type"] == "step.finished"
                 and e["data"].get("step_id") == step_id]
        return found[-1]["data"] if found else {}

    def step_row(self, task, step_id):
        return next((s for s in task.get("steps", []) if s["step_id"] == step_id), {})

    @staticmethod
    def clicks(actions, name):
        """A target is normalized to `any_of` on the way in, so look inside it."""
        for a in actions:
            if a.get("type") != "click":
                continue
            target = a.get("target") or {}
            for candidate in [target, *(target.get("any_of") or [])]:
                if candidate.get("name") == name:
                    return True
        return False

    # ------------------------------------------------------------- scenarios
    def good(self):
        print("\n== a broken read-only step is repaired")
        self.reset_site()
        self.set_mode("good")
        home = os.path.join(self.tmp, "good")
        r = self.task_run(self.spec("good"), home)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode})")
        check("[repaired: succeeded]" in r.stderr,
              "the CLI summary marks the model-written step")
        tid = self.task_id(home)
        check(tid is not None, "one task was created")
        if not tid:
            return
        t = self.show(home, tid)
        check(t.get("state") == "succeeded", f"state succeeded (got {t.get('state')})")
        check(t.get("model_calls") == 1, f"one repair call (got {t.get('model_calls')})")
        check(len(t.get("steps", [])) == 3, "the workflow ran all three steps")
        s2 = self.step_row(t, "s2")
        check(s2.get("state") == "succeeded", f"s2 succeeded (got {s2.get('state')})")
        check(s2.get("verified") is True, "the replacement satisfied the success condition")

        reports = (t.get("result") or {}).get("steps") or []
        rep = next((x for x in reports if x.get("step_id") == "s2"), {})
        check(rep.get("attempts") == 2,
              f"one original attempt + one repair (got {rep.get('attempts')})")
        info = rep.get("repair") or {}
        check(info.get("status") == "succeeded", f"report records the repair (got {info.get('status')})")
        check(info.get("model") == "llm", f"report names the model ({info.get('model')})")
        check(abs(float(info.get("confidence", 0)) - 0.82) < 1e-6,
              f"report keeps confidence ({info.get('confidence')})")
        check((info.get("step") or {}).get("id") == "s2", "replacement kept the step id")
        repl = info.get("step") or {}
        check((repl.get("success") or {}).get("type") == "js"
              and "#rows tr" in ((repl.get("success") or {}).get("script") or ""),
              "success condition is preserved verbatim")
        check(len(repl.get("preconditions") or []) == 1,
              "the original preconditions are preserved")
        check(repl.get("executor") == "auto", f"executor is unchanged ({repl.get('executor')})")
        check(repl.get("side_effect") == "read_only",
              f"side effect is unchanged ({repl.get('side_effect')})")
        check(self.clicks((info.get("step") or {}).get("actions") or [], "失败"),
              "replacement clicks the real filter button")

        ev = self.step_event(home, tid, "s2")
        check((ev.get("repair") or {}).get("status") == "succeeded",
              "step.finished carries the repair")
        check(ev.get("attempts") == 2, "step.finished counts both attempts")
        check(len([e for e in self.events(home, tid) if e["type"] == "step.started"]) == 3,
              "a repaired step is not restarted from the beginning")

        asked = self.last_call()
        check(asked.get("calls") == 1, f"model called once (got {asked.get('calls')})")
        text = asked.get("text") or ""
        check("Failed step:" in text and "Failure:" in text and "Current page:" in text,
              "request carries the failed step, the error, and the page")
        check('"id": "s2"' in text, "request names the failed step")

        arts = run([self.args.bhx, "task", "artifacts", tid, "--home", home])
        rows = [line.split("\t") for line in arts.stdout.splitlines() if "\t" in line]
        check(bool(rows), "the repaired workflow still produced an artifact")
        if rows:
            path, size, digest = rows[0][0], int(rows[0][1]), rows[0][2]
            blob = open(path, "rb").read()
            check(len(blob) == size and hashlib.sha256(blob).hexdigest() == digest,
                  "artifact matches its listed size and sha256")
            _, state = request(self.site, "GET", "/admin/__state")
            recorded = state["downloads"][-1]["sha256"]
            check(digest == recorded,
                  "the CSV matches the site's server-side download record")

    def fail_action(self):
        print("\n== a replacement that still fails is reported")
        self.reset_site()
        self.set_mode("fail_action")
        home = os.path.join(self.tmp, "fail-action")
        r = self.task_run(self.spec("fail-action"), home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        s2 = self.step_row(t, "s2")
        check(s2.get("state") == "failed", f"s2 failed (got {s2.get('state')})")
        check("repaired step failed" in (s2.get("error") or ""),
              "step error says the repair also failed")
        info = (self.step_event(home, tid, "s2").get("repair") or {})
        check(info.get("status") == "failed", f"repair status failed (got {info.get('status')})")
        check(bool(info.get("error")), "repair records why it failed")
        check(t.get("model_calls") == 1, f"the repair was tried once (got {t.get('model_calls')})")
        check(self.last_call().get("calls") == 1, "the model was called once")

    def reversible_is_not_repaired(self):
        print("\n== a non-read-only step is never auto-repaired")
        self.reset_site()
        self.set_mode("good")
        home = os.path.join(self.tmp, "reversible")
        r = self.task_run(self.spec("reversible", side_effect="reversible"), home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        s2 = self.step_row(t, "s2")
        check(s2.get("state") == "failed", f"s2 failed (got {s2.get('state')})")
        check("repair" not in (s2.get("error") or ""),
              "the step was not re-diagnosed by a model")
        check(t.get("model_calls") == 0, f"no model call was charged (got {t.get('model_calls')})")
        check(self.last_call().get("calls") == 0, "the model was never asked")
        check(not (self.step_event(home, tid, "s2").get("repair")),
              "no repair is recorded")

    def rejected(self, name, mode, needle):
        print(f"\n== rejected proposal: {mode}")
        self.reset_site()
        self.set_mode(mode)
        home = os.path.join(self.tmp, name)
        r = self.task_run(self.spec(name), home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        if not tid:
            bad("task was created")
            return
        t = self.show(home, tid)
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        s2 = self.step_row(t, "s2")
        err = s2.get("error") or ""
        check("repair failed" in err, "step error mentions the rejected repair")
        check(needle in err, f"error names the violation ({needle!r})")
        check(not (self.step_event(home, tid, "s2").get("repair")),
              "a rejected proposal is not recorded as a repair attempt")
        check(not self.step_row(t, "s3"),
              "the export step never ran after a rejected repair")
        check(t.get("model_calls") == 1, f"one proposal was charged (got {t.get('model_calls')})")
        check(self.last_call().get("calls") == 1, "the model was called once")

    def run_all(self):
        self.good()
        self.fail_action()
        self.reversible_is_not_repaired()
        self.rejected("raise-side-effect", "raise_side_effect", "raised side_effect")
        self.rejected("change-id", "change_id", "changed the step id")
        self.rejected("change-executor", "change_executor", "changed the executor")
        self.rejected("change-success", "change_success", "changed the success condition")
        self.rejected("change-preconditions", "change_preconditions",
                      "changed the step's preconditions")
        self.rejected("inject-js", "inject_js", "introduced a script or file-upload action")
        self.rejected("inject-extract", "inject_extract",
                      "introduced a script or file-upload action")
        self.rejected("external-nav", "external_nav", "host not in allowed_hosts")
        self.rejected("drop-success", "drop_success", "dropped the success condition")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--repairer", default="http://127.0.0.1:8792")
    ap.add_argument("--spec", default="testkit/sites/admin/task.json")
    ap.add_argument("--tmp", required=True)
    args = ap.parse_args()
    Suite(args).run_all()
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
