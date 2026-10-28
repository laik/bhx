#!/usr/bin/env bash
# Phase 3 acceptance: C group (interactive controller) and B group (vision).
#
# C cases are about the loop, not about the model: what matters is that a
# fixed-rate policy can hold and release input, change what it does when the
# page state changes, and stop safely on a deadline.
#
# B cases run the whole vision path — screenshot, model call, schema checks,
# box-to-CSS conversion, action, state verification — against a deterministic
# pixel-scanning stand-in for a VLM (testkit/vlm.py). That proves the harness
# owns its half of the contract; it does not measure a real model's accuracy.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8768}"
VPORT="${VPORT:-8790}"
BASE="http://127.0.0.1:$PORT"
BIN="./target/debug/bhx"
TMP="$(mktemp -d)"
PASS=0
FAIL=0
FAILED=()

pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); FAILED+=("$*"); }
cleanup() {
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  [ -n "${VLM:-}" ] && kill "$VLM" 2>/dev/null
}
trap cleanup EXIT

echo "== build"
cargo build -q || exit 1

echo "== serve testkit on :$PORT"
python3 testkit/server.py --port "$PORT" >"$TMP/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do
  curl -sf "$BASE/game/__state" >/dev/null 2>&1 && break
  sleep 0.05
done
curl -sf "$BASE/game/__state" >/dev/null || { echo "server did not start"; cat "$TMP/server.log"; exit 1; }

echo "== serve vision stand-in on :$VPORT"
python3 testkit/vlm.py --port "$VPORT" >"$TMP/vlm.log" 2>&1 &
VLM=$!
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$VPORT/health" >/dev/null 2>&1 && break
  sleep 0.05
done
curl -sf "http://127.0.0.1:$VPORT/health" >/dev/null || { echo "vlm stub did not start"; cat "$TMP/vlm.log"; exit 1; }
export BH_VLM_URL="http://127.0.0.1:$VPORT/v1/chat/completions"
export BH_VLM_MODEL="pixel-scan"

reset() {
  curl -sf -X POST "$BASE/game/__reset" >/dev/null
  curl -sf -X POST "$BASE/canvas/__reset" >/dev/null
}
prep() { # spec path -> per-run copy with the right port
  local out="$TMP/$(basename "$1")"
  sed "s/8766/$PORT/g" "$1" >"$out"
  echo "$out"
}
field() { # report.json dotted.path
  python3 -I -c 'import json,sys
v=json.load(open(sys.argv[1]))
for k in sys.argv[2].split("."):
    v = v[int(k)] if isinstance(v, list) else v[k]
print(v)' "$1" "$2"
}
home_for() { echo "$TMP/h-$(basename "$1" .json)"; }

# run_case label spec expected-state
# Leaves the report in $TMP/<label>.json and the exit code in $RC.
run_case() {
  local label=$1 spec=$2 want=$3 home
  home=$(home_for "$spec")
  reset
  "$BIN" task run "$spec" --home "$home" >"$TMP/$label.json" 2>"$TMP/$label.err"
  RC=$?
  local got
  got=$(field "$TMP/$label.json" state)
  if [ "$got" != "$want" ]; then
    fail "$label: state $got, expected $want"
    tail -6 "$TMP/$label.err"
    return 1
  fi
  return 0
}

why_is() { # label expected
  local got
  got=$(field "$TMP/$1.json" controller.why)
  if [ "$got" = "$2" ]; then pass "$1: stopped with $got"; else fail "$1: stopped with $got, expected $2"; fi
}

actions_at_least() { # label n
  local got
  got=$(field "$TMP/$1.json" controller.actions)
  if [ "$got" -ge "$2" ]; then pass "$1: dispatched $got actions"; else fail "$1: only $got actions"; fi
}

model_budget() { # label home [max]
  local n max=${3:-1}
  n=$("$BIN" task show "$(field "$TMP/$1.json" task_id)" --home "$2" --json |
    python3 -I -c 'import json,sys;print(json.load(sys.stdin)["model_calls"])')
  if [ "$n" -le "$max" ]; then pass "$1: model calls $n <= $max"; else fail "$1: model calls $n > $max"; fi
}

verified_last() { # label
  local ok
  ok=$(python3 -I -c 'import json,sys
r=json.load(open(sys.argv[1]))
s=r.get("steps",[])
print(bool(s) and s[-1].get("verified") is True)' "$TMP/$1.json")
  if [ "$ok" = "True" ]; then pass "$1: the action step is verified"; else fail "$1: action step not verified"; fi
}

check() { # label command...
  local label=$1
  shift
  if "$@"; then pass "$label: server truth holds"; else fail "$label: server-truth check failed"; fi
}

elapsed() { # label
  local ms
  ms=$(field "$TMP/$1.json" elapsed_ms)
  pass "$1: task elapsed ${ms} ms"
}

# ------------------------------------------------------------ C1 hold a key

echo
echo "== C1 连续键盘输入，退出后无粘键"
if run_case C1 "$(prep testkit/sites/game/c1-move.json)" succeeded; then
  why_is C1 goal_met
  actions_at_least C1 1
  check C1 python3 -I testkit/check.py game --state "$BASE/game/__state" --min-x 120
fi
model_budget C1 "$(home_for "$TMP/c1-move.json")"

# ----------------------------------------------------------- C2 point click

echo
echo "== C2 在画布上按坐标点击"
if run_case C2 "$(prep testkit/sites/game/c2-click.json)" succeeded; then
  why_is C2 goal_met
  check C2 python3 -I testkit/check.py game --state "$BASE/game/__state" --clicks 1
fi
model_budget C2 "$(home_for "$TMP/c2-click.json")"

# ------------------------------------------------ C3 close the loop on state

echo
echo "== C3 观察状态并切换动作（先右后下）"
if run_case C3 "$(prep testkit/sites/game/c3-loop.json)" succeeded; then
  why_is C3 goal_met
  actions_at_least C3 2
  check C3 python3 -I testkit/check.py game --state "$BASE/game/__state" --min-x 120 --min-y 80
fi
model_budget C3 "$(home_for "$TMP/c3-loop.json")"

# ------------------------------------------------- C4 deadline releases input

echo
echo "== C4 超时后安全停止并释放全部输入"
# This case is expected to fail the task; the exit code must say so too.
if run_case C4 "$(prep testkit/sites/game/c4-deadline.json)" failed; then
  if [ "$RC" -eq 1 ]; then pass "C4: exit code 1 for a failed task"; else fail "C4: exit code $RC, expected 1"; fi
  why_is C4 deadline
  MS=$(field "$TMP/C4.json" controller.elapsed_ms)
  if [ "$MS" -ge 1000 ] && [ "$MS" -le 3000 ]; then
    pass "C4: stopped on the deadline ($MS ms)"
  else
    fail "C4: deadline fired at $MS ms, expected 1000–3000"
  fi
  check C4 python3 -I testkit/check.py game --state "$BASE/game/__state" --min-x 100
fi
model_budget C4 "$(home_for "$TMP/c4-deadline.json")"

# ------------------------------------------------------- B1 visual on DOM
# The page has a normal semantic button; the target is still {"visual": ...},
# so the click can only happen if the screenshot→model→point path works.

echo
echo "== B1 从截图定位目标控件（页面上有正常语义）"
if run_case B1 "$(prep testkit/sites/canvas/b1-visual-click.json)" succeeded; then
  verified_last B1
  elapsed B1
  check B1 python3 -I testkit/check.py canvas --state "$BASE/canvas/__state" --min-clicks 1 --source dom
fi
model_budget B1 "$(home_for "$TMP/b1-visual-click.json")" 2

# --------------------------------------------------- B2 no semantic label
# Canvas only: no button, no role=button, no links. The step's precondition
# asserts that in-browser before the visual locate runs.

echo
echo "== B2 无语义标签的 canvas 按钮"
if run_case B2 "$(prep testkit/sites/canvas/b2-no-semantics.json)" succeeded; then
  verified_last B2
  elapsed B2
  check B2 python3 -I testkit/check.py canvas --state "$BASE/canvas/__state" --min-clicks 1 \
    --source canvas --click-near 170 94 --tolerance 6
fi
model_budget B2 "$(home_for "$TMP/b2-no-semantics.json")" 2

# --------------------------------------------------------------- B3 drag
# Two visual locates in one action: start point and drop zone, then a real
# press/move/release gesture. The server checks both ends, ±4 px.

echo
echo "== B3 拖拽：起点与落点误差 ≤ 4px"
if run_case B3 "$(prep testkit/sites/canvas/b3-drag.json)" succeeded; then
  verified_last B3
  elapsed B3
  check B3 python3 -I testkit/check.py canvas --state "$BASE/canvas/__state" \
    --drag-start 140 200 --drag-end 500 200 --tolerance 4
fi
model_budget B3 "$(home_for "$TMP/b3-drag.json")" 2

# ------------------------------------------------- B4 state, not pixels
# The button looks identical before and after; the server confirms after
# 300 ms and only page state knows. Success must come from the state predicate.

echo
echo "== B4 用状态判断成功，而不是截图相似度"
if run_case B4 "$(prep testkit/sites/canvas/b4-state-verified.json)" succeeded; then
  verified_last B4
  MS=$(field "$TMP/B4.json" steps.1.elapsed_ms)
  if [ "$MS" -ge 300 ]; then pass "B4: verification waited for the server ($MS ms)"; else fail "B4: step finished in $MS ms, before the 300 ms confirmation"; fi
  elapsed B4
  check B4 python3 -I testkit/check.py canvas --state "$BASE/canvas/__state" --min-clicks 1 --confirmed
fi
model_budget B4 "$(home_for "$TMP/b4-state-verified.json")" 2

# -------------------------------------------------- B5 re-locate after relayout
# Same description, different layout: the old point is stale by construction.

echo
echo "== B5 布局变化后重新识别目标"
if run_case B5 "$(prep testkit/sites/canvas/b5-relayout.json)" succeeded; then
  verified_last B5
  elapsed B5
  check B5 python3 -I testkit/check.py canvas --state "$BASE/canvas/__state" --min-clicks 1 \
    --source canvas --click-near 460 278 --tolerance 6
fi
model_budget B5 "$(home_for "$TMP/b5-relayout.json")" 2

# ------------------------------------------------- B6 DOM -> vision reroute
# The step names a control that exists in the DOM but has no layout box, so the
# AX tree cannot see it. With `fallback_visual` set, the runner re-tries the
# same action from a screenshot instead of failing the task.

echo
echo "== B6 DOM 定位失败后就地改道视觉"
if run_case B6 "$(prep testkit/sites/canvas/b6-reroute.json)" succeeded; then
  verified_last B6
  elapsed B6
  check B6 python3 -I testkit/check.py canvas --state "$BASE/canvas/__state" --min-clicks 1 \
    --source canvas --click-near 170 94 --tolerance 6
fi
model_budget B6 "$(home_for "$TMP/b6-reroute.json")" 2

# ------------------------------------------------------------------ summary

echo
echo "passed: $PASS, failed: $FAIL"
if [ "$FAIL" -ne 0 ]; then
  printf '  failed: %s\n' "${FAILED[@]}"
  echo "  artifacts kept in $TMP"
fi
[ "$FAIL" -eq 0 ]
