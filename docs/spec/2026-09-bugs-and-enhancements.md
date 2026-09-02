# glep: bugs and enhancements, September 2026

Status: proposed. Written against `main` at v0.3.0 (`d75ff95`) with PR #9 unmerged
on `origin/fix/path-filter-normalize`.

## What glep is for

glep is indexed grep and glob for coding agents. It builds a trigram index once
under `.glep/`, answers repeated searches against that index in tens of
milliseconds, and prints output byte-compatible with ripgrep so an agent can swap
it in without learning a new format. A Claude Code and Cursor hook redirects the
built-in Grep and Glob tools to glep when an index exists.

Everything below follows from that promise: results must match rg, the index
must not lie, and the tool must be safe to call from an agent that does not know
where it is standing.

## Snapshot

| Item | Value |
|---|---|
| Version | 0.3.0 (Cargo.toml is the source of truth, pyproject is dynamic) |
| Rust | ~4 KLOC in `src/` |
| Tests | ~22 CLI tests, 24-case rg parity harness, unit tests per module |
| Open issue | #8 path filters and cwd-scoped index |
| Open PR | #9, partial fix for #8 (see below) |
| TODO/FIXME in source | none |

## PR #9 assessment

PR #9 should not be merged as is, and should not close #8.

- It fixes interior `..` in path filters (`canonicalize(root.join(p))`).
- It does not fix the cwd-scoped index, which is the item that breaks agents.
  The new warning at `src/cli.rs` fires and then falls through to
  `Index::open_or_build(&root)` with `root = current_dir()`. Running
  `glep pattern /abs/project` from `~` warns and then indexes the home directory.
- The canonicalize fallback misclassifies nonexistent filters. When
  `canonicalize(joined)` fails the code strips the uncanonicalized path against
  the canonicalized root. On Windows (`\\?\C:\` prefix) and on macOS with a
  symlinked cwd, every not-yet-existing filter warns "outside indexed tree".
- The new test `warns_when_path_filter_outside_index_root` uses a nonexistent
  child of the root, not a path outside it. It passes on macOS only because
  `/var/folders` is a symlink. On `ubuntu-latest` the path strips cleanly, no
  warning fires, and the stderr assertion fails. Needs a second tempdir.
- The warning runs before subcommand dispatch, so `glep index /other/repo`
  warns and then indexes cwd.

Required before merge: fix the test, canonicalize root once and reuse it in the
fallback, decide whether outside-tree is a warning or exit 2, and drop the
"Closes #8" claim.

## Bugs

Severity: P0 breaks the promise, P1 wrong results or agent-hostile, P2 quality.

### P0

**B1. Index root is always cwd, so a subdirectory query builds a second index.**
`src/cli.rs:188`, `src/index/mod.rs:126`. `cd src && glep foo` creates
`src/.glep/`, a full second index that diverges from the root one forever. This
is the structural cause of issue #8 and the hook's `.glep`-in-cwd check
(`claude/hooks/glep_redirect.py:31`) inherits it.
Fix: discover the root by walking up from cwd to the nearest `.glep/`, then the
nearest `.git/`, then cwd. Add `--index-root` and `GLEP_ROOT`. Path filters and
subcommands resolve against that root. The hook should use the same walk.

**B2. Files with a NUL in the first 8 KB are invisible, while rg finds them.**
`src/index/mod.rs:54-57` stamps `FLAG_SKIP_BINARY` and the candidates path
never re-adds those files (`:215`), unlike `FLAG_SKIP_TOO_LARGE` which is
re-added for a live scan (`:235-241`). UTF-16 source, `.resx`, and any file with
a BOM plus NULs are silently absent. rg transcodes UTF-16 by default.
Fix: treat binary-flagged files like too-large ones and hand them to
grep-searcher live, which already applies rg's binary and BOM rules. Add a
UTF-16LE file and a true binary to the parity corpus.

**B3. Broken pipe exits 2 with an error line.** `src/search.rs:262,267` propagate
the write error; `src/main.rs:14-17` prints `glep: Broken pipe` and exits 2.
`glep pattern | head -5` is the most common agent invocation.
Fix: treat `ErrorKind::BrokenPipe` as success and exit 0, matching rg.

### P1

**B4. Output is line-buffered through `StdoutLock`.** `src/cli.rs:281-283`,
`:170-172`, `src/search.rs:267`; `--files` uses `println!` per path
(`src/cli.rs:249-251`, `:144-146`). One `write(2)` per line, ~86k syscalls for
a kernel-tree listing. Fix: `BufWriter::new(stdout.lock())` at both call sites.

**B5. Read-only mode returns tombstoned files and floods stderr.**
`src/index/mod.rs:318-322` returns before applying `dead_ids` when another
process holds the lock. `search_one` then fails to open each deleted path and
`src/search.rs:248` prints one `No such file` line per file.
Fix: apply `dead_ids` from the manifest even in read-only mode, and suppress
ENOENT for index-listed paths.

**B6. Staleness is mtime plus size only.** `src/index/mod.rs:303-305`. A
`git checkout` that restores same-size content with a preserved mtime, or
`cp -p`, leaves stale trigrams and yields false negatives with no self-heal.
Fix: add ctime and inode to `FileEntry`, and let `glep status` report the
number of files whose ctime moved. Document `glep index` as the recovery.

**B7. Ignore semantics diverge from rg outside git repos.** `src/walk.rs:165`
sets `require_git(false)`, so `.gitignore` applies even where rg would not
apply it. The parity harness hides this by passing rg `--no-require-git`
(`tests/parity.rs:71`). Fix: match rg's default, or document the divergence
next to the `bytes_printed` note in the README and keep the harness flag.

**B8. `glep index <path>` ignores the path.** `src/cli.rs:194-197` builds at
cwd regardless of `args.paths`. With PR #9 it also warns about a filter it never
honours. Folds into B1.

**B9. `--files -e X` drops `-e`.** `src/cli.rs:183` guards the promotion with
`!args.files`, and files mode reads only `args.pattern`. Fix: error out when
`-e` is combined with `--files`, or honour it as the glob.

**B10. Hook drops `multiline` and `head_limit`.**
`claude/hooks/glep_redirect.py:35-59`. A Grep call with `multiline: true` is
rewritten without `-U`, which returns different results silently. Losing
`head_limit` means unbounded output into the agent context.
Fix: map `multiline` to `-U`; map `head_limit` to `-m` once B-E4 lands, and
until then pipe through `head -n`.

### P2

**B11. macOS bulk sweep aborts on any `.ignore` or `.rgignore`.**
`src/walk_bulk.rs:645-655`, `src/walk.rs:141-147`. A deep `.ignore` costs a
near-complete bulk traversal, then a second full walk, on every query. Fix:
teach the bulk walker to parse `.ignore` files the same way it seeds
`.gitignore`, or at least cache the "has .ignore" verdict per sweep epoch.

**B12. Windows rename over a live mmap.** `src/index/postings.rs:65-66` renames
`delta.bin` and `postings.bin` while `self.delta` or `self.main` may still map
them (`src/index/mod.rs:360`, `:110`, `:374-375`). Unverified on Windows; CI
is green but may not reach a second-generation write. Add a Windows test that
edits, queries, edits, queries.

**B13. Path filters do not narrow the sweep or the intersection.**
`apply_filters` runs after `candidates()` (`src/cli.rs:262-266`) and the
freshness sweep walks the whole tree. `glep foo src/one/` costs the same as
`glep foo`. Fix: sweep only the filter subtrees when filters are present, and
prefilter the manifest before the postings intersection.

**B14. Symlinked directory filters are rewritten.** With PR #9, `glep foo link/`
where `link -> src` returns paths under `src/`. Document, or preserve the
user-supplied prefix in output.

## Docs drift

- `site/index.html:171-172` claims `--files` answers "with no directory
  traversal". Every query runs the freshness sweep unless `--ttl` is inside its
  window. Rewrite to match `README.md:19`.
- `README.md:27` says rg ran at default parallelism. `bench/bench.sh:13` passes
  rg `--sort path`, which forces it single-threaded. Either drop `--sort` from
  the benchmark and re-measure, or label the number.
- `bench/bench.sh` does not reproduce the README table: no index-build timing,
  `--ttl 60` rather than 5, one pattern, one glob.
- `README.md:7` folds the 21 ms burst number into the 21-298 ms range and then
  restates it. The site's wording (`site/index.html:166`) is correct.
- `GLEP_NO_BULK_SWEEP` and `GLEP_TIMING` are undocumented.
- `claude/SKILL.md:31` tells agents to run `glep index` first; the index builds
  lazily. Remove the step, especially given B1.
- Neither the binary-file skip (B2) nor the ignore divergence (B7) is listed
  beside the `bytes_printed` divergence in `README.md:63`.

## Test gaps

- No binary or UTF-16 file in any corpus.
- No broken-pipe test.
- `tests/parity.rs:4-10` and `tests/json_parity.rs:19` return early when rg is
  missing and the test passes. If `cargo install ripgrep` fails in CI the
  advertised harness silently runs zero cases. Make a missing rg a hard failure
  in CI (`GLEP_REQUIRE_RG=1`).
- Parity does not cover `-g`, `-t`, path filters, `-U` with context, CRLF,
  `--max-filesize` boundaries, or multiple context groups in one file.
- One in-process read-only test; nothing exercises two CLI processes, the lock,
  or B5.
- No compaction-under-reader test (B12).
- No path-filter test for a real outside-tree path, a nonexistent path, or a
  filter that matches zero indexed files.

## Enhancements

Ordered by what an agent hits first.

1. **E1. Index root discovery** (B1). Also fixes #8 properly.
2. **E2. `-m/--max-count` and `--max-columns`.** Without them an agent
   grepping a common token receives unbounded output. Pairs with the hook's
   `head_limit`.
3. **E3. `-w/--word-regexp`, `-x/--line-regexp`, `-S/--smart-case`.** All three
   are a few lines on `RegexMatcherBuilder` (`src/search.rs:100-118`). Smart
   case is the one agents ask for most.
4. **E4. `-v/--invert-match`.** Falls back to `Plan::All`, which is already
   sound.
5. **E5. `-n/-N`, `--heading`, `-o`, `--color`.** `termcolor` is a declared but
   unused dependency (`Cargo.toml:27`). Either implement `--color` or drop it.
6. **E6. Explicit file argument.** `rg pattern file.rs` searches the file even
   if it is gitignored or binary; glep only treats it as a filter over the
   index. When a path argument is a regular file, search it live.
7. **E7. `--stats` outside `--json`.** The machinery exists
   (`src/search.rs:91-98`).
8. **E8. `--quiet` / `--no-warn`** once PR #9's stderr hint lands.
9. **E9. Warn when a filter resolves inside the tree but matches nothing.**
   Second half of issue #8's follow-up list.

## Hygiene

- No CHANGELOG across eight tags. Add `CHANGELOG.md` and make `release:`
  commits append to it; issue #8 exists partly because users on 0.1.2 had no
  upgrade notes.
- `site/index.html:217` hardcodes `v0.3.0`. Add a CI grep against
  `Cargo.toml`.
- `claude/hooks/glep_redirect.py` and `cursor/hooks/glep_redirect.py` are
  near-copies and the Cursor one has already diverged forward. Collapse to one
  module with a flavour switch and one test script.
- `release.yml:36` publishes with `if: always()`, so a partial wheel matrix
  ships a partial platform set that `--skip-existing` can never repair. Gate on
  success of every leg.
- `docs/superpowers/plans/2026-07-14-glep-v1.md` is a 2,570-line build
  scaffold. Move it out of the repo or under `docs/archive/`. Keep the design
  spec.
- The captured rg JSON payload in `src/search.rs:18-48` belongs in
  `tests/json_parity.rs` or `docs/`, not the hot-path module.
- Drop `termcolor` or use it (E5).

## Sequencing

1. Land a corrected PR #9 without the "closes #8" claim.
2. v0.4.0: E1 root discovery, B2 binary handling, B3 broken pipe, B4 buffering,
   E2 max-count, hook `multiline` and `head_limit`. This closes #8.
3. v0.4.1: B5, B6, B7 with parity corpus additions and the hard-fail rg check.
4. v0.5.0: E3 through E6, CHANGELOG, hook consolidation, docs drift.
