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

    if tool == "Grep":
        pat = ti.get("pattern")
        if not pat or ti.get("output_mode") == "count":
            allow()
        cmd = ["glep"]
        if ti.get("-i"):
            cmd.append("-i")
        if ti.get("output_mode") == "files_with_matches":
            cmd.append("-l")
        if ti.get("glob"):
            cmd += ["-g", ti["glob"]]
        if ti.get("type"):
            cmd += ["-t", ti["type"]]
        for ctx_key in ("-C", "-A", "-B"):
            if ti.get(ctx_key):
                cmd += ["-C", str(ti[ctx_key])]
                break
        cmd += ["-e", pat]
        if ti.get("path"):
            cmd.append(ti["path"])
    elif tool == "Glob":
        pat = ti.get("pattern")
        if not pat:
            allow()
        cmd = ["glep", "--files", pat]
        if ti.get("path"):
            cmd.append(ti["path"])
    else:
        allow()

    shown = " ".join(shlex.quote(c) for c in cmd)
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
