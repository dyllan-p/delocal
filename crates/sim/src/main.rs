//! `delocal-sim`: sweep seeds through the simulator (DESIGN.md §14.1).
//!
//! ```text
//! delocal-sim --seeds N [--start S] [--steps K] [--corruption P] [--drop-watcher P]
//!             [--delay-ms LO..HI] [--crash-after-rename P] [--crash-between-renames P]
//!             [--nodes N] [--keep-going] [--jobs J] [--digests]
//! ```
//!
//! Seeds run on J worker threads, by default one for each core the system
//! offers; with J of 1 they run on the main thread. Every seed's run is
//! independent and deterministic, so J changes only how long a sweep takes:
//! results are reported in seed order, as one thread would report them.
//!
//! On the first failing seed the step list is delta-debugged down to a
//! minimal list that still fails, and the report (seed, knobs, steps, the
//! invariant and its counterexample) is printed. With `--keep-going` the
//! sweep runs every seed instead, shrinks nothing, and ends with the failing
//! seeds grouped by invariant and a pass/fail count: what CI's advisory
//! sweep prints while the random sweep is still red (§14.1). Exit status 1
//! on any failure either way.
//!
//! Standard output depends only on the arguments, not on J or the time a
//! run takes, so two runs of the same seeds can be compared with `cmp`.
//! Progress and wall-clock times go to standard error. `--digests` adds a
//! line per passing seed with its outcome's fingerprint, the hash I6
//! compares, which covers every node's final engine state, filesystem and
//! trash and the whole action log.

// The binary may not unwrap or expect either (CLAUDE.md); errors go to stderr.
// Failure is large by design; see lib.rs.
#![allow(clippy::result_large_err)]
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use delocal_sim::{Failure, Knobs, Outcome, Step, run_twice, steps};

struct Args {
    seeds: u64,
    start: u64,
    steps: usize,
    knobs: Knobs,
    keep_going: bool,
    jobs: NonZeroUsize,
    digests: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seeds: 1000,
        start: 0,
        steps: 400,
        knobs: Knobs::default(),
        keep_going: false,
        jobs: std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN),
        digests: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--seeds" => args.seeds = value()?.parse().map_err(|e| format!("--seeds: {e}"))?,
            "--start" => args.start = value()?.parse().map_err(|e| format!("--start: {e}"))?,
            "--steps" => args.steps = value()?.parse().map_err(|e| format!("--steps: {e}"))?,
            "--corruption" => {
                args.knobs.corruption =
                    value()?.parse().map_err(|e| format!("--corruption: {e}"))?
            }
            "--drop-watcher" => {
                args.knobs.drop_watcher = value()?
                    .parse()
                    .map_err(|e| format!("--drop-watcher: {e}"))?
            }
            "--crash-after-rename" => {
                args.knobs.crash_after_rename = value()?
                    .parse()
                    .map_err(|e| format!("--crash-after-rename: {e}"))?;
            }
            "--crash-between-renames" => {
                args.knobs.crash_between_renames = value()?
                    .parse()
                    .map_err(|e| format!("--crash-between-renames: {e}"))?;
            }
            "--nodes" => {
                args.knobs.nodes = Some(value()?.parse().map_err(|e| format!("--nodes: {e}"))?)
            }
            "--delay-ms" => {
                let v = value()?;
                let (lo, hi) = v
                    .split_once("..")
                    .ok_or_else(|| "--delay-ms wants LO..HI".to_owned())?;
                args.knobs.delay_ms = (
                    lo.parse().map_err(|e| format!("--delay-ms: {e}"))?,
                    hi.parse().map_err(|e| format!("--delay-ms: {e}"))?,
                );
            }
            "--keep-going" => args.keep_going = true,
            "--jobs" => args.jobs = value()?.parse().map_err(|e| format!("--jobs: {e}"))?,
            "--digests" => args.digests = true,
            "--help" | "-h" => {
                return Err("usage: delocal-sim --seeds N [--start S] [--steps K] [--corruption P] [--drop-watcher P] [--delay-ms LO..HI] [--crash-after-rename P] [--crash-between-renames P] [--nodes N] [--keep-going] [--jobs J] [--digests]".to_owned());
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(args)
}

/// Shrink a failing step list by delta debugging: drop chunks, then single
/// steps, while the run still fails **with the same invariant**, so the
/// minimal list reproduces the failure that was found and not a different
/// one it happened to uncover on the way.
fn shrink(seed: u64, knobs: &Knobs, steps: &[Step], original: &Failure) -> Failure {
    let mut current = steps.to_vec();
    let mut last: Option<Failure> = None;
    let mut chunk = (current.len() / 2).max(1);
    while chunk >= 1 {
        let mut i = 0;
        let mut removed_any = false;
        while i < current.len() {
            let end = (i + chunk).min(current.len());
            let mut candidate = current.clone();
            candidate.drain(i..end);
            match run_twice(seed, knobs, &candidate) {
                Err(f) if f.invariant == original.invariant => {
                    current = candidate;
                    last = Some(f);
                    removed_any = true;
                }
                _ => i = end,
            }
        }
        if chunk == 1 && !removed_any {
            break;
        }
        chunk /= 2;
    }
    last.unwrap_or_else(|| original.clone())
}

/// Run every seed of the sweep on `args.jobs` threads and hand each result
/// to `report` in seed order. `report` returns false to end the sweep: it
/// is given no result after that one, and the workers stop once the seeds
/// they are running finish.
fn run_in_order(args: &Args, mut report: impl FnMut(u64, Result<Outcome, Failure>) -> bool) {
    let run = |i: u64| {
        let seed = args.start + i;
        let list = steps::generate(seed, &args.knobs, args.steps);
        run_twice(seed, &args.knobs, &list)
    };
    // One worker runs the seeds on this thread and starts no other. A
    // process that has ever had a second thread runs the simulator about a
    // tenth slower, even if that thread only sleeps, so a spawned worker
    // would make `--jobs 1` slower than the loop it replaces.
    if args.jobs.get() == 1 {
        for i in 0..args.seeds {
            if !report(args.start + i, run(i)) {
                return;
            }
        }
        return;
    }

    // How many seeds have been handed out, counting from `args.start`. Each
    // worker takes the next seed nobody has taken yet, rather than a fixed
    // share of the range, so no worker sits idle while another still has a
    // queue of slow seeds. `fetch_update` stops at `args.seeds`: once every
    // seed is taken it fails and leaves the count alone. Relaxed ordering is
    // enough, because the count only has to hand each number out once, which
    // any atomic update does, and the results travel over the channel, which
    // orders them itself.
    let taken = AtomicU64::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..args.jobs.get() {
            let (tx, taken, run) = (tx.clone(), &taken, &run);
            scope.spawn(move || {
                while let Ok(i) = taken.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |i| {
                    (i < args.seeds).then_some(i + 1)
                }) {
                    // A panicking seed is caught here and sent on like any
                    // result, to be raised again on the main thread when
                    // its turn comes. One thread would never have run the
                    // seeds after a failure that ends the sweep, so their
                    // panics must not end it first. `AssertUnwindSafe` is
                    // sound because nothing the run touched is used after a
                    // panic: the simulation is dropped as it unwinds.
                    let result = panic::catch_unwind(AssertUnwindSafe(|| run(i)));
                    // Sending fails once the main thread has stopped
                    // listening, which is how the workers learn the sweep
                    // is over.
                    if tx.send((i, result)).is_err() {
                        break;
                    }
                }
            });
        }
        // The workers now hold the only senders, so the loop below ends
        // when the last of them has finished.
        drop(tx);
        // Results arrive in the order the seeds finish. Each one waits in
        // `early` until every seed before it has been reported.
        let mut early = BTreeMap::new();
        let mut due = 0;
        for (i, result) in rx {
            early.insert(i, result);
            while let Some(result) = early.remove(&due) {
                let result = result.unwrap_or_else(|panic| panic::resume_unwind(panic));
                if !report(args.start + due, result) {
                    return;
                }
                due += 1;
            }
        }
    });
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "delocal-sim: seeds {}..{} with {} steps each; knobs: {}",
        args.start,
        args.start + args.seeds,
        args.steps,
        args.knobs
    );
    eprintln!("delocal-sim: --jobs {}", args.jobs);
    let started = std::time::Instant::now();
    let mut totals = delocal_sim::sim::Stats::default();
    // Failing seeds by invariant, for the --keep-going summary.
    let mut failed: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    // Without --keep-going, the first failing seed, shrunk after the sweep.
    let mut to_shrink: Option<(u64, Failure)> = None;
    run_in_order(&args, |seed, result| {
        let i = seed - args.start;
        match result {
            Ok(outcome) => {
                totals.batches_sent += outcome.stats.batches_sent;
                totals.held += outcome.stats.held;
                totals.paused += outcome.stats.paused;
                totals.approvals += outcome.stats.approvals;
                totals.denials += outcome.stats.denials;
                totals.reverts += outcome.stats.reverts;
                totals.crashes += outcome.stats.crashes;
                totals.fetches += outcome.stats.fetches;
                totals.not_available += outcome.stats.not_available;
                totals.mismatches += outcome.stats.mismatches;
                totals.changed_underneath += outcome.stats.changed_underneath;
                totals.stalled += outcome.stats.stalled;
                totals.conflict_copies += outcome.stats.conflict_copies;
                totals.events += outcome.stats.events;
                totals.crashes_between_renames += outcome.stats.crashes_between_renames;
                totals.displacements_undone += outcome.stats.displacements_undone;
                if args.digests {
                    println!("seed {seed} digest {}", hex(&outcome.fingerprint));
                }
                if (i + 1) % 100 == 0 {
                    eprintln!("  {} seeds ok ({:.0?})", i + 1, started.elapsed());
                }
                true
            }
            Err(first) if args.keep_going => {
                println!("seed {seed} failed ({}): {}", first.invariant, first.detail);
                failed
                    .entry(first.invariant.clone())
                    .or_default()
                    .push(seed);
                true
            }
            Err(first) => {
                to_shrink = Some((seed, first));
                false
            }
        }
    });
    if let Some((seed, first)) = to_shrink {
        let list = steps::generate(seed, &args.knobs, args.steps);
        println!(
            "seed {seed} failed ({}); shrinking {} steps...",
            first.invariant,
            list.len()
        );
        let minimal = shrink(seed, &args.knobs, &list, &first);
        println!("{minimal}");
        println!(
            "reproduce: delocal-sim --seeds 1 --start {seed} --steps {} {}",
            args.steps, args.knobs
        );
        return ExitCode::from(1);
    }
    eprintln!("delocal-sim: finished in {:.0?}", started.elapsed());
    let failures: u64 = failed.values().map(|v| v.len() as u64).sum();
    if failures == 0 {
        println!("all {} seeds passed: {:?}", args.seeds, totals);
        return ExitCode::SUCCESS;
    }
    println!(
        "{} of {} seeds passed; {failures} failed:",
        args.seeds - failures,
        args.seeds
    );
    for (invariant, seeds) in &failed {
        let shown: Vec<String> = seeds.iter().take(20).map(u64::to_string).collect();
        let more = if seeds.len() > 20 { ", ..." } else { "" };
        println!(
            "  {:>4}  {invariant}: seeds {}{more}",
            seeds.len(),
            shown.join(" ")
        );
    }
    println!(
        "reproduce one: delocal-sim --seeds 1 --start <seed> --steps {} {}",
        args.steps, args.knobs
    );
    ExitCode::from(1)
}
