# CLAUDE.md

## What delocal is

`delocal` keeps a folder identical across all of your machines. It runs over your
tailnet, so it has no accounts, no device IDs, no ports to open and no config file.
You install it with one command, run `delocal up`, and your machines find each other.
It is a single static Rust binary with a background service and a small, modern CLI.
It is built to never lose a file: every remote change that overwrites or deletes
something goes through a local trash, and any change large enough to be a mistake is
held for approval before it spreads. Sync is not backup; the README says so first.

## DESIGN.md wins

`DESIGN.md` is the source of truth. It wins over the code and over your own judgement.
When behaviour changes, the document changes first, then the code. If the code and the
document disagree, the code is wrong. If something in the document seems wrong or
ambiguous, stop and ask rather than deciding yourself. Items marked **[decision]** use
the defaults in §17 unless told otherwise; items marked **[verify]** are checked against
reality in the phase that needs them, not before.

`main` takes pull requests only, with linear history and green CI; nobody pushes to it
directly. A new DESIGN.md draft therefore arrives in the working tree uncommitted, and the
PR that implements it commits the draft as its first commit, so the document still changes
before the code in every history.

## Crate boundary (DESIGN.md Appendix B)

Three crates:

- `crates/engine` (package `delocal-engine`): pure sync logic. Index, versions,
  batches, conflicts, brake, quarantine, revert semantics. One `Engine` per node that
  consumes `Event`s and returns `Action`s. It does no I/O and never reads a clock or a
  random number generator: timestamps and fresh identifiers arrive in events.
- `crates/sim` (package `delocal-sim`): the deterministic simulator. An in-memory host
  for N engines with a seeded PRNG, the invariants I1 to I6, a proptest test for
  shrinking, and a binary for the nightly run. Dev-only; it depends on the engine and
  nothing depends on it.
- `crates/delocal` (package `delocal`): the binary. Daemon, CLI, filesystem, SQLite,
  networking, Tailscale, service install. Wire messages (`Hello`, the `Batch` envelope,
  `RequestFile`, and so on) live here and contain engine value types.

`crates/engine` must never depend on `tokio`, `notify`, `rusqlite`, `rand` or anything
else that does I/O or touches the world, and its source never uses `std::fs`,
`std::net`, `SystemTime` or `Instant`. It may depend on `serde` with `derive` so that
its value types serialise directly, and on `proptest` for tests. The boundary is about
I/O and runtimes, not about traits over data. If the engine needs anything else, the
boundary is in the wrong place: move the code, do not add the dependency.

CI enforces this with two steps. One runs `cargo tree -p delocal-engine -e normal` and
fails if the output matches `tokio|notify|rusqlite|reqwest|hyper|mio|rand`. The other
greps `crates/engine/src` for `std::fs`, `std::net`, `SystemTime` and `Instant` and fails
on any hit, comments included. Do not weaken or skip either step; extend the patterns
if a new I/O crate appears in Appendix A.

## Working conventions

- Small commits, one concern each, imperative subject lines. I read every diff to learn
  Rust, so when a choice is not obvious, explain it in a short paragraph in the commit body.
- Only crates listed in DESIGN.md Appendix A. Anything else: ask first, with a reason.
- No unsafe. No unwrap or expect outside tests. Allow individual clippy lints only with
  a comment saying why.
- Every module starts with a doc comment naming the DESIGN.md section it implements.
- Tests live next to the code they test. Property tests use proptest.
- At the end of each phase, stop and write a summary: what was built, what you are unsure
  about, what the next phase needs from me. Do not start the next phase.
- From Phase 1 on, every step is a branch and a pull request against `main`, never a
  direct commit. I review and rebase-merge on GitHub. Open one PR at a time and stop
  after opening it; do not start the next step until I say the PR is merged.

## Toolchain

The toolchain is pinned to an exact version in `rust-toolchain.toml`, not a floating
channel, because CI denies clippy warnings and a new stable would otherwise break CI on
Rust release days for reasons unrelated to our code. Bump it deliberately, in its own
commit, roughly every six weeks.
