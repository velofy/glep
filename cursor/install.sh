#!/usr/bin/env bash
# Install glep redirect hook for Cursor (user scope).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
mkdir -p ~/.cursor/hooks
cp "$HERE/hooks/glep_redirect.py" ~/.cursor/hooks/glep_redirect.py
chmod +x ~/.cursor/hooks/glep_redirect.py

HOOKS_JSON=~/.cursor/hooks.json
if [[ -f "$HOOKS_JSON" ]]; then
  echo "Merge this into $HOOKS_JSON (hooks.preToolUse):"
else
  echo "Creating $HOOKS_JSON"
  cat >"$HOOKS_JSON" <<'EOF'
{
  "version": 1,
  "hooks": {
    "preToolUse": [
      {
        "command": "./hooks/glep_redirect.py",
        "matcher": "Grep|Glob"
      }
    ]
  }
}
EOF
  echo "Installed $HOOKS_JSON"
  exit 0
fi

cat <<EOF

{
  "version": 1,
  "hooks": {
    "preToolUse": [
      {
        "command": "./hooks/glep_redirect.py",
        "matcher": "Grep|Glob"
      }
    ]
  }
}

EOF
