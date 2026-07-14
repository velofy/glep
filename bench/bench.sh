#!/usr/bin/env bash
# Compare glep vs rg (content) and fd (listing) on a target directory.
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
  "rg -n --no-heading --color=never --sort path '$PATTERN'"
hyperfine --warmup 2 \
  "glep --files '**/*.rs'" \
  "fd -e rs"
