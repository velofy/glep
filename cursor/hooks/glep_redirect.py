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
    cwd = data.get("cwd") or os.getcwd()

    if shutil.which("glep") is None:
        allow()
    if not os.path.isdir(os.path.join(cwd, ".glep")):
        allow()

    if tool == "Grep":
        cmd = build_grep_cmd(ti)
    elif tool == "Glob":
        cmd = build_glob_cmd(ti)
    else:
        allow()

    if not cmd:
        allow()

    deny(data, quote_cmd(cmd))


if __name__ == "__main__":
    main()
