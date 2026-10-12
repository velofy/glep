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


def limit_pipes(ti):
    """Map head_limit/offset to shell pipes for the shown command.

    glep has no flag for capping or skipping total output lines, so the
    shown command gets pipes instead: offset K -> "| tail -n +{K+1}"
    (applied first), head_limit N -> "| head -N". Values that are not
    int-coercible, or negative, are skipped rather than emitted.
    """
    try:
        offset = int(ti["offset"]) if ti.get("offset") is not None else None
    except (TypeError, ValueError):
        offset = None
    try:
        head_limit = (
            int(ti["head_limit"]) if ti.get("head_limit") is not None else None
        )
    except (TypeError, ValueError):
        head_limit = None
    pipes = ""
    if offset is not None and offset >= 0:
        pipes += " | tail -n +" + str(offset + 1)
    if head_limit is not None and head_limit >= 0:
        pipes += " | head -" + str(head_limit)
    return pipes


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        allow()
    if not isinstance(data, dict):
        allow()
    tool = data.get("tool_name", "")
    ti = data.get("tool_input", {}) or {}
    if not isinstance(ti, dict):
        allow()
    cwd = data.get("cwd") or os.getcwd()

    if shutil.which("glep") is None:
        allow()
    if not os.path.isdir(os.path.join(cwd, ".glep")):
        allow()

    pattern_idx = None
    pipes = ""
    if tool == "Grep":
        pat = ti.get("pattern")
        if not pat:
            allow()
        output_mode = ti.get("output_mode")
        # glep always prints line numbers and has no flag to suppress
        # them; only the built-in tool can honor -n:false in content mode.
        if ti.get("-n") is False and output_mode in (None, "content"):
            allow()
        cmd = ["glep"]
        if ti.get("-i"):
            cmd.append("-i")
        if output_mode == "files_with_matches":
            cmd.append("-l")
        elif output_mode == "count":
            cmd.append("-c")
        if ti.get("multiline"):
            cmd.append("-U")
        if ti.get("glob"):
            cmd += ["-g", ti["glob"]]
        if ti.get("type"):
            cmd += ["-t", ti["type"]]
        if ti.get("-C"):
            cmd += ["-C", str(ti["-C"])]
        if ti.get("-A"):
            cmd += ["-A", str(ti["-A"])]
        if ti.get("-B"):
            cmd += ["-B", str(ti["-B"])]
        cmd += ["-e", pat]
        pattern_idx = len(cmd) - 1
        if ti.get("path"):
            cmd.append(ti["path"])
        pipes = limit_pipes(ti)
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
    shown = " ".join(parts) + pipes
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
