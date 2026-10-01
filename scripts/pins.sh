#!/usr/bin/env bash
# Check that every pin guards its fix (DESIGN.md §14.4).
#
#   scripts/pins.sh [--rev REV] [--jobs J] [--out DIR] [--fresh] [NAME...]
#
# A pinned seed guards its fix only while its history still reaches the
# bug, so every pin NAME is stored with scripts/pins/NAME.patch, a patch
# that disables the fix it guards. The patch starts with a paragraph that
# says what it disables and one line for each test written to guard that
# fix:
#
#   test: PACKAGE TEST
#
# For a pinned seed the first is `delocal-sim regressions::NAME`, and the
# fix's own unit tests follow: at least one, since every fix has one, and
# none that the patch merely happens to break, which guard other rules. A
# pin whose fix no seed reaches is replaced by a unit test next to the fix,
# and its patch names only unit tests. `git apply` skips the text before
# the first `diff`.
#
# A pin is valid only if each test it names passes at REV (default HEAD)
# and fails with its patch applied. The script first checks that every
# test in crates/sim/src/regressions.rs has a patch, then makes all its
# worktrees, one at a time, then builds REV once and runs every named test
# there, then applies each patch to a clean checkout of REV and runs its
# tests. J workers (default: one per four cores) each keep a worktree and
# a target directory and take the patches in turn, resetting the worktree
# to REV before each one, so a worker's second build recompiles only what
# its patches touch. Tests run in release, as CI's pinned job runs them,
# and a test that runs for more than ten minutes counts as not failing.
#
# A test the script could not run is not checked: its worker died, its
# worktree could not be made, or a build or test run of it stopped before
# the end without a compile error or a result. A test whose own process
# died before it reported, by an abort, a stack overflow or a call to
# exit, did run, and failed. A pin one of whose tests was checked and
# failed the check is invalid, whatever happened to its other tests. A pin
# with a test not checked and none that failed is not checked, which makes
# it neither valid nor invalid. If any pin is not checked, the check did
# not finish.
#
# A pin found valid is recorded in DIR/valid.tsv with a key: REV's tree
# of each crate its tests build (crates/engine for delocal-engine, that and
# crates/sim for delocal-sim, every crate for anything else), the build's
# own inputs (the root Cargo.toml, Cargo.lock, rust-toolchain.toml) and
# its patch as it stands. Those are everything both halves of its check
# depend on, the simulator and the tests being deterministic, so a pin
# whose key is the one recorded cannot have changed: it is skipped, and the
# summary says so and since which commit. The shortcut is exact, and it
# saves a run only where no crate a pin builds changed. --fresh ignores the
# record and checks every pin.
#
# With NAMEs only those pins are checked, and the coverage check is
# skipped. The logs of pin NAME go to DIR/NAME, by default
# target/pins/NAME, and DIR/summary.md holds the table the nightly puts in
# an issue. Exit status: 0, every pin is valid; 1, the check finished and
# some pin is not valid; 3, the check did not finish; 2, the arguments or a
# patch's header are wrong, and nothing ran.

set -euo pipefail

usage() {
  sed -n '2,/^$/{s/^# \{0,1\}//;p;}' "$0" >&2
  exit 2
}

rev=HEAD jobs= out= fresh=no
while [ $# -gt 0 ]; do
  case $1 in
    --fresh) fresh=yes; shift ;;
    --rev | --jobs | --out)
      [ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; }
      case $1 in
        --rev) rev=$2 ;;
        --jobs) jobs=$2 ;;
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
# nproc counts the cores this process may run on, so `taskset` limits the
# check; getconf is the fallback where there is no nproc (macOS).
cores=$(nproc 2> /dev/null || getconf _NPROCESSORS_ONLN)
jobs=${jobs:-$(( cores / 4 ))}
[[ $jobs =~ ^[0-9]+$ ]] || { echo "--jobs takes a whole number, not '$jobs'" >&2; exit 2; }
[ "$jobs" -ge 1 ] || jobs=1

repo=$(git rev-parse --show-toplevel)
commit=$(git rev-parse --verify --quiet "$rev^{commit}") || { echo "no commit $rev" >&2; exit 2; }
out=${out:-$repo/target/pins}
mkdir -p "$out"
out=$(cd "$out" && pwd)
pins=$repo/scripts/pins
# The logs are read by grep below, and by people in the nightly's
# artifact; colour codes would get in the way of both.
export CARGO_TERM_COLOR=never

# Every test in regressions.rs, read from REV: a `#[test]` line, perhaps
# an `#[ignore]`, then its `fn`.
pinned=$(git -C "$repo" show "$commit:crates/sim/src/regressions.rs" |
  awk '/^#\[test\]/ { t = 1; next } t && /^fn / { sub(/^fn /, ""); sub(/\(.*/, ""); print; t = 0; next } !/^#\[/ { t = 0 }')

# The pins to check and the tests each one names, in one list of tests:
# pin I names tests first[I] to first[I] + count[I] - 1.
names=() first=() count=() packages=() tests=()
add() {
  local name=$1 patch=$pins/$1.patch line package test extra seen=/ seeded=no units=0 start=${#tests[@]}
  # The name becomes a directory under DIR, removed at the start of a run.
  [[ $name =~ ^[a-z0-9_]+$ ]] || { echo "pin names are lower-case letters, digits and '_': $name" >&2; exit 2; }
  [ -f "$patch" ] || { echo "no patch $patch" >&2; exit 2; }
  # Only the header's `test:` lines count; it ends at the first `diff`.
  while IFS= read -r line; do
    read -r _ package test extra <<< "$line"
    [ -n "$test" ] && [ -z "${extra:-}" ] || { echo "$patch: '$line' is not 'test: PACKAGE TEST'" >&2; exit 2; }
    case $seen in *"/$package $test/"*) echo "$patch names $package $test twice" >&2; exit 2 ;; esac
    seen="$seen$package $test/"
    # A pinned seed's patch is named after its test, so a pin can be found
    # from either side, and no patch names another pin's seed.
    case $test in
      regressions::*) [ "$test" = "regressions::$name" ] || { echo "$patch names $test, not regressions::$name" >&2; exit 2; } ;;
      *) units=$(( units + 1 )) ;;
    esac
    [ "$package $test" != "delocal-sim regressions::$name" ] || seeded=yes
    packages+=("$package") tests+=("$test")
  done < <(sed -n -e '/^diff /q' -e '/^test: /p' "$patch")
  [ "${#tests[@]}" -gt "$start" ] || { echo "$patch needs a 'test: PACKAGE TEST' line" >&2; exit 2; }
  # Every fix has a unit test of its own, which guards it whether or not a
  # seed still reaches the bug.
  [ "$units" -gt 0 ] || { echo "$patch names no unit test of its fix, and every fix has one" >&2; exit 2; }
  # A pin that is still a seed at REV must be checked as one.
  if [ "$seeded" = no ] && grep -qxF "$name" <<< "$pinned"; then
    echo "$patch does not name its pinned seed, delocal-sim regressions::$name" >&2
    exit 2
  fi
  names+=("$name") first+=("$start") count+=("$(( ${#tests[@]} - start ))")
}
# Pinned seeds with no patch, which the summary lists as invalid.
unguarded=()
if [ $# -gt 0 ]; then
  for name in "$@"; do add "$name"; done
else
  for name in $pinned; do
    [ -f "$pins/$name.patch" ] || unguarded+=("$name")
  done
  for patch in "$pins"/*.patch; do
    [ -f "$patch" ] || continue
    add "$(basename "$patch" .patch)"
  done
fi
[ "${#names[@]}" -gt 0 ] || { echo "no patches in $pins" >&2; exit 2; }

# key_of I: what decides pin I's check, as one hash (see the header).
key_of() {
  local i=$1 package dirs=() d
  for package in $(printf '%s\n' "${packages[@]:${first[$i]}:${count[$i]}}" | sort -u); do
    case $package in
      delocal-engine) dirs+=(crates/engine) ;;
      delocal-sim) dirs+=(crates/engine crates/sim) ;;
      *) dirs+=(crates) ;;
    esac
  done
  {
    for d in $(printf '%s\n' "${dirs[@]}" Cargo.toml Cargo.lock rust-toolchain.toml | sort -u); do
      echo "$d $(git -C "$repo" rev-parse --verify --quiet "$commit:$d" || echo absent)"
    done
    echo "patch $(git -C "$repo" hash-object "$pins/${names[$i]}.patch")"
  } | git -C "$repo" hash-object --stdin
}
# The pins to run (todo, in order) and the ones skipped as unchanged: a
# skipped pin's since[I] is the commit its record was made at.
record=$out/valid.tsv
short=$(git -C "$repo" rev-parse --short "$commit")
keys=() since=() todo=()
for i in "${!names[@]}"; do
  keys[i]=$(key_of "$i")
  since[i]=
  if [ "$fresh" = no ] && [ -f "$record" ]; then
    since[i]=$(awk -F '\t' -v n="${names[$i]}" -v k="${keys[$i]}" '$1 == n && $2 == k { print $3 }' "$record")
  fi
  [ -n "${since[$i]}" ] || todo+=("$i")
done
[ "${#todo[@]}" -eq 0 ] || [ "$jobs" -le "${#todo[@]}" ] || jobs=${#todo[@]}
[ "$jobs" -ge 1 ] || jobs=1
threads=$(( cores / jobs ))
[ "$threads" -ge 1 ] || threads=1

# Remove every worktree this script makes, registration and directory
# both, so that a run that died without cleaning up does not stop the
# next one.
remove_trees() {
  local tree
  for tree in "$out"/base/tree "$out"/worker-*/tree; do
    [ -e "$tree" ] || continue
    git -C "$repo" worktree remove --force "$tree" 2> /dev/null || true
    rm -rf "$tree"
  done
  git -C "$repo" worktree prune
}

# kill_tree PID: stop PID and every process under it, children first, as
# in mutate.sh.
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

trap remove_trees EXIT
trap stop INT TERM
remove_trees

# Nothing from an earlier run may stand in for a result of this one.
rm -f "$out/summary.md" "$out"/base/*.log
for i in "${todo[@]}"; do
  rm -rf "${out:?}/${names[$i]}"
  mkdir -p "$out/${names[$i]}"
done

# result_in LOG TEST: "passes" or "fails" if libtest's log LOG has TEST's
# line ("test NAME ... ok" or "... FAILED"), "missing" if it has none but
# the run got to its "test result:" line, and nothing if the run did not
# get that far, which says nothing about TEST.
result_in() {
  [ -f "$1" ] || return 0
  if grep -qxF "test $2 ... ok" "$1"; then
    echo passes
  elif grep -qxF "test $2 ... FAILED" "$1"; then
    echo fails
  elif grep -q '^test result: ' "$1"; then
    echo missing
  fi
}

# ended_by LOG: how the test process of LOG ended, if it started (libtest's
# "running N tests") and did not exit successfully: "signal: 6, SIGABRT:
# process abort signal", say, or "exit status: 101", as cargo's "process
# didn't exit successfully" line gives it. Nothing if it never started.
ended_by() {
  [ -f "$1" ] && grep -qE '^running [0-9]+ tests?$' "$1" || return 0
  sed -nE "s/^ *process didn't exit successfully: .* \\((signal: [0-9]+[^)]*|exit status: [0-9]+)\\)\$/\\1/p" "$1" | tail -1
}

# died_of LOG: how the test process of LOG died, if it died of its own
# accord before it reported every test: "signal: 6, SIGABRT: process abort
# signal" for an abort or a stack overflow, or "exit status: 3" for a call
# to exit. Nothing if the process never started, which is the harness
# failing, or if the signal came from outside (SIGHUP, SIGINT, SIGKILL,
# SIGTERM: a user, the OOM killer, a runner shutting down), which says
# nothing about the test.
died_of() {
  local how
  how=$(ended_by "$1")
  case $how in
    "signal: 1,"* | "signal: 2,"* | "signal: 9,"* | "signal: 15,"*) ;;
    *) echo "$how" ;;
  esac
}

# run_one TREE PACKAGE TEST LOG: run one test at TREE in release, and
# print "passes", "fails", "hangs", "missing" (the test does not exist),
# or "not checked" with why. A test whose process died of its own accord
# before it reported fails: with one test to the process, the test is what
# died.
run_one() {
  local tree=$1 package=$2 test=$3 log=$4 status=0 result
  (cd "$tree" && timeout 600 cargo test --release --locked -j "$threads" -p "$package" --lib -- "$test" --exact) \
    > "$log" 2>&1 || status=$?
  result=$(result_in "$log" "$test")
  if [ "$status" -eq 124 ]; then
    echo hangs
  elif [ -n "$result" ]; then
    echo "$result"
  elif [ -n "$(died_of "$log")" ]; then
    echo fails
  elif [ -n "$(ended_by "$log")" ]; then
    echo "not checked: its test process was stopped from outside ($(ended_by "$log"))"
  else
    echo "not checked: its test run stopped (status $status)"
  fi
}

# why LOG: the first line of the failure, which for a pinned seed is
# "FAILED <invariant>: ...", and for a unit test the panic message. For a
# test whose process died, how, and the runtime's last word if it left one
# (a stack overflow does).
why() {
  local first how=
  first=$(awk '/^FAILED / { print; exit } /panicked at/ { getline; print; exit } /^fatal runtime error: / { print; exit }' "$1")
  grep -qE '^test .* \.\.\. FAILED$' "$1" || how=$(died_of "$1")
  [ -z "$how" ] || first="the test process died ($how)${first:+: $first}"
  printf '%s\n' "$first" | cut -c1-200
}

echo "pins: ${#names[@]} of $short, $(( ${#names[@]} - ${#todo[@]} )) unchanged since their last check, $jobs workers of $threads threads, in $out"

# Every worktree is made before anything runs, one at a time: two `git
# worktree add` at once can read the other's half-made entry under
# .git/worktrees and fail ("failed to read .../commondir"). A worktree
# that cannot be made leaves its pins not checked.
base=$out/base
mkdir -p "$base"
base_tree=no
trees=()
if [ "${#todo[@]}" -gt 0 ]; then
  base_tree=yes
  git -C "$repo" worktree add --quiet --detach "$base/tree" "$commit" 2> "$base/worktree.log" || base_tree=no
  for (( w = 0; w < jobs; w++ )); do
    mkdir -p "$out/worker-$w"
    trees[w]=yes
    git -C "$repo" worktree add --quiet --detach "$out/worker-$w/tree" "$commit" 2> "$out/worker-$w/worktree.log" ||
      trees[w]=no
  done
fi

# The unpatched half: one build of REV, every named test in it. Each test
# must pass here.
export CARGO_TARGET_DIR=$base/target
# The tests of the pins to run, by index.
run_tests=()
for i in "${todo[@]}"; do
  for (( j = first[i]; j < first[i] + count[i]; j++ )); do run_tests+=("$j"); done
done
if [ "$base_tree" = yes ]; then
  for package in $(for j in "${run_tests[@]}"; do echo "${packages[$j]}"; done | sort -u); do
    filters=()
    for j in "${run_tests[@]}"; do
      [ "${packages[$j]}" = "$package" ] && filters+=("${tests[$j]}")
    done
    (cd "$base/tree" && cargo test --release --locked -p "$package" --lib -- --exact "${filters[@]}") \
      > "$base/$package.log" 2>&1 || true
  done
fi
for i in "${todo[@]}"; do
  for (( j = first[i]; j < first[i] + count[i]; j++ )); do
    if [ "$base_tree" = no ]; then
      echo "not checked: the unpatched worktree could not be made"
      continue
    fi
    log=$base/${packages[$j]}.log
    result=$(result_in "$log" "${tests[$j]}")
    # A process that died partway reported none of the tests it had not
    # finished. Each of those runs again on its own, which says whether it
    # was the one that died.
    if [ -z "$result" ] && [ -n "$(died_of "$log")" ]; then
      result=$(run_one "$base/tree" "${packages[$j]}" "${tests[$j]}" "$base/test-$j.log")
    fi
    echo "${result:-not checked: the unpatched run stopped}"
  done > "$out/${names[$i]}/unpatched"
done

# The patched half. Worker W takes the pins to run W, W + J, W + 2J, ... A pin's
# results reach DIR/NAME/patched only once all its tests have run, so a
# pin whose worker died midway has none.
work() {
  local w=$1 tree=$out/worker-$1/tree t i j k name dir result package build
  export CARGO_TARGET_DIR=$out/worker-$1/target
  for (( t = w; t < ${#todo[@]}; t += jobs )); do
    i=${todo[$t]}
    name=${names[$i]} dir=$out/${names[$i]} result=
    git -C "$tree" checkout --quiet --force --detach "$commit"
    git -C "$tree" clean --quiet -fdx
    build=()
    for package in $(printf '%s\n' "${packages[@]:${first[$i]}:${count[$i]}}" | sort -u); do
      build+=(-p "$package")
    done
    if ! git -C "$tree" apply "$pins/$name.patch" 2> "$dir/apply.log"; then
      result=does-not-apply
    elif ! (cd "$tree" && cargo test --release --locked -j "$threads" "${build[@]}" --lib --no-run) \
      > "$dir/build.log" 2>&1; then
      # A compile error is the patch's; a build that stopped without one
      # (a killed compiler, a full disk) says nothing about the pin.
      if grep -q '^error: could not compile .* due to .*previous error' "$dir/build.log"; then
        result=does-not-build
      else
        result="not checked: its build stopped"
      fi
    fi
    for (( k = 0; k < count[i]; k++ )); do
      j=$(( first[i] + k ))
      if [ -n "$result" ]; then
        echo "$result"
      else
        run_one "$tree" "${packages[$j]}" "${tests[$j]}" "$dir/test-$k.log"
      fi
    done > "$dir/patched.part"
    mv "$dir/patched.part" "$dir/patched"
    # Joined with ", ", which a result's own text may also hold.
    echo "  $name: $(awk 'NR > 1 { printf ", " } { printf "%s", $0 }' "$dir/patched") with its patch"
  done
}
for (( w = 0; w < jobs; w++ )); do
  [ "${trees[w]:-no}" = yes ] || continue
  work "$w" &
  pids[w]=$!
done
# A worker's status says whether it got to the end of its pins.
ended=()
for w in "${!pids[@]}"; do
  ended[w]=0
  wait "${pids[w]}" || ended[w]=$?
done

# Why pin I has no patched results: its worker never started or died.
unrun() {
  local t w
  for t in "${!todo[@]}"; do [ "${todo[$t]}" = "$1" ] && w=$(( t % jobs )); done
  if [ "${trees[w]}" = no ]; then
    echo "not checked: worker $w's worktree could not be made"
  elif [ "${ended[w]}" -ne 0 ]; then
    echo "not checked: worker $w died (status ${ended[w]})"
  else
    echo "not checked: worker $w left no result"
  fi
}

# One row per test, the pin's name on its first, and the failure's first
# line says how a test failed with the patch. A test fails the check as
# soon as either half has a result that is wrong: it does not pass as it
# is, or it does not fail with the patch. That makes its pin invalid,
# whatever happened to its other tests, since nothing they could show would
# make it valid. A pin none of whose tests failed the check is not checked
# if one of them was not run both ways, and valid otherwise.
invalid=0 unchecked=0 checked=0 skipped=0
rows=() states=()
for i in "${!names[@]}"; do
  name=${names[$i]} dir=$out/${names[$i]}
  if [ -n "${since[$i]}" ]; then
    states[i]=skipped skipped=$(( skipped + 1 )) cell="$name (unchanged since ${since[$i]})"
    for (( k = 0; k < count[i]; k++ )); do
      j=$(( first[i] + k ))
      rows+=("| $cell | \`${packages[$j]} ${tests[$j]}\` | skipped | skipped |")
      cell=
    done
    continue
  fi
  [ -f "$dir/patched" ] || for (( k = 0; k < count[i]; k++ )); do unrun "$i"; done > "$dir/patched"
  state=valid
  for (( k = 0; k < count[i]; k++ )); do
    unpatched=$(sed -n "$(( k + 1 ))p" "$dir/unpatched")
    shown=$(sed -n "$(( k + 1 ))p" "$dir/patched")
    case $unpatched in "not checked"*) ;; passes) ;; *) state=invalid ;; esac
    case $shown in "not checked"*) ;; fails) ;; *) state=invalid ;; esac
    case "$unpatched $shown" in
      *"not checked"*) [ "$state" = invalid ] || state=unchecked ;;
      *) checked=$(( checked + 1 )) ;;
    esac
  done
  states[i]=$state
  case $state in
    invalid) cell="**$name** (invalid)"; invalid=$(( invalid + 1 )) ;;
    unchecked) cell="*$name* (not checked)"; unchecked=$(( unchecked + 1 )) ;;
    *) cell=$name ;;
  esac
  for (( k = 0; k < count[i]; k++ )); do
    j=$(( first[i] + k ))
    unpatched=$(sed -n "$(( k + 1 ))p" "$dir/unpatched")
    shown=$(sed -n "$(( k + 1 ))p" "$dir/patched")
    [ "$shown" != fails ] || shown="fails: $(why "$dir/test-$k.log")"
    rows+=("| $cell | \`${packages[$j]} ${tests[$j]}\` | $unpatched | ${shown//|/\\|} |")
    cell=
  done
done
for name in "${unguarded[@]}"; do
  rows+=("| **$name** (invalid) | \`delocal-sim regressions::$name\` | - | no patch in scripts/pins |")
  invalid=$(( invalid + 1 ))
done

total=$(( ${#names[@]} + ${#unguarded[@]} ))
{
  if [ "$unchecked" -gt 0 ]; then
    echo "**The check did not finish:** $unchecked of $total pins were not checked, so they are neither valid nor invalid. Each of their rows says why. A pin in bold is invalid: one of its tests was checked and failed the check, whatever happened to its others."
    echo
  fi
  echo '| Pin | Test | Unpatched | With its patch |'
  echo '|---|---|---|---|'
  printf '%s\n' "${rows[@]}"
  echo
  seeds=$(for j in "${run_tests[@]}"; do echo "${tests[$j]}"; done | grep -c '^regressions::' || true)
  echo "$checked of ${#run_tests[@]} named tests checked both ways ($seeds pinned seeds, $(( ${#run_tests[@]} - seeds )) unit tests)."
  [ "$skipped" -eq 0 ] || echo "$skipped pins skipped: nothing their check depends on changed since it last found them valid."
  if [ "$unchecked" -gt 0 ]; then
    echo "The check did not finish: $unchecked of $total pins not checked, $invalid invalid, $(( total - unchecked - invalid )) guard their fix."
  else
    echo "$(( total - invalid )) of $total pins guard their fix."
  fi
} > "$out/summary.md"
# The record, written whole: every pin found valid now under its key, every
# pin skipped as it was, and every pin this run did not look at as it was.
# A pin found invalid or not checked loses its line.
{
  for i in "${!names[@]}"; do
    case ${states[$i]} in
      valid) printf '%s\t%s\t%s\n' "${names[$i]}" "${keys[$i]}" "$short" ;;
      skipped) printf '%s\t%s\t%s\n' "${names[$i]}" "${keys[$i]}" "${since[$i]}" ;;
    esac
  done
  if [ -f "$record" ]; then
    awk -F '\t' -v seen=" ${names[*]} " 'index(seen, " " $1 " ") == 0' "$record"
  fi
} | sort > "$record.new"
mv "$record.new" "$record"
echo
cat "$out/summary.md"
if [ "$unchecked" -gt 0 ]; then
  exit 3
fi
[ "$invalid" -eq 0 ] || exit 1
