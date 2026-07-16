#!/usr/bin/env bash
# Hook behavior tests. Run from repo root: claude/hooks/test_hook.sh
set -euo pipefail
HOOK="$(dirname "$0")/glep_redirect.py"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

run_hook() { # $1 = json, $2 = cwd
  echo "$1" | python3 "$HOOK" 2>/dev/null || true
}

# 1. No .glep dir: hook must allow (empty output)
OUT=$(cd "$TMP" && run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x"},"cwd":"'"$TMP"'"}')
[ -z "$OUT" ] || { echo "FAIL: expected allow without .glep, got: $OUT"; exit 1; }

# 2. With .glep dir: Grep must be denied with a glep command
mkdir "$TMP/.glep"
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"foo bar","-i":true,"glob":"*.rs"},"cwd":"'"$TMP"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny"; exit 1; }
echo "$OUT" | grep -q "glep -i -g '\*.rs' -e 'foo bar'" || { echo "FAIL: bad command: $OUT"; exit 1; }

# 3. Glob maps to --files
OUT=$(run_hook '{"tool_name":"Glob","tool_input":{"pattern":"**/*.py"},"cwd":"'"$TMP"'"}')
echo "$OUT" | grep -q "glep --files '\*\*/\*.py'" || { echo "FAIL: bad glob mapping: $OUT"; exit 1; }

# 4. Grep count mode maps to glep -c
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","output_mode":"count"},"cwd":"'"$TMP"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny for count mode"; exit 1; }
echo "$OUT" | grep -q "glep -c -e 'x'" || { echo "FAIL: bad count command: $OUT"; exit 1; }

# 5. Grep -A context maps to glep -A, not -C
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","-A":2},"cwd":"'"$TMP"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny for -A context"; exit 1; }
echo "$OUT" | grep -q "glep -A 2 -e 'x'" || { echo "FAIL: bad -A command: $OUT"; exit 1; }
if echo "$OUT" | grep -q -- "-C"; then
  echo "FAIL: -A payload should not produce -C: $OUT"; exit 1
fi

# 6. Non-object JSON: must allow (exit 0, no output, no crash)
set +e
OUT=$(echo 'null' | python3 "$HOOK" 2>/dev/null)
RC=$?
set -e
[ "$RC" -eq 0 ] && [ -z "$OUT" ] || { echo "FAIL: non-object JSON should exit 0 with no output (rc=$RC out=$OUT)"; exit 1; }

echo "hook tests: OK"
