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

## Crate boundary (DESIGN.md Appendix B)

Two crates:

- `crates/engine` (package `delocal-engine`): pure sync logic. Index, versions,
  batches, conflicts, brake, quarantine, revert semantics, and the simulator. It takes
  events and returns actions. It does no I/O.
- `crates/delocal` (package `delocal`): the binary. Daemon, CLI, filesystem, SQLite,
  networking, Tailscale, service install.

`crates/engine` must never depend on `tokio`, `notify`, `rusqlite` or anything else
that does I/O or touches the world. If it needs to, the boundary is in the wrong place:
move the code, do not add the dependency.

CI enforces this. A step runs `cargo tree -p delocal-engine -e normal` and fails if the
output matches `tokio|notify|rusqlite|reqwest|hyper|mio`. Do not weaken or skip that
step; extend the pattern if a new I/O crate appears in Appendix A.

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

## Toolchain

The toolchain is pinned to an exact version in `rust-toolchain.toml`, not a floating
channel, because CI denies clippy warnings and a new stable would otherwise break CI on
Rust release days for reasons unrelated to our code. Bump it deliberately, in its own
commit, roughly every six weeks.
