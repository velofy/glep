# Changelog

All notable changes to glep. Dates are the tagged commit dates for versions up to 0.3.0. Full history: https://velofy.co/glep/changelog/

## Unreleased

- Ancestor index discovery: running glep in a subdirectory finds the nearest `.glep/` upward (or `GLEP_INDEX_PATH`, which points at an index dir directly), scopes the search to the cwd subtree, and prints paths relative to the cwd — no more duplicate per-subdirectory indexes.
- Missing path filters now print `glep: <path>: No such file or directory (os error 2)` and exit 2 instead of silently exiting 1; valid paths still produce their matches.
- New `--no-index` flag: live gitignore-aware walk + scan that never opens the index (and never leaves a `.glep` behind).

## 0.3.1 (2026-09-30)

- Package metadata now points to https://velofy.co/glep/ and https://github.com/velofy/glep (homepage, documentation, repository, changelog links on PyPI and crates.io).
- The README was rewritten and the old GitHub Pages site now forwards to the new documentation.
- No code changes since 0.3.0.
- Adds this CHANGELOG file.

## 0.3.0 (2026-07-23)

Escape hatches.

- New `--hidden` flag: include dot-prefixed files and directories. The index now stores hidden files with a flag, so the flag needs no rebuild. `.git` is always excluded.
- New `--no-ignore` flag: search gitignored and `.ignore`d files with a live scan that never opens, updates, or writes the index.
- `--json` now ends with ripgrep's `summary` event.
- `-l` now conflicts with `--json`.
- `glep status` counts only binary and oversized files as skipped.
- With `--no-ignore`, `index` and `status` are treated as patterns.
- Cursor integration: `cursor/install.sh` and a hook that emits both Cursor and Claude Code output formats.

## 0.2.3 (2026-07-19)

- Path filters given as `./path` or as absolute paths are normalized to index-relative paths.
- README and site: full CLI documentation, package links, social cards, updated numbers.

## 0.2.2 (2026-07-17)

- Windows support, with a Windows CI job and a Windows wheel on PyPI.
- PyPI classifiers.

## 0.2.1 (2026-07-17)

- macOS: the freshness sweep uses `getattrlistbulk`, with a fallback to the portable walker. `GLEP_NO_BULK_SWEEP` forces the walker.
- The sweep collects results per thread instead of taking a lock per file.
- New `GLEP_TIMING` environment variable for per-stage timings on stderr.
- The bulk sweep honors all gitignore sources; a `.ignore` file triggers the walker fallback.
- The release workflow can be started manually.

## 0.2.0 (2026-07-17)

- New `-c` / `--count` mode, with ripgrep parity. The Claude Code hook maps Grep count queries to `-c`.
- New `-A` and `-B` context flags.
- New `-U` / `--multiline` mode.
- `glep status` updates the index before reporting.
- Per-file read errors during a search print a warning to stderr instead of failing.
- New wheels: macOS x86_64, Linux aarch64, Linux musl x86_64.

## 0.1.2 (2026-07-16)

- Performance: the matcher is compiled once, `-l` stops at the first match in each file, and output streams in chunks.
- Queries where nothing changed no longer write to the index.
- First published kernel-scale benchmark numbers.

## 0.1.1 (2026-07-15)

- README links and images use absolute URLs so they render on PyPI.
- The crates.io package excludes the site, docs, and assets.

## 0.1.0 (2026-07-15)

First release.

- Trigram index in `.glep/` with a memory-mapped postings file, a manifest, incremental updates through a delta segment, and full-rebuild compaction.
- Self-healing freshness sweep on every query, `--ttl`, and `--max-filesize`.
- Content search with ripgrep-format output, `-i`, `-F`, `-l`, `-g`, `-t`, `-C`, `--json`, and `-e`.
- `--files` glob listing with gitignore-style glob semantics.
- `glep index` and `glep status`.
- Claude Code skill and PreToolUse hook.
- Wheels on PyPI for macOS arm64 and Linux x86_64.
