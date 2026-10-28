#!/usr/bin/env bash
# Phase 2 acceptance: the task engine, server-side truth, chaos, crash recovery.
#
# A-group cases (docs/requirements.md §7): extract, form, export, cross-page,
# popup+API-error recovery. Plus the Phase-2 exit condition: kill the process
# mid-step, resume from the last checkpoint, finish the task.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8766}"
BASE="http://127.0.0.1:$PORT"
BIN="./target/debug/bhx"
TMP="$(mktemp -d)"
PASS=0
FAIL=0
FAILED=()

pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); FAILED+=("$*"); }
cleanup() { [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; }
trap cleanup EXIT

echo "== build"
cargo build -q || exit 1

echo "== serve testkit on :$PORT (server-side truth + chaos)"
python3 testkit/server.py --port "$PORT" >"$TMP/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do
  curl -sf "$BASE/admin/__state" >/dev/null 2>&1 && break
  sleep 0.05
done
curl -sf "$BASE/admin/__state" >/dev/null || { echo "server did not start"; cat "$TMP/server.log"; exit 1; }

reset() {
  curl -sf -X POST "$BASE/admin/__reset" >/dev/null
  curl -sf -X POST "$BASE/form/__reset" >/dev/null
}
prep() { # spec path -> per-run copy with the right port
  local out="$TMP/$(basename "$1")"
  sed "s/8766/$PORT/g" "$1" >"$out"
  echo "$out"
}
id_of() { python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1]))["task_id"])' "$1"; }
artifact_of() { # home id suffix
  "$BIN" task artifacts "$2" --home "$1" | cut -f1 | grep -e "$3" | head -1
}
model_calls() { # home id
  "$BIN" task show "$2" --home "$1" --json |
    python3 -I -c 'import json,sys;print(json.load(sys.stdin)["model_calls"])'
}
elapsed_of() {
  python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1]))["elapsed_ms"])' "$1"
}
budget_ok() { # label home id [run-output.json]
  local n
  [ -n "${4:-}" ] && echo "      elapsed $(elapsed_of "$4") ms"
  n=$(model_calls "$2" "$3")
  if [ "$n" -le 2 ]; then pass "$1: model calls $n <= 2"; else fail "$1: model calls $n > 2"; fi
}

check_extract() { # label spec
  local label=$1 spec=$2 home="$TMP/h-$(basename "$2" .json)"
  reset
  if ! "$BIN" task run "$spec" --home "$home" >"$TMP/$label.json" 2>"$TMP/$label.err"; then
    fail "$label (run failed)"; tail -6 "$TMP/$label.err"; return
  fi
  local id art
  id=$(id_of "$TMP/$label.json")
  art=$(artifact_of "$home" "$id" '\.json$')
  if [ -z "$art" ] || ! python3 -I testkit/check.py extract --artifact "$art" --state "$BASE/admin/__state"; then
    fail "$label (extract mismatch)"
  else
    pass "$label"
  fi
  budget_ok "$label" "$home" "$id" "$TMP/$label.json"
}

# ---------------------------------------------------------------- A1 extract

echo
echo "== A1 提取页面内容为结构化数据"
check_extract A1 "$(prep testkit/sites/admin/task-extract.json)"

# ------------------------------------------------------------------- A2 form

echo
echo "== A2 多步表单，服务端验收"
reset
FORM_SPEC=$(prep testkit/sites/form/task-form.json)
if "$BIN" task run "$FORM_SPEC" --home "$TMP/h-form" >"$TMP/a2.json" 2>"$TMP/a2.err"; then
  if python3 -I testkit/check.py form --state "$BASE/form/__state" \
    --expect '{"name":"王小明","email":"wang@example.com","phone":"13800001111","company":"蓝鲸科技","role":"engineer"}'; then
    pass "A2 form"
  else
    fail "A2 form (server state mismatch)"
  fi
  budget_ok "A2 form" "$TMP/h-form" "$(id_of "$TMP/a2.json")" "$TMP/a2.json"
else
  fail "A2 form (run failed)"; tail -6 "$TMP/a2.err"
fi

# ----------------------------------------------------------------- A3 export

echo
echo "== A3 条件查询并导出 CSV，与服务器产物逐字节比对"
reset
EXPORT_SPEC=$(prep testkit/sites/admin/task.json)
if "$BIN" task run "$EXPORT_SPEC" --home "$TMP/h-export" >"$TMP/a3.json" 2>"$TMP/a3.err"; then
  ID=$(id_of "$TMP/a3.json")
  ART=$(artifact_of "$TMP/h-export" "$ID" '\.csv$')
  if [ -z "$ART" ] || ! python3 -I testkit/check.py download --artifact "$ART" --state "$BASE/admin/__state"; then
    fail "A3 export (artifact does not match server)"
  elif ! python3 -I testkit/check.py rows --state "$BASE/admin/__state" --count 6; then
    fail "A3 export (server order set changed)"
  else
    pass "A3 export"
  fi
  budget_ok "A3 export" "$TMP/h-export" "$ID" "$TMP/a3.json"
else
  fail "A3 export (run failed)"; tail -6 "$TMP/a3.err"
fi

# ------------------------------------------------------------ A4 cross-page

echo
echo "== A4 跨站点、跨页面 6 步流程"
reset
CROSS_SPEC=$(prep testkit/sites/admin/task-cross.json)
if "$BIN" task run "$CROSS_SPEC" --home "$TMP/h-cross" >"$TMP/a4.json" 2>"$TMP/a4.err"; then
  ID=$(id_of "$TMP/a4.json")
  ART=$(artifact_of "$TMP/h-cross" "$ID" '\.json$')
  OK=1
  [ -n "$ART" ] && python3 -I testkit/check.py extract --artifact "$ART" --state "$BASE/admin/__state" || OK=0
  python3 -I testkit/check.py form --state "$BASE/form/__state" \
    --expect '{"name":"李雷","email":"lilei@example.com","phone":"13900002222","company":"云栖数据","role":"manager"}' || OK=0
  if [ "$OK" = 1 ]; then pass "A4 cross-page"; else fail "A4 cross-page (state mismatch)"; fi
  budget_ok "A4 cross-page" "$TMP/h-cross" "$ID" "$TMP/a4.json"
else
  fail "A4 cross-page (run failed)"; tail -6 "$TMP/a4.err"
fi

# ------------------------------------------------------- A5 popup + API 500

echo
echo "== A5 弹窗 + 接口错误后仍完成导出"
reset
curl -sf -X POST "$BASE/admin/__chaos" -H 'content-type: application/json' -d '{
  "seed": 42,
  "rules": [
    { "on": "click", "selector": "#export", "popup": "confirm", "message": "导出将包含 3 条记录，继续？", "times": 1 },
    { "on": "request", "path": "/admin/api/export", "status": 500, "times": 1 }
  ]
}' >/dev/null
CHAOS_SPEC=$(prep testkit/sites/admin/task-chaos.json)
if "$BIN" task run "$CHAOS_SPEC" --home "$TMP/h-chaos" >"$TMP/a5.json" 2>"$TMP/a5.err"; then
  ID=$(id_of "$TMP/a5.json")
  ART=$(artifact_of "$TMP/h-chaos" "$ID" '\.csv$')
  OK=1
  python3 -I testkit/check.py chaos --state "$BASE/admin/__state" || OK=0
  [ -n "$ART" ] && python3 -I testkit/check.py download --artifact "$ART" --state "$BASE/admin/__state" || OK=0
  if [ "$OK" = 1 ]; then pass "A5 chaos recovery"; else fail "A5 chaos recovery (state mismatch)"; fi
  # fail_when must cut the failed attempt short; the step timeout is 8 s.
  A5_MS=$(elapsed_of "$TMP/a5.json")
  if [ "$A5_MS" -le 5000 ]; then
    pass "A5 failed attempt exited on the page's error signal ($A5_MS ms)"
  else
    fail "A5 took $A5_MS ms: fail_when did not preempt step.timeout_ms"
  fi
  budget_ok "A5 chaos recovery" "$TMP/h-chaos" "$ID" "$TMP/a5.json"
else
  fail "A5 chaos recovery (run failed)"; tail -8 "$TMP/a5.err"
  python3 -I testkit/check.py chaos --state "$BASE/admin/__state" >/dev/null 2>&1 ||
    echo "      (chaos rules did not both fire, so this failure is not evidence of recovery)"
fi

# -------------------------------------------------- R1 kill mid-step, resume

echo
echo "== R1 进程被杀后从检查点恢复"
reset
CRASH_SPEC=$(prep testkit/sites/admin/task-crash.json)
CRASH_HOME="$TMP/h-crash"
rm -rf "$CRASH_HOME"
"$BIN" task run "$CRASH_SPEC" --home "$CRASH_HOME" >"$TMP/crash.out" 2>&1 &
CRASH_PID=$!
FOUND=0
for _ in $(seq 1 400); do
  F=$(ls "$CRASH_HOME"/tasks/*/events.jsonl 2>/dev/null | head -1)
  if [ -n "${F:-}" ] && grep -q '"step_id":"s4"' "$F" 2>/dev/null; then FOUND=1; break; fi
  sleep 0.05
done
if [ "$FOUND" != 1 ]; then
  kill -9 "$CRASH_PID" 2>/dev/null
  fail "R1 (never reached the slow step)"; tail -5 "$TMP/crash.out"
else
  kill -9 "$CRASH_PID" 2>/dev/null
  wait "$CRASH_PID" 2>/dev/null
  sleep 0.3
  CID=$(ls "$CRASH_HOME/tasks" | head -1)

  "$BIN" task resume "$CID" --home "$CRASH_HOME" >"$TMP/r1a.out" 2>&1
  RC=$?
  if [ "$RC" -ne 3 ]; then
    fail "R1 resume after crash should ask for approval (exit 3), got $RC"
  else
    pass "R1 interrupted step asks a human instead of retrying"
    "$BIN" task approve "$CID" --home "$CRASH_HOME" >/dev/null 2>&1
    if "$BIN" task resume "$CID" --home "$CRASH_HOME" >"$TMP/r1b.json" 2>"$TMP/r1b.err"; then
      RAN=$(python3 -I -c 'import json,sys;d=json.load(open(sys.argv[1]));print(",".join(s["step_id"] for s in d["steps"]))' "$TMP/r1b.json")
      if [ "$RAN" = "s4" ]; then
        pass "R1 resumed at the checkpoint and ran only s4 ($(elapsed_of "$TMP/r1b.json") ms)"
      else
        fail "R1 resumed steps [$RAN], expected only s4"
      fi
      ART=$(artifact_of "$CRASH_HOME" "$CID" '\.csv$')
      if [ -n "$ART" ] && [ -s "$ART" ]; then
        pass "R1 artifact from the pre-crash steps survived"
      else
        fail "R1 artifact from the pre-crash steps is missing"
      fi
    else
      fail "R1 resumed run failed"; tail -8 "$TMP/r1b.err"
    fi
  fi
fi

# ------------------------------------------------------------------ summary

echo
echo "passed: $PASS, failed: $FAIL"
if [ "$FAIL" -ne 0 ]; then
  printf '  failed: %s\n' "${FAILED[@]}"
  echo "  artifacts kept in $TMP"
fi
[ "$FAIL" -eq 0 ]
