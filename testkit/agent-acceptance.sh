#!/usr/bin/env bash
# Cross-agent acceptance, run by hand (a paid model is involved, so this is not
# part of run-phase4.sh). Starts the testkit admin site, points one external
# agent at `bhx mcp`, gives it the same prompt every agent gets, and then checks
# what the *site* recorded rather than what the agent claimed.
#
#   ./testkit/agent-acceptance.sh claude     # claude -p
#   ./testkit/agent-acceptance.sh pi         # pi -p (local Qwen3 provider)
#   AGENT_CMD='some-agent ...' ./testkit/agent-acceptance.sh cmd
#
# For `cmd`, these are exported and the shell command is run with the prompt on
# stdin: PROMPT, MCP_CONFIG (a --mcp-config file naming this bhx), HOME_DIR,
# SITE (base URL). The script exits non-zero unless the site records the export.
set -uo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-}"
case "$MODE" in
  claude|pi|cmd) ;;
  *) echo "usage: $0 claude|pi|cmd"; exit 2 ;;
esac

BH="$PWD/target/debug/bhx"
[ -x "$BH" ] || { echo "build first: cargo build"; exit 2; }

PORT="${PORT:-8788}"
SITE="http://127.0.0.1:$PORT"
TMP="$(mktemp -d)"
HOME_DIR="$TMP/home"
export HOME_DIR SITE

cleanup() { [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; }
trap cleanup EXIT

python3 -I testkit/server.py --port "$PORT" >"$TMP/site.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do curl -sf "$SITE/admin/__state" >/dev/null && break; sleep 0.05; done

printf '{"mcpServers":{"bhx":{"command":"%s","args":["mcp","--home","%s"]}}}' \
  "$BH" "$HOME_DIR" >"$TMP/mcp.json"
MCP_CONFIG="$TMP/mcp.json"
export MCP_CONFIG

# The same admin spec the smoke suites use, repointed at this server's port.
SPEC=$(sed "s/8766/$PORT/" testkit/sites/admin/task.json \
  | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin), ensure_ascii=False))')
PROMPT="Use the MCP tool mcp__bhx__task_run exactly once with this argument: {\"task\": $SPEC}. Then call mcp__bhx__task_status exactly once with the returned task_id. Reply with just the task_id and final state. Do not use any other tool."
export PROMPT

START=$(date +%s)
case "$MODE" in
  claude)
    printf '%s' "$PROMPT" | (cd "$TMP" && timeout 300 claude -p \
      --mcp-config "$MCP_CONFIG" --strict-mcp-config --setting-sources user \
      --allowedTools 'mcp__bhx__task_run' 'mcp__bhx__task_status') >"$TMP/agent.out" 2>&1
    RC=$?
    ;;
  pi)
    mkdir -p "$TMP/pi"
    cp ~/.pi/agent/models.json "$TMP/pi/models.json"
    PI_CODING_AGENT_DIR="$TMP/pi" pi mcp add bhx --exposure direct -- \
      "$BH" mcp --home "$HOME_DIR" >/dev/null 2>&1
    PI_CODING_AGENT_DIR="$TMP/pi" timeout 300 pi -p --no-session \
      --provider local --model qwen3.8-flash-next --tools 'mcp__bhx__*' \
      "$PROMPT" >"$TMP/agent.out" 2>&1
    RC=$?
    ;;
  cmd)
    printf '%s' "$PROMPT" | (cd "$TMP" && timeout 300 sh -c "$AGENT_CMD") \
      >"$TMP/agent.out" 2>&1
    RC=$?
    ;;
esac
END=$(date +%s)
WALL=$((END - START))

echo "== agent ($MODE) exit=$RC wall=${WALL}s"
tail -20 "$TMP/agent.out"

echo "== site truth (what the server recorded, not what the agent said)"
curl -s "$SITE/admin/__state" | python3 -I -c '
import json, sys
d = json.load(sys.stdin)
dl = d["downloads"]
print("downloads:", [(x["name"], x["sha256"][:16]) for x in dl])
sys.exit(0 if dl else 1)
'
DL=$?
echo "== artifacts kept in $TMP"

[ "$RC" -eq 0 ] && [ "$DL" -eq 0 ] || exit 1
echo "PASS  $MODE completed the task (${WALL}s), export recorded by the site"
