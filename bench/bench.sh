#!/usr/bin/env bash
# Compare glep against rg (content search) and against rg --files and fd
# (listing) on a target directory.
#
# rg runs at its default parallelism, which is its fastest mode. Its output
# flags match glep's output format (-n --no-heading --color=never) and
# --no-require-git matches glep's gitignore handling outside git repositories,
# so both tools do the same work on the same file set. --sort path is left
# out on purpose: it makes rg single-threaded and belongs to the byte-parity
# harness (tests/parity.rs), not to a speed comparison, so rg's output order
# here differs from glep's deterministic order.
#
# Usage: bench/bench.sh /path/to/big/repo 'some_pattern' ['*.rs']
set -euo pipefail
TARGET="${1:?usage: bench.sh <dir> <pattern> [listing-glob]}"
PATTERN="${2:?usage: bench.sh <dir> <pattern> [listing-glob]}"
GLOB="${3:-*.rs}"
command -v hyperfine >/dev/null || { echo "install hyperfine first" >&2; exit 1; }
cd "$TARGET"
echo "glep: $(command -v glep) ($(glep --version)); $(rg --version | head -1); hyperfine $(hyperfine --version | cut -d' ' -f2)" >&2
glep index
RG="rg -n --no-heading --color=never --no-require-git"
# -N skips the shell so 20 ms measurements are not padded by shell startup;
# -i keeps going when a pattern has no matches (exit 1 in both tools).
hyperfine -N -i --warmup 2 \
  "glep '$PATTERN'" \
  "glep --ttl 60 '$PATTERN'" \
  "$RG '$PATTERN'" \
  "glep -l '$PATTERN'" \
  "glep --ttl 60 -l '$PATTERN'" \
  "$RG -l '$PATTERN'" \
  "glep -i '$PATTERN'" \
  "glep --ttl 60 -i '$PATTERN'" \
  "$RG -i '$PATTERN'" \
  "glep -U '$PATTERN'" \
  "glep --ttl 60 -U '$PATTERN'" \
  "$RG -U '$PATTERN'" \
  "glep -C 2 '$PATTERN'" \
  "glep --ttl 60 -C 2 '$PATTERN'" \
  "$RG -C 2 '$PATTERN'"
LISTING=(
  "glep --files '**/$GLOB'"
  "glep --ttl 60 --files '**/$GLOB'"
  "rg --no-require-git --files -g '$GLOB'"
)
if command -v fd >/dev/null; then LISTING+=("fd --no-require-git -g '$GLOB'"); fi
hyperfine -N --warmup 2 "${LISTING[@]}"
