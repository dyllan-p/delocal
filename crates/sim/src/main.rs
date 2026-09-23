//! `delocal-sim`: sweep seeds through the simulator (DESIGN.md §14.1).
//!
//! ```text
//! delocal-sim --seeds N [--start S] [--steps K] [--corruption P] [--drop-watcher P]
//!             [--delay-ms LO..HI] [--crash-after-rename P] [--nodes N] [--keep-going]
//! ```
//!
//! On the first failing seed the step list is delta-debugged down to a
//! minimal list that still fails, and the report (seed, knobs, steps, the
//! invariant and its counterexample) is printed. With `--keep-going` the
//! sweep runs every seed instead, shrinks nothing, and ends with the failing
//! seeds grouped by invariant and a pass/fail count: what CI's advisory
//! sweep prints while the random sweep is still red (§14.1). Exit status 1
//! on any failure either way.

// The binary may not unwrap or expect either (CLAUDE.md); errors go to stderr.
// Failure is large by design; see lib.rs.
#![allow(clippy::result_large_err)]
use std::collections::BTreeMap;
use std::process::ExitCode;

use delocal_sim::{Failure, Knobs, Step, run_twice, steps};

struct Args {
    seeds: u64,
    start: u64,
    steps: usize,
    knobs: Knobs,
    keep_going: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seeds: 1000,
        start: 0,
        steps: 400,
        knobs: Knobs::default(),
        keep_going: false,
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
            "--help" | "-h" => {
                return Err("usage: delocal-sim --seeds N [--start S] [--steps K] [--corruption P] [--drop-watcher P] [--delay-ms LO..HI] [--crash-after-rename P] [--nodes N] [--keep-going]".to_owned());
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
    let started = std::time::Instant::now();
    let mut totals = delocal_sim::sim::Stats::default();
    // Failing seeds by invariant, for the --keep-going summary.
    let mut failed: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (i, seed) in (args.start..args.start + args.seeds).enumerate() {
        let list = steps::generate(seed, &args.knobs, args.steps);
        match run_twice(seed, &args.knobs, &list) {
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
                if (i + 1) % 100 == 0 {
                    println!("  {} seeds ok ({:.0?})", i + 1, started.elapsed());
                }
            }
            Err(first) if args.keep_going => {
                println!("seed {seed} failed ({}): {}", first.invariant, first.detail);
                failed
                    .entry(first.invariant.clone())
                    .or_default()
                    .push(seed);
            }
            Err(first) => {
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
        }
    }
    let failures: u64 = failed.values().map(|v| v.len() as u64).sum();
    if failures == 0 {
        println!(
            "all {} seeds passed in {:.0?}: {:?}",
            args.seeds,
            started.elapsed(),
            totals
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "{} of {} seeds passed in {:.0?}; {failures} failed:",
        args.seeds - failures,
        args.seeds,
        started.elapsed()
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
