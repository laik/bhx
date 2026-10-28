#!/usr/bin/env python3
"""End-to-end check for the Laya sidecar adapter (BH_LAYA_DECIDE).

testkit/laya-stub.py speaks the same protocol as sidecars/laya/decide.py but
has no model: it answers from markers in the state text, and logs every spawn,
request and decision to LAYA_STUB_LOG. That lets this suite prove the parts
that the real model cannot prove deterministically:

  local only    -> verify answered by Laya, one call, task succeeds
  with a picture-> Laya refuses the image instead of judging blind
  not_achieved  -> all steps verified, task fails on the verdict
  uncertain     -> waiting_approval; approving finishes without a second call
  remote first  -> with BH_LLM_URL set, the sidecar is not even started
  remote down   -> the sidecar answers as the fallback, still one call
  local first   -> BH_LAYA_PRIORITY=first lets the local verdict win
  no fallback   -> the task fails honestly instead of skipping the check
  invalid choice-> rejected by the router, remote adapter answers instead
  missing script-> Laya is dropped; the failure says no adapter, 0 calls

Checks, in order, also assert what crossed the sidecar boundary: the task's
goal and page state, exactly the two fixed verdict ids, no credential value,
one process per task run.

Exit code 0 only if every check passed.
"""

import argparse
import http.client
import json
import os
import subprocess
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
        self.verifier = args.verifier.rstrip("/") if args.verifier else None
        self.stub = os.path.abspath(args.stub)
        self.base = json.load(open(args.spec, encoding="utf-8"))
        self.tmp = args.tmp
        os.makedirs(self.tmp, exist_ok=True)

    # ------------------------------------------------------------- fixtures
    def reset_site(self):
        request(self.site, "POST", "/admin/__reset")

    def verifier_mode(self, choice="achieved", confidence=0.9):
        if not self.verifier:
            return
        status, value = request(
            self.verifier, "POST", "/__mode", {"choice": choice, "confidence": confidence}
        )
        if status != 200:
            raise RuntimeError(f"could not set verifier mode: {status} {value}")

    def spec(self, name, semantic, max_calls=2):
        raw = json.dumps(self.base, ensure_ascii=False)
        raw = raw.replace("http://127.0.0.1:8766", self.site)
        spec = json.loads(raw)
        spec["policy"]["max_model_calls"] = max_calls
        spec["policy"]["semantic_verify"] = semantic
        path = os.path.join(self.tmp, f"{name}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(spec, f, ensure_ascii=False, indent=2)
        return path

    def env(self, decide=None, stub_mode=None, log=None, remote_url=None, priority=None):
        env = os.environ.copy()
        for k in (
            "BH_LLM_URL", "BH_VLM_URL", "BH_LAYA_DECIDE", "BH_LAYA_PYTHON",
            "BH_LAYA_PRIORITY", "LAYA_STUB_MODE", "LAYA_STUB_LOG",
            "LAYA_BACKEND", "LAYA_DEVICE",
        ):
            env.pop(k, None)
        if remote_url:
            env["BH_LLM_URL"] = remote_url
        if decide:
            env["BH_LAYA_DECIDE"] = decide
        if stub_mode:
            env["LAYA_STUB_MODE"] = stub_mode
        if log:
            env["LAYA_STUB_LOG"] = log
        if priority:
            env["BH_LAYA_PRIORITY"] = priority
        return env

    @property
    def remote_url(self):
        return (self.verifier + "/v1/chat/completions") if self.verifier else None

    def task_run(self, spec_path, home, **env):
        return run(
            [self.args.bhx, "task", "run", spec_path, "--home", home],
            env=self.env(**env),
        )

    def task_id(self, home):
        tasks = os.path.join(home, "tasks")
        ids = ([n for n in os.listdir(tasks) if os.path.isdir(os.path.join(tasks, n))]
               if os.path.isdir(tasks) else [])
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

    def log_lines(self, path):
        if not os.path.exists(path):
            return []
        return [line.rstrip("\n") for line in open(path, encoding="utf-8")]

    @staticmethod
    def count(lines, prefix):
        return len([line for line in lines if line.startswith(prefix)])

    def started(self, spec, home, **env):
        """Run one task and return (proc, task_id, task, events, sidecar log)."""
        log = os.path.join(home + ".sidecar.log")
        r = self.task_run(spec, home, decide=self.stub, log=log, **env)
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        ev = self.events(home, tid) if tid else []
        return r, tid, t, ev, self.log_lines(log)

    # ------------------------------------------------------------- scenarios
    def achieved(self):
        print("\n== local sidecar answers achieved")
        self.reset_site()
        home = os.path.join(self.tmp, "achieved")
        r, tid, t, ev, log = self.started(self.spec("achieved", {"threshold": 0.6}), home)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode}: {r.stderr.strip()})")
        if not tid:
            bad("one task was created")
            return
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "succeeded", f"state succeeded (got {t.get('state')})")
        check(sem.get("verdict") == "achieved", f"verdict achieved (got {sem.get('verdict')})")
        check(sem.get("model") == "laya", f"verdict names the local model (got {sem.get('model')})")
        check(abs((sem.get("confidence") or 0) - 0.9) < 1e-6,
              f"confidence comes from the sidecar (got {sem.get('confidence')})")
        check(sem.get("threshold") == 0.6, "threshold is reported back")
        check("stub chose achieved" in (sem.get("reason") or ""),
              f"reason explains the choice ({sem.get('reason')!r})")
        check(t.get("model_calls") == 1, f"one model call (got {t.get('model_calls')})")
        check(all(s["verified"] for s in t.get("steps", [])) and len(t.get("steps", [])) == 3,
              "all three mechanical steps are verified")
        verified = [e for e in ev if e["type"] == "task.verified"]
        check(len(verified) == 1 and verified[0]["data"].get("model") == "laya",
              "task.verified event names laya")

        check(self.count(log, "spawn") == 1, f"one sidecar process ({self.count(log, 'spawn')})")
        check(self.count(log, "decide") == 1, f"one decision ({self.count(log, 'decide')})")
        states = [line[len("state "):] for line in log if line.startswith("state ")]
        check(len(states) == 1 and "Task: 登录后台" in states[0],
              "the sidecar saw the task goal")
        check(len(states) == 1 and "导出 CSV" in states[0],
              "the sidecar saw the page controls")
        criteria = [line[len("criteria "):] for line in log if line.startswith("criteria ")]
        check(len(criteria) == 1 and
              json.loads(criteria[0]).keys() == {"achieved", "not_achieved"},
              "the sidecar was offered exactly the two verdict ids")
        check("admin123" not in "\n".join(log),
              "no credential value crossed into the sidecar")

    def screenshot_is_refused(self):
        print("\n== a screenshot with a text-only sidecar")
        self.reset_site()
        home = os.path.join(self.tmp, "screenshot")
        sem = {"threshold": 0.6, "include_screenshot": True}
        r, tid, t, ev, log = self.started(self.spec("screenshot", sem), home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        if not tid:
            bad("task was created")
            return
        message = (t.get("error") or {}).get("message") or ""
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        check("laya has no vision" in message,
              f"the error names the missing capability ({message[:120]!r})")
        check(all(s["verified"] for s in t.get("steps", [])),
              "every mechanical step had passed")
        check(not any(e["type"] == "task.verified" for e in ev),
              "no verdict is invented")
        check(self.count(log, "spawn") == 0,
              "the sidecar is not started for a request it cannot answer")
        check(self.count(log, "decide") == 0,
              "nothing was asked of a model that cannot see")
        check(t.get("model_calls") == 1,
              f"the refused attempt is still charged, like any attempt (got {t.get('model_calls')})")
        shot = os.path.join(home, "tasks", tid, "artifacts", "verify-final.png")
        check(not os.path.exists(shot),
              "no verdict screenshot is kept without a verdict")

    def not_achieved(self):
        print("\n== local sidecar answers not_achieved")
        self.reset_site()
        home = os.path.join(self.tmp, "not-achieved")
        sem = {"threshold": 0.6, "goal": "EXPECT_FAIL 导出结果应包含失败订单"}
        r, tid, t, _ev, log = self.started(self.spec("not-achieved", sem), home)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        if not tid:
            bad("task was created")
            return
        sem = (t.get("result") or {}).get("semantic") or {}
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        check(all(s["verified"] for s in t.get("steps", [])),
              "every mechanical step had passed")
        check(sem.get("verdict") == "not_achieved",
              f"verdict not_achieved (got {sem.get('verdict')})")
        check("semantic verification did not confirm" in
              ((t.get("error") or {}).get("message") or ""),
              "task error says semantic verification failed")
        check(t.get("model_calls") == 1, f"one model call (got {t.get('model_calls')})")
        check(self.count(log, "decide") == 1, "the sidecar answered once")

    def uncertain_then_approve(self):
        print("\n== uncertain -> human approval")
        self.reset_site()
        home = os.path.join(self.tmp, "uncertain")
        sem = {"threshold": 0.6, "goal": "UNSURE 导出结果应包含失败订单"}
        r, tid, t, _ev, log = self.started(self.spec("uncertain", sem), home)
        check(r.returncode == 3, f"run stopped for approval (exit {r.returncode})")
        if not tid:
            bad("task was created")
            return
        check(t.get("state") == "waiting_approval", f"state waiting_approval (got {t.get('state')})")
        approvals = t.get("approvals") or []
        check(len(approvals) == 1, f"one approval requested (got {len(approvals)})")
        if approvals:
            payload = approvals[0].get("payload") or {}
            check((payload.get("semantic") or {}).get("model") == "laya",
                  "pending approval records the local verdict")
            check((payload.get("semantic") or {}).get("verdict") == "uncertain",
                  "pending approval records uncertainty")

        approve = run([self.args.bhx, "task", "approve", tid, "--home", home])
        check(approve.returncode == 0, f"approval accepted (exit {approve.returncode})")
        resume = run(
            [self.args.bhx, "task", "resume", tid, "--home", home],
            env=self.env(decide=self.stub, log=os.path.join(home + ".sidecar.log")),
        )
        check(resume.returncode == 0, f"resume finished (exit {resume.returncode})")
        t = self.show(home, tid)
        check(t.get("state") == "succeeded", f"state succeeded (got {t.get('state')})")
        check(t.get("model_calls") == 1,
              f"resume did not call the model again (got {t.get('model_calls')})")
        ev = self.events(home, tid)
        check(len([e for e in ev if e["type"] == "step.started"]) == 3,
              "resume did not redo steps")
        log = self.log_lines(os.path.join(home + ".sidecar.log"))
        check(self.count(log, "decide") == 1,
              f"approving uncertain makes no second decision ({self.count(log, 'decide')})")

    def remote_goes_first_by_default(self):
        print("\n== with a remote adapter configured, it answers first")
        self.reset_site()
        self.verifier_mode(choice="achieved", confidence=0.9)
        home = os.path.join(self.tmp, "precedence")
        log = os.path.join(home + ".sidecar.log")
        # EXPECT_FAIL would make the stub say not_achieved. The remote adapter
        # says achieved; the default order must let the remote answer.
        sem = {"threshold": 0.6, "goal": "EXPECT_FAIL 导出结果应包含失败订单"}
        r = self.task_run(self.spec("precedence", sem), home,
                          decide=self.stub, log=log, remote_url=self.remote_url)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode}: {r.stderr.strip()})")
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        sem = (t.get("result") or {}).get("semantic") or {}
        check(sem.get("model") == "llm",
              f"the remote verdict won (got {sem.get('model')}, {sem.get('verdict')})")
        check(sem.get("verdict") == "achieved", f"verdict achieved (got {sem.get('verdict')})")
        check(self.count(self.log_lines(log), "spawn") == 0,
              "the sidecar was not even started")
        check(t.get("model_calls") == 1, f"one model call (got {t.get('model_calls')})")

    def local_answers_when_the_remote_fails(self):
        print("\n== remote unreachable -> local sidecar answers")
        self.reset_site()
        home = os.path.join(self.tmp, "fallback")
        log = os.path.join(home + ".sidecar.log")
        dead = "http://127.0.0.1:9/v1/chat/completions"
        r = self.task_run(self.spec("fallback", {"threshold": 0.6}), home,
                          decide=self.stub, log=log, remote_url=dead)
        check(r.returncode == 0, f"task succeeded (exit {r.returncode}: {r.stderr.strip()})")
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        sem = (t.get("result") or {}).get("semantic") or {}
        check(sem.get("model") == "laya",
              f"the local adapter answered the fallback (got {sem.get('model')})")
        check(sem.get("verdict") == "achieved", f"verdict achieved (got {sem.get('verdict')})")
        check(t.get("model_calls") == 1,
              f"a failed remote and a fallback cost one call (got {t.get('model_calls')})")
        lines = self.log_lines(log)
        check(self.count(lines, "spawn") == 1, "one sidecar process")
        check(self.count(lines, "decide") == 1, "it answered")

    def local_first_when_asked(self):
        print("\n== BH_LAYA_PRIORITY=first puts the sidecar in front")
        self.reset_site()
        self.verifier_mode(choice="achieved", confidence=0.9)
        home = os.path.join(self.tmp, "local-first")
        log = os.path.join(home + ".sidecar.log")
        sem = {"threshold": 0.6, "goal": "EXPECT_FAIL 导出结果应包含失败订单"}
        r = self.task_run(self.spec("local-first", sem), home,
                          decide=self.stub, log=log, remote_url=self.remote_url,
                          priority="first")
        check(r.returncode == 1, f"the local not_achieved failed the task (exit {r.returncode})")
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        sem = (t.get("result") or {}).get("semantic") or {}
        check(sem.get("model") == "laya",
              f"the local verdict was used (got {sem.get('model')})")
        check(sem.get("verdict") == "not_achieved",
              f"verdict not_achieved (got {sem.get('verdict')})")
        check(self.count(self.log_lines(log), "decide") == 1, "the sidecar answered once")

    def no_fallback_fails_honestly(self):
        print("\n== sidecar dies and nothing else is configured")
        self.reset_site()
        home = os.path.join(self.tmp, "no-fallback")
        r = self.task_run(self.spec("no-fallback", {"threshold": 0.6}), home,
                          decide=self.stub, stub_mode="die")
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        message = (t.get("error") or {}).get("message") or ""
        check(t.get("state") == "failed", f"state failed (got {t.get('state')})")
        check(all(s["verified"] for s in t.get("steps", [])),
              "mechanical steps had passed, so only the check failed")
        check("semantic verification failed" in message,
              f"error says the check could not run ({message!r})")
        check("laya sidecar exited before the ready handshake" in message,
              "error names the sidecar failure")
        check(t.get("model_calls") == 1,
              f"the attempt was charged (got {t.get('model_calls')})")

    def invalid_choice_falls_back(self):
        print("\n== invalid choice from the sidecar -> remote adapter")
        self.reset_site()
        self.verifier_mode(choice="achieved", confidence=0.9)
        home = os.path.join(self.tmp, "bad-choice")
        log = os.path.join(home + ".sidecar.log")
        sem = {"threshold": 0.6, "goal": "BADCHOICE 导出结果应包含失败订单"}
        r = self.task_run(self.spec("bad-choice", sem), home,
                          decide=self.stub, log=log, remote_url=self.remote_url,
                          priority="first")
        check(r.returncode == 0, f"task succeeded (exit {r.returncode}: {r.stderr.strip()})")
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        sem = (t.get("result") or {}).get("semantic") or {}
        check(sem.get("model") == "llm",
              f"the rejected proposal did not win (got {sem.get('model')})")
        lines = self.log_lines(log)
        check(self.count(lines, "decide e99") == 1,
              "the sidecar did propose an id outside the candidates")
        check(self.count(lines, "spawn") == 1,
              "a rejected proposal does not restart the process")

    def missing_script_is_dropped(self):
        print("\n== BH_LAYA_DECIDE points at nothing")
        self.reset_site()
        home = os.path.join(self.tmp, "missing")
        missing = os.path.join(self.tmp, "does-not-exist.py")
        r = self.task_run(self.spec("missing", {"threshold": 0.6}), home, decide=missing)
        check(r.returncode == 1, f"task failed (exit {r.returncode})")
        tid = self.task_id(home)
        t = self.show(home, tid) if tid else {}
        message = (t.get("error") or {}).get("message") or ""
        check("no model adapter" in message,
              f"a misconfigured Laya is not silently used ({message!r})")
        check(t.get("model_calls") == 0,
              f"no call was charged (got {t.get('model_calls')})")

    def mcp_verify_reuses_one_sidecar(self):
        print("\n== MCP browser_verify reuses one warm sidecar")
        self.reset_site()
        home = os.path.join(self.tmp, "mcp-home")
        os.makedirs(home, exist_ok=True)
        log = os.path.join(self.tmp, "mcp.sidecar.log")
        proc = subprocess.Popen(
            [self.args.bhx, "mcp", "--home", home],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            env=self.env(decide=self.stub, log=log),
        )
        next_id = [0]

        def send(method, params=None, notify=False):
            msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
            if not notify:
                next_id[0] += 1
                msg["id"] = next_id[0]
            proc.stdin.write(json.dumps(msg) + "\n")
            proc.stdin.flush()
            return None if notify else next_id[0]

        def wait_for(want, timeout=90.0):
            deadline = time.time() + timeout
            while time.time() < deadline:
                line = proc.stdout.readline()
                if not line:
                    raise RuntimeError("bhx mcp closed stdout")
                msg = json.loads(line)
                if msg.get("id") == want:
                    return msg
            raise TimeoutError(f"no MCP response for id {want}")

        def call(method, params=None):
            return wait_for(send(method, params))

        def tool(name, arguments=None):
            r = call("tools/call", {"name": name, "arguments": arguments or {}})
            if "error" in r:
                raise AssertionError(f"{name}: rpc error {r['error']}")
            res = r["result"]
            text = "".join(c.get("text", "") for c in res["content"] if c["type"] == "text")
            return res.get("isError", False), text

        try:
            init = call("initialize", {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "laya-smoke", "version": "0"},
            })
            check(init["result"]["serverInfo"]["name"] == "bhx", "MCP server initialized")
            send("notifications/initialized", notify=True)

            is_err, text = tool("browser_navigate", {"url": self.site + "/"})
            check(not is_err, f"navigate ({text.strip()[:80]})")

            # Each browser_verify builds its own router; the sidecar must
            # survive that, or every call pays the ~5 s model load.
            is_err, text = tool("browser_verify", {"goal": "EXPECT_FAIL 已登录后台"})
            lines = text.strip().splitlines()
            check(not is_err and lines and lines[0] == "not_achieved",
                  f"first verify is not_achieved ({text.strip()[:80]})")
            check(any(line.startswith("model: laya") for line in lines),
                  f"first verdict names laya ({lines})")

            is_err, text = tool("browser_verify", {"goal": "已登录后台并看到订单列表"})
            lines = text.strip().splitlines()
            check(not is_err and lines and lines[0] == "achieved",
                  f"second verify is achieved ({text.strip()[:80]})")

            is_err, _text = tool("browser_close")
            check(not is_err, "browser closed")
        finally:
            try:
                proc.stdin.close()
            except OSError:
                pass
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

        deadline = time.time() + 5
        while time.time() < deadline:
            lines = self.log_lines(log)
            if self.count(lines, "exit"):
                break
            time.sleep(0.05)
        lines = self.log_lines(log)
        check(self.count(lines, "spawn") == 1,
              f"two tool calls used one process ({self.count(lines, 'spawn')})")
        check(self.count(lines, "decide") == 2,
              f"both calls were answered ({self.count(lines, 'decide')})")
        check(self.count(lines, "exit") == 1,
              "the sidecar exits when its parent closes stdin")

    def run_all(self):
        self.achieved()
        self.screenshot_is_refused()
        self.not_achieved()
        self.uncertain_then_approve()
        self.remote_goes_first_by_default()
        self.local_answers_when_the_remote_fails()
        self.local_first_when_asked()
        self.no_fallback_fails_honestly()
        self.invalid_choice_falls_back()
        self.missing_script_is_dropped()
        self.mcp_verify_reuses_one_sidecar()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bhx", default="target/debug/bhx")
    ap.add_argument("--site", default="http://127.0.0.1:8766")
    ap.add_argument("--verifier", default="")
    ap.add_argument("--stub", default="testkit/laya-stub.py")
    ap.add_argument("--spec", default="testkit/sites/admin/task.json")
    ap.add_argument("--tmp", required=True)
    args = ap.parse_args()
    Suite(args).run_all()
    print(f"\npassed: {PASS}, failed: {FAIL}")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
