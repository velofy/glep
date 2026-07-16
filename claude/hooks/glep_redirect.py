#!/usr/bin/env python3
"""PreToolUse hook: redirect built-in Grep/Glob to glep when indexed.

Allow (exit 0, no output) unless glep is installed AND the project has a
.glep index. Never breaks a vanilla session.
"""
import json
import os
import shlex
import shutil
import sys


def allow():
    sys.exit(0)


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        allow()
    if not isinstance(data, dict):
        allow()
    tool = data.get("tool_name", "")
    ti = data.get("tool_input", {}) or {}
    cwd = data.get("cwd") or os.getcwd()

    if shutil.which("glep") is None:
        allow()
    if not os.path.isdir(os.path.join(cwd, ".glep")):
        allow()

    pattern_idx = None
    if tool == "Grep":
        pat = ti.get("pattern")
        if not pat:
            allow()
        cmd = ["glep"]
        if ti.get("-i"):
            cmd.append("-i")
        if ti.get("output_mode") == "files_with_matches":
            cmd.append("-l")
        elif ti.get("output_mode") == "count":
            cmd.append("-c")
        if ti.get("glob"):
            cmd += ["-g", ti["glob"]]
        if ti.get("type"):
            cmd += ["-t", ti["type"]]
        for ctx_key in ("-C", "-A", "-B"):
            if ti.get(ctx_key):
                cmd += ["-C", str(ti[ctx_key])]
                break
        cmd += ["-e", pat]
        pattern_idx = len(cmd) - 1
        if ti.get("path"):
            cmd.append(ti["path"])
    elif tool == "Glob":
        pat = ti.get("pattern")
        if not pat:
            allow()
        cmd = ["glep", "--files", pat]
        pattern_idx = len(cmd) - 1
        if ti.get("path"):
            cmd.append(ti["path"])
    else:
        allow()

    # The search/glob pattern is always shown single-quoted for visual
    # clarity, regardless of whether the shell strictly requires it.
    parts = []
    for idx, c in enumerate(cmd):
        if idx == pattern_idx:
            parts.append("'" + c.replace("'", "'\"'\"'") + "'")
        else:
            parts.append(shlex.quote(c))
    shown = " ".join(parts)
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": (
                        "This project has a glep index. "
                        "Run the equivalent indexed search via Bash instead: "
                        + shown
                    ),
                }
            },
            indent=1,
        )
    )


if __name__ == "__main__":
    main()
