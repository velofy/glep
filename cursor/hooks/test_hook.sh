#!/usr/bin/env bash
# Hook behavior tests. Run from repo root: cursor/hooks/test_hook.sh
# Covers both output formats the hook can emit: Cursor's permission-deny
# schema and Claude Code's hookSpecificOutput schema.
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

run_hook() { # $1 = json
  echo "$1" | python3 "$HOOK" 2>/dev/null || true
}

# 1a. No .glep dir, Cursor-format payload: hook must allow (empty output)
OUT=$(cd "$TMP" && run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x"},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: expected allow (cursor format) without .glep, got: $OUT"; exit 1; }

# 1b. No .glep dir, Claude-format payload: hook must allow (empty output)
OUT=$(cd "$TMP" && run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x"},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: expected allow (claude format) without .glep, got: $OUT"; exit 1; }

mkdir "$TMP/.glep"

# 2. Cursor-format Grep payload with .glep present: expect Cursor-schema deny
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"foo bar","-i":true,"glob":"*.rs"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: expected cursor-schema deny: $OUT"; exit 1; }
echo "$OUT" | grep -q "glep -i -g '\*.rs' -e 'foo bar'" || { echo "FAIL: bad command: $OUT"; exit 1; }

# 3. Claude-format payload: expect hookSpecificOutput deny
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"foo bar","-i":true,"glob":"*.rs"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected hookSpecificOutput deny: $OUT"; exit 1; }
echo "$OUT" | grep -q "glep -i -g '\*.rs' -e 'foo bar'" || { echo "FAIL: bad command: $OUT"; exit 1; }

# 4a. Grep count mode maps to glep -c (Cursor format)
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","output_mode":"count"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: expected cursor deny for count mode"; exit 1; }
echo "$OUT" | grep -q "glep -c -e 'x'" || { echo "FAIL: bad count command (cursor): $OUT"; exit 1; }

# 4b. Grep count mode maps to glep -c (Claude format)
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{"pattern":"x","output_mode":"count"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permissionDecision": "deny"' || { echo "FAIL: expected claude deny for count mode"; exit 1; }
echo "$OUT" | grep -q "glep -c -e 'x'" || { echo "FAIL: bad count command (claude): $OUT"; exit 1; }

# 5a. Malformed JSON: must allow (exit 0, no output, no crash)
set +e
OUT=$(echo '{not valid json' | python3 "$HOOK" 2>/dev/null)
RC=$?
set -e
[ "$RC" -eq 0 ] && [ -z "$OUT" ] || { echo "FAIL: malformed JSON should exit 0 with no output (rc=$RC out=$OUT)"; exit 1; }

# 5b. Non-dict JSON (null): must allow (exit 0, no output, no crash)
set +e
OUT=$(echo 'null' | python3 "$HOOK" 2>/dev/null)
RC=$?
set -e
[ "$RC" -eq 0 ] && [ -z "$OUT" ] || { echo "FAIL: non-dict JSON (null) should exit 0 with no output (rc=$RC out=$OUT)"; exit 1; }

# 5c. Non-dict JSON (array): must allow (exit 0, no output, no crash)
set +e
OUT=$(echo '[1,2,3]' | python3 "$HOOK" 2>/dev/null)
RC=$?
set -e
[ "$RC" -eq 0 ] && [ -z "$OUT" ] || { echo "FAIL: non-dict JSON (array) should exit 0 with no output (rc=$RC out=$OUT)"; exit 1; }

# 6. Missing pattern: allow
OUT=$(run_hook '{"tool_name":"Grep","tool_input":{},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: expected allow for missing pattern, got: $OUT"; exit 1; }

echo "hook tests: OK"
