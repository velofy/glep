#!/usr/bin/env bash
# Hook behavior tests. Run from repo root: claude/hooks/test_hook.sh
set -euo pipefail
HOOK="$(dirname "$0")/glep_redirect.py"
TMP=$(mktemp -d)
# Native tools (python) need a native path on Windows; cygpath exists in Git Bash.
if command -v cygpath >/dev/null 2>&1; then
  TMPN=$(cygpath -m "$TMP")
else
  TMPN="$TMP"
fi
trap 'rm -rf "$TMP"' EXIT

run_hook() { # $1 = json, $2 = cwd
  echo "$1" | python3 "$HOOK" 2>/dev/null || true
}

# 1. No .glep dir: hook must allow (empty output)
OUT=$(cd "$TMP" && run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x"},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: expected allow without .glep, got: $OUT"; exit 1; }

# 2. With .glep dir: Grep must be denied with a glep command
mkdir "$TMP/.glep"
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"foo bar","-i":true,"glob":"*.rs"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny"; exit 1; }
echo "$OUT" | grep -q "glep -i -g '\*.rs' -e 'foo bar'" || { echo "FAIL: bad command: $OUT"; exit 1; }

# 3. Glob maps to --files
OUT=$(run_hook '{"tool_name":"Glob","tool_input":{"pattern":"**/*.py"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q "glep --files '\*\*/\*.py'" || { echo "FAIL: bad glob mapping: $OUT"; exit 1; }

# 4. Grep count mode maps to glep -c
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","output_mode":"count"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny for count mode"; exit 1; }
echo "$OUT" | grep -q "glep -c -e 'x'" || { echo "FAIL: bad count command: $OUT"; exit 1; }

# 5. Grep -A context maps to glep -A, not -C
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","-A":2},"cwd":"'"$TMPN"'"}')
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

# 7. multiline maps to glep -U
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"a\\nb","multiline":true},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny for multiline"; exit 1; }
echo "$OUT" | grep -q "glep -U -e" || { echo "FAIL: multiline should emit -U: $OUT"; exit 1; }

# 8. head_limit appends "| head -N" to the shown command
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","head_limit":50},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny for head_limit"; exit 1; }
echo "$OUT" | grep -qF "| head -50" || { echo "FAIL: head_limit should append a head pipe: $OUT"; exit 1; }

# 9. offset + head_limit: tail pipe first, then head
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","head_limit":50,"offset":10},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected deny for offset+head_limit"; exit 1; }
echo "$OUT" | grep -qF "| tail -n +11 | head -50" || { echo "FAIL: bad offset+head_limit pipes: $OUT"; exit 1; }

# 10. offset alone appends "| tail -n +{K+1}"
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","offset":10},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -qF "| tail -n +11" || { echo "FAIL: offset should append a tail pipe: $OUT"; exit 1; }

# 11. -n:false in content mode allows (glep cannot omit line numbers)
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","-n":false},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: -n:false content mode should allow, got: $OUT"; exit 1; }
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","-n":false,"output_mode":"content"},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: -n:false explicit content mode should allow, got: $OUT"; exit 1; }

# 12. -n:false is meaningless in count mode: still denied
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","-n":false,"output_mode":"count"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: -n:false count mode should still deny"; exit 1; }
echo "$OUT" | grep -q "glep -c -e 'x'" || { echo "FAIL: bad count command with -n:false: $OUT"; exit 1; }

# 13. Malformed head_limit: still denied, no head pipe, no crash
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","head_limit":"abc"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: malformed head_limit should still deny"; exit 1; }
if echo "$OUT" | grep -qF "| head"; then
  echo "FAIL: malformed head_limit should not emit a head pipe: $OUT"; exit 1
fi

echo "hook tests: OK"
