#!/usr/bin/env bash
# Phase 4 acceptance: REST/WebSocket front door, planning (explicit and
# engine-side), semantic verification (remote and Laya sidecar), interactive
# jobs over MCP, and auto-repair. Starts the testkit site, `bhx serve`, and
# deterministic model stand-ins, then runs the eight smoke suites and prints
# one summary line per group.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8770}"
APORT="${APORT:-8771}"
PPORT="${PPORT:-8791}"
VPORT="${VPORT:-8793}"
RPORT="${RPORT:-8792}"
CPORT="${CPORT:-8794}"
BASE="http://127.0.0.1:$PORT"
API="http://127.0.0.1:$APORT"
TMP="$(mktemp -d)"
PASS=0
FAIL=0

pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }
cleanup() {
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  [ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null
  [ -n "${PLANNER:-}" ] && kill "$PLANNER" 2>/dev/null
  [ -n "${VERIFIER:-}" ] && kill "$VERIFIER" 2>/dev/null
  [ -n "${REPAIRER:-}" ] && kill "$REPAIRER" 2>/dev/null
  [ -n "${CHOOSER:-}" ] && kill "$CHOOSER" 2>/dev/null
}
trap cleanup EXIT

echo "== build"
cargo build -q || exit 1

echo "== serve testkit on :$PORT, REST on :$APORT, planner on :$PPORT, verifier on :$VPORT, repairer on :$RPORT"
python3 testkit/server.py --port "$PORT" >"$TMP/server.log" 2>&1 &
SRV=$!
env -u BH_LLM_URL -u BH_VLM_URL ./target/debug/bhx serve --addr "127.0.0.1:$APORT" --home "$TMP/home" >"$TMP/api.log" 2>&1 &
API_PID=$!
python3 -I testkit/planner.py --port "$PPORT" >"$TMP/planner.log" 2>&1 &
PLANNER=$!
python3 -I testkit/verifier.py --port "$VPORT" >"$TMP/verifier.log" 2>&1 &
VERIFIER=$!
python3 -I testkit/repairer.py --port "$RPORT" >"$TMP/repairer.log" 2>&1 &
REPAIRER=$!
python3 -I testkit/chooser.py --port "$CPORT" >"$TMP/chooser.log" 2>&1 &
CHOOSER=$!
for _ in $(seq 1 100); do
  curl -sf "$BASE/admin/__state" >/dev/null 2>&1 && break
  sleep 0.05
done
for _ in $(seq 1 100); do
  curl -sf "$API/v1/health" >/dev/null 2>&1 && break
  sleep 0.05
done
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$PPORT/health" >/dev/null 2>&1 && break
  sleep 0.05
done
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$CPORT/health" >/dev/null 2>&1 && break
  sleep 0.05
done
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$VPORT/health" >/dev/null 2>&1 && break
  sleep 0.05
done
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$RPORT/health" >/dev/null 2>&1 && break
  sleep 0.05
done
curl -sf "$BASE/admin/__state" >/dev/null || { echo "server did not start"; cat "$TMP/server.log"; exit 1; }
curl -sf "$API/v1/health" >/dev/null || { echo "bhx serve did not start"; cat "$TMP/api.log"; exit 1; }
curl -sf "http://127.0.0.1:$PPORT/health" >/dev/null || { echo "planner did not start"; cat "$TMP/planner.log"; exit 1; }
curl -sf "http://127.0.0.1:$VPORT/health" >/dev/null || { echo "verifier did not start"; cat "$TMP/verifier.log"; exit 1; }
curl -sf "http://127.0.0.1:$RPORT/health" >/dev/null || { echo "repairer did not start"; cat "$TMP/repairer.log"; exit 1; }

echo
echo "== REST + WS end to end"
python3 -I testkit/rest-smoke.py --api "$API" --site "$BASE" \
  --spec testkit/sites/admin/task.json
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "REST/WS: create, status, events, artifact, errors, cancel"
else
  fail "REST/WS smoke (exit $RC)"
fi

echo
echo "== REST session-level browser control"
python3 -I testkit/session-smoke.py --api "$API" --site "$BASE" --home "$TMP/home"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "sessions: navigate, observe, actions, condition, JS isolation, screenshot, downloads, close"
else
  fail "session smoke (exit $RC)"
fi

echo
echo "== run history and trace replay"
python3 -I testkit/trace-stats-smoke.py --bin ./target/debug/bhx --port 8772
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "trace/stats: durations, success rate, failed actions, action events, trace bundle"
else
  fail "trace/stats smoke (exit $RC)"
fi

echo
echo "== scheduled execution"
python3 -I testkit/schedule-smoke.py --bin ./target/debug/bhx --port 8773
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "schedule: names not values, loud missing credential, one export, no blind retry, daemon"
else
  fail "schedule smoke (exit $RC)"
fi

echo
echo "== remote browser (attach to a Chrome we did not start)"
python3 -I testkit/remote-smoke.py --bin ./target/debug/bhx --port 8775 --cdp-port 9333
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "remote: http and ws endpoints, tab closed, browser survives, dead endpoint refused"
else
  fail "remote browser smoke (exit $RC)"
fi

echo
echo "== plan → task → artifact"
BH_LLM_URL="http://127.0.0.1:$PPORT/v1/chat/completions" \
  python3 -I testkit/plan-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --home "$TMP/home-plan" --out "$TMP/plan.json"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "plan: one model call, validated spec, real CSV, no credentials on disk"
else
  fail "plan smoke (exit $RC)"
fi

echo
echo "== engine-side planning for an empty workflow"
python3 -I testkit/auto-plan-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --planner "http://127.0.0.1:$PPORT" --tmp "$TMP/auto-plan"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "auto-plan: observed start_url, persisted plan, no secrets, 8 rejected paths"
else
  fail "auto-plan smoke (exit $RC)"
fi

echo
echo "== semantic verification of a finished task"
BH_LLM_URL="http://127.0.0.1:$VPORT/v1/chat/completions" \
  python3 -I testkit/semantic-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --verifier "http://127.0.0.1:$VPORT" \
  --spec testkit/sites/admin/task.json --tmp "$TMP/semantic"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "semantic verify: achieved / not_achieved / uncertain / no-adapter / bad-threshold"
else
  fail "semantic smoke (exit $RC)"
fi

echo
echo "== Laya sidecar for semantic verification"
python3 -I testkit/laya-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --verifier "http://127.0.0.1:$VPORT" \
  --stub testkit/laya-stub.py \
  --spec testkit/sites/admin/task.json --tmp "$TMP/laya"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "laya: local verdict, honest failure, fallback, invalid choice, missing script"
else
  fail "laya smoke (exit $RC)"
fi

echo
echo "== interactive jobs over MCP (start / status / update / stop)"
python3 -I testkit/interactive-smoke.py --bin ./target/debug/bhx --site "$BASE"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "interactive: background start, live status, policy swap, stale refusal, stop releases input"
else
  fail "interactive smoke (exit $RC)"
fi

echo
echo "== auto-repair of a failed read-only step"
BH_LLM_URL="http://127.0.0.1:$RPORT/v1/chat/completions" \
  python3 -I testkit/repair-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --repairer "http://127.0.0.1:$RPORT" \
  --spec testkit/sites/admin/task.json --tmp "$TMP/repair"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "repair: success, failed replacement, non-read-only skip, nine rejected proposals"
else
  fail "repair smoke (exit $RC)"
fi

echo
echo "== ambiguity arbitration of a multi-match locator"
python3 -I testkit/arbiter-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --chooser "http://127.0.0.1:$CPORT" --tmp "$TMP/arbiter"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "arbiter: default refusal, confident pick, floor, invented id, wrong-but-honoured pick, model down, too many, bad policy"
else
  fail "arbiter smoke (exit $RC)"
fi

echo
echo "== model-chosen strategy inside an interactive controller"
python3 -I testkit/decide-smoke.py --bhx ./target/debug/bhx \
  --site "$BASE" --chooser "http://127.0.0.1:$CPORT" --tmp "$TMP/decide"
RC=$?
if [ "$RC" -eq 0 ]; then
  pass "decide: two strategies score differently, off means zero calls, floor/invented/budget/no-adapter, bad specs refused"
else
  fail "decide smoke (exit $RC)"
fi

echo
echo "passed: $PASS, failed: $FAIL"
if [ "$FAIL" -ne 0 ]; then
  echo "  artifacts kept in $TMP"
fi
[ "$FAIL" -eq 0 ]
