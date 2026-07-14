<p align="center">
  <img src="assets/logo.svg" width="400" alt="glep">
</p>

<p align="center"><strong>Indexed grep + glob for AI agents.</strong></p>

Ripgrep pays the full scan cost on every query. glep pays it once: a persistent, self-healing trigram index answers warm content queries in ~1-20ms and glob listings with zero filesystem traversal, with output byte-compatible with ripgrep's and no daemon.

## Why

Coding agents call Grep and Glob dozens of times per session. On monorepo-scale projects each call costs seconds. glep replaces both with index-backed equivalents built on ripgrep's own crates (`ignore`, `grep-searcher`, `regex-syntax`), so correctness is inherited, not reimplemented.

## How it works

- A file-level trigram inverted index (the Russ Cox / csearch model) lives in `.glep/`, memory-mapped, a few percent of corpus size.
- Every query self-heals: a fast parallel mtime sweep incrementally reindexes only what changed, then answers. No watcher, no background process.
- The regex becomes a trigram plan, postings intersection yields a handful of candidate files, and ripgrep's searcher runs over just those.
- Patterns trigrams can't narrow fall back to a full parallel scan: never a wrong answer, worst case is rg-speed.

## Planned interface

```bash
glep 'fn parse_intent' src/     # content search (Grep replacement)
glep --files '**/*.py'          # glob listing (Glob replacement)
glep --json 'pattern'           # machine-readable output for agents
glep index                      # explicit (re)build; lazy on first query
glep status                     # index stats
```

Ships with a Claude Code skill and a PreToolUse hook that routes built-in Grep/Glob calls through glep automatically.

## Status

Design phase. The full spec: [docs/superpowers/specs/2026-07-14-glep-design.md](docs/superpowers/specs/2026-07-14-glep-design.md).

Planned distribution: `pip install glep` (maturin binary wheels, the ruff model), `cargo install glep`, GitHub releases.
