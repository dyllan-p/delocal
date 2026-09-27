#!/usr/bin/env bash
# Check that every pin guards its fix (DESIGN.md §14.4).
#
#   scripts/pins.sh [--rev REV] [--jobs J] [--out DIR] [NAME...]
#
# A pinned seed guards its fix only while its history still reaches the
# bug, so every pin NAME is stored with scripts/pins/NAME.patch, a patch
# that disables the fix it guards. The patch starts with a paragraph that
# says what it disables and one line for each test that guards that fix:
#
#   test: PACKAGE TEST
#
# For a pinned seed the first is `delocal-sim regressions::NAME`, and any
# unit tests of the same fix follow. A pin whose fix no seed reaches is
# replaced by a unit test next to the fix, and its patch names only unit
# tests. `git apply` skips the text before the first `diff`.
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
# A pin the script could not run is not checked, which makes it neither
# valid nor invalid: its worker died, its worktree could not be made, or a
# build or test run of it stopped before the end without a compile error
# or a result. If any pin is not checked, the check did not finish.
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

rev=HEAD jobs= out=
while [ $# -gt 0 ]; do
  case $1 in
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
  local name=$1 patch=$pins/$1.patch line package test extra seen=/ seeded=no start=${#tests[@]}
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
    esac
    [ "$package $test" != "delocal-sim regressions::$name" ] || seeded=yes
    packages+=("$package") tests+=("$test")
  done < <(sed -n -e '/^diff /q' -e '/^test: /p' "$patch")
  [ "${#tests[@]}" -gt "$start" ] || { echo "$patch needs a 'test: PACKAGE TEST' line" >&2; exit 2; }
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
[ "$jobs" -le "${#names[@]}" ] || jobs=${#names[@]}
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
for name in "${names[@]}"; do
  rm -rf "${out:?}/$name"
  mkdir -p "$out/$name"
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

# run_one TREE PACKAGE TEST LOG: run one test at TREE in release, and
# print "passes", "fails", "hangs", "missing" (the test does not exist),
# or "not checked" with why.
run_one() {
  local tree=$1 package=$2 test=$3 log=$4 status=0 result
  (cd "$tree" && timeout 600 cargo test --release --locked -j "$threads" -p "$package" --lib -- "$test" --exact) \
    > "$log" 2>&1 || status=$?
  result=$(result_in "$log" "$test")
  if [ "$status" -eq 124 ]; then
    echo hangs
  elif [ -n "$result" ]; then
    echo "$result"
  else
    echo "not checked: its test run stopped (status $status)"
  fi
}

# why LOG: the first line of the failure, which for a pinned seed is
# "FAILED <invariant>: ...", and for a unit test the panic message.
why() {
  awk '/^FAILED / { print; exit } /panicked at/ { getline; print; exit }' "$1" | cut -c1-200
}

echo "pins: ${#names[@]} of $(git -C "$repo" rev-parse --short "$commit"), $jobs workers of $threads threads, in $out"

# Every worktree is made before anything runs, one at a time: two `git
# worktree add` at once can read the other's half-made entry under
# .git/worktrees and fail ("failed to read .../commondir"). A worktree
# that cannot be made leaves its pins not checked.
base=$out/base
mkdir -p "$base"
base_tree=yes
git -C "$repo" worktree add --quiet --detach "$base/tree" "$commit" 2> "$base/worktree.log" || base_tree=no
trees=()
for (( w = 0; w < jobs; w++ )); do
  mkdir -p "$out/worker-$w"
  trees[w]=yes
  git -C "$repo" worktree add --quiet --detach "$out/worker-$w/tree" "$commit" 2> "$out/worker-$w/worktree.log" ||
    trees[w]=no
done

# The unpatched half: one build of REV, every named test in it. Each test
# must pass here.
export CARGO_TARGET_DIR=$base/target
if [ "$base_tree" = yes ]; then
  for package in $(printf '%s\n' "${packages[@]}" | sort -u); do
    filters=()
    for j in "${!tests[@]}"; do
      [ "${packages[$j]}" = "$package" ] && filters+=("${tests[$j]}")
    done
    (cd "$base/tree" && cargo test --release --locked -p "$package" --lib -- --exact "${filters[@]}") \
      > "$base/$package.log" 2>&1 || true
  done
fi
for i in "${!names[@]}"; do
  for (( j = first[i]; j < first[i] + count[i]; j++ )); do
    if [ "$base_tree" = no ]; then
      echo "not checked: the unpatched worktree could not be made"
    else
      result=$(result_in "$base/${packages[$j]}.log" "${tests[$j]}")
      echo "${result:-not checked: the unpatched run stopped}"
    fi
  done > "$out/${names[$i]}/unpatched"
done

# The patched half. Worker W takes pins W, W + J, W + 2J, ... A pin's
# results reach DIR/NAME/patched only once all its tests have run, so a
# pin whose worker died midway has none.
work() {
  local w=$1 tree=$out/worker-$1/tree i j k name dir result package build
  export CARGO_TARGET_DIR=$out/worker-$1/target
  for (( i = w; i < ${#names[@]}; i += jobs )); do
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
    echo "  $name: $(paste -sd, "$dir/patched" | sed 's/,/, /g') with its patch"
  done
}
for (( w = 0; w < jobs; w++ )); do
  [ "${trees[w]}" = yes ] || continue
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
  local w=$(( $1 % jobs ))
  if [ "${trees[w]}" = no ]; then
    echo "not checked: worker $w's worktree could not be made"
  elif [ "${ended[w]}" -ne 0 ]; then
    echo "not checked: worker $w died (status ${ended[w]})"
  else
    echo "not checked: worker $w left no result"
  fi
}

# One row per test, the pin's name on its first. A pin is not checked if
# any of its tests was not run both ways; otherwise it is valid if each of
# its tests passes unpatched and fails patched, and the failure's first
# line says how.
invalid=0 unchecked=0 checked=0
rows=()
for i in "${!names[@]}"; do
  name=${names[$i]} dir=$out/${names[$i]}
  [ -f "$dir/patched" ] || for (( k = 0; k < count[i]; k++ )); do unrun "$i"; done > "$dir/patched"
  state=valid
  for (( k = 0; k < count[i]; k++ )); do
    case "$(sed -n "$(( k + 1 ))p" "$dir/unpatched") $(sed -n "$(( k + 1 ))p" "$dir/patched")" in
      *"not checked"*) state=unchecked ;;
      "passes fails") checked=$(( checked + 1 )) ;;
      *) checked=$(( checked + 1 )); [ "$state" = unchecked ] || state=invalid ;;
    esac
  done
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
    echo "**The check did not finish:** $unchecked of $total pins were not checked, so they are neither valid nor invalid. Each of their rows says why."
    echo
  fi
  echo '| Pin | Test | Unpatched | With its patch |'
  echo '|---|---|---|---|'
  printf '%s\n' "${rows[@]}"
  echo
  echo "$checked of ${#tests[@]} named tests checked both ways."
  if [ "$unchecked" -gt 0 ]; then
    echo "The check did not finish: $unchecked of $total pins not checked, $invalid invalid, $(( total - unchecked - invalid )) guard their fix."
  else
    echo "$(( total - invalid )) of $total pins guard their fix."
  fi
} > "$out/summary.md"
echo
cat "$out/summary.md"
if [ "$unchecked" -gt 0 ]; then
  exit 3
fi
[ "$invalid" -eq 0 ] || exit 1
