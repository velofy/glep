#!/usr/bin/env bash
# glep vs ripgrep benchmark + differential on a large tree.
#
# Usage:
#   GLEP=/path/to/glep bench/compare.sh /path/to/corpus [out_dir]
#
# Env: GLEP (required), RG (default: rg), RUNS (hyperfine runs, default 7),
# SCEN_FILE (optional file of scenarios; one per line, "name|arg;arg;..."),
# LABEL (default: basename of out dir).
#
# Produces, under out_dir: index_build.txt, diff_summary.txt, timings.txt,
# hf_*.json/md, summary.md. Scenario args use ';' separators so patterns
# may contain spaces and '|'. The default scenarios assume a large C
# codebase; provide SCEN_FILE for other corpora.
set -uo pipefail

CORPUS="${1:?usage: compare.sh <corpus_dir> [out_dir]}"
OUT="${2:-/tmp/glep-bench/results/${LABEL:-run}}"
LABEL="${LABEL:-$(basename "$OUT")}"
GLEP="${GLEP:?set GLEP to the glep binary under test}"
RG="${RG:-rg}"
RUNS="${RUNS:-7}"
command -v hyperfine >/dev/null || { echo "install hyperfine first" >&2; exit 1; }
mkdir -p "$OUT"
cd "$CORPUS"

RGQ="$RG --no-require-git -n --no-heading --color=never"
RGS="$RGQ --sort path"

echo "== $LABEL: $($GLEP --version) / $($RG --version | head -1)" | tee "$OUT/meta.txt"
uname -a | tee -a "$OUT/meta.txt"

# --- index build (cold index) ---
rm -rf .glep
/usr/bin/time -p "$GLEP" index 2> "$OUT/index_build.txt" || "$GLEP" index 2>>"$OUT/index_build.txt"
du -sk .glep | tee -a "$OUT/index_build.txt"
ls -la .glep >> "$OUT/index_build.txt"

if [ -n "${SCEN_FILE:-}" ]; then
  SCEN=()
  while IFS= read -r line; do
    [ -n "$line" ] && SCEN+=("$line")
  done < "$SCEN_FILE"
else
  SCEN=(
    "rare|PCI_EXP_LNKCTL2_TLS"
    "mid|struct file_operations"
    "common|EXPORT_SYMBOL_GPL"
    "verycommon|GFP_KERNEL"
    "zero|zz_absent_token_zz"
    "short_all_count|-c;ab"
    "icase|-i;kmalloc"
    "alternation|TODO|FIXME"
    "regex|spin_lock_irqsave\\(&"
    "files_with_matches|-l;EXPORT_SYMBOL_GPL"
    "pathfilter|GFP_KERNEL;drivers/net"
    "context|-C;2;PCI_EXP_LNKCTL2_TLS"
  )
fi

# shell-quoted argument string for a scenario's args field
qargs() { local IFS=';'; local a; read -ra a <<< "$1"; printf '%q ' "${a[@]}"; }

# --- differential: glep output must equal rg --sort path output byte for byte ---
echo "== differential" | tee "$OUT/diff_summary.txt"
"$GLEP" index >/dev/null 2>&1
for s in "${SCEN[@]}"; do
  name="${s%%|*}"; args="${s#*|}"
  eval "$GLEP $(qargs "$args")" > "$OUT/diff_${name}.glep" 2> "$OUT/diff_${name}.glep.err"; gc=$?
  eval "$RGS $(qargs "$args")" > "$OUT/diff_${name}.rg" 2> "$OUT/diff_${name}.rg.err"; rc=$?
  if cmp -s "$OUT/diff_${name}.glep" "$OUT/diff_${name}.rg" && [ "$gc" = "$rc" ]; then
    echo "OK   $name (exit $gc, $(wc -l < "$OUT/diff_${name}.rg") lines)" | tee -a "$OUT/diff_summary.txt"
  else
    echo "DIFF $name glep_exit=$gc rg_exit=$rc glep_lines=$(wc -l < "$OUT/diff_${name}.glep") rg_lines=$(wc -l < "$OUT/diff_${name}.rg")" | tee -a "$OUT/diff_summary.txt"
    diff "$OUT/diff_${name}.glep" "$OUT/diff_${name}.rg" | head -20 >> "$OUT/diff_summary.txt"
  fi
done
# --files differential
"$GLEP" --files '*.c' > "$OUT/diff_files.glep"; "$RG" --no-require-git --files --sort path -g '*.c' > "$OUT/diff_files.rg"
if cmp -s "$OUT/diff_files.glep" "$OUT/diff_files.rg"; then echo "OK   files_c ($(wc -l < "$OUT/diff_files.rg") lines)"; else echo "DIFF files_c"; diff "$OUT/diff_files.glep" "$OUT/diff_files.rg" | head; fi | tee -a "$OUT/diff_summary.txt"
"$GLEP" --files > "$OUT/diff_allfiles.glep"; "$RG" --no-require-git --files --sort path > "$OUT/diff_allfiles.rg"
if cmp -s "$OUT/diff_allfiles.glep" "$OUT/diff_allfiles.rg"; then echo "OK   files_all ($(wc -l < "$OUT/diff_allfiles.rg") lines)"; else echo "DIFF files_all glep=$(wc -l < "$OUT/diff_allfiles.glep") rg=$(wc -l < "$OUT/diff_allfiles.rg")"; diff "$OUT/diff_allfiles.glep" "$OUT/diff_allfiles.rg" | head; fi | tee -a "$OUT/diff_summary.txt"

# --- stage timings (GLEP_TIMING) 3 runs each, default mode ---
echo "== stage timings" > "$OUT/timings.txt"
for s in "${SCEN[@]}"; do
  name="${s%%|*}"; args="${s#*|}"
  for i in 1 2 3; do
    echo "--- $name run $i" >> "$OUT/timings.txt"
    eval "GLEP_TIMING=1 $GLEP $(qargs "$args")" 2>&1 >/dev/null | grep 'glep timing' >> "$OUT/timings.txt"
  done
done

# --- hyperfine ---
for s in "${SCEN[@]}"; do
  name="${s%%|*}"; args="${s#*|}"
  q=$(qargs "$args")
  hyperfine -N --warmup 2 --runs "$RUNS" --ignore-failure \
    --export-json "$OUT/hf_${name}.json" --export-markdown "$OUT/hf_${name}.md" \
    -n "glep" "$GLEP $q" \
    -n "glep --ttl 60" "$GLEP --ttl 60 $q" \
    -n "rg" "$RGQ $q" \
    -n "rg --sort path" "$RGS $q" 2>&1 | tee "$OUT/hf_${name}.txt"
done
hyperfine -N --warmup 2 --runs "$RUNS" --export-json "$OUT/hf_files.json" --export-markdown "$OUT/hf_files.md" \
  -n "glep --files *.c" "$GLEP --files *.c" \
  -n "glep --ttl 60 --files *.c" "$GLEP --ttl 60 --files *.c" \
  -n "rg --files -g *.c" "$RG --no-require-git --files -g *.c" \
  -n "fd -e c" "fd -e c" 2>&1 | tee "$OUT/hf_files.txt"

# --- summary table from json ---
python3 - "$OUT" <<'EOF'
import json, glob, os, sys
out = sys.argv[1]
rows = []
for f in sorted(glob.glob(os.path.join(out, "hf_*.json"))):
    d = json.load(open(f))
    name = os.path.basename(f)[3:-5]
    cells = {r["command"]: r for r in d["results"]}
    rows.append((name, cells))
cmds = []
for _, cells in rows:
    for c in cells:
        if c not in cmds: cmds.append(c)
hdr = "| scenario | " + " | ".join(cmds) + " |"
lines = [hdr, "|---|" + "---|" * len(cmds)]
for name, cells in rows:
    vals = []
    for c in cmds:
        r = cells.get(c)
        vals.append(f"{r['median']*1000:.0f} ms" if r else "")
    lines.append(f"| {name} | " + " | ".join(vals) + " |")
open(os.path.join(out, "summary.md"), "w").write("\n".join(lines) + "\n")
print("\n".join(lines))
EOF
echo "done: $OUT"
