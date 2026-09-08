// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The prose dump.

use core::cmp::Reverse;
use core::fmt::Write as _;
use core::time::Duration;

use crate::HashMap;
use crate::advise::human;
use crate::discover::Plan;
use crate::exec::{Session, TestBinary};
use crate::model::{Mutant, Outcome, Summary};
use crate::report::quantity;

/// How many rows the "worst offender" tables keep.
///
/// Long enough to show a pattern rather than a single outlier, short enough that the whole dump
/// still fits on a screen next to the run that produced it.
const TOP: usize = 10;

/// One row of a per-something breakdown.
#[derive(Debug, Default)]
struct Bucket {
    mutants: usize,
    cpu: Duration,
    survivors: usize,
    unviable: usize,
}

impl Bucket {
    /// Folds one mutant into the tally.
    fn absorb(&mut self, mutant: &Mutant) {
        self.mutants += 1;
        self.cpu += Duration::from_millis(mutant.elapsed_ms);

        match mutant.outcome {
            Outcome::Survived => self.survivors += 1,
            Outcome::CompileError => self.unviable += 1,
            _other => {}
        }
    }
}

/// Renders the whole dump.
///
/// `session` is absent when nothing was live, so nothing was built or measured; the population is
/// still worth reporting, because a run that found no work is exactly the kind that wants
/// explaining.
#[must_use]
pub fn render(plan: &Plan, session: Option<&Session>, jobs: usize, wall: Duration) -> String {
    let summary = Summary::of(&plan.mutants);
    let mut text = String::new();

    let _ = writeln!(text, "── diag ──────────────────────────────────────────────");

    // The build and the baseline are the run's fixed cost, so what is left is the only part that
    // scales with the population and the only part worth judging the scheduler on.
    let fixed = session.map_or(Duration::ZERO, |session| session.build + session.baseline_wall);
    let testing = wall.saturating_sub(fixed);

    let _ = writeln!(
        text,
        "run       wall {}, of which {} testing, {} jobs",
        human(wall),
        human(testing),
        jobs
    );

    let _ = writeln!(text, "          root {}", plan.root);

    let _ = writeln!(
        text,
        "discover  {}, {}, {}, {}",
        quantity(plan.files.len(), "file"),
        quantity(plan.mutants.len(), "mutant"),
        quantity(plan.reach.len(), "package"),
        quantity(plan.reach.values().map(crate::HashSet::len).sum::<usize>(), "reach edge")
    );

    let _ = writeln!(
        text,
        "          withheld: {} suppressed, {} out of shard, {} already settled",
        plan.suppressed, plan.sharded_out, plan.settled_out
    );

    // Every outcome is named, including the ones that are zero: this is the dump a user sends when
    // they cannot explain a run, and it sits beside a JSON bundle that carries all ten. A line
    // listing a hand-picked subset makes the two halves of the same dump disagree, and leaves a
    // reader unable to tell a category nobody printed from mutants that went missing.
    let counts: Vec<String> = Outcome::ALL
        .iter()
        .map(|outcome| format!("{} {outcome}", summary.count(*outcome)))
        .collect();

    let _ = writeln!(text, "outcomes  {}", counts.join(", "));

    if let Some(session) = session {
        write_session(&mut text, session);
    }

    write_throughput(&mut text, &plan.mutants, session, jobs, testing);
    write_slowest(&mut text, &plan.mutants);
    write_breakdown(&mut text, "mutator", &group(&plan.mutants, |mutant| mutant.mutator.to_string()));
    write_breakdown(&mut text, "package", &group(&plan.mutants, |mutant| mutant.package.to_string()));
    write_breakdown(&mut text, "file", &group(&plan.mutants, |mutant| mutant.file.to_string()));

    if let Some(session) = session {
        write_binaries(&mut text, session);
    }

    text
}

/// Reports what the fixed cost of the run was, and what it bought.
fn write_session(text: &mut String, session: &Session) {
    let _ = writeln!(
        text,
        "build     {} over {} rounds, {} withdrawn, selection {}",
        human(session.build),
        session.rounds,
        session.withdrawn,
        if session.widened { "widened to the workspace" } else { "kept" }
    );

    write_rounds(text, session);
    write_ordering(text, session);
    write_census(text, session);
    write_phases(text, session);

    let _ = writeln!(
        text,
        "baseline  {} wall, {} cumulative, longest silence {}, stall budget {}",
        human(session.baseline_wall),
        human(session.baseline),
        human(session.quiet),
        session.stall.map_or_else(|| "off".to_owned(), human)
    );

    // The one figure that says whether the suite ran at all, which is otherwise only ever said on
    // the progress line — and progress resolves to whether a terminal is attached, so on a CI
    // runner it is said to nobody.
    let _ = writeln!(
        text,
        "tests     {}",
        session
            .tests
            .map_or_else(|| "no harness announced a count".to_owned(), |tests| quantity(tests, "test"))
    );

    let _ = writeln!(
        text,
        "memory    baseline peak {}",
        session.peak.map_or_else(|| "not measured".to_owned(), crate::report::bytes)
    );

    // Walked here rather than carried on the session, because it is a stat per build artifact and
    // this dump is the only thing that asks for it. A run that fills a CI runner's disk fails at its
    // next step with nothing pointing back here, so the path goes out beside the figure.
    let _ = writeln!(
        text,
        "disk      {} under {}",
        crate::report::bytes(crate::exec::footprint(&session.scratch)),
        session.scratch
    );

    // Read from the process rather than the session, because it is a property of the process: a
    // reader the drain gave up on outlives the mutant that produced it, which is the whole reason
    // it is worth counting. With `jobs` concurrent mutants and two streams apiece an untroubled run
    // peaks near twice the job count; far above that means test binaries are leaving descendants
    // holding their output pipes open, and the readers waiting on them are never reclaimed.
    let _ = writeln!(
        text,
        "readers   {} peak, {} still running",
        crate::exec::READERS.peak(),
        crate::exec::READERS.live()
    );
}

/// Reports where the run's time went, phase by phase.
///
/// The build total folds the copy, the preflight and the compile into one number, and the census's
/// cost hides inside it too — so whether the per-test census pays for itself against the launches it
/// spares the sweep cannot be read from the aggregates alone. These phase timings are what make that
/// trade visible: the census's `walked` count against the sweep's `launches`, with the probe count
/// saying whether the killer hints earn their keep. The copy and the preflight are components of the
/// build, and the census and the sweep of the testing window, so neither set sums to its aggregate.
fn write_phases(text: &mut String, session: &Session) {
    let phases = &session.phases;

    let _ = writeln!(
        text,
        "phases    copy {}, preflight {}, baseline {}",
        human(phases.copy),
        human(phases.preflight),
        human(session.baseline_wall)
    );

    if let Some(census) = &phases.census {
        let _ = writeln!(
            text,
            "          census {}, {} over {} binaries",
            human(census.elapsed),
            quantity(census.walked, "test"),
            census.binaries
        );
    }

    if let Some(sweep) = &phases.sweep {
        let _ = writeln!(
            text,
            "          sweep {}, {} launches, {} probes",
            human(sweep.elapsed),
            sweep.launches,
            sweep.probes
        );
    }
}

/// Reports why the withdrawn mutants were withdrawn.
///
/// A withdrawal count says a number; it does not say whether the number is worth acting on. Grouped
/// by rustc error code and mutator, it does: a mutator that keeps drawing a type error is one that
/// could be taught to look before it mutates, while a spread of borrow-checker codes across every
/// mutator is the cost of the schema and not a bug in anything. Nothing else in the run says this,
/// and deriving it otherwise means patching the tool by hand.
fn write_census(text: &mut String, session: &Session) {
    if session.census.is_empty() {
        return;
    }

    let _ = writeln!(text, "withdrew  by rustc error code and mutator");

    for entry in &session.census {
        let _ = writeln!(
            text,
            "  {:<8}{:<28}{}",
            if entry.code.is_empty() { "(none)" } else { &entry.code },
            if entry.mutator.is_empty() { "(unknown)" } else { &entry.mutator },
            quantity(entry.mutants, "mutant")
        );
    }
}

/// Reports what each round of the build cost, and what it bought.
///
/// A build total on its own cannot tell a tree that compiled first time from one that spent most of
/// its time withdrawing mutants that were never going to compile, and those two want opposite
/// remedies: a faster machine against fewer unviable mutants. That is what the split says. The
/// first round is what building this workspace costs at all; every round after it exists only
/// because some mutant did not compile, and its time is the price of that mutant.
fn write_rounds(text: &mut String, session: &Session) {
    let Some((first, rest)) = session.rounds_taken.split_first() else {
        return;
    };

    let converging: Duration = rest.iter().map(|round| round.elapsed).sum();
    let total = first.elapsed.saturating_add(converging);

    #[expect(clippy::cast_possible_truncation, reason = "a percentage of a ratio in [0, 1]")]
    #[expect(clippy::cast_sign_loss, reason = "both durations are non-negative")]
    let share = if total.is_zero() {
        0
    } else {
        (converging.as_secs_f64() / total.as_secs_f64() * 100.0).round() as u64
    };

    let _ = writeln!(
        text,
        "rounds    first {}, then {} over {} ({share}% of the build's rounds)",
        human(first.elapsed),
        human(converging),
        quantity(rest.len(), "further round")
    );

    for (index, round) in session.rounds_taken.iter().enumerate() {
        let _ = writeln!(
            text,
            "  round {:<4}{:>8}  {}",
            index.saturating_add(1),
            human(round.elapsed),
            if round.withdrew == 0 {
                "nothing withdrawn".to_owned()
            } else {
                format!("{} withdrawn", quantity(round.withdrew, "mutant"))
            }
        );
    }
}

/// Reports what front-loading the mutants an earlier run could not compile actually bought.
///
/// Deliberately not phrased as a saving. The rounds a probe avoided are the length of a convergence
/// that never happened, and a number invented for it would be a model of a counterfactual printed
/// as a measurement — in a file whose entire value is that everything in it is something that
/// happened. What is printed is the trade: how many mutants went in front of the compiler early,
/// how many of those it then refused, and what the probes cost in rounds. A confirmation rate near
/// zero over several rounds is a hint set to regenerate or delete, and that is the decision this
/// line exists to support.
fn write_ordering(text: &mut String, session: &Session) {
    let hints = session.ordering;

    if hints.rounds == 0 && hints.offered == 0 {
        return;
    }

    let _ = writeln!(
        text,
        "hints     {} front-loaded over {}, {} confirmed unviable by the compiler",
        quantity(hints.offered, "mutant"),
        quantity(hints.rounds as usize, "probe round"),
        hints.confirmed
    );
}

/// Reports how well the run kept its workers busy.
///
/// The number that matters is the effective job count: CPU over wall. A scheduler that is working
/// lands within a fraction of `--jobs`, and everything short of that is time spent waiting for the
/// slowest binary of a batch rather than testing anything.
fn write_throughput(text: &mut String, mutants: &[Mutant], session: Option<&Session>, jobs: usize, testing: Duration) {
    let mut spent: Vec<Duration> = mutants
        .iter()
        .filter(|mutant| mutant.elapsed_ms > 0)
        .map(|mutant| Duration::from_millis(mutant.elapsed_ms))
        .collect();

    if spent.is_empty() {
        let _ = writeln!(text, "mutants   nothing was run");
        return;
    }

    spent.sort_unstable();

    let cpu: Duration = spent.iter().sum();

    let _ = writeln!(
        text,
        "mutants   {} evaluated, {} cpu, {} effective jobs of {jobs}",
        spent.len(),
        human(cpu),
        ratio(cpu, testing)
    );

    let median = percentile(&spent, 0.50);

    let _ = writeln!(
        text,
        "          min {}, p50 {}, p90 {}, p99 {}, max {}",
        human(spent[0]),
        human(median),
        human(percentile(&spent, 0.90)),
        human(percentile(&spent, 0.99)),
        human(spent[spent.len() - 1])
    );

    if let Some(session) = session
        && let Some(min_budget) = session.binaries.iter().filter_map(|binary| binary.budget).min()
        && crowded_timeout(min_budget, median)
    {
        let _ = writeln!(
            text,
            "warning   test binary timeout {} sits within {NOISE_FACTOR}x the p50 mutant duration {}: \
             timeouts here are scheduling noise, and every one of them is scored as a kill",
            human(min_budget),
            human(median)
        );
    }
}

/// How much larger than a typical run the timeout has to be before it is a ceiling rather than a
/// coin toss.
///
/// A mutant's budget is meant to catch a suite that has stopped making progress, not one that took
/// longer than usual. Between the two is a band where whichever mutants happen to land on a busy
/// moment time out — and a timeout counts as undetected, so the noise lowers the score and can fail
/// the run.
const NOISE_FACTOR: u32 = 2;

/// Whether the derived mutant timeout is close enough to a typical run to be producing noise.
///
/// A run whose median mutant takes longer than the whole budget divided by [`NOISE_FACTOR`] is one
/// where the timeout is doing something other than what it was set for.
fn crowded_timeout(timeout: Duration, median: Duration) -> bool {
    !median.is_zero() && !timeout.is_zero() && timeout < median.saturating_mul(NOISE_FACTOR)
}

/// Names the mutants that cost the most, which is where a scheduling change shows up first.
fn write_slowest(text: &mut String, mutants: &[Mutant]) {
    let mut ranked: Vec<&Mutant> = mutants.iter().filter(|mutant| mutant.elapsed_ms > 0).collect();

    if ranked.is_empty() {
        return;
    }

    ranked.sort_unstable_by_key(|mutant| Reverse(mutant.elapsed_ms));
    ranked.truncate(TOP);

    let _ = writeln!(text, "\nslowest mutants");

    for mutant in ranked {
        let _ = writeln!(
            text,
            "  {:>8}  {:<9} {}",
            human(Duration::from_millis(mutant.elapsed_ms)),
            label(mutant.outcome),
            mutant.describe()
        );
    }
}

/// Reports one grouping, ranked by the CPU it consumed.
///
/// Ranked by cost rather than by name because the question this table answers is always "what
/// should be looked at first", and the answer is whatever is at the top.
fn write_breakdown(text: &mut String, noun: &str, buckets: &HashMap<String, Bucket>) {
    if buckets.is_empty() {
        return;
    }

    let mut rows: Vec<(&String, &Bucket)> = buckets.iter().collect();

    rows.sort_by(|(left_name, left), (right_name, right)| right.cpu.cmp(&left.cpu).then_with(|| left_name.cmp(right_name)));

    let shown = rows.len().min(TOP);

    let _ = writeln!(text, "\nby {noun} ({shown} of {})", rows.len());
    let _ = writeln!(
        text,
        "  {:>8}  {:>7}  {:>9}  {:>8}  {noun}",
        "cpu", "mutants", "survivors", "unviable"
    );

    for (name, bucket) in rows.into_iter().take(TOP) {
        let _ = writeln!(
            text,
            "  {:>8}  {:>7}  {:>9}  {:>8}  {name}",
            human(bucket.cpu),
            bucket.mutants,
            bucket.survivors,
            bucket.unviable
        );
    }
}

/// Reports what each test binary cost the run.
///
/// A binary's baseline is charged to every mutant that can reach it, so a single slow one is
/// multiplied by the population and is the most leveraged thing in a run.
fn write_binaries(text: &mut String, session: &Session) {
    if session.binaries.is_empty() {
        return;
    }

    let mut binaries: Vec<&TestBinary> = session.binaries.iter().collect();

    binaries.sort_by_key(|binary| Reverse(binary.baseline));

    let total: Duration = binaries.iter().map(|binary| binary.baseline).sum();

    let _ = writeln!(text, "\ntest binaries ({}, {} baseline)", binaries.len(), human(total));
    let _ = writeln!(
        text,
        "  {:>8}  {:>8}  {:>10}  {:>10}  {:<20} binary",
        "baseline", "budget", "peak", "ceiling", "package"
    );

    for binary in binaries.into_iter().take(TOP) {
        let _ = writeln!(
            text,
            "  {:>8}  {:>8}  {:>10}  {:>10}  {:<20} {}",
            human(binary.baseline),
            binary.budget.map_or_else(|| "-".to_owned(), human),
            binary.peak.map_or_else(|| "-".to_owned(), crate::report::bytes),
            binary.memory.map_or_else(|| "-".to_owned(), crate::report::bytes),
            binary.package,
            binary.path.file_name().unwrap_or(binary.path.as_str())
        );
    }
}

/// Buckets the population by whatever `key` names.
fn group(mutants: &[Mutant], key: impl Fn(&Mutant) -> String) -> HashMap<String, Bucket> {
    let mut buckets: HashMap<String, Bucket> = HashMap::default();

    for mutant in mutants {
        buckets.entry(key(mutant)).or_default().absorb(mutant);
    }

    buckets
}

/// The value at `fraction` through an already-sorted list.
fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }

    #[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
    let position = fraction * (sorted.len() - 1) as f64;

    #[expect(clippy::cast_possible_truncation, reason = "the operand is an index into the list above")]
    #[expect(clippy::cast_sign_loss, reason = "the fraction and the length are both non-negative")]
    let index = position.round() as usize;

    sorted[index.min(sorted.len() - 1)]
}

/// How many workers the run actually kept busy, as a printable ratio.
fn ratio(cpu: Duration, wall: Duration) -> String {
    if wall.is_zero() {
        return "?".to_owned();
    }

    format!("{:.1}", cpu.as_secs_f64() / wall.as_secs_f64())
}

/// The unstyled name of an outcome, so the columns line up whatever the terminal is.
const fn label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Killed => "killed",
        Outcome::Survived => "survived",
        Outcome::Timeout => "timeout",
        Outcome::OutOfMemory => "outofmem",
        Outcome::Flaky => "flaky",
        Outcome::CompileError => "unviable",
        Outcome::Ignored => "ignored",
        Outcome::NoCoverage => "uncovered",
        Outcome::NotBuilt => "notbuilt",
        Outcome::Pending => "pending",
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::exec::Round;
    use crate::fixtures;

    fn mutant(file: &str, mutator: &str, outcome: Outcome, ms: u64) -> Mutant {
        Mutant {
            id: format!("{file}:{mutator}:{ms}").into(),
            file: (Utf8PathBuf::from(file)).into(),
            mutator: (mutator.to_owned()).into(),
            original: "a + b".to_owned().into(),
            replacement: "a - b".to_owned().into(),
            outcome,
            elapsed_ms: ms,
            ..fixtures::mutant()
        }
    }

    fn plan(mutants: Vec<Mutant>) -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants,
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    fn session(binaries: Vec<TestBinary>) -> Session {
        Session {
            census: Vec::new(),
            baseline: Duration::from_secs(5),
            baseline_wall: Duration::from_secs(2),
            tests: Some(12),
            quiet: Duration::from_secs(2),
            stall: Some(Duration::from_secs(3)),
            build: Duration::from_secs(7),
            metered: false,
            unbounded: None,
            withdrawn: 4,
            rounds: 2,
            rounds_taken: vec![
                Round {
                    elapsed: Duration::from_secs(5),
                    withdrew: 4,
                },
                Round {
                    elapsed: Duration::from_secs(2),
                    withdrew: 0,
                },
            ],
            binaries,
            peak: None,
            scratch: Utf8PathBuf::new(),
            filtered: 0,
            widened: true,
            ordering: crate::exec::OrderingHints::default(),
            phases: crate::exec::Phases::default(),
        }
    }

    #[test]
    fn a_percentile_never_indexes_past_the_end() {
        let sorted = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(3)];

        assert_eq!(percentile(&sorted, 0.0), Duration::from_secs(1));
        assert_eq!(percentile(&sorted, 1.0), Duration::from_secs(3));
        assert_eq!(percentile(&sorted, 0.5), Duration::from_secs(2));
    }

    #[test]
    fn a_percentile_of_nothing_is_zero_rather_than_a_panic() {
        assert_eq!(percentile(&[], 0.9), Duration::ZERO);
    }

    #[test]
    fn effective_jobs_is_cpu_over_wall() {
        assert_eq!(ratio(Duration::from_secs(80), Duration::from_secs(10)), "8.0");
    }

    #[test]
    fn a_run_with_no_wall_time_reports_no_ratio_rather_than_an_infinite_one() {
        assert_eq!(ratio(Duration::from_secs(80), Duration::ZERO), "?");
    }

    #[test]
    fn a_breakdown_is_ranked_by_cost() {
        let mutants = vec![
            mutant("a.rs", "arith.add_to_sub", Outcome::Killed, 100),
            mutant("b.rs", "literal.int_to_zero", Outcome::Survived, 900),
        ];

        let text = render(&plan(mutants), None, 4, Duration::from_secs(1));
        let cheap = text.find("arith.add_to_sub").expect("the cheap family is listed");
        let dear = text.find("literal.int_to_zero").expect("the expensive family is listed");

        assert!(dear < cheap, "the expensive family must come first:\n{text}");
    }

    #[test]
    fn a_run_that_tested_nothing_still_reports_its_population() {
        let text = render(
            &plan(vec![mutant("a.rs", "arith.add_to_sub", Outcome::Pending, 0)]),
            None,
            4,
            Duration::ZERO,
        );

        assert!(text.contains("1 mutant,"), "{text}");
        assert!(text.contains("nothing was run"), "{text}");

        // With no session there is nothing to say about a build that never happened, and inventing
        // zeroes for it would read as a build that took no time.
        assert!(!text.contains("baseline"), "{text}");
    }

    #[test]
    fn a_live_run_reports_the_session_costs_and_scope() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        // These fixed costs explain how much of the wall clock was not mutant execution.
        assert!(
            text.contains("build     7.0s over 2 rounds, 4 withdrawn, selection widened to the workspace"),
            "{text}"
        );
        assert!(
            text.contains("baseline  2.0s wall, 5.0s cumulative, longest silence 2.0s, stall budget 3.0s"),
            "{text}"
        );
        assert!(text.contains("wall 20.0s, of which 11.0s testing"), "{text}");
    }

    /// The disk a run left behind is measured here and nowhere else.
    ///
    /// It is a stat per build artifact over a directory that holds every object file of every
    /// round, so nothing may compute it unless it is going to be printed — which makes this dump,
    /// the only thing that prints it, also the only thing that pays for it.
    #[test]
    fn a_live_run_reports_the_disk_it_left_behind() {
        let temporary = tempfile::tempdir().expect("a scratch directory");
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("the scratch path is UTF-8");

        std::fs::write(base.join("artifact.o").as_std_path(), vec![0u8; 2048]).expect("an artifact");

        let live = Session {
            scratch: base.clone(),
            ..session(Vec::new())
        };

        let text = render(&plan(Vec::new()), Some(&live), 4, Duration::from_secs(20));

        assert!(text.contains(&format!("disk      2.0 KB under {base}")), "{text}");
    }

    /// The build total folds the copy, the preflight and the census into one number, so the phase
    /// line is the only place the dump says where the fixed and testing time actually went — the
    /// census's walk against the sweep's launches, with the probe count on whether the hints paid.
    #[test]
    fn a_live_run_reports_where_each_phase_spent_its_time() {
        let live = Session {
            phases: crate::exec::Phases {
                copy: Duration::from_secs(1),
                preflight: Duration::from_secs(2),
                census: Some(crate::exec::CensusCost {
                    elapsed: Duration::from_secs(8),
                    walked: 1_681,
                    binaries: 30,
                }),
                sweep: Some(crate::exec::SweepCost {
                    elapsed: Duration::from_secs(42),
                    launches: 47,
                    probes: 12,
                }),
            },
            ..session(Vec::new())
        };

        let text = render(&plan(Vec::new()), Some(&live), 4, Duration::from_mins(1));

        assert!(text.contains("phases    copy 1.0s, preflight 2.0s, baseline 2.0s"), "{text}");
        assert!(text.contains("census 8.0s, 1681 tests over 30 binaries"), "{text}");
        assert!(text.contains("sweep 42.0s, 47 launches, 12 probes"), "{text}");
    }

    /// A census that did not run leaves no census line, because a run that was not asked to census
    /// did not census in zero time — it did not census, and inventing a zero would say otherwise.
    #[test]
    fn a_live_run_without_a_census_prints_no_census_phase() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        assert!(text.contains("phases    copy"), "{text}");
        assert!(!text.contains("census 0"), "an unrun census must not be printed: {text}");
    }

    /// The withdrawal count says a number; only the codes behind it say whether the number is a
    /// mutator that could be taught to look before it mutates or an unavoidable cost of the schema.
    /// The dump is the only place that answer is available without patching the tool.
    #[test]
    fn a_live_run_reports_why_its_mutants_were_withdrawn() {
        let live = Session {
            census: vec![
                crate::exec::Withdrawal {
                    code: "E0308".to_owned(),
                    mutator: "lit.true_to_false".to_owned(),
                    mutants: 12,
                },
                crate::exec::Withdrawal {
                    code: String::new(),
                    mutator: String::new(),
                    mutants: 1,
                },
            ],
            ..session(Vec::new())
        };

        let text = render(&plan(Vec::new()), Some(&live), 4, Duration::from_secs(20));

        assert!(text.contains("withdrew  by rustc error code and mutator"), "{text}");
        assert!(text.contains("E0308"), "{text}");
        assert!(text.contains("lit.true_to_false"), "{text}");
        assert!(text.contains("12 mutants"), "{text}");
        assert!(text.contains("(none)"), "{text}");
        assert!(text.contains("(unknown)"), "{text}");
    }

    /// A run that withdrew nothing has nothing to say, and a heading over an empty list reads as a
    /// missing measurement rather than as an absent one.
    #[test]
    fn a_run_that_withdrew_nothing_says_nothing_about_why() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        assert!(!text.contains("withdrew  "), "{text}");
    }

    /// The reader count is the measurement that decides whether a bound on stray readers is needed.
    ///
    /// A reader the drain gave up on holds a thread and a descriptor for the rest of the run, and
    /// nothing else in the tool would ever say so — the mutant it belonged to has long since been
    /// scored. Reporting the peak is what turns "this might accumulate" into a number, so the line
    /// has to actually appear rather than be something a reader is assumed to know to ask for.
    #[test]
    fn a_live_run_reports_how_many_output_readers_it_needed() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        assert!(text.contains("readers   "), "{text}");
        assert!(text.contains(" peak, "), "{text}");
        assert!(text.contains(" still running"), "{text}");
    }

    /// A build total cannot distinguish a tree that compiled first time from one that converged for
    /// most of its time, and those two want opposite remedies. The split has to be stated.
    #[test]
    fn a_live_run_reports_what_each_build_round_cost() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        assert!(
            text.contains("rounds    first 5.0s, then 2.0s over 1 further round (29% of the build's rounds)"),
            "{text}"
        );
        assert!(text.contains("round 1       5.0s  4 mutants withdrawn"), "{text}");
        assert!(text.contains("round 2       2.0s  nothing withdrawn"), "{text}");
    }

    /// A run whose rounds were never recorded says nothing about them rather than claiming a build
    /// that took no time at all.
    #[test]
    fn a_run_with_no_recorded_rounds_says_nothing_about_them() {
        let mut session = session(Vec::new());
        session.rounds_taken = Vec::new();

        let text = render(&plan(Vec::new()), Some(&session), 4, Duration::from_secs(20));

        assert!(!text.contains("rounds  "), "{text}");
    }

    ///
    /// Regression, issue-035. The only other place this figure is ever stated is the progress line
    /// the baseline closes with, and progress resolves to whether a terminal is attached — so on a
    /// CI runner, where "did my suite run at all" is precisely the question this answers, nobody
    /// was told.
    #[test]
    fn a_live_run_reports_how_many_tests_the_baseline_ran() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        assert!(text.contains("tests     12 tests"), "{text}");
    }

    /// A suite whose harness announced no count says so rather than reporting zero.
    ///
    /// A `harness = false` target announces nothing, and a run that printed `0 tests` for it would
    /// read exactly like a suite that ran none — which is the alarming case this figure exists to
    /// make visible.
    #[test]
    fn a_baseline_no_harness_counted_says_so_rather_than_reporting_zero() {
        let mut session = session(Vec::new());

        session.tests = None;

        let text = render(&plan(Vec::new()), Some(&session), 4, Duration::from_secs(20));

        assert!(text.contains("tests     no harness announced a count"), "{text}");
    }

    /// A timeout no larger than a typical mutant run is called out, because the timeouts it
    /// produces are noise scored as detections.
    ///
    /// Regression, issue-016. A mutant that times out counts as killed, so a budget sitting in the
    /// band where scheduling decides the answer inflates the score and sends the reader looking for
    /// hangs that were never there.
    #[test]
    fn a_timeout_close_to_the_typical_mutant_duration_is_called_out() {
        let binary = TestBinary {
            budget: Some(Duration::from_millis(120)),
            ..crate::testing::test_binary("/w/target/debug/deps/a")
        };
        let session = session(vec![binary]);

        let text = render(
            &plan(vec![
                mutant("a.rs", "m", Outcome::Killed, 100),
                mutant("b.rs", "m", Outcome::Killed, 100),
            ]),
            Some(&session),
            4,
            Duration::from_secs(1),
        );

        assert!(text.contains("warning   test binary timeout"), "{text}");
        assert!(text.contains("scored as a kill"), "{text}");
    }

    /// A timeout with room above a typical run says nothing, because there is nothing wrong.
    #[test]
    fn a_timeout_with_room_above_the_typical_mutant_duration_says_nothing() {
        let text = render(
            &plan(vec![mutant("a.rs", "m", Outcome::Killed, 100)]),
            Some(&session(Vec::new())),
            4,
            Duration::from_secs(1),
        );

        assert!(!text.contains("warning"), "{text}");
    }

    /// A run that measured no mutant durations cannot say anything about its timeout, and does not
    /// invent a comparison against a median of zero.
    #[test]
    fn a_median_of_nothing_is_not_compared_against_the_timeout() {
        assert!(!crowded_timeout(Duration::from_secs(30), Duration::ZERO));
        assert!(!crowded_timeout(Duration::ZERO, Duration::from_secs(30)));
        assert!(crowded_timeout(Duration::from_secs(30), Duration::from_secs(20)));
        assert!(!crowded_timeout(Duration::from_secs(30), Duration::from_secs(15)));
    }

    #[test]
    fn a_session_without_a_stall_budget_says_it_is_off() {
        let mut session = session(Vec::new());

        session.stall = None;
        session.widened = false;

        let text = render(&plan(Vec::new()), Some(&session), 4, Duration::from_secs(20));

        // The absence of a stall budget is a real operating mode, not a zero-second timeout.
        assert!(text.contains("selection kept"), "{text}");
        assert!(text.contains("stall budget off"), "{text}");
    }

    #[test]
    fn a_breakdown_counts_unviable_mutants_separately_from_survivors() {
        let text = render(
            &plan(vec![
                mutant("a.rs", "arith.add_to_sub", Outcome::CompileError, 10),
                mutant("a.rs", "arith.add_to_sub", Outcome::Survived, 20),
            ]),
            None,
            4,
            Duration::from_secs(1),
        );

        // Unviable mutants are withdrawn from the score, but the diagnostic table keeps their cost
        // visible to someone improving the mutator.
        assert!(text.contains("outcomes  0 killed, 0 timeout, 0 outofmem, 1 survived,"), "{text}");
        assert!(text.contains(", 1 unviable, 0 ignored, 0 notbuilt, 0 pending"), "{text}");
        assert!(
            text.contains("      30ms        2          1         1  arith.add_to_sub"),
            "{text}"
        );
    }

    #[test]
    fn the_outcome_line_accounts_for_every_mutant_in_the_population() {
        // The dump is what a user sends when they cannot explain a run, and the JSON bundle beside
        // it carries all ten counters. A prose line naming a subset makes the two disagree, and a
        // reader cannot tell an unlisted category from a mutant that went missing. Distinct
        // multiplicities so that a figure reading the wrong counter fails as loudly as a missing
        // one.
        let mut mutants = Vec::new();

        for (index, outcome) in Outcome::ALL.into_iter().enumerate() {
            for _ in 0..=index {
                mutants.push(mutant("a.rs", "arith.add_to_sub", outcome, 10));
            }
        }

        let population = mutants.len();
        let summary = Summary::of(&mutants);
        let text = render(&plan(mutants), None, 4, Duration::from_secs(1));

        let line = text
            .lines()
            .find(|line| line.starts_with("outcomes  "))
            .expect("the dump names the outcomes");

        let counted: u32 = line
            .trim_start_matches("outcomes  ")
            .split(", ")
            .map(|figure| {
                figure
                    .split_once(' ')
                    .expect("a figure is a count and a name")
                    .0
                    .parse::<u32>()
                    .expect("a figure counts mutants")
            })
            .sum();

        assert_eq!(counted as usize, population, "{text}");

        for outcome in Outcome::ALL {
            assert!(
                line.contains(&format!("{} {outcome}", summary.count(outcome))),
                "{outcome} is missing from `{line}`"
            );
        }
    }

    #[test]
    fn the_slowest_table_uses_plain_outcome_labels_for_every_non_survivor_result() {
        let text = render(
            &plan(vec![
                mutant("timeout.rs", "m", Outcome::Timeout, 70),
                mutant("unviable.rs", "m", Outcome::CompileError, 60),
                mutant("ignored.rs", "m", Outcome::Ignored, 50),
                mutant("uncovered.rs", "m", Outcome::NoCoverage, 40),
                mutant("pending.rs", "m", Outcome::Pending, 30),
                mutant("outofmem.rs", "m", Outcome::OutOfMemory, 20),
                mutant("notbuilt.rs", "m", Outcome::NotBuilt, 10),
            ]),
            None,
            4,
            Duration::from_secs(1),
        );

        // The table is intentionally unstyled, so every verdict has to be rendered as text that
        // still lines up in a plain diagnostic dump.
        for label in ["timeout", "unviable", "ignored", "uncovered", "pending", "outofmem", "notbuilt"] {
            assert!(text.contains(label), "{text}");
        }
    }

    #[test]
    fn test_binaries_are_ranked_by_baseline_cost() {
        let text = render(
            &plan(Vec::new()),
            Some(&session(vec![
                TestBinary {
                    package: "fast".to_owned(),
                    baseline: Duration::from_secs(1),
                    budget: Some(Duration::from_secs(10)),
                    ..crate::testing::test_binary("/w/target/debug/deps/fast-abc")
                },
                TestBinary {
                    package: "slow".to_owned(),
                    baseline: Duration::from_secs(3),
                    budget: Some(Duration::from_secs(30)),
                    ..crate::testing::test_binary("/w/target/debug/deps/slow-def")
                },
            ])),
            4,
            Duration::from_secs(20),
        );

        let slow = text.find("slow-def").expect("slow binary is listed");
        let fast = text.find("fast-abc").expect("fast binary is listed");

        // A slow binary is multiplied by every mutant that reaches it, so the most expensive one
        // must be shown first.
        assert!(text.contains("test binaries (2, 4.0s baseline)"), "{text}");
        assert!(slow < fast, "{text}");
    }
}
