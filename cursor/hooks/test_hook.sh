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

# 7. multiline maps to glep -U (Cursor format)
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"a\\nb","multiline":true},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: expected cursor deny for multiline"; exit 1; }
echo "$OUT" | grep -q "glep -U -e" || { echo "FAIL: multiline should emit -U: $OUT"; exit 1; }

# 8. head_limit appends "| head -N" to the shown command
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","head_limit":50},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: expected cursor deny for head_limit"; exit 1; }
echo "$OUT" | grep -qF "| head -50" || { echo "FAIL: head_limit should append a head pipe: $OUT"; exit 1; }

# 9. offset + head_limit: tail pipe first, then head
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","head_limit":50,"offset":10},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -qF "| tail -n +11 | head -50" || { echo "FAIL: bad offset+head_limit pipes: $OUT"; exit 1; }

# 10. -n:false in content mode allows (glep cannot omit line numbers)
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","-n":false},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: -n:false content mode should allow, got: $OUT"; exit 1; }
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","-n":false,"output_mode":"content"},"cwd":"'"$TMPN"'"}')
[ -z "$OUT" ] || { echo "FAIL: -n:false explicit content mode should allow, got: $OUT"; exit 1; }

# 11. -n:false is meaningless in count mode: still denied
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","-n":false,"output_mode":"count"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: -n:false count mode should still deny"; exit 1; }
echo "$OUT" | grep -q "glep -c -e 'x'" || { echo "FAIL: bad count command with -n:false: $OUT"; exit 1; }

# 12. case_sensitive:false maps to -i
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","case_sensitive":false},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: expected deny for case_sensitive:false"; exit 1; }
echo "$OUT" | grep -q "glep -i -e 'x'" || { echo "FAIL: case_sensitive:false should emit -i: $OUT"; exit 1; }

# 13. "include" is accepted as an alias for "glob"
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","include":"*.py"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q "glep -g '\*.py' -e 'x'" || { echo "FAIL: include should map to -g: $OUT"; exit 1; }

# 14. Malformed head_limit: still denied, no head pipe, no crash
OUT=$(run_hook '{"hook_event_name":"preToolUse","tool_name":"Grep","tool_input":{"pattern":"x","head_limit":"abc"},"cwd":"'"$TMPN"'"}')
echo "$OUT" | grep -q '"permission": "deny"' || { echo "FAIL: malformed head_limit should still deny"; exit 1; }
if echo "$OUT" | grep -qF "| head"; then
  echo "FAIL: malformed head_limit should not emit a head pipe: $OUT"; exit 1
fi

echo "hook tests: OK"
