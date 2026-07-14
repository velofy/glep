# glep — Design Spec

**Date:** 2026-07-14
**Status:** Approved pending user review
**One-liner:** An indexed grep+glob CLI for AI agents. Ripgrep pays the full scan cost on every query; glep pays it once and answers subsequent queries from a persistent, self-healing trigram index.

## Purpose

Claude Code agents call Grep and Glob constantly. Both re-traverse and (for Grep) re-scan the repo on every call. On monorepo-scale projects a single search costs seconds, multiplied across dozens of calls per session. glep replaces both tools with index-backed equivalents: warm content queries in ~1–20ms, glob listings with zero filesystem I/O.

This is the same conclusion Cursor reached with their indexed search for agents (sparse n-grams, local index). glep is the no-daemon, open, CLI-shaped version of that idea, built on ripgrep's own crates so output correctness is inherited rather than reimplemented.

## Decisions (settled during brainstorming)

| Question | Decision |
|---|---|
| Audience | Claude Code / AI agents first; humans get the same CLI |
| Corpus scope | One project at a time; index lives in `.glep/` at the project root |
| Freshness | Check-on-query: mtime+size sweep per query, incremental reindex, no daemon |
| Integration | CLI + Claude Code skill + PreToolUse hook on built-in Grep/Glob |
| Stack | Rust, single static binary |
| Engine | File-level trigram inverted index (Cox/csearch model) wrapping ripgrep's crates; NOT positional (Zoekt) and NOT a RAM daemon |

## Architecture

One binary, `glep`, no background processes. Four modules with narrow interfaces:

| Module | Responsibility | Interface |
|---|---|---|
| `walk` | Parallel gitignore-aware traversal + mtime sweep, via the `ignore` crate | `sweep(root) -> Vec<FileMeta>` |
| `index` | Build/open/update/query the trigram index | `Index::open()`, `update(changes)`, `candidates(plan) -> FileSet` |
| `query` | Regex → trigram query planning (Cox's algorithm), via `regex-syntax` | `plan(regex) -> Plan::{And, Or, All}` |
| `search` | Run matches over candidate files, via `grep-searcher`/`grep-regex` | `run(pattern, files, opts) -> matches` |

Key dependencies (all BurntSushi's ripgrep internals): `ignore`, `grep-searcher`, `grep-regex`, `regex-syntax`, plus `memmap2` for index access.

## CLI surface

```
glep <pattern> [path...]     content search (Grep replacement)
glep --files [glob]          glob listing (Glob replacement) — pure index lookup
glep index                   explicit (re)build; also lazy on first query
glep status                  index stats: file count, size, segments, last update
```

Flags mirror ripgrep where they overlap: `-i`, `-F`, `-l`, `-g <glob>`, `-t <type>`, `-C <n>`, `--json`. Default human output is byte-compatible with ripgrep's format; `--json` emits machine-readable matches for agents.

## Index format

Directory `.glep/` at project root (added to `.gitignore` on creation). Two memory-mapped files, versioned headers, atomic write-temp-rename updates, advisory flock: single writer, many readers.

- **`manifest`** — file table: path, file ID, mtime, size, skip-flag (binary or over size cap). Doubles as the glob corpus: `--files` filters these paths in memory and never touches the filesystem.
- **`postings`** — trigram → delta-encoded sorted file-ID list. Trigrams are raw bytes; no case folding stored. `-i` expands case variants of each trigram at query time (≤8 lookups per trigram).

**Incremental updates are log-structured:** changed/new files append to a small delta segment; replaced/deleted file IDs enter a tombstone set; delta segments compact into the main index past a size threshold. Query = union over segments minus tombstones. Per-query freshness cost is proportional to what changed, never to repo size.

## Query paths

**Content query:**
1. Sweep: parallel mtime+size walk, diff against manifest → reindex changed files, tombstone deleted ones.
2. Plan: extract required literals from the regex → trigram plan (`AND` within a literal, `OR` across alternations).
3. Candidates: intersect/union postings → candidate file set.
4. Match: `grep-searcher` over candidates plus all skip-flagged text files (so results stay complete).
5. Fallback: if the plan degenerates to `All` (`.*`, 1–2 char patterns), scan every manifest file in parallel — no traversal cost, so still faster than cold ripgrep. Never a wrong answer; worst case is rg-speed.

**Glob query (`--files`):** match the glob against manifest paths in memory after the sweep. No I/O beyond the sweep itself.

**Binary files:** NUL-sniffed and skipped, matching ripgrep's behavior.

## Claude Code integration

- **Skill** (`~/.claude/skills/glep/SKILL.md`): triggers on code-search tasks; teaches Grep→`glep`, Glob→`glep --files`, `--json` for parsing. Same distribution pattern as curl_reap.
- **Hook**: `PreToolUse` on built-in `Grep` and `Glob`. The hook script translates tool params (pattern, path, glob, case mode, output mode) to an equivalent `glep` command line and denies with that suggestion, so the agent retries via Bash+glep. It passes through untouched when the glep binary is missing or the project has no `.glep/` — vanilla sessions are never broken.

## Error handling

Theme: never wrong, never stuck.

- Corrupt or version-mismatched index → delete and rebuild automatically; note on stderr.
- Writer lock held by another process → answer read-only from the existing index, live-scanning swept-as-changed files; skip the index write.
- Oversized (default cap 1MB, `--max-filesize` to change) or unindexable files → skip-flagged in manifest, always live-scanned during content queries.
- Not a git repo / no .gitignore → `ignore` crate degrades gracefully; glep works in any directory.
- First build on a huge repo → progress on stderr; cost ≈ one ripgrep scan.

## Testing

- **Differential parity harness (centerpiece):** randomized patterns and flags over test corpora; glep output must byte-match ripgrep output. Any divergence is a bug.
- **Freshness tests:** create/edit/delete files, query immediately; changes must be visible.
- **Unit tests:** trigram planning against Cox's published examples; postings encode/decode roundtrip; tombstone + compaction behavior; case-variant expansion.
- **Benchmarks (hyperfine):** vs `rg` and `fd` on small (~1k files), medium (~20k), and monorepo (~300k, e.g. Linux kernel) corpora; track warm-query latency and index build time.

## Success criteria

- Warm content query < 20ms on a 20k-file repo (vs ~100–300ms cold rg).
- Warm content query < 100ms on a 300k-file monorepo (vs multi-second cold rg).
- `--files` glob < 10ms regardless of repo size.
- Zero parity mismatches vs ripgrep in the differential harness.
- A Claude Code session in an indexed repo routes searches through glep without being asked (hook + skill working).

## Out of scope (v1)

- Multi-repo / machine-wide indexes (index format shouldn't preclude it, but no v1 work).
- Positional trigrams / index-only answers (Zoekt-style) — revisit only if candidate-scan latency proves insufficient.
- Any daemon or file watcher; any MCP server (house rule).
- Semantic / embedding search; ranking.
- Windows support (macOS + Linux first).
