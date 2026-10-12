#!/usr/bin/env python3
"""preToolUse hook: redirect built-in Grep/Glob to glep when indexed.

Supports Cursor (permission deny JSON) and Claude Code (hookSpecificOutput).
Allow (exit 0) unless glep is installed AND the project has a .glep index.
"""
import json
import os
import shlex
import shutil
import sys


def allow():
    sys.exit(0)


def is_cursor(data):
    return data.get("hook_event_name") == "preToolUse" or "cursor_version" in data


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


def deny(data, cmd):
    reason = (
        "This project has a glep index. "
        "Run the equivalent indexed search via Bash instead: "
        + cmd
    )
    if is_cursor(data):
        print(
            json.dumps(
                {
                    "permission": "deny",
                    "user_message": "Use glep via Bash (indexed search).",
                    "agent_message": reason,
                },
                indent=1,
            )
        )
    else:
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    }
                },
                indent=1,
            )
        )


def quote_cmd(cmd):
    pattern_idx = None
    for i, c in enumerate(cmd):
        if c == "-e" and i + 1 < len(cmd):
            pattern_idx = i + 1
            break
        if c == "--files" and i + 1 < len(cmd):
            pattern_idx = i + 1
            break
    parts = []
    for idx, c in enumerate(cmd):
        if idx == pattern_idx:
            parts.append("'" + c.replace("'", "'\"'\"'") + "'")
        else:
            parts.append(shlex.quote(c))
    return " ".join(parts)


def build_grep_cmd(ti):
    pat = ti.get("pattern")
    if not pat:
        return None
    output_mode = ti.get("output_mode")
    # glep always prints line numbers and has no flag to suppress them;
    # only the built-in tool can honor -n:false in content mode.
    if ti.get("-n") is False and output_mode in (None, "content"):
        return None
    cmd = ["glep"]
    # case_sensitive:false is a defensive alias for -i (field name is not
    # documented for every editor); an absent field changes nothing.
    if ti.get("-i") or ti.get("case_sensitive") is False:
        cmd.append("-i")
    if output_mode == "files_with_matches":
        cmd.append("-l")
    elif output_mode == "count":
        cmd.append("-c")
    if ti.get("multiline"):
        cmd.append("-U")
    grep_glob = ti.get("glob") or ti.get("include")
    if grep_glob:
        cmd += ["-g", grep_glob]
    if ti.get("type"):
        cmd += ["-t", ti["type"]]
    if ti.get("-C"):
        cmd += ["-C", str(ti["-C"])]
    if ti.get("-A"):
        cmd += ["-A", str(ti["-A"])]
    if ti.get("-B"):
        cmd += ["-B", str(ti["-B"])]
    cmd += ["-e", pat]
    if ti.get("path"):
        cmd.append(ti["path"])
    return cmd


def build_glob_cmd(ti):
    pat = ti.get("glob_pattern") or ti.get("pattern")
    if not pat:
        return None
    cmd = ["glep", "--files", pat]
    target = ti.get("target_directory") or ti.get("path")
    if target:
        cmd.append(target)
    return cmd


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

    if tool == "Grep":
        cmd = build_grep_cmd(ti)
        pipes = limit_pipes(ti) if cmd else ""
    elif tool == "Glob":
        cmd = build_glob_cmd(ti)
        pipes = ""
    else:
        allow()

    if not cmd:
        allow()

    deny(data, quote_cmd(cmd) + pipes)


if __name__ == "__main__":
    main()
