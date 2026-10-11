#!/usr/bin/env bash
# Compare glep vs rg at rg's FASTEST (default parallel mode, no --sort) on a
# target directory. rg --sort path is ~4x slower than rg's default and is
# only needed for byte-parity harnesses, not speed benches.
# Usage: bench/bench.sh /path/to/big/repo 'some_pattern'
set -euo pipefail
TARGET="${1:?usage: bench.sh <dir> <pattern>}"
PATTERN="${2:?usage: bench.sh <dir> <pattern>}"
command -v hyperfine >/dev/null || { echo "install hyperfine first" >&2; exit 1; }
cd "$TARGET"
glep index
hyperfine --warmup 2 \
  "glep '$PATTERN'" \
  "glep --ttl 60 '$PATTERN'" \
  "glep --ttl 60 -l '$PATTERN'" \
  "rg '$PATTERN'" \
  "rg -l '$PATTERN'" \
  "glep --ttl 60 -U '$PATTERN'" \
  "rg -U '$PATTERN'"
hyperfine --warmup 2 \
  "glep --files '**/*.rs'" \
  "rg --files -g '*.rs'"
