#!/usr/bin/env bash
# Phase 1 acceptance: run the real workflow against the local test site and
# drive the same browser over MCP. Any failing check exits non-zero.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8766}"
BIN="${BIN:-./target/debug/bhx}"
TMP="$(mktemp -d)"
PASS=0
FAIL=0

cleanup() { [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; }
trap cleanup EXIT

echo "== build"
cargo build -q || exit 1

echo "== serve testkit on :$PORT"
python3 testkit/server.py --port "$PORT" >/dev/null 2>&1 &
SRV=$!
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$PORT/admin/__state" >/dev/null && break
  sleep 0.05
done

sed "s/8766/$PORT/g" testkit/sites/admin/task.json > "$TMP/task.json"
sed "s/8766/$PORT/g" testkit/sites/admin/task-dialog.json > "$TMP/task-dialog.json"

check() {
  local name="$1"; shift
  if "$@" >/dev/null 2>"$TMP/err.txt"; then
    echo "PASS  $name"; PASS=$((PASS + 1))
  else
    echo "FAIL  $name"; sed 's/^/      /' "$TMP/err.txt" | tail -5; FAIL=$((FAIL + 1))
  fi
}

echo "== acceptance task (login -> filter -> export), 3 consecutive runs"
for i in 1 2 3; do
  check "run #$i" "$BIN" run "$TMP/task.json" --artifacts "$TMP/artifacts-$i"
done

echo "== artifact check"
CSV="$TMP/artifacts-1/orders-失败.csv"
if [ -s "$CSV" ] && [ "$(wc -l < "$CSV" | tr -d ' ')" -eq 3 ]; then
  echo "PASS  exported CSV has header + 2 filtered rows"; PASS=$((PASS + 1))
else
  echo "FAIL  exported CSV missing or wrong row count"; FAIL=$((FAIL + 1))
fi

echo "== injected alert during export"
check "dialog recovery" "$BIN" run "$TMP/task-dialog.json" --artifacts "$TMP/artifacts-dialog"

echo "== MCP stdio"
check "mcp smoke" python3 -I testkit/mcp-smoke.py --bin "$BIN" --url "http://127.0.0.1:$PORT/"

echo
echo "passed: $PASS, failed: $FAIL"
[ "$FAIL" -eq 0 ]
