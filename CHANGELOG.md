# Changelog

All notable changes to glep. Dates are the tagged commit dates for versions up to 0.3.0. Full history: https://velofy.co/glep/changelog/

## Unreleased

- New flags `-a/--text` and `--binary` (last-wins pair): binary files are searched as raw text under `-a`, and reported via a "binary file matches" notice under `--binary`. Binary-flagged files join live-scan candidates only under these flags; default quit detection still suppresses them entirely, matching rg's output and exit codes.
- `!` whitelist rules in `.gitignore`/`.ignore` now rescue dotfiles from the hidden filter: `!.clang-format` un-hides the file, `!.github/` un-hides the dir and lets its children be judged on their own names. A dot-prefixed path stays hidden only when some dot component earns no whitelist verdict — matching the reference rule where the hidden filter applies only when ignore matchers return no verdict. Applies to `--files` and content search. Postings format v2 → v4 forces a one-time rebuild so existing indexes recompute `FLAG_HIDDEN`.
- Ancestor index discovery: running glep in a subdirectory finds the nearest `.glep/` upward (or `GLEP_INDEX_PATH`, which points at an index dir directly), scopes the search to the cwd subtree, and prints paths relative to the cwd — no more duplicate per-subdirectory indexes.
- Missing path filters now print `glep: <path>: No such file or directory (os error 2)` and exit 2 instead of silently exiting 1; valid paths still produce their matches.
- New `--no-index` flag: live gitignore-aware walk + scan that never opens the index (and never leaves a `.glep` behind).
- Output plumbing flags, all verified byte-identical: `--column`, `-b/--byte-offset`, `--vimgrep` (one line per match, `path:line:col:text`), `--trim` (leading whitespace), `-0/--null` (NUL path terminator, also honored by `-l` and `--files`), `--path-separator` (single byte), `--include-zero` (`-c` prints `path:0` for searched files — forces the full walked set), `--max-depth`/`--maxdepth` (operand-relative depth), `-j/--threads`, `-H/--with-filename`, `-I/--no-filename`. A single file operand now drops the path prefix unless `-H`; `-I` forces it off. Implicit-scope runs whose filters empty the walked pool print rg's "No files were searched" warning and exit 2.
- Literal analysis rewritten as a recursive required-literal extractor over the parsed regex: adjacent literals fuse into longer substrings (better trigram narrowing), required literals survive unexpandable spans (`[0-9][0-9][0-9]-foo` now narrows on `-foo` instead of falling back to a full scan), literals on both sides of a wildcard are AND-required (`abc\w+def` requires both), and alternation products are bounded per-element instead of failing wholesale.
- New `--stats` flag: prints the rg-style stats block after results (`N matches / matched lines / files contained matches / files searched / bytes printed / bytes searched / seconds`) — exact counters for content, `-c`, and `-l` modes (occurrences vs lines distinguished correctly under `-U` too).
- Path filters now scope the freshness sweep: `glep pat src/` only walks `src/` for mtime checks instead of the whole tree (ancestor ignore files still apply). The global sweep epoch is only advanced by full sweeps so `--ttl` can never suppress a needed unscoped sweep; compaction is skipped for scoped deltas (bounded by the scope, compacted on the next full sweep).
- `-e`/`--regexp` is now repeatable: multiple patterns OR together (each arm is wrapped non-capturing so anchors stay per-arm; under `-F` each arm is a literal). With any `-e` present positionals remain paths, as before.
- New flags: `-E`/`--encoding` (transcode files before searching — non-UTF-8 encodings bypass index narrowing since trigrams index raw bytes), `--multiline-dotall` (`.` spans newlines), `--line-buffered` (flush per record). `-g`/`--glob`/`--iglob` now honors gitignore-style `!` negations with last-match-wins ordering instead of plain any-match.
- New `-L`/`--follow`: follows symlinks during search. Implemented as a live-scan escape hatch (like `--no-ignore`): files reached through symlinked dirs aren't in the index, so index narrowing can't soundly narrow them — `-L` sweeps the followed tree (ignore rules still apply) and scans the result.
- New flags: `--sort`/`--sortr` (path, modified via manifest mtimes, accessed/created via stat, none), `-T`/`--type-not`, `--count-matches` (occurrences vs `-c`'s lines), `--engine` (accepts `default`/`auto`, errors on others like the reference), `--no-config` no-op.
- New flags: `--require-git` (git-derived ignore rules apply only inside a real repo; outside, `gitignored` files aren't indexed so a live scan covers them), `--ignore-file` (extra rules file, resolved relative to its dir — narrows candidates post-hoc).
- New flags: `--color=always|auto|never` (rg's default spec set; `auto` colors only on a tty), `--colors` user spec overrides, `-u`/`-uu` unrestricted aliases (fold into `--no-ignore`/`--hidden`; `-uuu` reserved until `-a` lands).
- New flags: `--passthru` (emit every line of every searched file — forces a full scan since non-matching lines must appear), `--no-unicode` (matcher unicode off).
- New flags: `-f`/`--file` (patterns from file, one per line, OR'd with `-e`/positional; each arm non-capturing so anchors stay local; `-F` arms escape individually), `--files-without-match` (names of files with no match — exit 0 when any listed).
- New flags: `--null-data` (NUL is the record separator — matcher + searcher line terminators switch to NUL, binary detection turns off, and binary-flagged files join the candidates since NULs are data there), `--dfa-size-limit`, `--regex-size-limit`, `--one-file-system` (live scan that stays on the root's filesystem; mount-point subtrees never enter the index).
- New flags: `--field-match-separator`, `--field-context-separator` (printer-level byte sequences), `--context-separator` (replaces the between-file `--`; empty disables it).
- New `-P`/`--pcre2`: PCRE2 engine (lookaround, backrefs, atomic groups...) via grep-pcre2 with JIT. The search path is now generic over `Matcher` (one monomorphized copy per engine); PCRE2-only syntax simply degrades the index plan to a full scan — never a wrong answer.

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
