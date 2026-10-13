# Changelog

All notable changes to glep. Dates are the tagged commit dates for versions up to 0.3.0. Full history: https://velofy.co/glep/changelog/

## Unreleased

### Performance

- Worker-pool search: one reusable searcher per worker thread, an in-order printer thread with a bounded window, zero-copy mmap verification and coalesced writes, so output stays byte-identical to `rg --sort path` at unordered parallel throughput. The freshness diff runs sharded in parallel over an unsorted sweep, and on macOS `--files` answers from a dir-mtime listing cache. Every flag below runs on this pipeline; flags whose bytes are not the file's own (`--pre`, `-z`, `--binary`, `--no-mmap`) read through the searcher's buffer inside the worker.

### Matching and output

- New flags with ripgrep semantics: `-w/--word-regexp`, `-x/--line-regexp`, `-S/--smart-case`, `-v/--invert-match`, `-m/--max-count`, `-M/--max-columns`, `-o/--only-matching`, `-n/--line-number` (no-op alias, already the default), `-N/--no-line-number`, `--heading`, and `-q/--quiet`. `-w`/`-x` and `-i`/`-S` are last-wins flag pairs, matching rg. Under `-q`, `--json` still emits the closing summary event and `--stats` still prints its block; as in rg, `-q` then searches every file so the counts are complete, and stops at the first deciding file only when no summary is printed. `-q --files-without-match` prints nothing and exits 0 when some searched file has no match.
- The Rust-regex and PCRE2 matchers are configured knob for knob like rg's: outside `-U` the line terminator is `\n`, so a pattern that must match it is an error (exit 2); `-U --crlf` keeps CRLF-aware anchors; a pattern that must match NUL is an error unless `-a`; PCRE2 gets rg's 10 MB JIT stack. `--engine auto` falls back to PCRE2 per pattern whenever the default engine rejects it, and `-P`/`--engine` are a last-wins pair.
- `-s` is `--case-sensitive` (a last-wins trio with `-i`/`-S`), as in rg; `--no-messages` is long-only. `--regex-size-limit`, `--dfa-size-limit` and `--max-filesize` take `K`/`M`/`G` suffixes. `-E auto` and `-E none` (byte-order-mark sniffing off) are accepted, plus `--no-encoding`.
- `-f` keeps empty lines (an empty pattern matches every line) and `-f -` reads patterns from stdin. Like rg, `-m 0` or an empty pattern set (without `-v`) exits 1 without searching, printing, or checking path operands.
- `-p/--pretty` is an order-aware alias for `--color always --heading --line-number`, as in rg: a later `--color`, `--no-heading` or `-N` overrides its part, an earlier one does not.
- Under `-U`, `-c` counts matches only when the pattern can really span lines; a pattern that can never match a line terminator is searched line by line and `-c` counts lines, as in rg.
- `-v` disables trigram narrowing (a file with zero occurrences matches every line under inversion); `-S` always narrows case-insensitively as a safe superset.
- `-e`/`--regexp` is repeatable and `-f`/`--file` reads patterns from a file: all patterns OR together, each arm wrapped non-capturing so anchors stay per-arm (under `-F` each arm is a literal). With any `-e`/`-f` present, positionals are paths.
- Binary files behave as under rg's default reader. The index's binary sniff covers the searcher's whole first 64 KB buffer, and summary modes squash a file once binary data shows up under quit detection: no `-c` record (not even `path:0` under `--include-zero`) and no `--files-without-match` listing, though the file still counts toward that mode's exit status. A NUL past the first 64 KB of a memory-mapped file is caught by re-reading that file through the searcher's buffer whenever the outcome is observable, so its matches stop at the NUL with rg's warning. UTF-16 files with a byte-order mark are transcoded and searched, like rg (the index never indexes them and always scans them).
- `-l`, `--files-without-match`, `-c`, `--count-matches` and `--json` are last-wins, like rg's search modes (`-r` still conflicts with `--json`: rg 15's JSON carries replacements, which grep-printer 0.2 cannot emit). `glep status` reports `skipped (binary/oversized/utf16)`.
- New `-a/--text` and `--binary` (last-wins pair): binary files are searched as raw text under `-a` and reported with a "binary file matches" notice under `--binary`. Binary-flagged files join the candidates only under these flags (and `--null-data`, a non-UTF-8 `-E`, or `--pre`); default quit detection still suppresses them, matching rg's output and exit codes.
- New `-P`/`--pcre2` (and `--engine pcre2|default|auto`): PCRE2 with JIT, configured like rg's. PCRE2-only syntax degrades the index plan to a full scan, never a wrong answer.
- New `-E`/`--encoding` (transcode before searching; non-UTF-8 encodings bypass index narrowing since trigrams index raw bytes), `--multiline-dotall`, `--line-buffered`, `--null-data` (NUL is the record separator), `--crlf`, `--no-unicode`, `--dfa-size-limit`, `--regex-size-limit`.
- New `-r/--replace` (captures resolve), `--passthru`/`--passthrough`, `--count-matches`, `--files-without-match`, `--include-zero`, `--stats` (rg's stats block with exact counters for content, `-c` and `-l` modes), `--stop-on-nonmatch` (stops reading a file at the first non-matching line after a match, per file, like rg).
- Output plumbing, all verified byte-identical: `--column`, `-b/--byte-offset`, `--vimgrep`, `--trim`, `-0/--null`/`--print0`, `--path-separator`, `-H`/`-I`, `--field-match-separator`, `--field-context-separator`, `--context-separator`, `-p/--pretty`, `--color`/`--colors`, `--hyperlink-format`/`--hostname-bin`, `--max-columns-preview`. A single operand that is not a directory drops the path prefix unless `-H`, as in rg: the operands count as typed (a missing second operand still keeps `path:` prefixes), and `--vimgrep` always prints the path (unless `-I`) and never prints headings.
- Under `--color always` (or `ansi`, or `auto` on a tty) the path of every `-l`, `-c`, `--count-matches` and `--files-without-match` record and of the `--files` listing carries rg's path color and, with `--hyperlink-format`, rg's OSC 8 link; links stay off without color, as in rg. `--hostname-bin` runs the named program for `{host}` (platform hostname on failure), and invalid `--color`, `--colors` or `--hyperlink-format` values exit 2.
- Records and separators end with the searcher's line terminator, like rg's printers: under `--crlf` the `-c`, `-l`, `--count-matches` and `--files-without-match` records and the separators between files end in CRLF, under `--null-data` in NUL. `--crlf`/`--null-data` and `--passthru`/`-A`/`-B`/`-C` are last-wins pairs. Separator flags decode escapes (`\t`, `\xNN`); `--context-separator ""` still prints a line break, the new `--no-context-separator` prints none, and `--heading` always separates files with a bare line break. `--path-separator ""` keeps the default.
- `--sort`/`--sortr` (path, modified, accessed, created, none; last-wins pair): timestamp sorts reproduce rg exactly, including ties, which keep rg's walk order. `--sort-files` is an alias.
- New `--pre`/`--pre-glob` (search a command's output per file), `-z`/`--search-zip` (gzip decoded in-stream), `--mmap`/`--no-mmap`, `-j/--threads`, `-s/--no-messages`, `--no-ignore-messages`, `--generate {man,complete-*}`, `--pcre2-version`, `--type-list`, `--debug`/`--trace`, and the no-op compat flags `--no-config` and `--block-buffered`.

### Files and the index

- Ancestor index discovery: running glep in a subdirectory finds the nearest `.glep/` upward (or `GLEP_INDEX_PATH`, which points at an index dir directly), scopes the search to the cwd subtree, and prints paths relative to the cwd. Operands outside the cwd print with `..` components.
- Path filters scope the freshness sweep: `glep pat src/` only sweeps `src/`, walking from the root so every ancestor ignore rule applies and an ignored operand never enters the index. Only full sweeps advance the `--ttl` epoch or trigger compaction.
- `!` whitelist rules in `.gitignore`/`.ignore` rescue dotfiles from the hidden filter, in searches, listings and the listing cache alike. Existing indexes rebuild once (postings format v2 to v5) to recompute `FLAG_HIDDEN` and the binary flags described above.
- `-g`/`--glob`/`--iglob` honor gitignore-style `!` negations with last-match-wins ordering; `--iglob` matches case-insensitively and, as in rg, its rules follow every `-g` rule. `--glob-case-insensitive`, `-t`/`-T`/`--type-add`/`--type-clear`, `--ignore-file` (rules match relative to the cwd, as in rg, with `--ignore-file-case-insensitive`) and `-d/--max-depth` filter candidates by path.
- A path operand that names a file is searched unconditionally, as in rg: ignore rules, the hidden filter, the index's binary flags, `-g`/`-t`/`--ignore-file` and `--max-depth` never drop it, and binary data in it is reported (`binary file matches`) instead of hiding the file. Before, an ignored, hidden or binary file named on the command line silently produced nothing.
- Several path operands behave as in rg: results group by operand in command-line order (path order within each), a file under two operands prints twice, `-d` counts from each operand, and `--sortr path` sorts across operands. When every operand is missing nothing is searched (the `--json` summary and `--stats` block still print) and glep exits 2.
- Live-scan escape hatches that never touch the index: `--no-index`, `-L`/`--follow`, `--one-file-system`, `--require-git` outside a repo, the per-source toggles `--no-ignore-dot`/`--no-ignore-vcs`/`--no-ignore-exclude`/`--no-ignore-global`/`--no-ignore-parent`, and the `-u`/`-uu`/`-uuu` ladder (`-uuu` adds `--binary`, as in rg: binary files are searched and reported with a "binary file matches" notice).
- Missing path filters print `glep: <path>: No such file or directory (os error 2)` and exit 2; valid paths still produce their matches. Implicit-scope runs whose filters empty the walked pool print rg's "No files were searched" warning and exit 2.
- Literal analysis is a recursive required-literal extractor over the parsed regex: adjacent literals fuse into longer substrings, required literals survive unexpandable spans (`[0-9][0-9][0-9]-foo` narrows on `-foo`), and literals on both sides of a wildcard are AND-required.
- The macOS listing cache is rebuilt only after a full, written sweep (never after a `--ttl` skip, a scoped sweep, or a lock-contended read-only pass), so a stale manifest is never re-certified by current directory mtimes.

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
