#!/usr/bin/env bash
# Mutation checks for the simulator (DESIGN.md §14.1, §14.4).
#
#   scripts/mutate.sh [--rev REV] [--seeds N] [--start S] [--out DIR] PATCH...
#
# A mutation is a patch that breaks the code on purpose, such as deleting
# the line that emits an action. The simulator should fail every one; a
# mutation it passes marks something it does not check. Make one by editing
# a scratch checkout and saving `git diff` to a file.
#
# All the mutations run at once, each in its own git worktree at REV
# (default HEAD) with its own target directory and an equal share of the
# cores. Each builds, runs the pinned regression seeds as CI does, and then
# sweeps seeds S to S+N-1 with --keep-going (default 0 to 999, CI's sweep;
# --seeds 0 skips the sweep). REV must have `delocal-sim --jobs`.
#
# The logs of mutation NAME (the patch's file name without .patch or .diff)
# go to DIR/NAME, by default target/mutate/NAME, beside its target
# directory, which is kept so the next run builds incrementally. The
# worktrees are removed at the end. Exit status 0 means the simulator
# caught every mutation; a mutation that does not apply or does not build
# counts as not caught, since it tested nothing. Nothing times out: if a
# mutation makes the engine loop forever, stop the script with Ctrl-C.

set -euo pipefail

usage() {
  sed -n '2,/^$/{s/^# \{0,1\}//;p;}' "$0" >&2
  exit 2
}

rev=HEAD seeds=1000 start=0 out=
while [ $# -gt 0 ]; do
  case $1 in
    --rev | --seeds | --start | --out)
      [ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; }
      case $1 in
        --rev) rev=$2 ;;
        --seeds) seeds=$2 ;;
        --start) start=$2 ;;
        --out) out=$2 ;;
      esac
      shift 2
      ;;
    -h | --help) usage ;;
    --) shift; break ;;
    -*) echo "unknown flag $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || usage
for n in "$seeds" "$start"; do
  [[ $n =~ ^[0-9]+$ ]] || { echo "--seeds and --start take whole numbers, not '$n'" >&2; exit 2; }
done

repo=$(git rev-parse --show-toplevel)
commit=$(git rev-parse --verify --quiet "$rev^{commit}") || { echo "no commit $rev" >&2; exit 2; }
out=${out:-$repo/target/mutate}
mkdir -p "$out"
out=$(cd "$out" && pwd)

# Names and absolute paths, checked before anything starts: the worktrees
# change directory, and two patches with one name would share a directory.
names=() patches=() seen=/
for p in "$@"; do
  [ -f "$p" ] || { echo "no patch file $p" >&2; exit 2; }
  name=$(basename "$p")
  name=${name%.patch}
  name=${name%.diff}
  [[ $name =~ ^[A-Za-z0-9._-]+$ ]] || { echo "patch names are letters, digits, '.', '_' and '-': $p" >&2; exit 2; }
  case $seen in *"/$name/"*) echo "two patches are named $name" >&2; exit 2 ;; esac
  seen=$seen$name/
  names+=("$name")
  patches+=("$(cd "$(dirname "$p")" && pwd)/$(basename "$p")")
done

# Each mutation gets an equal share of the cores for its build, its pinned
# tests and its sweep, so the machine runs about one thread per core, or
# one per mutation if there are more mutations than cores.
jobs=$(( $(getconf _NPROCESSORS_ONLN) / ${#names[@]} ))
[ "$jobs" -ge 1 ] || jobs=1

# Remove each mutation's worktree, registration and directory both, so that
# a run that died without cleaning up does not stop the next one.
remove_trees() {
  for name in "${names[@]}"; do
    git -C "$repo" worktree remove --force "$out/$name/tree" 2> /dev/null || true
    rm -rf "$out/$name/tree"
  done
  git -C "$repo" worktree prune
}

# kill_tree PID: stop PID and every process under it, children first so
# that none is left to start more. A script's background jobs ignore
# Ctrl-C, so the checks would otherwise keep building and sweeping after
# the script has gone.
kill_tree() {
  local child
  for child in $(pgrep -P "$1"); do
    kill_tree "$child"
  done
  kill "$1" 2> /dev/null || true
}

pids=()
stop() {
  trap - INT TERM
  for pid in "${pids[@]}"; do
    kill_tree "$pid"
  done
  wait
  exit 130
}

# The worktrees go even if the run is interrupted; the logs and target
# directories stay.
trap remove_trees EXIT
trap stop INT TERM
remove_trees

# check NAME PATCH: build and run one mutation, and write its outcome to
# DIR/NAME/result as "VERDICT PINNED_FAILED PINNED_TOTAL SWEEP_FAILED",
# with "-" for what did not run.
check() {
  local name=$1 patch=$2 dir=$out/$1
  local tree=$dir/tree
  mkdir -p "$dir"
  rm -f "$dir"/*.log "$dir/result"
  if ! git -C "$repo" worktree add --quiet --detach "$tree" "$commit" 2> "$dir/apply.log" ||
    ! git -C "$tree" apply "$patch" 2>> "$dir/apply.log"; then
    echo "does-not-apply - - -" > "$dir/result"
    return
  fi
  export CARGO_TARGET_DIR=$dir/target
  if ! (cd "$tree" &&
    cargo build --release --locked -j "$jobs" -p delocal-sim &&
    cargo test --release --locked -j "$jobs" -p delocal-sim --no-run) > "$dir/build.log" 2>&1; then
    echo "does-not-build - - -" > "$dir/result"
    return
  fi

  (cd "$tree" && cargo test --release --locked -j "$jobs" -p delocal-sim -- regressions:: --test-threads "$jobs") \
    > "$dir/pinned.log" 2>&1 || true
  # cargo prints one "test result:" line per test binary; add them up.
  local pinned
  pinned=$(awk '/^test result:/ { n++; for (i = 1; i < NF; i++) { if ($(i+1) ~ /^passed;/) p += $i; if ($(i+1) ~ /^failed;/) f += $i } }
    END { if (n) print f + 0, p + f; else print "- -" }' "$dir/pinned.log")

  # The sweep exits 0 when every seed passed, 1 when some failed, 2 on bad
  # arguments (a REV without --jobs), and 101 on a panic, which counts as
  # caught. Anything else means it was stopped, by a signal for one, and
  # says nothing about the mutation.
  local sweep=- status=0
  if [ "$seeds" -gt 0 ]; then
    "$CARGO_TARGET_DIR/release/delocal-sim" --seeds "$seeds" --start "$start" --keep-going --jobs "$jobs" \
      > "$dir/sweep.log" 2>&1 || status=$?
    case $status in
      0) sweep=0 ;;
      1) sweep=$(sed -nE 's/^[0-9]+ of [0-9]+ seeds passed; ([0-9]+) failed:$/\1/p' "$dir/sweep.log")
        [ -n "$sweep" ] || { echo "sweep-did-not-run $pinned -" > "$dir/result"; return; } ;;
      101) sweep=crashed ;;
      *) echo "sweep-did-not-run $pinned -" > "$dir/result"; return ;;
    esac
  fi

  local verdict=caught
  case "$pinned" in
    -*) verdict=pinned-did-not-run ;;
    0\ *) [ "$sweep" != 0 ] && [ "$sweep" != - ] || verdict=survived ;;
  esac
  echo "$verdict $pinned $sweep" > "$dir/result"
}

noun=mutations
[ "${#names[@]}" -gt 1 ] || noun=mutation
echo "mutate: ${#names[@]} $noun of $(git -C "$repo" rev-parse --short "$commit"), $jobs threads each, in $out"
for i in "${!names[@]}"; do
  (
    check "${names[$i]}" "${patches[$i]}"
    echo "  ${names[$i]}: $(cut -d' ' -f1 "$out/${names[$i]}/result" | tr - ' ')"
  ) &
  pids+=($!)
done
wait

# One row per mutation, then the invariants behind each catch: a pinned
# test panics with "FAILED <invariant>: ...", and the sweep's summary has a
# line per invariant that failed.
printf '\n%-24s %-16s %-16s %s\n' mutation "pinned failed" "sweep failed" verdict
status=0
for name in "${names[@]}"; do
  # A check that stopped on an unexpected error left no result.
  [ -f "$out/$name/result" ] || echo "error - - -" > "$out/$name/result"
  read -r verdict pf pt sf < "$out/$name/result"
  pinned="$pf of $pt"
  [ "$pf" != - ] || pinned=-
  case $sf in
    - | crashed) sweep=$sf ;;
    *) sweep="$sf of $seeds" ;;
  esac
  printf '%-24s %-16s %-16s %s\n' "$name" "$pinned" "$sweep" "$(echo "$verdict" | tr - ' ')"
  [ "$verdict" = caught ] || status=1
done
echo
for name in "${names[@]}"; do
  dir=$out/$name
  read -r verdict _ < "$dir/result"
  if [ "$verdict" != caught ]; then
    echo "$name: see $dir"
    continue
  fi
  by=$(
    {
      grep -hoE '^FAILED [^:]+' "$dir/pinned.log" | sed 's/^FAILED //' | sort | uniq -c |
        awk '{ n = $1; $1 = ""; print substr($0, 2) " x" n " (pinned)" }'
      # With --seeds 0 there is no sweep log.
      [ ! -f "$dir/sweep.log" ] ||
        sed -nE 's/^ +([0-9]+)  ([^:]+): seeds .*/\2 x\1 (sweep)/p' "$dir/sweep.log"
    } | paste -sd';' - | sed 's/;/; /g'
  )
  echo "$name: ${by:-the sweep crashed; see $dir/sweep.log}"
done
exit "$status"
