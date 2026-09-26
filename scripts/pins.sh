#!/usr/bin/env bash
# Check that every pin guards its fix (DESIGN.md §14.4).
#
#   scripts/pins.sh [--rev REV] [--jobs J] [--out DIR] [NAME...]
#
# A pinned seed guards its fix only while its history still reaches the
# bug, so every pin NAME is stored with scripts/pins/NAME.patch, a patch
# that disables the fix it guards. The patch starts with a paragraph that
# says what it disables and one line naming the test it must break:
#
#   test: PACKAGE TEST
#
# For a pinned seed that is `delocal-sim regressions::NAME`. A pin whose
# fix no seed reaches is replaced by a unit test next to the fix, and its
# patch names that test instead. `git apply` skips the text before the
# first `diff`.
#
# A pin is valid only if its test passes at REV (default HEAD) and fails
# with its patch applied. The script first checks that every test in
# crates/sim/src/regressions.rs has a patch, then builds REV once and runs
# every named test there, then applies each patch to a clean checkout of
# REV and runs its test alone. J workers (default: one per four cores)
# each keep a worktree and a target directory and take the patches in
# turn, resetting the worktree to REV before each one, so a worker's
# second build recompiles only what its patches touch. Tests run in
# release, as CI's pinned job runs them, and a test that runs for more
# than ten minutes counts as not failing.
#
# With NAMEs only those pins are checked, and the coverage check is
# skipped. The logs of pin NAME go to DIR/NAME, by default
# target/pins/NAME, and DIR/summary.md holds the table the nightly puts in
# an issue. Exit status 0 means every pin checked is valid.

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

# Every test in regressions.rs, read from REV: a `#[test]` line, perhaps
# an `#[ignore]`, then its `fn`.
pinned=$(git -C "$repo" show "$commit:crates/sim/src/regressions.rs" |
  awk '/^#\[test\]/ { t = 1; next } t && /^fn / { sub(/^fn /, ""); sub(/\(.*/, ""); print; t = 0; next } !/^#\[/ { t = 0 }')

# The pins to check, and the test each one names.
names=() packages=() tests=()
add() {
  local name=$1 patch=$pins/$1.patch line
  [ -f "$patch" ] || { echo "no patch $patch" >&2; exit 2; }
  line=$(grep -E '^test: ' "$patch" || true)
  [ "$(echo "$line" | grep -c .)" -eq 1 ] || { echo "$patch needs exactly one 'test: PACKAGE TEST' line" >&2; exit 2; }
  read -r _ package test extra <<< "$line"
  [ -n "$test" ] && [ -z "${extra:-}" ] || { echo "$patch: '$line' is not 'test: PACKAGE TEST'" >&2; exit 2; }
  # A pinned seed's patch is named after its test, so a pin can be found
  # from either side.
  case $test in
    regressions::*) [ "$test" = "regressions::$name" ] || { echo "$patch names $test, not regressions::$name" >&2; exit 2; } ;;
  esac
  names+=("$name") packages+=("$package") tests+=("$test")
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

# run_one TREE PACKAGE TEST LOG: run one test at TREE in release, and
# print "passes", "fails", "hangs" or "missing" (the test does not exist).
# libtest prints "test NAME ... ok" or "... FAILED" for every test it runs.
run_one() {
  local tree=$1 package=$2 test=$3 log=$4 status=0
  (cd "$tree" && timeout 600 cargo test --release --locked -j "$threads" -p "$package" --lib -- "$test" --exact) \
    > "$log" 2>&1 || status=$?
  if [ "$status" -eq 124 ]; then
    echo hangs
  elif grep -qxF "test $test ... ok" "$log"; then
    echo passes
  elif grep -qxF "test $test ... FAILED" "$log"; then
    echo fails
  else
    echo missing
  fi
}

# why LOG: the first line of the failure, which for a pinned seed is
# "FAILED <invariant>: ...", and for a unit test the panic message.
why() {
  awk '/^FAILED / { print; exit } /panicked at/ { getline; print; exit }' "$1" | cut -c1-200
}

echo "pins: ${#names[@]} of $(git -C "$repo" rev-parse --short "$commit"), $jobs workers of $threads threads, in $out"

# The unpatched half: one build of REV, every named test in it. Each pin
# must pass here.
base=$out/base
mkdir -p "$base"
git -C "$repo" worktree add --quiet --detach "$base/tree" "$commit"
export CARGO_TARGET_DIR=$base/target
for package in $(printf '%s\n' "${packages[@]}" | sort -u); do
  filters=()
  for i in "${!names[@]}"; do
    [ "${packages[$i]}" = "$package" ] && filters+=("${tests[$i]}")
  done
  (cd "$base/tree" && cargo test --release --locked -p "$package" --lib -- --exact "${filters[@]}") \
    > "$base/$package.log" 2>&1 || true
done
for i in "${!names[@]}"; do
  mkdir -p "$out/${names[$i]}"
  log=$base/${packages[$i]}.log
  if grep -qxF "test ${tests[$i]} ... ok" "$log"; then
    echo passes
  elif grep -qxF "test ${tests[$i]} ... FAILED" "$log"; then
    echo fails
  else
    echo missing
  fi > "$out/${names[$i]}/unpatched"
done

# The patched half. Worker W takes pins W, W + J, W + 2J, ...
work() {
  local w=$1 dir=$out/worker-$1 i name patch result
  local tree=$dir/tree
  mkdir -p "$dir"
  git -C "$repo" worktree add --quiet --detach "$tree" "$commit"
  export CARGO_TARGET_DIR=$dir/target
  for (( i = w; i < ${#names[@]}; i += jobs )); do
    name=${names[$i]} patch=$pins/${names[$i]}.patch
    rm -f "$out/$name/patched" "$out/$name"/*.log
    git -C "$tree" checkout --quiet --force --detach "$commit"
    git -C "$tree" clean --quiet -fdx
    if ! git -C "$tree" apply "$patch" 2> "$out/$name/apply.log"; then
      echo does-not-apply > "$out/$name/patched"
      continue
    fi
    if ! (cd "$tree" && cargo test --release --locked -j "$threads" -p "${packages[$i]}" --lib --no-run) \
      > "$out/$name/build.log" 2>&1; then
      echo does-not-build > "$out/$name/patched"
      continue
    fi
    result=$(run_one "$tree" "${packages[$i]}" "${tests[$i]}" "$out/$name/test.log")
    echo "$result" > "$out/$name/patched"
    echo "  $name: $result with its patch"
  done
}
for (( w = 0; w < jobs; w++ )); do
  work "$w" &
  pids+=($!)
done
wait

# One row per pin. A pin is valid if it passes unpatched and fails
# patched; the failure's first line says how.
status=0 invalid=0
{
  echo '| Pin | Test | Unpatched | With its patch |'
  echo '|---|---|---|---|'
  for i in "${!names[@]}"; do
    name=${names[$i]} dir=$out/${names[$i]}
    unpatched=$(cat "$dir/unpatched" 2> /dev/null || echo error)
    patched=$(cat "$dir/patched" 2> /dev/null || echo error)
    shown=$patched
    [ "$patched" != fails ] || shown="fails: $(why "$dir/test.log")"
    if [ "$unpatched" != passes ] || [ "$patched" != fails ]; then
      name="**$name** (invalid)"
      invalid=$(( invalid + 1 ))
    fi
    echo "| $name | \`${packages[$i]} ${tests[$i]}\` | $unpatched | ${shown//|/\\|} |"
  done
  for name in "${unguarded[@]}"; do
    echo "| **$name** (invalid) | \`delocal-sim regressions::$name\` | - | no patch in scripts/pins |"
    invalid=$(( invalid + 1 ))
  done
} > "$out/summary.md"
[ "$invalid" -eq 0 ] || status=1
echo
cat "$out/summary.md"
echo
total=$(( ${#names[@]} + ${#unguarded[@]} ))
echo "$(( total - invalid )) of $total pins guard their fix."
exit "$status"
