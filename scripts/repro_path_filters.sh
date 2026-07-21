#!/usr/bin/env bash
# Minimal repro for glep path-filter bugs. Run from a repo with a .glep index.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GLEP="${GLEP:-glep}"

if ! command -v "$GLEP" >/dev/null; then
  echo "glep not found; set GLEP= to a binary" >&2
  exit 2
fi

VER="$($GLEP --version 2>/dev/null || true)"
echo "glep: $VER"
if [[ "$VER" == *"0.1."* ]]; then
  echo "NOTE  glep <=0.1.x is missing path normalization fixes shipped in 0.2.3."
  echo "      Upgrade: cargo install glep --force   or   pip install -U glep"
fi
echo "repo: $ROOT"
echo

pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAIL=1; }

FAIL=0
cd "$ROOT"

# Baseline: no path arg should match.
BASE=$($GLEP -l 'fn normalize_path_filters' src/cli.rs 2>/dev/null | wc -l | tr -d ' ' || true)
if [[ "$BASE" -ge 1 ]]; then pass "no path arg"; else fail "no path arg (got $BASE lines)"; fi

# Broken on <=0.2.2: absolute path filter from repo root.
ABS=$($GLEP -l 'fn normalize_path_filters' "$ROOT/src/cli.rs" 2>/dev/null | wc -l | tr -d ' ' || true)
if [[ "$ABS" -ge 1 ]]; then pass "absolute path"; else fail "absolute path (got $ABS lines)"; fi

# Broken on <=0.2.2: ./ prefix.
DOT=$($GLEP -l 'fn normalize_path_filters' ./src/cli.rs 2>/dev/null | wc -l | tr -d ' ' || true)
if [[ "$DOT" -ge 1 ]]; then pass "./path"; else fail "./path (got $DOT lines)"; fi

# Broken on <=0.2.2: interior .. components.
DOTDOT=$($GLEP -l 'fn normalize_path_filters' src/../src/cli.rs 2>/dev/null | wc -l | tr -d ' ' || true)
if [[ "$DOTDOT" -ge 1 ]]; then pass "src/../src path"; else fail "src/../src path (got $DOTDOT lines)"; fi

# cwd must be the indexed tree; absolute path from elsewhere silently misses.
OUTSIDE=$(
  cd /tmp
  $GLEP -l 'fn normalize_path_filters' "$ROOT/src/cli.rs" 2>/dev/null | wc -l | tr -d ' ' || true
)
if [[ "$OUTSIDE" -ge 1 ]]; then
  pass "absolute path from /tmp cwd"
else
  echo "NOTE  absolute path from /tmp cwd returns 0 matches (exit 1, no stderr)."
  echo "      glep indexes cwd, not the path argument. cd into the repo first."
fi

echo
if [[ "$FAIL" -eq 0 ]]; then
  echo "All required checks passed."
  exit 0
fi
echo "Some checks failed."
exit 1
