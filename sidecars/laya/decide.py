#!/usr/bin/env python3
"""Laya typed-decision sidecar (torch/MPS backend by default).

One JSON request per stdin line, one JSON reply per line. Unlike the skill's
sidecar, this one emits a {"ready": true, "backend": ...} handshake before
accepting requests, so callers know when the 5.5 s model load is done.

    {"state": str|dict, "instructions": str, "criteria": {id: rubric}}
    -> {"choice": id, "probs": {...}, "conf": float, "top": [...], "ms": int,
        "backend": "torch/mps", "n_criteria": int}

Env:
    LAYA_BACKEND   torch | onnx        (default: torch)
    LAYA_DEVICE    mps | cpu           (default: mps when available)
    LAYA_MODEL     HF id               (default: convaiinnovations/laya)
    LAYA_ONNX      path to .onnx graph (backend=onnx only)

Measured on Apple M5 (see docs/benchmarks.md):
    torch/mps   load 4.4 s, first 427 ms, steady 57 ms @3 options -- 1.7 GB
    onnx/cpu    load 5.6 s,               steady 344 ms @3 options -- 0.8 GB

Keep the process warm; a fresh process pays the load cost every time.
"""

import json
import os
import sys
import time

MAX_CRITERIA = int(os.environ.get("LAYA_MAX_CRITERIA", "255"))
WARN_ABOVE = 10  # above this, top probability collapses (measured)


def build():
    backend = os.environ.get("LAYA_BACKEND", "torch").lower()
    if backend == "onnx":
        from laya.onnx_agent import ONNXAgent

        onnx = os.environ.get(
            "LAYA_ONNX",
            os.path.expanduser(
                "~/.local/share/laya-demo/models/laya-onnx/onnx/model_fp16.onnx"
            ),
        )
        if not os.path.exists(onnx):
            raise SystemExit(f"Laya ONNX graph not found: {onnx}")
        return ONNXAgent(os.environ.get("LAYA_MODEL", "convaiinnovations/laya"),
                         onnx_path=onnx), "onnx/cpu"

    import torch
    from laya import Agent

    device = os.environ.get("LAYA_DEVICE", "").lower() or (
        "mps" if torch.backends.mps.is_available() else "cpu"
    )
    agent = Agent(os.environ.get("LAYA_MODEL", "convaiinnovations/laya"), device=device)
    return agent, f"torch/{device}"


def answer(agent, backend, req):
    crit = req.get("criteria") or {}
    if not crit:
        raise ValueError("criteria must contain at least one option")
    if len(crit) > MAX_CRITERIA:
        raise ValueError(f"{len(crit)} criteria exceeds LAYA_MAX_CRITERIA={MAX_CRITERIA}")

    state = req["state"]
    if isinstance(state, (dict, list)):
        state = json.dumps(state, ensure_ascii=False)

    t0 = time.time()
    r = agent.predict(
        state,
        {"pick": {"type": "choice", "instructions": req["instructions"], "criteria": crit}},
    )
    a = r["answers"]["pick"]
    out = {
        "choice": a["choice"],
        "probs": a["probabilities"],
        "conf": a["answer_confidence"],
        "top": sorted(a["probabilities"].items(), key=lambda kv: -kv[1])[:5],
        "ms": round((time.time() - t0) * 1000),
        "backend": backend,
        "n_criteria": len(crit),
    }
    if len(crit) > WARN_ABOVE:
        out["warning"] = (
            f"{len(crit)} candidates: top probability collapses above ~30. "
            "Pre-filter to <=10 with rules before asking the model."
        )
    return out


def main():
    agent, backend = build()

    if "--once" in sys.argv:
        print(json.dumps(answer(agent, backend, json.load(sys.stdin)), ensure_ascii=False),
              flush=True)
        return

    # `ready` has to mean "answers at steady speed", not "the process is up".
    # The first forward pass costs ~7 s on torch/MPS (kernel JIT) where later
    # ones cost ~0.1 s, and a strategy model that answers 7 s after being
    # asked answers after the moment its decision was about. Measured, not
    # assumed: warming only the load moved a game's first accepted decision
    # from 5.9 s to 4.7 s on the page clock, still past a window that closed
    # at 5.2 s; warming one inference is what makes the first answer fast.
    # A throwaway request on a throwaway state — it costs the caller startup
    # they were going to pay anyway, just at a moment nothing depends on it.
    try:
        answer(agent, backend, {
            # Length matters: an MPS kernel is compiled for a sequence length,
            # so a 7-character warm-up does not warm the path a ~500-character
            # page state takes. Use a realistic size and two options, which is
            # what real requests look like here.
            "state": "Task: warm up the model\nURL: http://127.0.0.1/warm\n"
                     + ("padding for a realistic sequence length " * 12),
            "instructions": "warmup",
            "criteria": {"a": "warmup option a", "b": "warmup option b"},
        })
    except Exception:
        pass  # a failed warm-up must not stop the sidecar from starting

    print(json.dumps({"ready": True, "backend": backend}), flush=True)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            out = answer(agent, backend, json.loads(line))
        except Exception as e:  # keep the sidecar alive across bad requests
            out = {"error": f"{type(e).__name__}: {e}"}
        print(json.dumps(out, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
