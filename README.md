<p align="center">
  <a href="https://velofy.co/glep/"><picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/velofy/glep/main/assets/tile-dark.svg">
    <img alt="glep" src="https://raw.githubusercontent.com/velofy/glep/main/assets/tile-light.svg" width="360">
  </picture></a>
</p>

**Indexed grep + glob for AI coding agents.**

[![PyPI](https://img.shields.io/pypi/v/glep)](https://pypi.org/project/glep/)
[![crates.io](https://img.shields.io/crates/v/glep)](https://crates.io/crates/glep)
[![CI](https://github.com/velofy/glep/actions/workflows/ci.yml/badge.svg)](https://github.com/velofy/glep/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](https://github.com/velofy/glep/blob/main/LICENSE)

Documentation: **https://velofy.co/glep/**

ripgrep pays the full scan cost on every query. glep pays it once: a persistent, self-healing trigram index answers warm queries in 21 to 298 ms on a Linux-kernel-sized tree where ripgrep takes 1.4 s, with text output byte-compatible with ripgrep's, enforced by a 58-case differential harness in CI. No daemon.

## Install

```bash
pip install glep          # binary wheel, no Rust toolchain needed
# or
cargo install glep
```

`pipx install glep` and `uv tool install glep` also work. crates.io currently has 0.2.3; PyPI has 0.3.0. See [Installation](https://velofy.co/glep/installation/) for platforms and other routes.

## Example

```bash
cd your-project
glep index                      # one-time build; lazy on first query anyway
glep 'fn parse_intent' src/     # content search (Grep replacement)
glep --files '**/*.py'          # glob listing (Glob replacement)
```

## Features

- **Trigram index** in `.glep/`, memory-mapped, about 10% of corpus size on the kernel tree.
- **Self-healing:** every query runs a parallel mtime sweep and reindexes only what changed. No watcher, no background process.
- **Sound fallback:** patterns the index cannot narrow fall back to a full parallel scan. Never a wrong answer; worst case is ripgrep speed.
- **ripgrep-compatible:** built on ripgrep's crates (`ignore`, `grep-searcher`, `regex-syntax`). Text output is byte-compatible with `rg`; `--json` emits rg's event stream including the closing summary event.
- **Familiar flags:** `-i -S -F -w -x -v -l -c -o -U -e -g -t -C -A -B -m -M -n -N -q --json --heading --hidden --no-ignore`.
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
glep --ttl 5 ...                # skip the freshness sweep within a read burst
glep --max-filesize 2000000 ... # raise the 1MB index cap
glep index                      # explicit (re)build; lazy on first query
glep status                     # index stats
```

`--no-ignore` always pays a full scan: it bypasses the index entirely, so gitignored trees never enter it. It costs the same as `rg --no-ignore`, every time.

With an explicit path argument, `bytes_printed` in the JSON summary can differ from rg's (rg prints `./`-prefixed paths; glep prints them bare).

Other known differences from rg: files with a NUL byte in the first 8 KB are treated as binary and never searched (UTF-16 files with a BOM are searched via live scan, matching rg), and `.gitignore` rules apply even outside a git repository (like `rg --no-require-git`). The index lives in `.glep/` in the directory you run glep from, so run it from the project root.

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
- rg features glep lacks: replacements, PCRE2, compressed files.
- Corpora dominated by binaries or files over the 1 MB cap (live-scanned anyway).

## Numbers

Linux kernel 6.12 checkout: 86,605 files, about 1.5 GB. Apple Silicon macOS, hyperfine medians, warm filesystem cache, rg and fd at their default parallelism.

| Scenario | glep (glep --ttl 5) | rg / fd |
|---|---|---|
| Rare pattern | 173 ms (21 ms) | rg 1.42 s |
| Common pattern, ~10k matches | 298 ms (90 ms) | rg 1.54 s |
| List all .c files | 242 ms (44 ms) | fd 92 ms |

Index build (one-time): 24 s. Index size: 154 MB. Default glep pays the freshness sweep (a stat of every file) on each query; `--ttl` amortizes it across read bursts. Details: [Benchmarks](https://velofy.co/glep/benchmarks/).

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
cargo test --all                                  # parity tests need rg on PATH
cargo build
PATH="$PWD/target/debug:$PATH" claude/hooks/test_hook.sh
PATH="$PWD/target/debug:$PATH" cursor/hooks/test_hook.sh
```

CI also rejects em dash and en dash characters anywhere in the repository.

## License

MIT. See [LICENSE](https://github.com/velofy/glep/blob/main/LICENSE).
