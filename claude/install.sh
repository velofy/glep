#!/usr/bin/env bash
# Install the glep skill + hook for Claude Code (user scope).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
mkdir -p ~/.claude/skills/glep ~/.claude/hooks
cp "$HERE/SKILL.md" ~/.claude/skills/glep/SKILL.md
cp "$HERE/hooks/glep_redirect.py" ~/.claude/hooks/glep_redirect.py
chmod +x ~/.claude/hooks/glep_redirect.py
cat <<'EOF'
Installed:
  ~/.claude/skills/glep/SKILL.md
  ~/.claude/hooks/glep_redirect.py

Add this to ~/.claude/settings.json under "hooks" (merge with existing):

{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Grep|Glob",
        "hooks": [
          { "type": "command", "command": "python3 ~/.claude/hooks/glep_redirect.py" }
        ]
      }
    ]
  }
}
EOF
