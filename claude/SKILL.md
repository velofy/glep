---
name: glep
description: Indexed grep + glob. Use for ANY content search or file listing in a project that has a .glep directory (or when the user mentions glep). Much faster than Grep/Glob tools on large repos; identical output format to ripgrep.
---

# glep: indexed grep + glob

glep answers searches from a persistent trigram index in .glep/ instead of
rescanning the repo. Every query self-heals (mtime sweep + incremental
reindex), so results are always fresh.

## Command mapping

| Instead of | Run via Bash |
|---|---|
| Grep pattern X | `glep -e 'X'` |
| Grep with glob filter | `glep -g '*.rs' -e 'X'` |
| Grep files_with_matches | `glep -l -e 'X'` |
| Grep case-insensitive | `glep -i -e 'X'` |
| Grep with type filter | `glep -t rust -e 'X'` |
| Grep with context N | `glep -C N -e 'X'` (or `-A N` / `-B N`) |
| Grep count mode | `glep -c -e 'X'` |
| Grep multiline | `glep -U -e 'X'` |
| Glob pattern P | `glep --files 'P'` |
| List all files | `glep --files` |

Machine-readable results: add `--json` (rg-compatible JSON events).

## Rules

- First use in a project: run `glep index` once (costs one full scan).
- Exit code 1 means no matches, not an error.
- Read-only exploration bursts: add `--ttl 5` to amortize the freshness
  sweep across consecutive queries. DROP the flag after any file edit,
  or the edit may be invisible for up to 5 seconds.
- If glep errors unexpectedly, fall back to the built-in Grep/Glob tools.
