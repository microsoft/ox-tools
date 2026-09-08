// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Turning a completed run into measured findings and per-family yields.

use core::cmp::Ordering;
use core::time::Duration;

use camino::Utf8Path;

use super::text::{human, plural};
use super::{Finding, Timing, Yield};
use crate::exec::TestBinary;
use crate::model::{Mutant, Outcome, Summary};
use crate::{HashMap, HashSet};

/// The fraction of the population one file must hold before it is worth naming.
const HOT_FILE_SHARE: f64 = 0.10;

/// The population below which share-based findings are arithmetic rather than evidence.
///
/// In a run of four mutants every file is a hot file and every family is a quarter of the budget.
/// Reporting that is not a smaller version of the real finding, it is a different and false one,
/// and a tool that cries wolf on a toy project is one nobody reads on a real one.
const MIN_POPULATION: usize = 50;

/// The CPU time a family must consume before its yield is worth judging at all.
///
/// A share threshold alone would flag a family that used 40% of six seconds.
const MIN_YIELD_CPU: Duration = Duration::from_mins(1);

/// The fraction of mutant execution time a family must consume before its yield is worth judging.
///
/// Below this, a family with no survivors is not a problem: disabling it would save nothing, and
/// the advice would be pure noise on a report someone has to read.
const YIELD_FLOOR_SHARE: f64 = 0.05;

/// The fraction of wall time the fixed cost must exceed before it is the thing to fix.
const FIXED_COST_SHARE: f64 = 0.30;

/// The fraction of wall time mutant execution must occupy before optimization is worth trying.
const EXECUTION_DOMINANT_SHARE: f64 = 0.60;

/// The fraction of the population that must be unviable before rollback is worth reporting.
const UNVIABLE_SHARE: f64 = 0.05;

/// The fraction of executed mutants that must exhaust memory before the ceiling is the suspect.
///
/// A ceiling is at least twice its binary's measured baseline peak, so passing it means roughly
/// doubling the memory of the whole test binary. A handful of mutants genuinely can — an inverted
/// loop bound, a capacity computed by multiplication — but one mutant in twenty doing it says more
/// about the baseline the ceiling was derived from than about the mutants held to it.
const MEMORY_CEILING_SHARE: f64 = 0.05;

/// The fraction of valid mutants that must be uncovered before it is the headline.
const UNCOVERED_SHARE: f64 = 0.10;

/// The baseline duration above which every mutant is paying a noticeable fixed cost.
const SLOW_BASELINE: Duration = Duration::from_secs(10);

/// The wall time above which a run will not fit in a routine CI job.
const LONG_RUN: Duration = Duration::from_mins(30);

/// The wall time a shard should aim for, used to size the suggested rotation.
const TARGET_SHARD: Duration = Duration::from_mins(15);

#[derive(Clone, Copy)]
enum CargoProfile<'a> {
    Unknown,
    Default,
    Named(&'a str),
}

/// Analyzes a completed run.
///
/// Findings come back in a fixed diagnostic order: run-wide costs first, then costly verdicts,
/// population concentration, mutator yield, and uncovered code.
#[must_use]
pub fn analyze(mutants: &[Mutant], timing: &Timing) -> Vec<Finding> {
    analyze_context(mutants, timing, CargoProfile::Unknown, &[])
}

/// Analyzes a completed run with the user-controlled build and test context.
#[must_use]
pub fn analyze_run(mutants: &[Mutant], timing: &Timing, profile: Option<&str>, binaries: &[TestBinary]) -> Vec<Finding> {
    let profile = profile.map_or(CargoProfile::Default, CargoProfile::Named);

    analyze_context(mutants, timing, profile, binaries)
}

fn analyze_context(mutants: &[Mutant], timing: &Timing, profile: CargoProfile<'_>, binaries: &[TestBinary]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let summary = Summary::of(mutants);
    let executed: Duration = mutants.iter().map(|mutant| Duration::from_millis(mutant.elapsed_ms)).sum();

    if let Some(finding) = fixed_cost(timing) {
        findings.push(finding);
    }

    if let Some(finding) = slow_baseline(timing, mutants, binaries) {
        findings.push(finding);
    }

    if let Some(finding) = unoptimized_execution(timing, profile) {
        findings.push(finding);
    }

    if let Some(finding) = long_run(timing) {
        findings.push(finding);
    }

    if let Some(finding) = timeouts(summary, mutants) {
        findings.push(finding);
    }

    if let Some(finding) = out_of_memory(summary, mutants) {
        findings.push(finding);
    }

    if let Some(finding) = unviable(summary) {
        findings.push(finding);
    }

    findings.extend(hot_files(mutants));
    findings.extend(low_yield(mutants, executed));

    if let Some(finding) = uncovered(summary) {
        findings.push(finding);
    }

    findings
}

/// Reports the cost and value of each mutator family, worst ratio last.
#[must_use]
pub fn yields(mutants: &[Mutant]) -> Vec<Yield> {
    let mut buckets: HashMap<String, Yield> = HashMap::default();

    for mutant in mutants {
        let family = family_of(&mutant.mutator).to_owned();
        let entry = buckets.entry(family.clone()).or_insert_with(|| Yield {
            family,
            mutants: 0,
            cpu: Duration::ZERO,
            survivors: 0,
        });

        entry.mutants += 1;
        entry.cpu += Duration::from_millis(mutant.elapsed_ms);

        if mutant.outcome == Outcome::Survived {
            entry.survivors += 1;
        }
    }

    let mut rows: Vec<Yield> = buckets.into_values().collect();

    rows.sort_by(|left, right| {
        right
            .per_cpu_hour()
            .partial_cmp(&left.per_cpu_hour())
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.family.cmp(&right.family))
    });

    rows
}

/// The family part of a mutator name: everything before the first dot.
fn family_of(mutator: &str) -> &str {
    mutator.split_once('.').map_or(mutator, |(family, _)| family)
}

/// A build that costs more than the testing it enables.
fn fixed_cost(timing: &Timing) -> Option<Finding> {
    let wall = timing.wall.as_secs_f64();
    let fixed = timing.build.as_secs_f64() + timing.baseline.as_secs_f64();

    if wall <= 0.0 || fixed / wall < FIXED_COST_SHARE {
        return None;
    }

    Some(Finding {
        code: "fixed-cost",
        headline: format!(
            "{:.0}% of the run was the build and baseline, not mutation testing",
            fixed / wall * 100.0
        ),
        detail: vec![
            format!("build {}, baseline {}", human(timing.build), human(timing.baseline)),
            format!(
                "mutant execution {}",
                human(timing.wall.saturating_sub(timing.build + timing.baseline))
            ),
        ],
        remedy: "test more mutants per build: widen `--mutators`, or drop `--shard-count` so each run \
                 amortizes the build over more work. A build cache such as sccache helps the build \
                 itself."
            .to_owned(),
        cost: "none — this is the one finding whose remedy costs no signal at all".to_owned(),
    })
}

/// A suite whose fixed per-run cost is paid once per mutant.
#[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
fn slow_baseline(timing: &Timing, mutants: &[Mutant], binaries: &[TestBinary]) -> Option<Finding> {
    if timing.baseline < SLOW_BASELINE {
        return None;
    }

    let live = mutants.iter().filter(|mutant| ran_test_binary(mutant.outcome)).count();
    let projected = timing.baseline.mul_f64(live as f64).div_f64(timing.jobs.max(1) as f64);

    let mut detail = vec![
        format!("every one of the {live} tested mutants pays that cost"),
        format!("floor for this run at {} jobs: {}", timing.jobs, human(projected)),
    ];

    let mut slowest: Vec<&TestBinary> = binaries.iter().filter(|binary| !binary.baseline.is_zero()).collect();

    slowest.sort_by(|left, right| {
        right
            .baseline
            .cmp(&left.baseline)
            .then_with(|| left.package.cmp(&right.package))
            .then_with(|| left.target.cmp(&right.target))
    });

    for binary in slowest.into_iter().take(3) {
        let tests = binary.tests.map_or_else(String::new, |tests| format!(", {tests} tests"));

        detail.push(format!(
            "test target `{}::{}`: {} baseline{tests}",
            binary.package,
            binary.target,
            human(binary.baseline)
        ));
    }

    Some(Finding {
        code: "slow-baseline",
        headline: format!("the suite takes {} with no mutant active", human(timing.baseline)),
        detail,
        remedy: "profile the named test targets and shorten repeated fixture setup, sleeps, network \
                 calls, and oversized inputs. Split performance or load checks from correctness \
                 assertions so mutation runs can keep the fast oracle; if a target carries no \
                 mutation-relevant assertions, omit it with `--exclude-test <target>`."
            .to_owned(),
        cost: "optimizing or splitting tests preserves signal; `--exclude-test` removes every \
               assertion in that target from mutation verdicts"
            .to_owned(),
    })
}

/// A test-dominated run using Cargo's unoptimized test or development profile.
fn unoptimized_execution(timing: &Timing, profile: CargoProfile<'_>) -> Option<Finding> {
    let profile = match profile {
        CargoProfile::Default => "Cargo's default test profile",
        CargoProfile::Named(profile @ ("dev" | "test")) => profile,
        CargoProfile::Unknown | CargoProfile::Named(_) => return None,
    };

    if timing.wall.is_zero() {
        return None;
    }

    let executed = timing.wall.saturating_sub(timing.build + timing.baseline);
    let share = executed.as_secs_f64() / timing.wall.as_secs_f64();

    if share < EXECUTION_DOMINANT_SHARE {
        return None;
    }

    Some(Finding {
        code: "unoptimized-execution",
        headline: format!("{:.0}% of the run was mutant execution under {profile}", share * 100.0),
        detail: vec![
            format!("mutant execution {}, build {}", human(executed), human(timing.build)),
            "the build is paid once, while the selected test code is executed for every mutant".to_owned(),
        ],
        remedy: "if the named test targets are CPU-bound, compare a representative shard with the \
                 documented optimized profile: `cargo gamma run --profile gamma`. Keep that profile \
                 fixed for every shard whose reports will be merged."
            .to_owned(),
        cost: "optimization makes the build slower, does not help I/O-bound tests, and can change \
               verdicts through code generation; scores from different profiles are not comparable"
            .to_owned(),
    })
}

/// Whether assigning this outcome ran a test binary with the mutant active.
const fn ran_test_binary(outcome: Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Killed | Outcome::Survived | Outcome::Timeout | Outcome::OutOfMemory | Outcome::Flaky
    )
}

/// A run too long to sit in a routine CI job.
fn long_run(timing: &Timing) -> Option<Finding> {
    if timing.wall < LONG_RUN {
        return None;
    }

    let shards = (timing.wall.as_secs_f64() / TARGET_SHARD.as_secs_f64()).ceil().max(2.0);

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded by the ratio of two durations"
    )]
    let shards = shards as u32;

    Some(Finding {
        code: "long-run",
        headline: format!("the run took {}, which will not fit a per-commit job", human(timing.wall)),
        detail: vec![format!("at {} per shard that is a rotation of {shards}", human(TARGET_SHARD))],
        remedy: format!(
            "run one shard a night — `--shard-count {shards} --shard-index <n>` — and combine the \
             reports with `cargo gamma merge`. Shards are assigned by content, so coverage \
             accumulates as the code changes instead of resetting."
        ),
        cost: "none in total coverage, but a verdict is up to one rotation old rather than current".to_owned(),
    })
}

/// Mutants that hung, and the budget they burned proving it.
fn timeouts(summary: Summary, mutants: &[Mutant]) -> Option<Finding> {
    if summary.timeout == 0 {
        return None;
    }

    let spent: Duration = mutants
        .iter()
        .filter(|mutant| mutant.outcome == Outcome::Timeout)
        .map(|mutant| Duration::from_millis(mutant.elapsed_ms))
        .sum();

    Some(Finding {
        code: "timeouts",
        headline: format!(
            "{} {} ran out their whole budget",
            summary.timeout,
            plural(summary.timeout, "mutant")
        ),
        detail: vec![format!("{} of CPU time spent waiting for them", human(spent))],
        remedy: "a mutant that hangs is a mutant the suite detected, so this is signal, not \
                 failure — it is just expensive signal. `cargo gamma suppress` writes suppressions \
                 for them so the next run does not pay again."
            .to_owned(),
        cost: "a suppressed timeout leaves the score unchanged today, but stops being retested, so \
               a later edit that makes it terminate goes unnoticed"
            .to_owned(),
    })
}

/// Mutants the memory ceiling stopped, and whether the ceiling or the sites are the likelier fault.
#[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
fn out_of_memory(summary: Summary, mutants: &[Mutant]) -> Option<Finding> {
    let stopped: Vec<&Mutant> = mutants.iter().filter(|mutant| mutant.outcome == Outcome::OutOfMemory).collect();

    if stopped.is_empty() {
        return None;
    }

    let spent: Duration = stopped.iter().map(|mutant| Duration::from_millis(mutant.elapsed_ms)).sum();
    let files: HashSet<&Utf8Path> = stopped.iter().map(|mutant| &*mutant.file).collect();

    let located = match files.iter().copied().next() {
        Some(only) if files.len() == 1 && stopped.len() == 1 => format!("the one is in {only}"),
        Some(only) if files.len() == 1 => format!("all {} are in {only}", stopped.len()),
        _ => format!("spread across {} files, so no one site explains them", files.len()),
    };

    let mut detail = vec![
        format!("{} of CPU time spent before the kernel stopped them", human(spent)),
        located,
    ];

    // The share is only evidence once enough mutants ran to make it one, and only the mutants that
    // ran a test binary could have reached a ceiling — an uncovered mutant never allocated anything,
    // so counting it would dilute the very signal this line exists to raise.
    let executed = mutants.iter().filter(|mutant| ran_test_binary(mutant.outcome)).count();

    if executed >= MIN_POPULATION && stopped.len() as f64 / executed as f64 >= MEMORY_CEILING_SHARE {
        detail.push(format!(
            "that is {:.0}% of the {executed} mutants that ran, which points at the ceiling rather \
             than the sites",
            stopped.len() as f64 / executed as f64 * 100.0
        ));
    }

    Some(Finding {
        code: "out-of-memory",
        headline: format!(
            "{} {} hit the memory ceiling",
            summary.out_of_memory,
            plural(summary.out_of_memory, "mutant")
        ),
        detail,
        remedy: "this is the verdict most likely to be wrong, because a ceiling set too tight \
                 convicts a healthy mutant — establish which it is before acting. `--memory \
                 measure` reports each binary's peak without stopping anything, and \
                 `--memory-multiplier` or `--memory-headroom` widen the ceiling if the baseline it \
                 came from was unrepresentative. A site that is genuinely allowed to allocate this \
                 much is eligible for `cargo gamma suppress` by default."
            .to_owned(),
        cost: "a widened ceiling stops bounding the runaway allocation it was there to catch, and a \
               suppressed site stops being retested, so a later edit that makes it allocate without \
               bound goes unnoticed"
            .to_owned(),
    })
}

/// Mutants that could not be compiled, and the rebuild rounds they forced.
fn unviable(summary: Summary) -> Option<Finding> {
    let total = summary.valid() + summary.unviable + summary.ignored;

    // #[gamma::skip(expr.decrement, reason = "on conversion failure both usize::MAX and usize::MAX - 1 are far above the 50-mutant threshold, so the branch is identical")]
    if usize::try_from(total).unwrap_or(usize::MAX) < MIN_POPULATION || f64::from(summary.unviable) / f64::from(total) < UNVIABLE_SHARE {
        return None;
    }

    Some(Finding {
        code: "unviable",
        headline: format!(
            "{} of {total} mutants could not compile ({:.0}%)",
            summary.unviable,
            f64::from(summary.unviable) / f64::from(total) * 100.0
        ),
        detail: vec![
            "each withdrawal round is a full rebuild of the instrumented tree".to_owned(),
            "they are excluded from the score, so the cost bought nothing".to_owned(),
        ],
        remedy: "`cargo gamma suppress --eligible unviable` records them in the source so later runs \
                 skip them without discovering their unviability again. If they cluster in one \
                 operator, narrow `--mutators` instead."
            .to_owned(),
        cost: "none — an unviable mutant never contributed to the score".to_owned(),
    })
}

/// Files holding an outsized share of the population.
#[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
fn hot_files(mutants: &[Mutant]) -> Vec<Finding> {
    let total = mutants.len();

    if total < MIN_POPULATION {
        return Vec::new();
    }

    let mut counts: HashMap<&str, (u32, u32, Duration)> = HashMap::default();

    for mutant in mutants {
        let entry = counts.entry(mutant.file.as_str()).or_insert((0, 0, Duration::ZERO));

        entry.0 += 1;
        entry.2 += Duration::from_millis(mutant.elapsed_ms);

        if mutant.outcome == Outcome::Survived {
            entry.1 += 1;
        }
    }

    let mut hot: Vec<(&str, u32, u32, Duration)> = counts
        .into_iter()
        .filter(|&(_, (count, _, _))| f64::from(count) / total as f64 >= HOT_FILE_SHARE)
        .map(|(file, (count, survivors, cpu))| (file, count, survivors, cpu))
        .collect();

    hot.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));

    hot.into_iter()
        .map(|(file, count, survivors, cpu)| Finding {
            code: "hot-file",
            headline: format!(
                "{file} alone is {:.0}% of the population ({count} {})",
                f64::from(count) / total as f64 * 100.0,
                plural(count, "mutant")
            ),
            detail: vec![format!(
                "{} of CPU time, {survivors} {} found there",
                human(cpu),
                plural(survivors, "survivor")
            )],
            remedy: "if it is generated, tabular or macro-expanded code, exclude it with \
                     `--exclude-file` or the `exclude-files` config key. If it is hand-written, \
                     this is not a problem — it is where the logic is."
                .to_owned(),
            cost: format!(
                "exactly {count} {} stop being tested, {survivors} of which are currently finding \
                 gaps in the suite",
                plural(count, "mutant")
            ),
        })
        .collect()
}

/// Families spending real time and finding nothing.
fn low_yield(mutants: &[Mutant], executed: Duration) -> Vec<Finding> {
    // #[gamma::skip(cond.always_false, reason = "when executed is zero every row's CPU is also zero, so the later 60-second filter returns the same empty result")]
    if executed.is_zero() {
        return Vec::new();
    }

    yields(mutants)
        .into_iter()
        .filter(|row| row.survivors == 0)
        .filter(|row| row.cpu >= MIN_YIELD_CPU)
        .filter(|row| row.cpu.as_secs_f64() / executed.as_secs_f64() >= YIELD_FLOOR_SHARE)
        .map(|row| Finding {
            code: "low-yield",
            headline: format!("the `{}` family spent {} and found no survivors", row.family, human(row.cpu)),
            detail: vec![format!(
                "{} {}, {:.0}% of mutant execution time",
                row.mutants,
                plural(row.mutants, "mutant"),
                row.cpu.as_secs_f64() / executed.as_secs_f64() * 100.0
            )],
            remedy: format!("`--mutators 'all,!{}'` drops it", row.family),
            cost: "real, and easy to underrate. A family that finds nothing today is a regression \
                   detector for tomorrow: this says the suite currently covers it, not that it \
                   always will"
                .to_owned(),
        })
        .collect()
}

/// Code no test reaches at all.
fn uncovered(summary: Summary) -> Option<Finding> {
    let valid = summary.valid();

    // #[gamma::skip(expr.decrement, reason = "on conversion failure both usize::MAX and usize::MAX - 1 are far above the 50-mutant threshold, so the branch is identical")]
    if usize::try_from(valid).unwrap_or(usize::MAX) < MIN_POPULATION || f64::from(summary.uncovered) / f64::from(valid) < UNCOVERED_SHARE {
        return None;
    }

    Some(Finding {
        code: "uncovered",
        headline: format!(
            "{} of {valid} mutants ({:.0}%) sit in code no test reaches",
            summary.uncovered,
            f64::from(summary.uncovered) / f64::from(valid) * 100.0
        ),
        detail: vec!["they count against the score, because untested code is the finding".to_owned()],
        remedy: "this is not a performance problem and there is nothing to tune. Write tests, or \
                 delete the code."
            .to_owned(),
        cost: "—".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::advise_fixture::{binary, find, mutant, timing};

    #[test]
    fn a_healthy_run_produces_no_findings() {
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100),
            mutant("b.rs", "arith.add_to_sub", Outcome::Survived, 100),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn findings_follow_the_documented_diagnostic_order() {
        let mut mutants: Vec<Mutant> = (0..47)
            .map(|index| mutant(&format!("killed-{index}.rs"), "relational.lt_to_le", Outcome::Killed, 1))
            .collect();
        mutants.push(mutant("timeout.rs", "relational.lt_to_le", Outcome::Timeout, 1));
        mutants.push(mutant("memory.rs", "relational.lt_to_le", Outcome::OutOfMemory, 1));
        mutants.extend((0..3).map(|index| mutant(&format!("unviable-{index}.rs"), "relational.lt_to_le", Outcome::CompileError, 1)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let codes: Vec<&str> = findings.iter().map(|finding| finding.code).collect();

        assert_eq!(codes, ["timeouts", "out-of-memory", "unviable"]);
    }

    #[test]
    fn a_dominant_build_is_reported() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let findings = analyze(&mutants, &timing(50, 5, 100));
        let finding = find(&findings, "fixed-cost").expect("expected a fixed-cost finding");

        assert!(finding.headline.contains("55%"), "{}", finding.headline);
        assert_eq!(finding.detail, ["build 50.0s, baseline 5.0s", "mutant execution 45.0s"]);
        assert!(finding.remedy.contains("test more mutants per build"), "{}", finding.remedy);
        assert!(finding.remedy.contains("sccache helps the build itself"), "{}", finding.remedy);
        assert_eq!(finding.cost, "none — this is the one finding whose remedy costs no signal at all");
    }

    #[test]
    fn a_build_that_is_a_small_part_of_the_run_is_not_reported() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let findings = analyze(&mutants, &timing(5, 1, 100));

        assert!(find(&findings, "fixed-cost").is_none(), "{findings:?}");
    }

    #[test]
    fn fixed_cost_threshold_and_zero_wall_are_handled_exactly() {
        assert!(fixed_cost(&timing(30, 0, 100)).is_some());
        assert!(fixed_cost(&timing(29, 0, 100)).is_none());
        assert!(fixed_cost(&timing(0, 0, 0)).is_none());
    }

    #[test]
    fn a_slow_baseline_projects_the_floor_of_the_run() {
        let mutants: Vec<Mutant> = (0..40)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        let findings = analyze(&mutants, &timing(1, 60, 4000));
        let finding = find(&findings, "slow-baseline").expect("expected a slow-baseline finding");

        // 40 mutants x 60s / 4 jobs = 600s.
        assert!(finding.detail.iter().any(|line| line.contains("10m")), "{finding:?}");
        assert_eq!(finding.detail[0], "every one of the 40 tested mutants pays that cost");
        assert!(finding.remedy.starts_with("profile the named test targets"), "{}", finding.remedy);
        assert!(finding.cost.contains("optimizing or splitting tests preserves signal"));
    }

    #[test]
    fn slow_baseline_starts_at_ten_seconds_and_never_divides_by_zero_jobs() {
        let mutants = vec![mutant("a.rs", "x", Outcome::Killed, 1)];
        assert!(slow_baseline(&timing(0, 9, 10), &mutants, &[]).is_none());
        let mut at_threshold = timing(0, 10, 10);
        at_threshold.jobs = 0;
        let finding = slow_baseline(&at_threshold, &mutants, &[]).expect("threshold is inclusive");
        assert_eq!(finding.detail[1], "floor for this run at 0 jobs: 10.0s");
    }

    #[test]
    fn uncovered_mutants_do_not_inflate_the_baseline_projection() {
        let mutants = vec![
            mutant("a.rs", "one", Outcome::Killed, 1),
            mutant("a.rs", "two", Outcome::Survived, 1),
            mutant("a.rs", "three", Outcome::Timeout, 1),
            mutant("b.rs", "four", Outcome::NoCoverage, 1),
            mutant("b.rs", "five", Outcome::NoCoverage, 1),
        ];
        let mut run = timing(0, 60, 100);
        run.jobs = 3;

        let finding = slow_baseline(&run, &mutants, &[]).expect("a 60-second baseline is slow");

        assert_eq!(
            finding.detail,
            [
                "every one of the 3 tested mutants pays that cost",
                "floor for this run at 3 jobs: 60.0s",
            ]
        );
    }

    #[test]
    fn a_slow_baseline_names_the_slowest_test_targets() {
        let mutants = vec![mutant("a.rs", "x", Outcome::Killed, 1)];
        let binaries = vec![
            binary("api", "unit", 3, Some(20)),
            binary("api", "security", 18, Some(4)),
            binary("cli", "robustness", 12, None),
            binary("core", "fast", 1, Some(100)),
        ];
        let finding = slow_baseline(&timing(0, 34, 100), &mutants, &binaries).expect("slow baseline");

        assert_eq!(
            &finding.detail[2..],
            [
                "test target `api::security`: 18.0s baseline, 4 tests",
                "test target `cli::robustness`: 12.0s baseline",
                "test target `api::unit`: 3.0s baseline, 20 tests",
            ]
        );
        assert!(finding.remedy.contains("--exclude-test <target>"), "{}", finding.remedy);
        assert!(finding.cost.contains("removes every assertion"), "{}", finding.cost);
    }

    #[test]
    fn execution_dominance_under_the_default_profile_suggests_the_gamma_profile() {
        let finding = unoptimized_execution(&timing(10, 5, 100), CargoProfile::Default).expect("execution dominates");

        assert_eq!(finding.code, "unoptimized-execution");
        assert!(finding.remedy.contains("cargo gamma run --profile gamma"), "{}", finding.remedy);
        assert!(finding.remedy.contains("CPU-bound"), "{}", finding.remedy);
        assert!(finding.cost.contains("I/O-bound"), "{}", finding.cost);
        assert!(finding.cost.contains("not comparable"), "{}", finding.cost);
    }

    #[test]
    fn profile_advice_requires_dominant_execution_and_an_unoptimized_profile() {
        assert!(unoptimized_execution(&timing(0, 0, 0), CargoProfile::Default).is_none());
        assert!(unoptimized_execution(&timing(30, 20, 100), CargoProfile::Default).is_none());
        assert!(unoptimized_execution(&timing(10, 5, 100), CargoProfile::Named("gamma")).is_none());
        assert!(unoptimized_execution(&timing(10, 5, 100), CargoProfile::Unknown).is_none());
    }

    #[test]
    fn a_long_run_suggests_a_rotation_sized_to_it() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let findings = analyze(&mutants, &timing(1, 1, 3600));
        let finding = find(&findings, "long-run").expect("expected a long-run finding");

        // An hour at fifteen minutes a shard is four shards.
        assert!(finding.remedy.contains("--shard-count 4"), "{}", finding.remedy);
        assert_eq!(finding.detail, ["at 15m per shard that is a rotation of 4"]);
        assert_eq!(
            finding.cost,
            "none in total coverage, but a verdict is up to one rotation old rather than current"
        );
    }

    #[test]
    fn long_run_starts_at_thirty_minutes() {
        assert!(long_run(&timing(0, 0, 1799)).is_none());
        assert!(long_run(&timing(0, 0, 1800)).is_some());
    }

    #[test]
    fn timeouts_report_the_budget_they_burned() {
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::Timeout, 30_000),
            mutant("a.rs", "relational.le_to_lt", Outcome::Timeout, 30_000),
            mutant("a.rs", "arith.add_to_sub", Outcome::Killed, 10),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "timeouts").expect("expected a timeouts finding");

        assert!(finding.headline.contains('2'), "{}", finding.headline);
        assert!(finding.detail[0].contains("60"), "{:?}", finding.detail);
        assert!(finding.remedy.contains("does not pay again"), "{}", finding.remedy);
    }

    #[test]
    fn the_timeout_remedy_names_its_cost() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Timeout, 10)];
        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "timeouts").expect("expected a timeouts finding");

        assert!(finding.cost.contains("goes unnoticed"), "{}", finding.cost);
    }

    #[test]
    fn out_of_memory_mutants_report_the_budget_they_burned_and_where_they_sit() {
        let mutants = vec![
            mutant("a.rs", "iter.min_to_max", Outcome::OutOfMemory, 20_000),
            mutant("a.rs", "arith.div_to_mul", Outcome::OutOfMemory, 10_000),
            mutant("b.rs", "arith.add_to_sub", Outcome::Killed, 10),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "out-of-memory").expect("expected an out-of-memory finding");

        assert_eq!(finding.headline, "2 mutants hit the memory ceiling");
        assert_eq!(
            finding.detail,
            ["30.0s of CPU time spent before the kernel stopped them", "all 2 are in a.rs",]
        );
    }

    /// The finding leads with verifying the ceiling, because a ceiling set too tight convicts a
    /// healthy mutant and suppressing it would then hide a working test rather than a hungry site.
    #[test]
    fn the_out_of_memory_remedy_names_both_routes_and_both_costs() {
        let mutants = vec![mutant("a.rs", "iter.min_to_max", Outcome::OutOfMemory, 10)];
        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "out-of-memory").expect("expected an out-of-memory finding");

        assert_eq!(finding.headline, "1 mutant hit the memory ceiling");
        assert_eq!(finding.detail[1], "the one is in a.rs");

        let verify = finding.remedy.find("--memory measure").expect("the remedy must offer measurement");
        let suppress = finding
            .remedy
            .find("cargo gamma suppress")
            .expect("the remedy must offer suppression");

        assert!(
            verify < suppress,
            "measuring must be offered before suppressing: {}",
            finding.remedy
        );
        assert!(finding.remedy.contains("--memory-multiplier"), "{}", finding.remedy);
        assert!(finding.cost.contains("stops bounding"), "{}", finding.cost);
        assert!(finding.cost.contains("goes unnoticed"), "{}", finding.cost);
    }

    #[test]
    fn out_of_memory_mutants_in_many_files_are_reported_as_spread() {
        let mutants = vec![
            mutant("a.rs", "iter.min_to_max", Outcome::OutOfMemory, 10),
            mutant("b.rs", "iter.min_to_max", Outcome::OutOfMemory, 10),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "out-of-memory").expect("expected an out-of-memory finding");

        assert_eq!(finding.detail[1], "spread across 2 files, so no one site explains them");
    }

    /// A share large enough to indict the ceiling is only evidence once enough mutants ran, and only
    /// the mutants that ran count — an uncovered mutant never allocated anything.
    #[test]
    fn a_large_share_of_out_of_memory_mutants_indicts_the_ceiling() {
        let mut mutants: Vec<Mutant> = (0..45)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..5).map(|index| mutant("b.rs", "iter.min_to_max", Outcome::OutOfMemory, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "out-of-memory").expect("expected an out-of-memory finding");

        assert_eq!(
            finding.detail[2],
            "that is 10% of the 50 mutants that ran, which points at the ceiling rather than the sites"
        );
    }

    /// The share threshold is inclusive, and a run just under it says nothing about the ceiling.
    #[test]
    fn the_out_of_memory_share_threshold_is_inclusive() {
        let indicts = |killed: u64, stopped: u64| {
            let mut mutants: Vec<Mutant> = (0..killed)
                .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
                .collect();

            mutants.extend((0..stopped).map(|index| mutant("b.rs", "iter.min_to_max", Outcome::OutOfMemory, index)));

            let finding = out_of_memory(Summary::of(&mutants), &mutants).expect("expected a finding");

            finding.detail.len() == 3
        };

        assert!(indicts(95, 5), "5 of 100 is exactly the threshold");
        assert!(!indicts(96, 4), "4 of 100 is below it");
    }

    /// Uncovered mutants never ran a test binary, so counting them would dilute the share below the
    /// threshold and silence a genuinely mis-calibrated ceiling.
    #[test]
    fn uncovered_mutants_do_not_dilute_the_out_of_memory_share() {
        let mut mutants: Vec<Mutant> = (0..47)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..3).map(|index| mutant("b.rs", "iter.min_to_max", Outcome::OutOfMemory, index)));
        mutants.extend((0..500).map(|index| mutant("c.rs", "arith.add_to_sub", Outcome::NoCoverage, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "out-of-memory").expect("expected an out-of-memory finding");

        assert_eq!(
            finding.detail[2],
            "that is 6% of the 50 mutants that ran, which points at the ceiling rather than the sites"
        );
    }

    /// Below the population floor a share is arithmetic rather than evidence: five out of forty-five
    /// is 11%, but a run that small cannot tell a mis-calibrated ceiling from three hungry sites.
    #[test]
    fn a_small_run_never_indicts_the_ceiling_however_large_the_share() {
        let mut mutants: Vec<Mutant> = (0..44)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..5).map(|index| mutant("b.rs", "iter.min_to_max", Outcome::OutOfMemory, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "out-of-memory").expect("expected an out-of-memory finding");

        assert_eq!(finding.detail.len(), 2, "{:?}", finding.detail);
    }

    #[test]
    fn a_run_with_no_out_of_memory_mutants_has_no_such_finding() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 10)];

        assert!(out_of_memory(Summary::of(&mutants), &mutants).is_none());
    }

    #[test]
    fn a_pile_of_unviable_mutants_is_reported() {
        let mut mutants: Vec<Mutant> = (0..90)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..10).map(|index| mutant("b.rs", "fn_value.default", Outcome::CompileError, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "unviable").expect("expected an unviable finding");

        assert!(finding.headline.contains("10 of 100"), "{}", finding.headline);
        assert_eq!(
            finding.detail,
            [
                "each withdrawal round is a full rebuild of the instrumented tree",
                "they are excluded from the score, so the cost bought nothing",
            ]
        );
        assert!(finding.remedy.contains("cargo gamma suppress --eligible unviable"));
        assert_eq!(finding.cost, "none — an unviable mutant never contributed to the score");
    }

    #[test]
    fn unviable_requires_the_population_and_share_thresholds_inclusively() {
        assert!(
            unviable(Summary {
                killed: 46,
                unviable: 3,
                ignored: 1,
                ..Summary::default()
            })
            .is_some()
        );
        assert!(
            unviable(Summary {
                killed: 57,
                unviable: 3,
                ..Summary::default()
            })
            .is_some()
        );
        assert!(
            unviable(Summary {
                killed: 46,
                unviable: 2,
                ignored: 1,
                ..Summary::default()
            })
            .is_none()
        );
        assert!(
            unviable(Summary {
                killed: 57,
                unviable: 2,
                ignored: 1,
                ..Summary::default()
            })
            .is_none()
        );
    }

    #[test]
    fn a_hot_file_names_the_survivors_that_would_be_lost() {
        let mut mutants: Vec<Mutant> = (0..80)
            .map(|index| mutant("generated.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.push(mutant("generated.rs", "arith.add_to_sub", Outcome::Survived, 1));
        mutants.extend((0..19).map(|index| mutant("real.rs", "relational.lt_to_le", Outcome::Killed, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "hot-file").expect("expected a hot-file finding");

        assert!(finding.headline.starts_with("generated.rs"), "{}", finding.headline);
        assert!(finding.cost.contains("81 mutants"), "{}", finding.cost);
        assert!(finding.cost.contains("1 of which"), "{}", finding.cost);
        assert_eq!(finding.detail, ["3.2s of CPU time, 1 survivor found there"]);
        assert!(finding.remedy.contains("where the logic is"), "{}", finding.remedy);
    }

    #[test]
    fn a_file_below_the_share_is_not_named() {
        let mut mutants: Vec<Mutant> = (0..95)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..5).map(|index| mutant("b.rs", "arith.add_to_sub", Outcome::Killed, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let hot: Vec<&Finding> = findings.iter().filter(|finding| finding.code == "hot-file").collect();

        assert_eq!(hot.len(), 1, "{hot:?}");
        assert!(hot[0].headline.starts_with("a.rs"), "{}", hot[0].headline);
    }

    #[test]
    fn a_file_at_exactly_ten_percent_is_hot_and_keeps_its_cpu_total() {
        let mut mutants: Vec<Mutant> = (0..45).map(|_| mutant("a.rs", "x", Outcome::Killed, 1)).collect();
        mutants.extend((0..5).map(|_| mutant("b.rs", "x", Outcome::Survived, 1000)));
        let hot = hot_files(&mutants);
        let b = hot.iter().find(|finding| finding.headline.starts_with("b.rs")).expect("10% is hot");
        assert_eq!(b.detail, ["5.0s of CPU time, 5 survivors found there"]);
    }

    #[test]
    fn a_family_that_finds_nothing_expensively_is_reported() {
        let mutants = vec![
            mutant("a.rs", "literal.int_bump", Outcome::Killed, 200_000),
            mutant("a.rs", "relational.lt_to_le", Outcome::Survived, 1000),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "low-yield").expect("expected a low-yield finding");

        assert!(finding.headline.contains("literal"), "{}", finding.headline);
        assert!(finding.remedy.contains("!literal"), "{}", finding.remedy);
        assert_eq!(finding.detail, ["1 mutant, 100% of mutant execution time"]);
        assert!(finding.cost.contains("not that it always will"), "{}", finding.cost);
    }

    #[test]
    fn a_family_that_finds_nothing_cheaply_is_left_alone() {
        let mutants = vec![
            mutant("a.rs", "literal.int_bump", Outcome::Killed, 10),
            mutant("a.rs", "relational.lt_to_le", Outcome::Survived, 50_000),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "low-yield").is_none(), "{findings:?}");
    }

    #[test]
    fn a_family_with_survivors_is_never_low_yield() {
        let mutants = vec![mutant("a.rs", "literal.int_bump", Outcome::Survived, 50_000)];
        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "low-yield").is_none(), "{findings:?}");
    }

    #[test]
    fn low_yield_honours_both_inclusive_thresholds_and_zero_execution() {
        assert!(low_yield(&[], Duration::ZERO).is_empty());
        let mutants = vec![
            mutant("a.rs", "literal.bump", Outcome::Killed, 60_000),
            mutant("a.rs", "other.bump", Outcome::Survived, 1_140_000),
        ];
        let findings = low_yield(&mutants, Duration::from_mins(20));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].headline.contains("literal"));
        assert!(low_yield(&mutants, Duration::from_secs(1201)).is_empty());
    }

    #[test]
    fn uncovered_code_is_reported_as_the_finding_it_is() {
        let mut mutants: Vec<Mutant> = (0..80)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..20).map(|index| mutant("b.rs", "arith.add_to_sub", Outcome::NoCoverage, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "uncovered").expect("expected an uncovered finding");

        assert!(finding.headline.contains("20 of 100"), "{}", finding.headline);
        assert!(finding.remedy.contains("Write tests"), "{}", finding.remedy);
        assert_eq!(
            finding.detail,
            ["they count against the score, because untested code is the finding"]
        );
        assert_eq!(finding.cost, "—");
    }

    #[test]
    fn uncovered_requires_the_population_and_share_thresholds_inclusively() {
        assert!(
            uncovered(Summary {
                killed: 45,
                uncovered: 5,
                ..Summary::default()
            })
            .is_some()
        );
        assert!(
            uncovered(Summary {
                killed: 44,
                uncovered: 5,
                ..Summary::default()
            })
            .is_none()
        );
        assert!(
            uncovered(Summary {
                killed: 46,
                uncovered: 4,
                ..Summary::default()
            })
            .is_none()
        );
    }

    #[test]
    fn yields_rank_families_by_survivors_per_cpu_hour() {
        let mutants = vec![
            mutant("a.rs", "stmt.delete", Outcome::Survived, 1000),
            mutant("a.rs", "literal.int_bump", Outcome::Survived, 100_000),
        ];

        let rows = yields(&mutants);

        assert_eq!(rows[0].family, "stmt");
        assert_eq!(rows[1].family, "literal");
        assert!(rows[0].per_cpu_hour() > rows[1].per_cpu_hour());
    }

    #[test]
    fn yields_aggregate_every_field_and_break_ties_by_family() {
        let mutants = vec![
            mutant("a.rs", "z.one", Outcome::Killed, 1000),
            mutant("a.rs", "z.two", Outcome::Survived, 2000),
            mutant("a.rs", "a.one", Outcome::Survived, 3000),
        ];
        let rows = yields(&mutants);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].family, "a");
        assert_eq!(rows[1].family, "z");
        assert_eq!((rows[1].mutants, rows[1].cpu, rows[1].survivors), (2, Duration::from_secs(3), 1));
    }

    #[test]
    fn a_mutator_without_a_family_is_its_own_family() {
        assert_eq!(family_of("relational.lt_to_le"), "relational");
        assert_eq!(family_of("odd"), "odd");
    }

    #[test]
    fn a_tiny_run_is_never_diagnosed_by_share() {
        // Every file in a two-mutant run is half the population, which is arithmetic, not evidence.
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::NoCoverage, 100),
            mutant("b.rs", "arith.add_to_sub", Outcome::CompileError, 100),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "hot-file").is_none(), "{findings:?}");
        assert!(find(&findings, "unviable").is_none(), "{findings:?}");
        assert!(find(&findings, "uncovered").is_none(), "{findings:?}");
    }

    #[test]
    fn a_family_dominating_a_few_seconds_is_not_a_finding() {
        // 90% of six seconds is not worth anybody's attention.
        let mutants = vec![
            mutant("a.rs", "literal.int_bump", Outcome::Killed, 5000),
            mutant("a.rs", "relational.lt_to_le", Outcome::Survived, 500),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "low-yield").is_none(), "{findings:?}");
    }
}
