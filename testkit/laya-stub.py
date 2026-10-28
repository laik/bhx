#!/usr/bin/env python3
"""Deterministic stand-in for sidecars/laya/decide.py.

It speaks the same protocol — `{"ready":true}` handshake, then one JSON reply
per stdin line — but has no model. The answer is driven by markers in the
state text, so a smoke test can ask for a specific outcome without touching
the stub's code:

    EXPECT_FAIL   -> not_achieved, confidence 0.95
    UNSURE        -> achieved, confidence 0.40 (below a 0.6 threshold)
    BADCHOICE     -> "e99", an id outside the candidates (the router must reject)
    HANG          -> sleep for an hour (the adapter must time out and kill us)

Process-level failures come from LAYA_STUB_MODE:

    die     exit 7 before the handshake
    hang    never print the handshake
    noisy   print junk lines before the handshake and before replies
    crash   answer one request, then exit
    good    normal (default)

LAYA_STUB_LOG, when set, gets one line per event (`spawn`, `decide <choice>`,
`exit`), which is how the smoke test proves the process is reused rather than
respawned per request.
"""

import json
import os
import sys
import time

MODE = os.environ.get("LAYA_STUB_MODE", "good").strip().lower()
LOG = os.environ.get("LAYA_STUB_LOG")


def log(event):
    if LOG:
        try:
            with open(LOG, "a", encoding="utf-8") as f:
                f.write(event + "\n")
        except OSError:
            pass


def send(obj):
    print(json.dumps(obj, ensure_ascii=False), flush=True)


def decide(req):
    state = req.get("state") or ""
    if isinstance(state, (dict, list)):
        state = json.dumps(state, ensure_ascii=False)
    log("state " + json.dumps(state, ensure_ascii=False))
    criteria = req.get("criteria") or {}
    log("criteria " + json.dumps(criteria, ensure_ascii=False))
    if not isinstance(criteria, dict) or not criteria:
        return {"error": "criteria must contain at least one option"}
    if "HANG" in state:
        log("hang")
        time.sleep(3600)
    outside = False
    if "BADCHOICE" in state:
        choice, conf, outside = "e99", 0.90, True
    elif "EXPECT_FAIL" in state:
        choice, conf = "not_achieved", 0.95
    elif "UNSURE" in state:
        choice, conf = "achieved", 0.40
    else:
        choice, conf = "achieved", 0.90
    if not outside and choice not in criteria:
        # A verify marker on a locate request (or vice versa): fall back to the
        # first candidate so the stub stays useful for both capabilities.
        choice = next(iter(criteria))
    probs = {cid: round((1.0 - conf) / max(1, len(criteria) - 1), 4) for cid in criteria}
    probs[choice] = conf
    return {
        "choice": choice,
        "probs": probs,
        "conf": conf,
        "top": sorted(probs.items(), key=lambda kv: -kv[1])[:5],
        "ms": 0,
        "backend": "stub",
        "n_criteria": len(criteria),
    }


def main():
    log("spawn")
    if MODE == "die":
        log("exit")
        sys.exit(7)
    if MODE == "hang":
        time.sleep(3600)

    if MODE == "noisy":
        print("Loading model weights ...", flush=True)
    send({"ready": True, "backend": "stub"})

    answered = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
            out = decide(req)
        except Exception as e:  # keep the sidecar alive across bad requests
            out = {"error": f"{type(e).__name__}: {e}"}
        if MODE == "noisy":
            print("(warning) confidence is uncalibrated", flush=True)
        log("decide " + str(out.get("choice") or out.get("error")))
        send(out)
        answered += 1
        if MODE == "crash" and answered == 1:
            log("exit")
            return
    log("exit")


if __name__ == "__main__":
    main()
