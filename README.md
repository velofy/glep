<p align="center">
  <a href="https://velofy.co/glep/"><picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/velofy/glep/main/assets/tile-dark.svg">
    <img alt="glep" src="https://raw.githubusercontent.com/velofy/glep/main/assets/tile-light.svg" width="360">
  </picture></a>
</p>

<p align="center"><strong>Indexed grep + glob for AI coding agents.</strong></p>

<p align="center">
  <a href="https://pypi.org/project/glep/"><img alt="PyPI" src="https://img.shields.io/pypi/v/glep?label=pypi&color=12130f"></a>
  <a href="https://crates.io/crates/glep"><img alt="crates.io" src="https://img.shields.io/crates/v/glep?label=crates.io&color=12130f"></a>
  <a href="https://github.com/velofy/glep/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/velofy/glep/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/velofy/glep/blob/main/LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-12130f"></a>
</p>

<p align="center">
  <a href="https://velofy.co/glep/">Documentation</a> ·
  <a href="https://velofy.co/glep/installation/">Installation</a> ·
  <a href="https://github.com/velofy/glep/blob/main/CHANGELOG.md">Changelog</a> ·
  <a href="https://github.com/velofy/glep/releases">Releases</a>
</p>

ripgrep pays the full scan cost on every query. glep pays it once: a persistent, self-healing trigram index answers warm queries in 20 to 234 ms on a Linux-kernel-sized tree where ripgrep's full scan takes 1.3 s at best, with text output byte-compatible with ripgrep's, enforced in CI by a differential harness that runs about 2,000 rg-vs-glep comparisons (some 880 distinct command lines), on both the single-thread and the worker-pool search paths. No daemon.

## Install

```bash
pip install glep          # binary wheel, no Rust toolchain needed
# or
cargo install glep
```

`pipx install glep` and `uv tool install glep` also work. Prebuilt binaries for macOS (Apple silicon and Intel), Linux (x86_64 and aarch64, plus a static musl build) and Windows are attached to each [GitHub release](https://github.com/velofy/glep/releases/latest) with a `SHA256SUMS` file: unpack one and put `glep` on your PATH.

From 2.0.0 on, PyPI and crates.io carry the same versions; 0.3.0 and 0.3.1 were published to PyPI only, so crates.io goes from 0.2.3 to 2.0.0. See [Installation](https://velofy.co/glep/installation/) for platforms and other routes.

## Example

```bash
cd your-project
glep index                      # one-time build; lazy on first query anyway
glep 'fn parse_intent' src/     # content search (Grep replacement)
glep --files '**/*.py'          # glob listing (Glob replacement)
```

## Features

- **Trigram index** in `.glep/`, memory-mapped, about 10% of corpus size on the kernel tree.
- **Self-healing:** every query runs a parallel mtime sweep and reindexes only what changed. No watcher and no daemon: the one background process is the first index build of a large tree, which runs once and exits.
- **Sound fallback:** patterns the index cannot narrow fall back to a full parallel scan. Never a wrong answer; worst case is ripgrep speed.
- **ripgrep-compatible:** built on ripgrep's crates (`ignore`, `grep-searcher`, `regex-syntax`). Text output is byte-compatible with `rg`; `--json` emits rg's event stream including the closing summary event.
- **Familiar flags:** ripgrep's own spellings, including `-i -S -F -w -x -v -l -c -o -q -U -e -f -g -t -T -C -A -B -m -M -n -N -a -z -P -E -L -u -0 -j -H -I -b` and `--json --stats --heading --column --vimgrep --trim --sort --sortr --count-matches --files-without-match --passthru --null-data --hidden --no-ignore --no-ignore-vcs --ignore-file --type-add --color --hyperlink-format`. Flags that change which files exist for a query (`--no-ignore`, `-L`, `--one-file-system` and friends) run as live scans that never touch the index.
- **Agent integrations:** a Claude Code skill and PreToolUse hook (`claude/install.sh`) and a Cursor hook (`cursor/install.sh`) that route built-in Grep/Glob calls through glep.

## Interface

```bash
glep 'fn parse_intent' src/     # content search (Grep replacement)
glep --files '**/*.py'          # glob listing (Glob replacement)
glep --json 'pattern'           # machine-readable output (includes rg's summary event)
glep -c 'pattern'               # per-file match counts (rg -c)
glep -l -i -F -U ...            # files-with-matches, case-insensitive, fixed, multiline
glep -w -x -S -v 'pattern'      # word, whole-line, smart-case, invert match
glep -o -m 5 -M 120 -N ...      # only-matching, max count, max columns, no line numbers
glep --heading -q 'pattern'     # grouped headings, or quiet exit-code-only
glep -A 2 -B 1 'pattern'        # context, or -C n for both sides
glep -g '*.rs' -t rust ...      # glob and type filters
glep --hidden 'TODO'            # include dotfiles (.git is always excluded)
glep --no-ignore ...            # search ignored files too (live scan, index untouched)
glep --no-index ...             # live walk + scan; never opens or writes the index
glep --ttl 5 ...                # skip the freshness sweep within a read burst
glep --max-filesize 2000000 ... # raise the 1MB index cap
glep index                      # explicit (re)build; lazy on first query
glep status                     # index stats
```

`--no-ignore` always pays a full scan: it bypasses the index entirely, so gitignored trees never enter it. It costs the same as `rg --no-ignore`, every time.

The index lives in `.glep/`. glep finds it automatically: the nearest ancestor directory containing `.glep/` is the search root (run from a subdirectory and the search is scoped to it, with output paths relative to your cwd, the same convention as `rg`). `GLEP_INDEX_PATH` points directly at an index directory when you need an explicit override. With no index anywhere, glep builds one in the current directory. On a tree holding more than 64 MiB the first query does not wait for that build: it answers with a full scan, at rg's cost, and then builds the index in a detached low-priority process, so later queries use it; a smaller tree builds it inline. A query no trigram can narrow (a pattern shorter than three bytes, `.*`, `-v`, `--passthru`, `-c --include-zero`) gets nothing from an index, so it builds none: it searches the tree like `--no-index`, and the first query an index can narrow builds it. `GLEP_SYNC_BUILD=1` always builds inline, and `GLEP_BG_BUILD_MIN_BYTES` moves the threshold (`K`, `M` and `G` suffixes accepted). On Windows the build is always inline.

Binary files are handled the way rg's default reader handles them (rg 15.0's, see below), and UTF-16 files with a byte-order mark are transcoded and searched, as in rg.

Known differences from rg:

- Paths under an operand print relative to the cwd: `glep pat ./src` prints `src/x` where rg prints `./src/x` (likewise for `..` operands), so `bytes_printed` in the JSON summary can differ too. A file operand prints the same way, with native separators: on Windows a typed `src/main.rs` prints as `src\main.rs`, where rg keeps the spelling typed.
- `--files` takes a glob in the pattern slot, where rg takes paths. Like `-g`, it matches the path relative to the cwd.
- A positive `-g`/`--iglob` glob does not bring back files or directories that ignore rules exclude. In rg, glob overrides outrank `.gitignore`, `.ignore` and `.rgignore`, so `rg -g '*.log'` also searches gitignored logs and `rg -g '*'` descends into every ignored directory; glep filters the indexed file set, which never holds ignored files. Add `--no-ignore` to search ignored files too. A glob or `-t` type does rescue a hidden file, as in rg.
- `.git` and `.glep` are never listed or searched, even under `-uu`.
- `.gitignore` rules apply even outside a git repository (like `rg --no-require-git`); pass `--require-git` for rg's default.
- `--max-filesize` caps what the index stores (larger files are still searched) instead of skipping files.
- glep never searches stdin (it reads stdin only for patterns, under `-f -`), so a search with no path argument always searches the tree.
- The `searches` and `bytes searched` counters of `--stats` and the `--json` summary count only the files glep actually opened, after index narrowing.
- A binary file is read as rg 15.0 reads it: in buffers of 64 KiB, each filled whole before it is searched, and a NUL ends the search before the buffer holding it is searched. rg 15.1 and later search after any read that ends a line, and their byte-order-mark check hands a file's first three bytes over in a read of their own, so on a binary file whose first line is one or two bytes long they print that line, and with a NUL past the first 64 KiB they can stop at another line than rg 15.0 and glep do.
- Every file is read with a fresh 64 KiB buffer. rg keeps one buffer per search thread, and a line of 64 KiB or more grows it for every file that thread searches afterwards, so under `--sort path` (one thread, in path order) a file with a NUL past its first 64 KiB that sorts after such a line can stop earlier in rg, or print nothing. rg's output for that file then depends on which files it searched before it, and without `--sort` on which thread searched them.

## When to use it

Use glep for:

- Agent sessions firing dozens of searches over one repo (the bundled hooks reroute Grep/Glob).
- Monorepos where rg takes 100 ms or more per query.
- Repeated glob listings: `glep --files` reads the manifest, no re-walk past the freshness sweep.
- Read-heavy bursts with `--ttl 5` to amortize the freshness sweep.
- Correctness-critical work: self-healing index, sound full-scan fallback.

Stick with rg / fd for:

- One-off searches in a tree you will never search again.
- Small repos where rg already answers in under about 50 ms.
- Ephemeral CI runners where the index never persists between runs.
- rg features glep lacks: searching stdin, and `-z` for compressed formats other than gzip.
- Corpora dominated by binaries or files over the 1 MB cap (live-scanned anyway).

## Numbers

Linux kernel 6.12 source tree (a tarball extract, not a git checkout): 86,604 files, 86,218 of them searched, 1.6 GB. Apple M3 Pro, macOS, glep 0.3.1 (the release before the 2.0.0 performance rewrite), rg 15.0 and fd 10.4 at their default parallelism, hyperfine means of 5 to 10 runs after warmup, warm filesystem cache, with other work running on the machine. glep's numbers did not move between sessions. rg's full scan did, from 1.3 s on its best run to 3.5 s under load, so the rg column shows the observed spread and every ratio is a lower bound, rounded down: indexed rows are compared against rg's fastest full scan (1.3 s), full-scan rows against the row's own fastest rg run.

| Scenario | glep | glep --ttl 60 | ripgrep | glep vs rg (lower bound) |
|---|---|---|---|---|
| Rare pattern (46 lines in 11 files) | 172 ms | 21 ms | 1.3 to 3.5 s | 7x / 61x |
| Rare pattern with `-l`, `-i`, `-U` or `-C 2` | 171 to 177 ms | 20 to 27 ms | 2.9 to 3.7 s under load | 7x / 48x |
| Common pattern (`EXPORT_SYMBOL_GPL`, 20k lines in 3.5k files) | 234 ms | 81 ms | 2.6 to 3.7 s under load | 5x / 16x |
| Very common pattern (`static`, 831k lines in 40k files) | 1.69 s | 1.37 s | 2.5 to 3.4 s | 1.5x / 1.8x |
| Single char `q` (no trigram, full scan) | 3.36 s | 3.13 s | 2.8 to 3.3 s | 0.8x / 0.8x |
| List all .c files (`--files`, 34,657 files) | 195 ms | 45 ms | rg --files 71 ms, fd 74 ms | 0.3x / 1.5x |

Index build (one-time): 28 s. Index size: 154 MB, about 10% of the corpus. Default glep pays the self-healing freshness sweep on each query: on macOS one bulk attribute call per directory, elsewhere a stat per file. `--ttl` skips the sweep inside a read burst, which is the 20 to 81 ms column. Patterns shorter than three bytes, or otherwise trigram-free like `.*`, cannot be narrowed by any trigram index, so glep falls back to a full parallel scan and runs at ripgrep speed (the `q` row). In 0.3.1 listings read the manifest after the sweep, so `--files` only beats `rg --files` and `fd` inside a `--ttl` window. On a small 86-file Python repository both tools answer in single-digit milliseconds (glep 4.5 ms, rg 7.0 ms; `--files` 3.4 ms against `rg --files` 4.9 ms and `fd` 7.5 ms), so the index pays off at scale, not on small trees.

2.0.0 ships the performance rewrite: worker-pool search with one reusable searcher per thread, a parallel freshness diff and, on macOS, a dir-mtime listing cache. Measured as a branch in the same sessions on the same corpus, before it merged (it then memory-mapped every file it searched; 2.0.0 reads files up to 1 MiB into a reused buffer instead): rare pattern 97 ms (24 ms with `--ttl`), common pattern 152 ms (76 ms), `static` 1.50 s (1.28 s), `--files` 61 ms with or without `--ttl`, ahead of `rg --files` (71 ms) and `fd` (74 ms). Later 2.0.0 changes were measured in sessions of their own, so they are not directly comparable with the table: on macOS a root-wide `--files` listing answered from the listing cache took 31.7 ms against rg's 72.1 ms, and a query no trigram can narrow, over the whole tree without `--ttl`, skips the index and its freshness sweep and searches the file set that cache lists. Details: [Benchmarks](https://velofy.co/glep/benchmarks/) and the [CHANGELOG](https://github.com/velofy/glep/blob/main/CHANGELOG.md).

## Documentation

- [Overview](https://velofy.co/glep/)
- [Installation](https://velofy.co/glep/installation/) and [Quickstart](https://velofy.co/glep/quickstart/)
- [Indexing and freshness](https://velofy.co/glep/indexing/)
- [Searching file contents](https://velofy.co/glep/content-search/) and [Listing files by name](https://velofy.co/glep/file-search/)
- [Claude Code](https://velofy.co/glep/claude-code/) and [Cursor](https://velofy.co/glep/cursor/) integrations
- [CLI reference](https://velofy.co/glep/cli-reference/) and [Output formats](https://velofy.co/glep/output-formats/)
- [Changelog](https://velofy.co/glep/changelog/)

Design spec: [docs/superpowers/specs/2026-07-14-glep-design.md](https://github.com/velofy/glep/blob/main/docs/superpowers/specs/2026-07-14-glep-design.md).

## Contributing

Issues and pull requests are welcome at https://github.com/velofy/glep. CI runs on Linux, macOS, and Windows:

```bash
cargo test --all                                  # parity tests need rg on PATH (-P cases: cargo install ripgrep --features pcre2)
cargo build
PATH="$PWD/target/debug:$PATH" claude/hooks/test_hook.sh
PATH="$PWD/target/debug:$PATH" cursor/hooks/test_hook.sh
```

Benchmarks: `bench/bench.sh /path/to/big/repo 'pattern' ['*.rs']` needs hyperfine and rg on PATH; fd is optional. For one pattern it runs the plain, `-l`, `-i`, `-U` and `-C 2` rows of the Numbers table plus the listing row.

CI also rejects em dash and en dash characters anywhere in the repository.

## License

MIT. See [LICENSE](https://github.com/velofy/glep/blob/main/LICENSE).
