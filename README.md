<p align="center">
  <img src="https://raw.githubusercontent.com/anishfyi/glep/main/assets/logo.svg" width="400" alt="glep">
</p>

<p align="center"><strong>Indexed grep + glob for AI agents.</strong></p>

Ripgrep pays the full scan cost on every query. glep pays it once: a persistent, self-healing trigram index answers warm queries in 21-298 ms on a Linux-kernel-sized tree where ripgrep takes 1.4 s (21 ms in --ttl burst mode), with text output byte-compatible with ripgrep's, enforced by a 22-case differential harness in CI. No daemon.

## Why

Coding agents call Grep and Glob dozens of times per session. On monorepo-scale projects each call costs seconds. glep replaces both with index-backed equivalents built on ripgrep's own crates (`ignore`, `grep-searcher`, `regex-syntax`), so correctness is inherited, not reimplemented.

## When to use it

| Use glep | Stick with rg / fd |
|---|---|
| Agent sessions firing dozens of searches over one repo (the bundled hook reroutes Grep/Glob) | One-off searches in a tree you will never search again |
| Monorepos where rg takes 100ms+ per query; glep measured 21-298 ms at kernel scale | Small repos where rg already answers in under ~50ms |
| Repeated glob listings: `glep --files` reads the manifest, no re-walk past the freshness sweep | Ephemeral CI runners where the index never persists between runs |
| Read-heavy bursts with `--ttl 5` to amortize the freshness sweep | rg features glep lacks: replacements, PCRE2, compressed files |
| Correctness-critical work: self-healing index, sound full-scan fallback | Corpora dominated by binaries or files over the 1MB cap (live-scanned anyway) |

`--hidden` includes dotfiles; `.git` itself is always excluded.

## Numbers

Linux kernel 6.12 checkout: 86,605 files, ~1.5 GB. Apple Silicon macOS, hyperfine medians, warm filesystem cache, rg and fd at their default parallelism.

| Scenario | glep | glep --ttl 5 | ripgrep | fd |
|---|---|---|---|---|
| Rare pattern | 173 ms | 21 ms | 1.42 s | |
| Common pattern (~10k matches) | 298 ms | 90 ms | 1.54 s | |
| List all .c files (--files) | 242 ms | 44 ms | | 92 ms |
| Index build (one-time) | 24 s | | | |

Default glep pays the self-healing freshness sweep (a stat of every file) on each query; `--ttl` amortizes it across read bursts. Index size: 154 MB, about 10% of the corpus. The parity harness pins byte-equality with rg's output; speed differs, bytes do not.

## How it works

- A file-level trigram inverted index (the Russ Cox / csearch model) lives in `.glep/`, memory-mapped, about 10% of corpus size measured on the kernel tree.
- Every query self-heals: a fast parallel mtime sweep incrementally reindexes only what changed, then answers. No watcher, no background process.
- The regex becomes a trigram plan, postings intersection yields a handful of candidate files, and ripgrep's searcher runs over just those.
- Patterns trigrams can't narrow fall back to a full parallel scan: never a wrong answer, worst case is rg-speed.

## Interface

```bash
glep 'fn parse_intent' src/     # content search (Grep replacement)
glep --files '**/*.py'          # glob listing (Glob replacement)
glep --json 'pattern'           # machine-readable output for agents
glep -c 'pattern'               # per-file match counts (rg -c)
glep -l -i -F -U ...            # files-with-matches, case-insensitive, fixed, multiline
glep -A 2 -B 1 'pattern'        # context, or -C n for both sides
glep -g '*.rs' -t rust ...      # glob and type filters
glep --hidden 'TODO'            # include dotfiles (.git is always excluded)
glep --ttl 5 ...                # skip the freshness sweep within a read burst
glep --max-filesize 2000000 ... # raise the 1MB index cap
glep index                      # explicit (re)build; lazy on first query
glep status                     # index stats
```

Ships with a Claude Code skill and a PreToolUse hook that routes built-in Grep/Glob calls through glep automatically.

## Install

```bash
pip install glep          # binary wheel, no Rust toolchain needed
# or
cargo install glep
```

Claude Code integration (skill + hook): `claude/install.sh`.

## Status

Spec: [docs/superpowers/specs/2026-07-14-glep-design.md](https://github.com/anishfyi/glep/blob/main/docs/superpowers/specs/2026-07-14-glep-design.md).
