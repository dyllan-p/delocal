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
# --seeds 0 skips the sweep). REV must have `delocal-sim --jobs`. The
# worktrees are made one at a time before any mutation starts, and a
# mutation whose worktree could not be made is not run.
#
# A mutation is caught if a pinned test or a seed of the sweep fails, or
# if the test process or the sweep dies of its own accord (a panic, an
# abort, a stack overflow), which is a failure too. A mutation that does
# not apply or does not build counts as not caught, since it tested
# nothing. A mutation the harness could not check is not run, never not
# caught: its worktree could not be made, its pinned tests or its sweep
# never started or were stopped from outside (SIGHUP, SIGINT, SIGKILL,
# SIGTERM: a user, the OOM killer, a runner shutting down), or its check
# stopped without leaving a result. What did not run says nothing about
# the simulator, unless what did run caught the mutation anyway.
#
# The logs of mutation NAME (the patch's file name without .patch or .diff)
# go to DIR/NAME, by default target/mutate/NAME, beside its target
# directory, which is kept so the next run builds incrementally. The
# worktrees are removed at the end. Exit status 0 means the simulator
# caught every mutation, and 1 that it did not catch some. Exit status 3
# means some mutation was not run, so the run did not finish. Nothing
# times out: if a mutation makes the engine loop forever, stop the script
# with Ctrl-C.

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

# ended_by LOG: how the test process of LOG ended, if it started (libtest's
# "running N tests") and did not exit successfully: "signal: 6, SIGABRT:
# process abort signal", say, or "exit status: 101", as cargo's "process
# didn't exit successfully" line gives it. Nothing if it never started.
# The same as in pins.sh.
ended_by() {
  [ -f "$1" ] && grep -qE '^running [0-9]+ tests?$' "$1" || return 0
  sed -nE "s/^ *process didn't exit successfully: .* \\((signal: [0-9]+[^)]*|exit status: [0-9]+)\\)\$/\\1/p" "$1" | tail -1
}

# died_of LOG: how the test process of LOG died, if it died of its own
# accord; nothing if it never started or was stopped from outside. The
# same as in pins.sh.
died_of() {
  local how
  how=$(ended_by "$1")
  case $how in
    "signal: 1,"* | "signal: 2,"* | "signal: 9,"* | "signal: 15,"*) ;;
    *) echo "$how" ;;
  esac
}

# not_run DIR WHY: the check of the mutation in DIR could not be made.
not_run() {
  echo "$2" > "$1/why"
}

# check NAME PATCH: build and run one mutation in its worktree, and write
# its outcome to DIR/NAME/result as "VERDICT PINNED_FAILED PINNED_TOTAL
# SWEEP_FAILED", with "-" for what was not asked for, "crashed" for what
# died of its own accord and "not-run" for what the harness could not
# run. A mutation that is not run says why in DIR/NAME/why.
check() {
  local name=$1 patch=$2 dir=$out/$1
  local tree=$dir/tree
  if [ ! -d "$tree" ]; then
    not_run "$dir" "its worktree could not be made; see $dir/worktree.log"
    echo "not-run - - -" > "$dir/result"
    return
  fi
  if ! git -C "$tree" apply "$patch" 2> "$dir/apply.log"; then
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
  # cargo prints one "test result:" line per test binary; add them up. A
  # test process that died before its line reported nothing: it crashed if
  # it died of its own accord, and otherwise the pinned tests did not run.
  local pinned why=
  pinned=$(awk '/^test result:/ { n++; for (i = 1; i < NF; i++) { if ($(i+1) ~ /^passed;/) p += $i; if ($(i+1) ~ /^failed;/) f += $i } }
    END { if (n) print f + 0, p + f; else print "- -" }' "$dir/pinned.log")
  if [ "$pinned" = "- -" ]; then
    if [ -n "$(died_of "$dir/pinned.log")" ]; then
      pinned="crashed -"
    else
      pinned="not-run -"
      local how
      how=$(ended_by "$dir/pinned.log")
      if [ -z "$how" ]; then
        how="never started"
        if grep -qE '^running [0-9]+ tests?$' "$dir/pinned.log"; then
          how="stopped before it said how it ended"
        fi
      fi
      why="its pinned tests did not run to the end ($how); see $dir/pinned.log"
    fi
  fi

  # The sweep exits 0 when every seed passed, 1 with its summary when some
  # failed, and 101 on a panic; a signal other than one from outside
  # (an abort, a stack overflow) also means it died of its own accord.
  # Those count. Anything else, 2 for bad arguments (a REV without
  # --jobs), a signal from outside, or a 1 without its summary, means the
  # sweep did not run to the end, which says nothing about the mutation.
  local sweep=- status=0
  if [ "$seeds" -gt 0 ]; then
    "$CARGO_TARGET_DIR/release/delocal-sim" --seeds "$seeds" --start "$start" --keep-going --jobs "$jobs" \
      > "$dir/sweep.log" 2>&1 || status=$?
    case $status in
      0) sweep=0 ;;
      1) sweep=$(sed -nE 's/^[0-9]+ of [0-9]+ seeds passed; ([0-9]+) failed:$/\1/p' "$dir/sweep.log") ;;
      101) sweep=crashed ;;
      129 | 130 | 137 | 143) ;;
      *) if [ "$status" -gt 128 ]; then sweep=crashed; fi ;;
    esac
    if [ -z "$sweep" ] || [ "$sweep" = - ]; then
      sweep=not-run
      why=${why:+$why; }"its sweep did not run to the end (status $status); see $dir/sweep.log"
    fi
  fi

  # Caught by whatever ran, even if something else did not run; otherwise
  # not run if anything did not run, and survived only if everything ran.
  # What did not run is noted either way.
  local verdict=survived pf=${pinned%% *}
  [ -z "$why" ] || not_run "$dir" "$why"
  if [ "$pf" = crashed ] || [ "$sweep" = crashed ] ||
    { [[ $pf =~ ^[0-9]+$ ]] && [ "$pf" -gt 0 ]; } || { [[ $sweep =~ ^[0-9]+$ ]] && [ "$sweep" -gt 0 ]; }; then
    verdict=caught
  elif [ -n "$why" ]; then
    verdict=not-run
  fi
  echo "$verdict $pinned $sweep" > "$dir/result"
}

noun=mutations
[ "${#names[@]}" -gt 1 ] || noun=mutation
echo "mutate: ${#names[@]} $noun of $(git -C "$repo" rev-parse --short "$commit"), $jobs threads each, in $out"
# Every worktree is made before any check starts, one at a time: two `git
# worktree add` at once can read the other's half-made entry under
# .git/worktrees and fail ("failed to read .../commondir"). A mutation
# whose worktree cannot be made is not run; it is never counted as one the
# simulator did not catch.
for name in "${names[@]}"; do
  mkdir -p "$out/$name"
  rm -f "$out/$name"/*.log "$out/$name/result" "$out/$name/why"
  git -C "$repo" worktree add --quiet --detach "$out/$name/tree" "$commit" 2> "$out/$name/worktree.log" ||
    rm -rf "$out/$name/tree"
done
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
status=0 not_run=0
for name in "${names[@]}"; do
  # A check that stopped on an unexpected error left no result, and was
  # not run.
  if [ ! -f "$out/$name/result" ]; then
    echo "not-run - - -" > "$out/$name/result"
    not_run "$out/$name" "its check stopped before it wrote a result"
  fi
  read -r verdict pf pt sf < "$out/$name/result"
  case $pf in
    - | crashed | not-run) pinned=$pf ;;
    *) pinned="$pf of $pt" ;;
  esac
  case $sf in
    - | crashed | not-run) sweep=$sf ;;
    *) sweep="$sf of $seeds" ;;
  esac
  printf '%-24s %-16s %-16s %s\n' "$name" "${pinned/not-run/not run}" "${sweep/not-run/not run}" "$(echo "$verdict" | tr - ' ')"
  case $verdict in
    caught) ;;
    not-run) not_run=$(( not_run + 1 )) ;;
    *) status=1 ;;
  esac
done
echo
for name in "${names[@]}"; do
  dir=$out/$name
  read -r verdict _ < "$dir/result"
  if [ "$verdict" = not-run ]; then
    echo "$name: not run, since $(cat "$dir/why")"
    continue
  elif [ "$verdict" != caught ]; then
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
  [ -n "$by" ] || by="failed without naming an invariant (a panic, or a process that died); see $dir"
  # Caught by what ran; say what did not.
  [ ! -f "$dir/why" ] || by="$by; though $(cat "$dir/why")"
  echo "$name: $by"
done
if [ "$not_run" -gt 0 ]; then
  echo
  echo "The run did not finish: $not_run of ${#names[@]} $noun not run."
  exit 3
fi
exit "$status"
