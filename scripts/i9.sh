#!/usr/bin/env bash
# Count I9 failures per slice by class (DESIGN.md §14.1).
#
#   scripts/i9.sh [--start S] [--seeds N] [--jobs J]
#
# I9, what revert discards stays discarded, is checked only with
# `--check-i9 true`, which the required CI jobs leave off while the engine
# still breaks it for reasons older than the invariant. A failure is named
# by its class (crates/sim/src/i9.rs), and this script says how many seeds
# of each slice fail with each class.
#
# The slices are laid out as the nightly lays out its 100,000 seeds: the
# default knobs from S, `--corruption 0.05` from S + 76,000, `--skip 0.02`
# from S + 86,000, and `--steps 2000` from S + 96,000. Each slice runs N
# seeds from its start (default 1,000), the long one N * 3 / 10, since a
# 2,000-step seed takes about five times as long. J worker threads per
# slice (default: a quarter of the cores each, as the four run at once).
#
# The script builds the simulator in release first. Each slice's sweep
# goes to target/i9/SLICE.txt. Exit status: 0 once every slice has run,
# whatever it found; 2 for bad arguments or a failed build.

set -euo pipefail

usage() {
  sed -n '2,/^$/{s/^# \{0,1\}//;p;}' "$0" >&2
  exit 2
}

start=0 seeds=1000 jobs=
while [ $# -gt 0 ]; do
  case $1 in
    --start | --seeds | --jobs)
      [ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; }
      case $1 in
        --start) start=$2 ;;
        --seeds) seeds=$2 ;;
        --jobs) jobs=$2 ;;
      esac
      shift 2
      ;;
    -h | --help) usage ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
cores=$(nproc 2> /dev/null || getconf _NPROCESSORS_ONLN)
jobs=${jobs:-$(( cores / 4 ))}
for n in "$start" "$seeds" "$jobs"; do
  [[ $n =~ ^[0-9]+$ ]] || { echo "--start, --seeds and --jobs take whole numbers, not '$n'" >&2; exit 2; }
done
[ "$jobs" -ge 1 ] || jobs=1

repo=$(git rev-parse --show-toplevel)
out=$repo/target/i9
mkdir -p "$out"
export CARGO_TERM_COLOR=never
(cd "$repo" && cargo build --release --locked -q -p delocal-sim) || exit 2
sim=$repo/target/release/delocal-sim

# name, first seed, seeds, knobs
slices=(
  "default $start $seeds"
  "corruption $(( start + 76000 )) $seeds --corruption 0.05"
  "skip $(( start + 86000 )) $seeds --skip 0.02"
  "long $(( start + 96000 )) $(( seeds * 3 / 10 )) --steps 2000"
)
for slice in "${slices[@]}"; do
  read -r name first n knobs <<< "$slice"
  # A failing seed makes the sweep exit 1, which is what it is here for.
  # shellcheck disable=SC2086 # the knobs are separate words
  "$sim" --seeds "$n" --start "$first" $knobs --check-i9 true --keep-going --jobs "$jobs" \
    > "$out/$name.txt" 2> /dev/null &
done
wait

# One row per slice, one column per class that any slice has.
for slice in "${slices[@]}"; do
  read -r name first n _ <<< "$slice"
  printf '%s\t%s\t%s\n' "$name" "$first" "$n"
  grep -oE '^seed [0-9]+ failed \(I9 [^)]*\)' "$out/$name.txt" | sed -E 's/^[^(]*\(I9 (.*)\)$/\1/' || true
done | awk -F'\t' '
  NF == 3 { row = $1; rows[++r] = row; first[row] = $2; n[row] = $3; next }
  { count[row, $0]++; failed[row]++; if (!($0 in seen)) { seen[$0]; classes[++c] = $0 } }
  END {
    # Commonest class first.
    for (i = 1; i <= c; i++) for (k = 1; k <= r; k++) total[classes[i]] += count[rows[k], classes[i]]
    for (i = 1; i <= c; i++) for (k = i + 1; k <= c; k++) if (total[classes[k]] > total[classes[i]]) {
      t = classes[i]; classes[i] = classes[k]; classes[k] = t
    }
    printf "%-11s %8s %6s %6s", "slice", "from", "seeds", "failed"
    for (i = 1; i <= c; i++) printf "  %s", classes[i]
    printf "\n"
    for (j = 1; j <= r; j++) {
      row = rows[j]
      printf "%-11s %8d %6d %6d", row, first[row], n[row], failed[row]
      for (i = 1; i <= c; i++) printf "  %*d", length(classes[i]), count[row, classes[i]]
      printf "\n"
    }
  }'
