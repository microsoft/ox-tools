// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::env::consts;

use serde::{Deserialize, Serialize};

use crate::discover::Plan;
use crate::exec::Session;
use crate::model::{Mutant, Outcome, Summary};

/// The schema version of the bundle.
///
/// The same discipline as [`crate::elements`]: a consumer reading a figure whose meaning has
/// silently changed is worse off than one that refuses to read it. Bump this whenever a field's
/// meaning changes or a field is removed; adding one does not need it.
const SCHEMA_VERSION: &str = "3";

/// How many rows the ranked tables keep, matching the prose dump.
const TOP: usize = 20;

/// How much of the identifier hash is kept.
///
/// Long enough that two packages in one workspace will not collide, short enough that the value
/// reads as an opaque label rather than as something to try to reverse.
const HASH_WIDTH: usize = 12;

/// What to do with the names of packages, files and binaries.
///
/// Timing data is entangled with things people cannot share: a path names employees and products,
/// a package name describes an unreleased codebase, and a test name can be as disclosing as the
/// code it covers. None of those are needed to read a cost profile — what is needed is to be able
/// to tell one row from another and to group rows that belong together, which a stable hash gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Redaction {
    /// Replace each identifier with a short stable hash of it. The default.
    ///
    /// Chosen over omitting because it costs the reader nothing — every question the bundle can
    /// answer is about how the cost is distributed, not about what anything is called — and because
    /// a hash still lets the person who sent it map a row back to their own tree.
    #[default]
    Hashed,

    /// Leave the identifiers as they are.
    ///
    /// For a tree whose names are already public, and for our own runs. Never the default: a user
    /// who cannot tell what they are sending will send nothing.
    Names,

    /// Drop the identifiers entirely.
    ///
    /// The rows survive, in order, without labels. For a tree where even the shape of the name
    /// space is sensitive.
    Omitted,
}

impl Redaction {
    /// Applies the policy to one identifier.
    fn apply(self, name: &str) -> Option<String> {
        match self {
            Self::Names => Some(name.to_owned()),
            Self::Omitted => None,
            Self::Hashed => {
                let digest = blake3::hash(name.as_bytes()).to_hex();

                Some(digest.get(..HASH_WIDTH).unwrap_or(&digest).to_owned())
            }
        }
    }
}

/// Everything a run measured about itself, as a document someone else can read.
///
/// **No source text, ever.** Not a replacement, not an original, not a line of context. The report
/// carries those because it is read against the tree it describes; this is read by someone who does
/// not have the tree and must not be given it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    /// The schema version this document claims to conform to.
    pub schema_version: String,

    /// What produced it.
    pub tool: Tool,

    /// Which identifier policy was applied, so a reader knows what the labels are.
    pub redaction: String,

    /// The machine it ran on, to whatever extent that is knowable and shareable.
    pub host: Host,

    /// What the run was asked to do.
    pub config: Config,

    /// What the run cost, and how well it used the machine.
    pub run: Run,

    /// What was found, before anything was run.
    pub population: Population,

    /// What every mutant came to.
    pub outcomes: Outcomes,

    /// What the instrumented build cost, and what it withdrew. Absent when nothing was built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,

    /// Where the run's time went, phase by phase. Absent when nothing was built, so no phase ran.
    ///
    /// The aggregates above say how much time the run spent; this says where. The copy and the
    /// preflight are components of `build.elapsedMs`; the census and the sweep are components of
    /// `run.testingMs`. Neither set sums to its aggregate — compiling sits between the copy and the
    /// baseline, and bookkeeping between the census and the sweep — because the point is to see
    /// which phase a slow run is slow in, not to reconcile the totals. Every phase that did not run
    /// is absent rather than zero: `null` means it did not happen, `0` would claim it happened
    /// instantly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phases: Option<Phases>,

    /// The distribution of mutant durations. Absent when nothing was run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durations: Option<Durations>,

    /// What each test binary cost, most expensive first.
    pub binaries: Vec<Binary>,

    /// What each mutator cost, most expensive first.
    ///
    /// Never redacted: a mutator name is ours, not the user's, and it is the single most useful
    /// axis in the whole document.
    pub mutators: Vec<Breakdown>,

    /// What each package cost, most expensive first.
    pub packages: Vec<Breakdown>,
}

/// What produced the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// Always `cargo-gamma`.
    pub name: String,

    /// The version of the tool that ran.
    pub version: String,
}

/// The machine, to whatever extent it is knowable and shareable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    /// The target family the tool was built for.
    pub os: String,

    /// The target architecture the tool was built for.
    pub arch: String,

    /// The parallelism the machine reports as available to this process.
    pub cores: usize,

    /// The compiler version string, which is the one host fact that changes a timing most.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,

    /// Whether `RUSTFLAGS` was set, without saying to what.
    ///
    /// The value can name internal crates, private registries and feature codenames. That it was
    /// set at all is the part that explains a build time.
    pub rustflags_set: bool,
}

/// What the run was asked to do.
///
/// Flags rather than a command line: a command line carries paths, package names and filter
/// patterns, and reconstructing it is not what a cost profile needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// The mutators that were selected, in registry order.
    pub mutators: Vec<String>,

    /// The job count the run was given.
    pub jobs: usize,

    /// The silence a mutant was allowed before it was presumed hung, when that was enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_ms: Option<u64>,

    /// Whether this run actually metered memory, which is not always what was configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metered: Option<bool>,

    /// Why memory went unbounded, when it was meant to be bounded and could not be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unbounded: Option<String>,

    /// The shard this run covered, as `index` and `count`, when it was sharded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<[u32; 2]>,
}

/// What the run cost, and how well it used the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    /// Wall time from the first thing the command did to the last.
    pub wall_ms: u64,

    /// The part that does not scale with the population: the instrumented build and the baseline.
    pub fixed_ms: u64,

    /// The part that does: everything left after the fixed cost.
    ///
    /// The split is the first question to ask of a slow run, because the two halves want opposite
    /// remedies — a faster machine against a smaller population.
    pub testing_ms: u64,

    /// The summed time of every mutant that ran.
    pub cpu_ms: u64,

    /// CPU over the testing window: how many workers the run really kept busy.
    ///
    /// A scheduler that is working lands within a fraction of the configured job count, and
    /// everything short of that is time spent waiting rather than testing.
    pub effective_jobs: f64,

    /// How large the scratch tree was at the end of the run.
    ///
    /// Only measured when `--diag` was given, because measuring it is a walk of every build
    /// artifact the run produced; `null` means it was not asked for, not that it was empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratch_bytes: Option<u64>,
}

/// What was found, before anything was run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Population {
    /// How many files were parsed.
    pub files: usize,

    /// How many mutants were generated.
    pub mutants: usize,

    /// How many workspace packages were involved.
    pub packages: usize,

    /// How many mutants a directive suppressed.
    pub suppressed: usize,

    /// How many skip directives suppressed nothing.
    pub idle_directives: usize,

    /// How many live mutants sharding excluded.
    pub sharded_out: usize,

    /// How many an earlier report had already settled.
    pub settled_out: usize,
}

/// What every mutant came to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcomes {
    pub killed: u32,
    pub survived: u32,
    pub timeout: u32,
    pub out_of_memory: u32,
    pub flaky: u32,
    pub unviable: u32,
    pub ignored: u32,
    pub uncovered: u32,
    pub not_built: u32,
    pub pending: u32,
}

/// What the instrumented build cost, and what it withdrew.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Build {
    /// How long the whole build took, across every round.
    pub elapsed_ms: u64,

    /// How long the baseline suite took.
    pub baseline_ms: u64,

    /// How many tests the baseline ran, when a harness announced a count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<usize>,

    /// How many mutants were withdrawn because they could not compile.
    pub withdrawn: usize,

    /// Whether the build had to widen to the whole workspace.
    pub widened: bool,

    /// What each round cost and withdrew, oldest first.
    ///
    /// The first round is what building this workspace costs at all; every round after it exists
    /// only because some mutant did not compile, and its time is the price of that mutant.
    pub rounds: Vec<Round>,

    /// Why the withdrawn mutants were withdrawn, densest pair first.
    ///
    /// Never redacted: a rustc error code and a mutator name are both ours, and together they are
    /// the difference between a mutator that could be taught to look before it mutates and an
    /// unavoidable cost of instrumenting the tree at all.
    pub withdrawals: Vec<Withdrawal>,

    /// The largest peak memory any one test binary reached during the baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_peak_bytes: Option<u64>,

    /// What the stale build-ordering hints did, when the run had any.
    ///
    /// Omitted entirely when no hint was available, which is different from a run that had hints
    /// and found them all wrong — the second is worth knowing about and the first is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordering_hints: Option<OrderingHints>,
}

/// What front-loading the mutants an earlier run could not compile actually bought.
///
/// There is deliberately no "rounds saved" here, and its absence is the honest part. That figure is
/// the length of a convergence that never ran, over a mutant population the compiler was never
/// shown in that shape; anything printed for it would be a model of a counterfactual presented as a
/// measurement, and a diagnostic bundle that does that once cannot be trusted anywhere.
///
/// What is measurable is the trade itself. `offered` is how many mutants the hints put in front of
/// the compiler early; `confirmed` is how many of those the compiler then refused, which is the
/// hints being right; `rounds` is the extra cargo invocations that cost. `confirmed` close to
/// `offered` is a hint set that is paying for its rounds, and `confirmed` near zero over several
/// rounds is one that is not — which is the question a reader actually has.
///
/// None of it can move a verdict. A mutant named here is built and judged exactly as it would have
/// been without the hint; only the order it met the compiler in changed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderingHints {
    /// How many hinted mutants were put in front of the compiler in a probe round.
    pub offered: usize,

    /// How many of those the compiler then refused to compile.
    pub confirmed: usize,

    /// How many extra build rounds the probes cost.
    pub rounds: u32,
}

/// One round of the instrumented build.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Round {
    pub elapsed_ms: u64,
    pub withdrew: usize,
}

/// One group of withdrawn mutants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Withdrawal {
    /// The rustc error code, or empty when the diagnostic carried none.
    pub code: String,

    /// The mutator whose mutants drew it, or empty when it could not be attributed.
    pub mutator: String,

    /// How many mutants this pair accounts for.
    pub mutants: usize,
}

/// Where the run's time went, phase by phase.
///
/// The copy and the preflight are components of `build.elapsedMs`; the baseline is `build.baselineMs`
/// restated so the fixed cost reads as a sequence rather than two unrelated numbers; the census and
/// the sweep are components of `run.testingMs`. The value of the split is that whether the per-test
/// census pays for itself — its cost against the launches it saves the sweep — can be read off one
/// run, where before it was folded invisibly inside the build.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Phases {
    /// What duplicating the workspace into the scratch tree cost. Part of `build.elapsedMs`.
    pub copy: Phase,

    /// What proving the tree compiles at all cost, before any mutant was staged. Part of
    /// `build.elapsedMs`.
    pub preflight: Phase,

    /// What the baseline suite cost. The same figure as `build.baselineMs`.
    pub baseline: Phase,

    /// What the per-test census cost and covered. Absent when `--whole-test-binaries` disabled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub census: Option<CensusPhase>,

    /// What the sweep cost and launched. Absent when nothing was swept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<SweepPhase>,
}

/// What one phase cost, when the cost is the only thing there is to say about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Phase {
    pub elapsed_ms: u64,
}

/// What the per-test census cost and covered.
///
/// The census spends one subprocess per test per binary; `walked` is exactly that count, and it is
/// the figure the census's cost has to be weighed against the sweep's launch count to know whether
/// the trade was positive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CensusPhase {
    pub elapsed_ms: u64,

    /// How many test executions the census walked, one subprocess apiece.
    pub walked: usize,

    /// How many test binaries the census examined.
    pub binaries: usize,
}

/// What the sweep cost and how it spent its launches.
///
/// `launches` is what turns the cost model's `build + Σ(launch + prefix)` from a formula into a
/// measurement; `probes` is what says whether the killer hints and the census are earning their
/// keep, since a probe is a launch the run only made because a hint pointed at it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepPhase {
    pub elapsed_ms: u64,

    /// How many test-binary subprocesses the sweep launched in total.
    pub launches: usize,

    /// How many of those launches were hint-directed probes.
    pub probes: usize,
}

/// The distribution of mutant durations.
///
/// Percentiles rather than every duration: the shape is what a cost profile needs, and a list one
/// entry per mutant would be both enormous and a fingerprint of the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Durations {
    pub evaluated: usize,
    pub min_ms: u64,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
}

/// What one test binary cost the run.
///
/// A binary's baseline is charged to every mutant that can reach it, so a single slow one is
/// multiplied by the population and is the most leveraged thing in a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binary {
    /// The package, subject to the redaction policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

    /// The binary's target name, subject to the redaction policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,

    pub baseline_ms: u64,
    /// `None` when no cutoff was calibrated, which is what a run with no baseline leaves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_bytes: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling_bytes: Option<u64>,
}

/// What one group of mutants cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakdown {
    /// What the group is, subject to the redaction policy where the name is the user's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    pub mutants: usize,
    pub cpu_ms: u64,
    pub survivors: usize,
    pub unviable: usize,
}

/// Everything the bundle needs that is not on the plan or the session.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// The parallelism the host makes available to this process.
    pub cores: usize,

    /// The job count the run was given.
    pub jobs: usize,

    /// Wall time for the whole command.
    pub wall: Duration,

    /// The mutators that were selected, in registry order.
    pub mutators: Vec<&'static str>,

    /// The shard this run covered, as `(count, index)`, when it was sharded.
    pub shard: Option<(u32, u32)>,

    /// How large the scratch tree was, when it was worth the walk to find out.
    pub scratch_bytes: Option<u64>,

    /// What to do with the identifiers.
    pub redaction: Redaction,

    /// The tool's version.
    pub version: &'a str,
}

/// Assembles the bundle.
///
/// `session` is absent when nothing was live, so nothing was built or measured. The population is
/// still worth describing: a run that found no work is exactly the kind that wants explaining.
#[must_use]
pub fn bundle(plan: &Plan, session: Option<&Session>, context: &Context<'_>) -> Bundle {
    let summary = Summary::of(&plan.mutants);
    let fixed = session.map_or(Duration::ZERO, |session| session.build + session.baseline_wall);
    let testing = context.wall.saturating_sub(fixed);
    let cpu: Duration = plan.mutants.iter().map(|mutant| Duration::from_millis(mutant.elapsed_ms)).sum();
    let wall_ms = millis(context.wall);
    let fixed_ms = millis(fixed);
    let testing_ms = wall_ms.saturating_sub(fixed_ms);

    Bundle {
        schema_version: SCHEMA_VERSION.to_owned(),
        tool: Tool {
            name: "cargo-gamma".to_owned(),
            version: context.version.to_owned(),
        },
        redaction: match context.redaction {
            Redaction::Hashed => "hashed",
            Redaction::Names => "names",
            Redaction::Omitted => "omitted",
        }
        .to_owned(),
        host: Host {
            os: consts::OS.to_owned(),
            arch: consts::ARCH.to_owned(),
            cores: context.cores,
            toolchain: redact_toolchain(crate::discover::toolchain(), context.redaction),
            rustflags_set: crate::discover::rustflags().is_some(),
        },
        config: Config {
            mutators: context.mutators.iter().map(|name| (*name).to_owned()).collect(),
            jobs: context.jobs,
            stall_ms: session.and_then(|session| session.stall).map(millis),
            metered: session.map(|session| session.metered),
            unbounded: session.and_then(|session| session.unbounded.clone()),
            shard: context.shard.map(|(count, index)| [index, count]),
        },
        run: Run {
            wall_ms,
            fixed_ms,
            testing_ms,
            cpu_ms: millis(cpu),
            effective_jobs: effective(cpu, testing),
            scratch_bytes: context.scratch_bytes,
        },
        population: Population {
            files: plan.files.len(),
            mutants: plan.mutants.len(),
            packages: plan.reach.len(),
            suppressed: plan.suppressed,
            idle_directives: plan.idle.len(),
            sharded_out: plan.sharded_out,
            settled_out: plan.settled_out,
        },
        outcomes: Outcomes {
            killed: summary.killed,
            survived: summary.survived,
            timeout: summary.timeout,
            out_of_memory: summary.out_of_memory,
            flaky: summary.flaky,
            unviable: summary.unviable,
            ignored: summary.ignored,
            uncovered: summary.uncovered,
            not_built: summary.not_built,
            pending: summary.pending,
        },
        build: session.map(build_of),
        phases: session.map(phases_of),
        durations: durations_of(&plan.mutants),
        binaries: session.map(|session| binaries_of(session, context.redaction)).unwrap_or_default(),
        mutators: breakdown(&plan.mutants, Redaction::Names, |mutant| mutant.mutator.to_string()),
        packages: breakdown(&plan.mutants, context.redaction, |mutant| mutant.package.to_string()),
    }
}

/// Applies the bundle's identifier policy to environment-derived tool program names.
///
/// Version output remains useful and does not contain the program paths used to invoke the tools.
fn redact_toolchain(toolchain: Option<String>, redaction: Redaction) -> Option<String> {
    let toolchain = toolchain?;

    Some(
        toolchain
            .lines()
            .filter_map(|line| {
                let (key, value) = ["rustc=", "cargo=", "rustc_wrapper=", "rustc_workspace_wrapper="]
                    .into_iter()
                    .find_map(|key| line.strip_prefix(key).map(|value| (key, value)))?;

                match redaction {
                    Redaction::Names => Some(line.to_owned()),
                    Redaction::Hashed => redaction.apply(value).map(|value| format!("{key}{value}")),
                    Redaction::Omitted => None,
                }
            })
            .chain(
                toolchain
                    .lines()
                    .filter(|line| {
                        !["rustc=", "cargo=", "rustc_wrapper=", "rustc_workspace_wrapper="]
                            .into_iter()
                            .any(|key| line.starts_with(key))
                    })
                    .map(str::to_owned),
            )
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Serializes the bundle as pretty-printed JSON.
///
/// Pretty rather than compact because the first thing anyone should do with it is read it, and a
/// file nobody can read before sending is a file nobody sends.
///
/// # Errors
///
/// Returns an error if the bundle cannot be serialized.
pub fn to_json(bundle: &Bundle) -> crate::Result<String> {
    serde_json::to_string_pretty(bundle)
        .map_err(|cause| crate::error::error!("could not serialize the diagnostics bundle").caused_by(cause))
}

/// What the instrumented build cost.
fn build_of(session: &Session) -> Build {
    Build {
        elapsed_ms: millis(session.build),
        baseline_ms: millis(session.baseline_wall),
        tests: session.tests,
        withdrawn: session.withdrawn,
        widened: session.widened,
        rounds: session
            .rounds_taken
            .iter()
            .map(|round| Round {
                elapsed_ms: millis(round.elapsed),
                withdrew: round.withdrew,
            })
            .collect(),
        withdrawals: session
            .census
            .iter()
            .map(|entry| Withdrawal {
                code: entry.code.clone(),
                mutator: entry.mutator.clone(),
                mutants: entry.mutants,
            })
            .collect(),
        baseline_peak_bytes: session.peak,

        // A run with no hint at all reports nothing rather than three zeros: zeros would read as
        // "the hints found nothing", which is a claim about the hints, and there were none.
        ordering_hints: (session.ordering.rounds > 0 || session.ordering.offered > 0).then_some(OrderingHints {
            offered: session.ordering.offered,
            confirmed: session.ordering.confirmed,
            rounds: session.ordering.rounds,
        }),
    }
}

/// Where the run's time went, phase by phase.
fn phases_of(session: &Session) -> Phases {
    Phases {
        copy: Phase {
            elapsed_ms: millis(session.phases.copy),
        },
        preflight: Phase {
            elapsed_ms: millis(session.phases.preflight),
        },
        baseline: Phase {
            elapsed_ms: millis(session.baseline_wall),
        },
        census: session.phases.census.as_ref().map(|census| CensusPhase {
            elapsed_ms: millis(census.elapsed),
            walked: census.walked,
            binaries: census.binaries,
        }),
        sweep: session.phases.sweep.as_ref().map(|sweep| SweepPhase {
            elapsed_ms: millis(sweep.elapsed),
            launches: sweep.launches,
            probes: sweep.probes,
        }),
    }
}

/// The test binaries, most expensive baseline first.
fn binaries_of(session: &Session, redaction: Redaction) -> Vec<Binary> {
    let mut binaries: Vec<&crate::exec::TestBinary> = session.binaries.iter().collect();

    binaries.sort_by_key(|binary| core::cmp::Reverse(binary.baseline));

    binaries
        .into_iter()
        .take(TOP)
        .map(|binary| Binary {
            package: redaction.apply(&binary.package),
            target: redaction.apply(&binary.target),
            baseline_ms: millis(binary.baseline),
            budget_ms: binary.budget.map(millis),
            peak_bytes: binary.peak,
            ceiling_bytes: binary.memory,
        })
        .collect()
}

/// The duration distribution, or `None` when nothing ran.
fn durations_of(mutants: &[Mutant]) -> Option<Durations> {
    let mut spent: Vec<u64> = mutants
        .iter()
        .map(|mutant| mutant.elapsed_ms)
        .filter(|elapsed| *elapsed > 0)
        .collect();

    if spent.is_empty() {
        return None;
    }

    spent.sort_unstable();

    Some(Durations {
        evaluated: spent.len(),
        min_ms: spent.first().copied().unwrap_or(0),
        p50_ms: percentile(&spent, 0.50),
        p90_ms: percentile(&spent, 0.90),
        p99_ms: percentile(&spent, 0.99),
        max_ms: spent.last().copied().unwrap_or(0),
    })
}

/// One ranked breakdown of the population, most expensive first.
fn breakdown(mutants: &[Mutant], redaction: Redaction, key: impl Fn(&Mutant) -> String) -> Vec<Breakdown> {
    let mut buckets: crate::HashMap<String, Breakdown> = crate::HashMap::default();

    for mutant in mutants {
        let entry = buckets.entry(key(mutant)).or_insert_with(|| Breakdown {
            name: None,
            mutants: 0,
            cpu_ms: 0,
            survivors: 0,
            unviable: 0,
        });

        entry.mutants += 1;
        entry.cpu_ms = entry.cpu_ms.saturating_add(mutant.elapsed_ms);

        match mutant.outcome {
            Outcome::Survived => entry.survivors += 1,
            Outcome::CompileError => entry.unviable += 1,
            _other => {}
        }
    }

    let mut rows: Vec<(String, Breakdown)> = buckets.into_iter().collect();

    rows.sort_by(|(left_name, left), (right_name, right)| right.cpu_ms.cmp(&left.cpu_ms).then_with(|| left_name.cmp(right_name)));

    rows.into_iter()
        .take(TOP)
        .map(|(name, row)| Breakdown {
            name: redaction.apply(&name),
            ..row
        })
        .collect()
}

/// CPU over the testing window, to one decimal place.
fn effective(cpu: Duration, testing: Duration) -> f64 {
    if testing.is_zero() {
        return 0.0;
    }

    (cpu.as_secs_f64() / testing.as_secs_f64() * 10.0).round() / 10.0
}

/// The value at a percentile of an ascending list.
fn percentile(ascending: &[u64], fraction: f64) -> u64 {
    if ascending.is_empty() {
        return 0;
    }

    #[expect(clippy::cast_precision_loss, reason = "a population that large has other problems")]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a fraction of a non-negative length"
    )]
    let index = ((ascending.len() as f64 - 1.0) * fraction).round() as usize;

    ascending.get(index).copied().unwrap_or(0)
}

/// A duration in whole milliseconds, which is the resolution everything else here is measured at.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::fixtures;

    fn context() -> Context<'static> {
        Context {
            cores: 3,
            jobs: 4,
            wall: Duration::from_secs(100),
            mutators: vec!["arith.add_to_sub"],
            shard: None,
            scratch_bytes: None,
            redaction: Redaction::default(),
            version: "0.0.0",
        }
    }

    fn plan() -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: "/work/subject".into(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    /// A run that built something, so the bundle has a session and therefore phase timings. The
    /// caller supplies the phase profile so a test can say what did and did not run.
    fn session_with(phases: crate::exec::Phases) -> Session {
        Session {
            baseline: Duration::from_millis(1500),
            baseline_wall: Duration::from_millis(500),
            tests: Some(12),
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::from_secs(7),
            peak: None,
            metered: false,
            unbounded: None,
            withdrawn: 0,
            census: Vec::new(),
            rounds: 1,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            scratch: "/work/scratch".into(),
            filtered: 0,
            widened: false,
            ordering: crate::exec::OrderingHints::default(),
            phases,
        }
    }

    /// A phase that ran is timed; one that did not is absent rather than zero, because a zero would
    /// read as a phase that happened instantly, and the whole question is whether it happened.
    #[test]
    fn a_run_that_built_something_carries_its_phase_timings() {
        let phases = crate::exec::Phases {
            copy: Duration::from_millis(12),
            preflight: Duration::from_millis(340),
            census: None,
            sweep: Some(crate::exec::SweepCost {
                elapsed: Duration::from_secs(42),
                launches: 47,
                probes: 12,
            }),
        };

        let built = bundle(&plan(), Some(&session_with(phases)), &context());
        let phases = built.phases.expect("a built run has phase timings");

        assert_eq!(phases.copy.elapsed_ms, 12);
        assert_eq!(phases.preflight.elapsed_ms, 340);
        assert_eq!(phases.baseline.elapsed_ms, 500);
        assert!(phases.census.is_none(), "no census ran, so the census phase is absent");

        let sweep = phases.sweep.expect("the sweep ran");

        assert_eq!(sweep.elapsed_ms, 42_000);
        assert_eq!(sweep.launches, 47);
        assert_eq!(sweep.probes, 12);
    }

    /// Nothing was built, so no phase ran, so there is nothing to profile.
    #[test]
    fn a_run_that_built_nothing_has_no_phases_at_all() {
        assert!(bundle(&plan(), None, &context()).phases.is_none());
    }

    /// The census is the whole subject here, and its figures must appear exactly when it ran.
    #[test]
    fn the_census_phase_is_present_only_when_a_census_ran() {
        let without = bundle(&plan(), Some(&session_with(crate::exec::Phases::default())), &context());

        assert!(without.phases.expect("phases").census.is_none(), "no census means no census phase");

        let phases = crate::exec::Phases {
            census: Some(crate::exec::CensusCost {
                elapsed: Duration::from_secs(8),
                walked: 1_681,
                binaries: 30,
            }),
            ..crate::exec::Phases::default()
        };

        let census = bundle(&plan(), Some(&session_with(phases)), &context())
            .phases
            .expect("phases")
            .census
            .expect("a census ran");

        assert_eq!(census.elapsed_ms, 8_000);
        assert_eq!(census.walked, 1_681);
        assert_eq!(census.binaries, 30);
    }

    /// The one relationship the bundle guarantees by construction: the baseline restated in the
    /// phase profile is the very figure the build reports, because both read `session.baseline_wall`.
    /// The copy and preflight are subsets of the build and the census and sweep of the testing
    /// window, but those are guaranteed by where the clocks sit in a real run, not by this code, so
    /// they are not asserted here.
    #[test]
    fn the_baseline_phase_restates_the_build_baseline_exactly() {
        let built = bundle(&plan(), Some(&session_with(crate::exec::Phases::default())), &context());

        let baseline_phase = built.phases.expect("phases").baseline.elapsed_ms;
        let build_baseline = built.build.expect("build").baseline_ms;

        assert_eq!(baseline_phase, build_baseline);
    }

    /// The document is read by a stranger, so the shape has to be exactly what old consumers expect:
    /// camelCase names, and a phase that did not run omitted rather than serialized as null.
    #[test]
    fn the_serialized_phases_use_camel_case_and_omit_the_phases_that_did_not_run() {
        let phases = crate::exec::Phases {
            copy: Duration::from_millis(1),
            preflight: Duration::from_millis(2),
            census: None,
            sweep: Some(crate::exec::SweepCost {
                elapsed: Duration::from_millis(3),
                launches: 4,
                probes: 5,
            }),
        };

        let json = to_json(&bundle(&plan(), Some(&session_with(phases)), &context())).expect("json");

        assert!(json.contains("\"phases\""), "{json}");
        assert!(json.contains("\"elapsedMs\""), "{json}");
        assert!(json.contains("\"launches\""), "{json}");
        assert!(json.contains("\"probes\""), "{json}");

        // The census did not run, so it must not appear as a key at all.
        assert!(!json.contains("\"census\""), "an absent census must be omitted, not null: {json}");
        assert!(!json.contains("\"walked\""), "{json}");
    }

    /// The whole point of the file. Anything a reader has to take on trust before attaching it to a
    /// public issue is a reason not to attach it.
    #[test]
    fn the_bundle_carries_no_absolute_path_and_no_source_text() {
        let json = to_json(&bundle(&plan(), None, &context())).expect("json");

        assert!(!json.contains("/work/subject"), "{json}");
        assert!(!json.contains("projectRoot"), "{json}");
    }

    #[test]
    fn hashing_is_the_default_and_hides_the_name() {
        assert_eq!(Redaction::default(), Redaction::Hashed);

        let hashed = Redaction::Hashed.apply("secret-product").expect("hashed");

        assert_ne!(hashed, "secret-product");
        assert_eq!(hashed.len(), HASH_WIDTH);
    }

    /// A hash nobody can group by is no better than omitting the name.
    #[test]
    fn hashing_the_same_name_twice_gives_the_same_label() {
        assert_eq!(Redaction::Hashed.apply("subject"), Redaction::Hashed.apply("subject"));
        assert_ne!(Redaction::Hashed.apply("subject"), Redaction::Hashed.apply("other"));
    }

    #[test]
    fn omitting_leaves_no_label_at_all() {
        assert_eq!(Redaction::Omitted.apply("secret-product"), None);
        assert_eq!(Redaction::Names.apply("secret-product").as_deref(), Some("secret-product"));
    }

    #[test]
    fn the_bundle_says_which_schema_it_is() {
        assert_eq!(bundle(&plan(), None, &context()).schema_version, "3");
    }

    /// The first question to ask of a slow run, so the split has to be right rather than plausible.
    #[test]
    fn the_fixed_and_testing_split_adds_up_to_the_wall_time() {
        let run = bundle(&plan(), None, &context()).run;

        assert_eq!(run.wall_ms, 100_000);
        assert_eq!(run.fixed_ms + run.testing_ms, run.wall_ms);
    }

    #[test]
    fn fractional_milliseconds_keep_the_displayed_duration_split_consistent() {
        let mut session = session_with(crate::exec::Phases::default());
        session.build = Duration::from_micros(4_400);
        session.baseline_wall = Duration::from_micros(2_500);
        let context = Context {
            wall: Duration::from_micros(10_100),
            ..context()
        };

        let run = bundle(&plan(), Some(&session), &context).run;

        assert_eq!((run.wall_ms, run.fixed_ms, run.testing_ms), (10, 6, 4));
        assert_eq!(run.fixed_ms + run.testing_ms, run.wall_ms);
    }

    #[test]
    fn redacted_toolchains_hide_environment_derived_program_paths() {
        let private = "/opt/acme/private-wrapper";
        let toolchain = Some(format!(
            "rustc=/toolchains/rustc\ncargo=/toolchains/cargo\nrustc_wrapper=\nrustc_workspace_wrapper={private}\nrustc 1.90.0\ncargo 1.90.0"
        ));

        for redaction in [Redaction::Hashed, Redaction::Omitted] {
            let redacted = redact_toolchain(toolchain.clone(), redaction).expect("toolchain");

            assert!(!redacted.contains(private), "{redaction:?}: {redacted}");
            assert!(redacted.contains("rustc 1.90.0"), "{redaction:?}: {redacted}");
        }
    }

    #[test]
    fn serialized_redacted_bundles_hide_workspace_wrapper_environment_paths() {
        const CHILD: &str = "CARGO_GAMMA_DIAG_REDACTION_CHILD";
        const PRIVATE: &str = "/opt/acme/private-wrapper";

        if std::env::var_os(CHILD).is_some() {
            for redaction in [Redaction::Hashed, Redaction::Omitted] {
                let context = Context { redaction, ..context() };
                let json = to_json(&bundle(&plan(), None, &context)).expect("serialized bundle");

                assert!(!json.contains(PRIVATE), "{redaction:?}: {json}");
            }

            return;
        }

        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "diag::bundle::tests::serialized_redacted_bundles_hide_workspace_wrapper_environment_paths",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("RUSTC_WORKSPACE_WRAPPER", PRIVATE)
            .output()
            .expect("diagnostic child process");

        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn concurrent_baselines_use_wall_time_and_keep_cores_distinct_from_jobs() {
        let mut session = session_with(crate::exec::Phases::default());
        session.build = Duration::from_secs(7);
        session.baseline = Duration::from_secs(5);
        session.baseline_wall = Duration::from_secs(2);

        let context = Context {
            cores: 3,
            jobs: 7,
            wall: Duration::from_secs(20),
            ..context()
        };
        let bundle = bundle(&plan(), Some(&session), &context);

        assert_eq!(
            (bundle.run.wall_ms, bundle.run.fixed_ms, bundle.run.testing_ms),
            (20_000, 9_000, 11_000)
        );
        assert_eq!(bundle.build.expect("build").baseline_ms, 2_000);
        assert_eq!(bundle.phases.expect("phases").baseline.elapsed_ms, 2_000);
        assert_eq!(bundle.host.cores, 3);
        assert_eq!(bundle.config.jobs, 7);
    }

    #[test]
    fn effective_jobs_is_cpu_over_the_testing_window() {
        assert!((effective(Duration::from_secs(30), Duration::from_secs(10)) - 3.0).abs() < f64::EPSILON);
        assert!(effective(Duration::from_secs(1), Duration::ZERO).abs() < f64::EPSILON);
    }

    #[test]
    fn percentiles_come_from_the_ascending_list() {
        let spent = [1_u64, 2, 3, 4, 5, 6, 7, 8, 9, 10];

        assert_eq!(percentile(&spent, 0.50), 6);
        assert_eq!(percentile(&spent, 0.90), 9);
        assert_eq!(percentile(&spent, 0.0), 1);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn a_run_that_measured_nothing_has_no_duration_distribution() {
        assert!(durations_of(&[]).is_none());
    }

    /// A mutator name is ours, and it is the most useful axis in the document; hashing it would
    /// leave the bundle unable to answer the question it is most often opened for.
    #[test]
    fn mutator_names_survive_redaction_while_package_names_do_not() {
        let mut plan = plan();

        plan.mutants = vec![mutant("subject", "arith.add_to_sub")];

        let built = bundle(&plan, None, &context());

        assert_eq!(built.mutators[0].name.as_deref(), Some("arith.add_to_sub"));
        assert_ne!(built.packages[0].name.as_deref(), Some("subject"));
        assert!(built.packages[0].name.is_some(), "the row still has to be groupable");
    }

    #[test]
    fn the_breakdown_ranks_the_most_expensive_group_first() {
        let mut plan = plan();
        let mut cheap = mutant("cheap", "arith.add_to_sub");
        let mut dear = mutant("dear", "relational.lt_to_le");

        cheap.elapsed_ms = 10;
        dear.elapsed_ms = 900;
        plan.mutants = vec![cheap, dear];

        let built = bundle(&plan, None, &context());

        assert_eq!(built.mutators[0].name.as_deref(), Some("relational.lt_to_le"));
        assert_eq!(built.mutators[0].cpu_ms, 900);
    }

    fn mutant(package: &str, mutator: &str) -> Mutant {
        Mutant {
            id: "id".to_owned().into(),
            package: package.to_owned().into(),
            mutator: mutator.to_owned().into(),
            ..fixtures::mutant()
        }
    }
}
