# Changelog

All notable changes to glep. Dates are the tagged commit dates for versions up to 0.3.0. Full history: https://velofy.co/glep/changelog/

## Unreleased

### Performance

- Worker-pool search: one reusable searcher per worker thread, an in-order printer thread with a bounded window, zero-copy mmap verification and coalesced writes, so output stays byte-identical to `rg --sort path` at unordered parallel throughput. The freshness diff runs sharded in parallel over an unsorted sweep, and on macOS `--files` answers from a dir-mtime listing cache. Every flag below runs on this pipeline; flags whose bytes are not the file's own (`--pre`, `-z`, `--binary`, `--no-mmap`) read through the searcher's buffer inside the worker.

### Matching and output

- New flags with ripgrep semantics: `-w/--word-regexp`, `-x/--line-regexp`, `-S/--smart-case`, `-v/--invert-match`, `-m/--max-count`, `-M/--max-columns`, `-o/--only-matching`, `-n/--line-number` (no-op alias, already the default), `-N/--no-line-number`, `--heading`, and `-q/--quiet`. `-w`/`-x` and `-i`/`-S` are last-wins flag pairs, matching rg. Under `-q`, `--json` still emits the closing summary event and `--stats` still prints its block; as in rg, `-q` then searches every file so the counts are complete, and stops at the first deciding file only when no summary is printed. `-q --files-without-match` prints nothing and exits 0 when some searched file has no match.
- The Rust-regex and PCRE2 matchers are configured knob for knob like rg's: outside `-U` the line terminator is `\n`, so a pattern that must match it is an error (exit 2); `-U --crlf` keeps CRLF-aware anchors; a pattern that must match NUL is an error unless `-a`; PCRE2 gets rg's 10 MB JIT stack. `--engine auto` falls back to PCRE2 per pattern whenever the default engine rejects it, and `-P`/`--engine` are a last-wins pair.
- A repeated flag is last-wins instead of a usage error, as in rg (`-n -n`, `--sort path --sort path`, `-A1 -A2`); repeatable lists such as `-e`, `-g` and `-t` keep accumulating. rg's negated and alias spellings are accepted: `--no-require-git`, `--unrestricted`, `--no-hidden`, `--ignore` and `--ignore-dot`/`-vcs`/`-exclude`/`-global`/`-parent`, `--no-ignore-files`/`--ignore-files`, `--no-text`, `--no-binary`, `--no-pcre2`, `--no-follow`, `--no-crlf`, `--no-multiline`, `--no-multiline-dotall`, `--no-fixed-strings`, `--no-invert-match`, `--no-json`, `--no-stats`, `--no-trim`, `--no-search-zip`, `--no-column`, `--no-byte-offset`, `--no-include-zero`, `--no-max-columns-preview`, `--no-line-buffered`, `--no-block-buffered`, `--no-one-file-system`, `--no-glob-case-insensitive`, `--no-ignore-file-case-insensitive`, `--no-pre`, `--unicode`, `--messages`, `--ignore-messages`, `--no-sort-files`, and the deprecated `--auto-hybrid-regex` and `--pcre2-unicode` pairs. Flags that set the same thing act where they stand, as in rg: `-uu --no-hidden` hides dotfiles again, `--no-ignore --ignore-vcs` keeps `.gitignore`, `-P --engine default` uses the default engine.
- A flag that takes a value takes the next word verbatim, as in rg, even one that starts with a dash: `-e -foo`, `-e '-> Result'`, `-g -x`, `-r -X`, `--context-separator --`. The agent hooks pass every Grep pattern as `-e '<pattern>'`, so a pattern starting with a dash used to fail as an unknown flag (exit 2).
- `-s` is `--case-sensitive` (a last-wins trio with `-i`/`-S`), as in rg; `--no-messages` is long-only. `--regex-size-limit`, `--dfa-size-limit` and `--max-filesize` take `K`/`M`/`G` suffixes. `-E auto` and `-E none` (byte-order-mark sniffing off) are accepted, plus `--no-encoding`.
- `-f` keeps empty lines (an empty pattern matches every line) and `-f -` reads patterns from stdin. Like rg, `-m 0` or an empty pattern set (without `-v`) exits 1 without searching, printing, or checking path operands.
- `-p/--pretty` is an order-aware alias for `--color always --heading --line-number`, as in rg: a later `--color`, `--no-heading` or `-N` overrides its part, an earlier one does not.
- Under `-U`, `-c` counts matches only when the pattern can really span lines; a pattern that can never match a line terminator is searched line by line and `-c` counts lines, as in rg.
- `-v` disables trigram narrowing (a file with zero occurrences matches every line under inversion); `-S` always narrows case-insensitively as a safe superset.
- `-e`/`--regexp` is repeatable and `-f`/`--file` reads patterns from a file: all patterns OR together, each arm wrapped non-capturing so anchors stay per-arm (under `-F` each arm is a literal). With any `-e`/`-f` present, positionals are paths.
- Binary files behave as under rg's default reader. The index's binary sniff covers the searcher's whole first 64 KB buffer, and summary modes squash a file once binary data shows up under quit detection: no `-c` record (not even `path:0` under `--include-zero`) and no `--files-without-match` listing, though the file still counts toward that mode's exit status. A NUL past the first 64 KB of a memory-mapped file is caught by re-reading that file through the searcher's buffer whenever the outcome is observable, so its matches stop at the NUL with rg's warning. UTF-16 files with a byte-order mark are transcoded and searched, like rg (the index never indexes them and always scans them).
- `-l`, `--files-without-match`, `-c`, `--count-matches` and `--json` are last-wins, like rg's search modes. `glep status` reports `skipped (binary/oversized/utf16)`.
- New `-a/--text` and `--binary` (last-wins pair): binary files are searched as raw text under `-a` and reported with a "binary file matches" notice under `--binary`. Binary-flagged files join the candidates only under these flags (and `--null-data`, a non-UTF-8 `-E`, or `--pre`); default quit detection still suppresses them, matching rg's output and exit codes.
- New `-P`/`--pcre2` (and `--engine pcre2|default|auto`): PCRE2 with JIT, configured like rg's. PCRE2-only syntax degrades the index plan to a full scan, never a wrong answer.
- New `-E`/`--encoding` (transcode before searching; non-UTF-8 encodings bypass index narrowing since trigrams index raw bytes), `--multiline-dotall`, `--line-buffered`, `--null-data` (NUL is the record separator), `--crlf`, `--no-unicode`, `--dfa-size-limit`, `--regex-size-limit`.
- Errors follow rg: an unreadable file or directory, a dangling link or a loop under `-L`, a corrupt `-z` stream, a failing `--pre` command (reported with its stderr as `preprocessor command failed`), a missing operand and the nothing-searched warning each make the exit code 2 while the search goes on, and with `-q` a match still exits 0. `--no-messages` hides the messages, never the exit code. `--pre` passes the cwd-relative path as the command's argument, like rg.
- A reader that closes the pipe early (`glep pat | head -1`, as the hooks' `head_limit` pipes do) ends the run quietly, as in rg: no `Broken pipe` message, and the exit status the results so far decide (0 after a match). Before, glep printed `glep: Broken pipe (os error 32)` and exited 2, and a search whose first file took long could hang for good: pool workers waiting on the printer's window were never released once the printer stopped.
- New `-r/--replace` (captures resolve), `--passthru`/`--passthrough`, `--count-matches`, `--files-without-match`, `--include-zero`, `--stats` (rg's stats block with exact counters for content, `-c` and `-l` modes), `--stop-on-nonmatch` (stops reading a file at the first non-matching line after a match, per file, like rg).
- The printers are grep-printer 0.3, rg 15's: `--colors 'highlight:...'` styles the non-matching text of a matching line (it was rejected as an unknown output type, exit 2), `-r` combines with `--json`, whose submatches carry the replacement, and under `--crlf` a rewritten line keeps its own terminator (a bare LF used to come out as CRLF).
- Output plumbing, all verified byte-identical: `--column`, `-b/--byte-offset`, `--vimgrep`, `--trim`, `-0/--null`/`--print0`, `--path-separator`, `-H`/`-I`, `--field-match-separator`, `--field-context-separator`, `--context-separator`, `-p/--pretty`, `--color`/`--colors`, `--hyperlink-format`/`--hostname-bin`, `--max-columns-preview`. A single operand that is not a directory drops the path prefix unless `-H`, as in rg: the operands count as typed (a missing second operand still keeps `path:` prefixes), and `--vimgrep` always prints the path (unless `-I`) and never prints headings.
- `--path-separator` rewrites every separator byte of a printed path, as rg's printers do, so a `-l`, `-c`, `--count-matches`, `--files-without-match` or `--files` record for an absolute operand keeps one leading separator (`%abs%x`, not `/%abs%x`; on Windows `C:/x`, not `C:/\/x`).
- Under `--color always` (or `ansi`, or `auto` on a tty) the path of every `-l`, `-c`, `--count-matches` and `--files-without-match` record and of the `--files` listing carries rg's path color and, with `--hyperlink-format`, rg's OSC 8 link; links stay off without color, as in rg. `--hostname-bin` runs the named program for `{host}` (platform hostname on failure), and invalid `--color`, `--colors` or `--hyperlink-format` values exit 2.
- Records and separators end with the searcher's line terminator, like rg's printers: under `--crlf` the `-c`, `-l`, `--count-matches` and `--files-without-match` records and the separators between files end in CRLF, under `--null-data` in NUL. `--crlf`/`--null-data` and `--passthru`/`-A`/`-B`/`-C` are last-wins pairs. Separator flags decode escapes (`\t`, `\xNN`); `--context-separator ""` still prints a line break, the new `--no-context-separator` prints none, and `--heading` always separates files with a bare line break. `--path-separator ""` keeps the default.
- `--sort`/`--sortr` (path, modified, accessed, created, none; last-wins pair): timestamp sorts reproduce rg exactly, including ties, which keep rg's walk order. `--sort-files` is an alias.
- New `--pre`/`--pre-glob` (search a command's output per file), `-z`/`--search-zip` (gzip decoded in-stream), `--mmap`/`--no-mmap`, `-j/--threads`, `-s/--no-messages`, `--no-ignore-messages`, `--generate {man,complete-*}`, `--pcre2-version`, `--type-list`, `--debug`/`--trace`, and the no-op compat flags `--no-config` and `--block-buffered`.

### Files and the index

- Ancestor index discovery: running glep in a subdirectory finds the nearest `.glep/` upward (or `GLEP_INDEX_PATH`, which points at an index dir directly), scopes the search to the cwd subtree, and prints paths relative to the cwd. Operands outside the cwd print with `..` components.
- Path filters scope the freshness sweep: `glep pat src/` only sweeps `src/`, walking from the root so every ancestor ignore rule applies and an ignored operand never enters the index. Only full sweeps advance the `--ttl` epoch or trigger compaction.
- `!` whitelist rules in `.gitignore`/`.ignore` rescue dotfiles from the hidden filter, in searches, listings and the listing cache alike. Existing indexes rebuild once (postings format v2 to v5) to recompute `FLAG_HIDDEN` and the binary flags described above.
- `-g`/`--glob`/`--iglob` honor gitignore-style `!` negations with last-match-wins ordering; `--iglob` matches case-insensitively and, as in rg, its rules follow every `-g` rule. `--glob-case-insensitive`, `-t`/`-T`/`--type-add`/`--type-clear`, `--ignore-file` (rules match relative to the cwd, as in rg, with `--ignore-file-case-insensitive`) and `-d/--max-depth` filter candidates by path.
- These filters follow rg's walker level by level below each operand: `-g '!dir'` (and `!dir/`, `!a/b`, `!**/dir`) prunes the whole directory, globs match the path relative to the cwd (so `-g 'deep/**'` works from a subdirectory), a `-g` or `-t` whitelist rescues a hidden file, a hidden directory operand is searched like any other, and an operand itself is never filtered. Type flags apply in command-line order (`-T md -t md` selects md). A missing `--ignore-file` is a warning, not an abort, the tree's own whitelist rules beat `--ignore-file` rules, and among `--ignore-file` rules the last file with an opinion decides. The `--files` glob matches the cwd-relative path too.
- A path operand that names a file is searched unconditionally, as in rg: ignore rules, the hidden filter, the index's binary flags, `-g`/`-t`/`--ignore-file` and `--max-depth` never drop it, and binary data in it is reported (`binary file matches`) instead of hiding the file. Before, an ignored, hidden or binary file named on the command line silently produced nothing.
- Several path operands behave as in rg: results group by operand in command-line order (path order within each), a file under two operands prints twice, `-d` counts from each operand, and `--sortr path` sorts across operands. When every operand is missing nothing is searched (the `--json` summary and `--stats` block still print) and glep exits 2.
- Live-scan escape hatches that never touch the index: `--no-index`, `-L`/`--follow`, `--one-file-system`, `--require-git` outside a repo, the per-source toggles `--no-ignore-dot`/`--no-ignore-vcs`/`--no-ignore-exclude`/`--no-ignore-global`/`--no-ignore-parent`, and the `-u`/`-uu`/`-uuu` ladder (`-uuu` adds `--binary`, as in rg: binary files are searched and reported with a "binary file matches" notice). Each walks every operand on its own with rg's walker configuration, so `--no-ignore-parent` drops the ignore files above a directory operand, as in rg.
- A directory operand the index holds no file under is walked live with rg's rules, as rg walks any operand without ever filtering the operand itself: an ignored directory (`glep pat build`, or running inside one), a symlink to a directory, and a spelling that differs from the one on disk on a case-insensitive filesystem (`glep pat SRC` prints `SRC/...` like rg). Each used to print nothing and exit 1. Operands outside the index root (`../../other` from a subdirectory, or an absolute path elsewhere) are walked the same way.
- rg's nothing-searched warning (exit 2) also fires from a subdirectory, where the cwd is the implicit scope, and the check stops at the first file that survives the filters instead of building the whole file list.
- Missing path filters print `glep: <path>: No such file or directory (os error 2)` and exit 2; valid paths still produce their matches. Implicit-scope runs whose filters empty the walked pool print rg's "No files were searched" warning and exit 2.
- Literal analysis is a recursive required-literal extractor over the parsed regex: adjacent literals fuse into longer substrings, required literals survive unexpandable spans (`[0-9][0-9][0-9]-foo` narrows on `-foo`), and literals on both sides of a wildcard are AND-required.
- Case-insensitive narrowing follows Unicode case folding, as rg's `-i` and `-S` match: ASCII `k` and `s` also match KELVIN SIGN (U+212A) and LONG S (U+017F), which no ASCII case variant of a trigram covers, so trigrams holding them no longer narrow (`-i kfree` missed a line spelled with the Kelvin sign, as it did in 0.3). The literal's other ASCII trigrams still narrow, also next to non-ASCII text, where any non-ASCII literal used to force a full scan; `--no-unicode` keeps ASCII-only folding.
- On macOS the bulk sweep and the listing cache apply `.ignore` files above the root themselves (ahead of every `.gitignore`, as in rg), and the cache's fingerprint covers them. Before, any such file, even an empty `~/.ignore`, sent every query to the portable walker and bypassed the cache, which was then rewritten on every `--files` run: searches ran about 2x and listings about 8x slower.
- Parallel first queries on a tree with no index yet all answer: while one glep builds the index under its lock, the others search with a live scan (what `--no-index` searches) instead of failing with `another glep holds the index lock` (exit 2). A torn index read while another glep is writing it is no longer rebuilt by the reader. `glep index` and `glep status` still report the busy lock.
- The macOS listing cache serves root-wide listings only, since its pruned walk always covers the whole tree: a listing scoped to a path operand or to a subdirectory cwd goes through the index's scoped sweep, which walks just that subtree, and a scoped listing (or `--files-without-match` universe) clones only the files under its operands. On the kernel tree `--files` in `rust/` went from 64 ms to 14 ms and `--files '**/*.c' drivers/net` (the Glob hook's form) from 63 ms to 25 ms.
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
