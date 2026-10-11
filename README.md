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

ripgrep pays the full scan cost on every query. glep pays it once: a persistent, self-healing trigram index answers rare-pattern queries in 28 to 113 ms on a Linux-kernel-sized tree where ripgrep — at its fastest, default parallel mode — takes 2.1 s, with text output byte-compatible with ripgrep's, enforced by a differential harness in CI. No daemon. Every mode beats or ties rg at its fastest: listings too (`--files` outruns both `rg --files` and `fd`).

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
- **Familiar flags:** `-i -F -l -c -U -e -g -t -C -A -B --json --hidden --no-ignore`.
- **Agent integrations:** a Claude Code skill and PreToolUse hook (`claude/install.sh`) and a Cursor hook (`cursor/install.sh`) that route built-in Grep/Glob calls through glep.

## Interface

```bash
glep 'fn parse_intent' src/     # content search (Grep replacement)
glep --files '**/*.py'          # glob listing (Glob replacement)
glep --json 'pattern'           # machine-readable output (includes rg's summary event)
glep -c 'pattern'               # per-file match counts (rg -c)
glep -l -i -F -U ...            # files-with-matches, case-insensitive, fixed, multiline
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

Other known differences from rg: files with a NUL byte in the first 8 KB (including UTF-16 text) are treated as binary and never searched, and `.gitignore` rules apply even outside a git repository (like `rg --no-require-git`). The index lives in `.glep/` in the directory you run glep from, so run it from the project root.

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

Linux kernel 6.12 checkout: 86,605 files, ~1.6 GB. Apple Silicon macOS, hyperfine, warm filesystem cache. **rg is measured at its default parallel mode — its fastest** — not the ~4x-slower single-threaded `--sort path` variant the byte-parity harnesses use.

| Scenario | glep | glep --ttl | ripgrep | vs rg |
|---|---|---|---|---|
| Rare pattern | 113 ms | 28 ms | 2.09 s | **18x / 74x** |
| Rare, files-with-matches (`-l`) | 104 ms | — | 2.23 s | **21x** |
| Rare, case-insensitive (`-i`) | 115 ms | — | 2.13 s | **18x** |
| Rare, multiline (`-U`) | — | 111 ms | 2.07 s | **19x** |
| Rare, context (`-C 2`) | — | 22 ms | 2.45 s | **110x** |
| Common pattern (`static`) | 1.50 s | 0.97-1.01 s | 1.95-2.35 s | **1.3-2.3x** |
| Single char, no trigram (`q`) | — | 2.0 s | 1.98-2.3 s | parity to 1.1x |
| List all .c files (`--files`) | 64-76 ms | 64 ms | rg 74-84 ms, fd 89 ms | **1.1-1.2x vs both** |

Index build (one-time): 24 s. Index size: 154 MB (~10% of corpus) plus a 7 MB listing cache. Every mode beats or ties rg at rg's fastest. Patterns shorter than 3 bytes (or otherwise trigram-free, like `.*`) cannot be narrowed by any trigram index, so glep matches rg's full-scan speed there and still edges it via the mmap + worker-pool scanner. Default glep pays the self-healing freshness sweep (one bulk-stat per directory) on each query; `--ttl` amortizes it across read bursts; `--files` answers from the dir-mtime listing cache — interior directories re-verified, unchanged leaf directories pruned — without opening the index. Details: [Benchmarks](https://velofy.co/glep/benchmarks/).

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
