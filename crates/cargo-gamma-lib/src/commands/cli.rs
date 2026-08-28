// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use super::When;
use crate::ci::{Annotations, Level};
use crate::error::error;
use crate::ops::registry::Selection;

/// Adapts a [`crate::bounds`] check to clap's parser signature.
macro_rules! bounded {
    ($name:ident) => {
        fn $name(text: &str) -> Result<f64, String> {
            let value: f64 = text.parse().map_err(|_cause| format!("`{text}` is not a number"))?;

            crate::bounds::$name(text, value)
        }
    };
}

bounded!(seconds);
bounded!(factor);
bounded!(percentage);

/// Adapts the memory-size check to clap's parser signature.
fn size(text: &str) -> Result<u64, String> {
    crate::bounds::size(text)
}

/// Fast mutation testing for Rust.
///
/// With no subcommand, `run` is implied by argument normalization rather than by flattening
/// `RunArgs` here, so each help page lists only its own options.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-gamma",
    bin_name = "cargo gamma",
    version,
    propagate_version = true,
    about = "Fast mutation testing for Rust.",
    long_about = "Fast mutation testing for Rust.\n\nEvery selected mutant is compiled into one \
                  set of test binaries and chosen at run time, so a whole workspace is mutated \
                  without rebuilding it once per mutant.\n\nWith no subcommand, `run` is implied.",
    max_term_width = 100
)]
pub struct Cli {
    /// The subcommand to run. Defaults to `run`.
    #[command(subcommand)]
    pub command: Command,

    /// When to use color in output.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto", help_heading = "Global options")]
    pub color: When,

    /// When to show the progress display.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto", help_heading = "Global options")]
    pub progress: When,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run mutation testing.
    Run(RunArgs),

    /// List what would be done, without doing it.
    List(ListArgs),

    /// Explain a mutator, a mutant, or a suppression.
    Explain(ExplainArgs),

    /// Write suppressions into the source for mutants that cannot usefully be tested.
    Suppress(SuppressArgs),

    /// Remove skip directives that no longer suppress anything.
    Unsuppress(UnsuppressArgs),

    /// Combine per-shard reports into one answer.
    Merge(MergeArgs),

    /// Promote what a run learned about speed into a file the workspace can check in.
    Hints(HintsArgs),

    /// Delete cargo-gamma's cached data for a workspace.
    Clean(CleanArgs),

    /// Print a shell completion script.
    Completions(CompletionsArgs),
}

/// Arguments for `clean`.
#[derive(Debug, Args, Default)]
pub struct CleanArgs {
    /// Path to the workspace or package whose cache should be deleted.
    #[arg(short = 'd', long, value_name = "PATH", default_value = ".")]
    pub dir: Utf8PathBuf,
}

/// Arguments for `merge`.
#[derive(Debug, Args)]
pub struct MergeArgs {
    /// The reports to merge. A directory is read for its `*.json` files.
    #[arg(value_name = "REPORTS", required = true)]
    pub inputs: Vec<Utf8PathBuf>,

    /// Write the merged `mutation-testing-elements` document here.
    #[arg(long, value_name = "PATH", help_heading = "Reporting")]
    pub json_report: Option<Utf8PathBuf>,

    /// Write a self-contained merged HTML report here.
    #[arg(long, value_name = "PATH", help_heading = "Reporting")]
    pub html_report: Option<Utf8PathBuf>,

    /// Days after which a verdict is reported as stale. Zero disables the freshness window.
    ///
    /// Stale verdicts are still counted. Dropping them would shrink the denominator, which raises
    /// the score by forgetting rather than by testing.
    #[arg(long, value_name = "DAYS", default_value = "30", help_heading = "Merging")]
    pub window: u64,

    /// Fail if the merged assertion-killed score is below this percentage.
    ///
    /// Score gates belong here rather than on a shard run: a shard's own score moves by a third of
    /// a point per undetected mutant, so a threshold set on one fires on noise. Timeouts count as
    /// undetected because no test assertion rejected them.
    #[arg(long, value_name = "PERCENT", value_parser = percentage, help_heading = "Run control")]
    pub min_score: Option<f64>,
}

/// Arguments for `suppress`.
#[derive(Debug, Args)]
pub struct SuppressArgs {
    /// The run to perform before writing anything.
    #[command(flatten)]
    pub run: RunArgs,

    /// Print the diff without changing anything.
    ///
    /// Spelled apart from the run's own `--dry-run`, which stops before building at all: this one
    /// runs everything and holds back only the source edit.
    #[arg(long, help_heading = "Suppressing")]
    pub dry_run_suppress: bool,

    /// Which verdicts may be suppressed.
    ///
    /// A surviving mutant is never eligible and cannot be made eligible: it is a real gap in the
    /// test suite, and suppressing it would remove the gap from the score rather than from the code.
    ///
    /// Timeouts and out-of-memory verdicts are both eligible by default because they are one
    /// situation seen through two ceilings: whichever the runaway mutant reaches first is a property
    /// of the machine, so suppressing only one produces directives that hold on one host and not on
    /// another.
    #[arg(long, value_name = "LIST", default_value = "timeout,outofmem", help_heading = "Suppressing")]
    pub eligible: String,

    /// Edit source files that have uncommitted changes.
    ///
    /// The edit is undone in this process if anything goes wrong, but nothing survives the process
    /// being killed part-way through it. A committed file needs no journal of its own, because
    /// version control already is one; a file with uncommitted changes has nothing to be put back
    /// from, so it is refused rather than edited.
    #[arg(long, help_heading = "Suppressing")]
    pub allow_dirty: bool,
}

/// Arguments for `unsuppress`.
#[derive(Debug, Args)]
pub struct UnsuppressArgs {
    /// What to look at.
    #[command(flatten)]
    pub select: SelectArgs,

    /// Remove the directives instead of printing what would be removed.
    ///
    /// The preview is the default, which is the reverse of `suppress`. Writing a directive can be
    /// read back and reverted at leisure; deleting one that was in fact load-bearing turns a
    /// considered decision into a survivor nobody chose to accept, and by then the reason it
    /// carried is gone too.
    #[arg(long, help_heading = "Suppressing")]
    pub apply: bool,

    /// Remove directives from source files that have uncommitted changes.
    ///
    /// The removal is undone in this process if anything goes wrong, but nothing survives the
    /// process being killed part-way through it — and what a removal takes out is a hand-written
    /// reason nobody can reconstruct. A committed file needs no journal of its own, because version
    /// control already is one; a file with uncommitted changes has nothing to be put back from, so
    /// it is refused rather than edited.
    #[arg(long, help_heading = "Suppressing")]
    pub allow_dirty: bool,
}

/// Arguments for `hints`.
#[derive(Debug, Args, Default)]
pub struct HintsArgs {
    /// What to look at.
    #[command(flatten)]
    pub select: SelectArgs,

    /// Read the run record from this cache directory instead of cargo-gamma's default.
    ///
    /// Spelled the same as the run's own flag and for the same reason: a run told to scratch
    /// elsewhere left its record there, and a promotion looking under `target` would find nothing
    /// and say so, which reads exactly like "that run learned nothing".
    #[arg(long, value_name = "PATH", help_heading = "Cache")]
    pub cache_dir: Option<Utf8PathBuf>,

    /// Report what would be promoted without writing anything.
    #[arg(long, help_heading = "Run control")]
    pub dry_run: bool,
}

/// Arguments shared by commands that select mutants.
#[derive(Debug, Args, Clone)]
#[command(next_help_heading = "Selecting what to mutate")]
pub struct SelectArgs {
    /// Path to the workspace or package to analyze.
    #[arg(short = 'd', long, value_name = "PATH", default_value = ".")]
    pub dir: Utf8PathBuf,

    /// Mutators to apply, as a comma-separated selector list.
    ///
    /// A selector is a mutator name (`arith.add_to_sub`), a family (`relational`), a preset
    /// (`@arithmetic`), or `all`. Prefix a selector with `!` to remove it from the set. Selectors
    /// are applied left to right, so `@arithmetic,!bitwise` means what it reads as.
    #[arg(long, value_name = "SELECTORS", allow_hyphen_values = true)]
    pub mutators: Option<String>,

    /// Only mutate files matching these glob patterns.
    #[arg(long = "file", value_name = "GLOB")]
    pub files: Vec<String>,

    /// Skip files matching these glob patterns.
    #[arg(long = "exclude-file", value_name = "GLOB")]
    pub exclude_files: Vec<String>,

    /// Number of shards to divide the mutants into.
    #[arg(long, value_name = "COUNT")]
    pub shard_count: Option<u32>,

    /// Which shard to run, from 0.
    #[arg(long, value_name = "INDEX")]
    pub shard_index: Option<u32>,

    /// Only mutate lines added or changed by this unified diff, or `-` for standard input.
    ///
    /// This is what makes a run affordable on a pull request: the population is restricted to the
    /// code under review rather than sampled from the whole tree, so the result speaks about the
    /// change. Sharding is not a substitute, since a shard is a slice of everything.
    #[arg(short = 'D', long, value_name = "PATH")]
    pub in_diff: Option<Utf8PathBuf>,

    /// Only mutate these packages. Defaults to Cargo's package selection for the current directory.
    #[arg(short = 'p', long = "package", value_name = "NAME")]
    pub packages: Vec<String>,

    /// Mutate every package in the workspace.
    ///
    /// Selection follows cargo: without this flag and without `--package`, a run mutates the
    /// package owning the directory it was invoked from, and the workspace's default members when
    /// that directory is the workspace root.
    #[arg(long, conflicts_with = "packages")]
    pub workspace: bool,

    /// Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`.
    ///
    /// `fn_value.err_default` only reaches error types that implement `Default`. Naming a value
    /// here — `--error 'std::io::Error::from(std::io::ErrorKind::Other)'` — reaches the rest.
    #[arg(long = "error", value_name = "EXPR")]
    pub errors: Vec<String>,

    /// Which cargo features to build with.
    #[command(flatten)]
    pub features: FeatureArgs,

    /// Where the configuration comes from.
    #[command(flatten)]
    pub config: ConfigArgs,
}

/// Cargo feature selection, shared by discovery and the build.
///
/// Discovery and the build must agree: finding files under one feature set and compiling under
/// another produces guards that are not in the compiled tree.
#[derive(Debug, Args, Clone, Default)]
#[command(next_help_heading = "Cargo features")]
pub struct FeatureArgs {
    /// Cargo features to activate, comma-separated or repeated.
    #[arg(long = "features", value_name = "FEATURES")]
    pub features: Vec<String>,

    /// Activate every feature of every selected package.
    #[arg(long, conflicts_with = "no_default_features")]
    pub all_features: bool,

    /// Do not activate the `default` feature.
    #[arg(long)]
    pub no_default_features: bool,
}

/// Where the configuration file comes from.
#[derive(Debug, Args, Clone, Default)]
#[command(next_help_heading = "Configuration")]
pub struct ConfigArgs {
    /// Read configuration from this file instead of `gamma.toml`.
    #[arg(long = "config", value_name = "PATH", conflicts_with = "no_config")]
    pub path: Option<Utf8PathBuf>,

    /// Ignore the configuration file entirely.
    ///
    /// Without this there is no way to script a run that is independent of whatever the project
    /// happens to have committed.
    #[arg(long)]
    pub no_config: bool,
}

/// How long a build may take before it is abandoned.
///
/// Not offered to `estimate`. These change nothing an estimate reports — they can only turn a
/// working estimate into an error — and capping the build is at odds with a subcommand whose job is
/// to tell you what the build costs.
#[derive(Debug, Args, Default)]
#[command(next_help_heading = "Building")]
pub struct BuildLimitArgs {
    /// Seconds the build may take before the run is abandoned.
    ///
    /// A run builds once, so a build that never finishes costs everything rather than one mutant.
    #[arg(long, value_name = "SECONDS", value_parser = seconds, conflicts_with = "build_timeout_multiplier")]
    pub build_timeout: Option<f64>,

    /// Multiple of the first successful build's duration that a later build round is allowed.
    ///
    /// Rollback rounds rebuild the same tree with fewer mutants, so a round that runs far longer
    /// than the first one is not making progress.
    #[arg(long, value_name = "FACTOR", value_parser = factor)]
    pub build_timeout_multiplier: Option<f64>,

    /// How many times the tree may be rebuilt while withdrawing mutants that do not compile.
    ///
    /// A mutant like `Some(Default::default())` only compiles when the type happens to implement
    /// `Default`, and rustc reports only the errors it reaches before it stops, so a large tree can
    /// need many rounds to converge. Raise this when a run stops with a rollback-limit error and the
    /// withdrawal counts it prints are still falling.
    #[arg(long, value_name = "ROUNDS", default_value_t = crate::exec::DEFAULT_ROLLBACK_ROUNDS)]
    pub rollback_rounds: u32,
}

/// The options common to every command that builds, measures a baseline and runs tests.
///
/// Shared by `run`, `estimate` and `advise`, because all three build the tree and measure the
/// baseline the same way — an estimate that measured differently from the run it predicts would be
/// predicting a different run.
#[derive(Debug, Args, Default)]
#[command(next_help_heading = "Running tests")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is an independent command-line flag, and grouping them would only obscure that"
)]
pub struct MeasureArgs {
    /// Let cargo's own build output through, instead of only its progress bar.
    ///
    /// A run reports how far along the build is and any errors it hits, and swallows the rest so
    /// that compiling several thousand instrumented files does not bury the run. This shows all of
    /// it, which is what you want when the build itself is what is going wrong.
    #[arg(long)]
    pub show_build: bool,

    /// How many mutants to test at once. Defaults to one more than the available parallelism.
    #[arg(short = 'j', long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Multiple of each test binary's baseline duration that a mutant is allowed.
    #[arg(long, value_name = "FACTOR", value_parser = factor)]
    pub test_timeout_multiplier: Option<f64>,

    /// Lower bound on a test binary's timeout, however fast the baseline was.
    ///
    /// A test binary that finishes in a fraction of a second gets a budget of just over that
    /// duration, which a loaded machine can miss for reasons that have nothing to do with the mutant.
    #[arg(long, value_name = "SECONDS", value_parser = seconds)]
    pub minimum_test_timeout: Option<f64>,

    /// How much memory control to place around each test binary. `enforce` by default.
    ///
    /// A mutation can turn bounded allocation into unbounded allocation, which the timeout only
    /// stops after the machine has already been driven into swap. `measure` records what each test
    /// binary's whole process tree uses during the baseline and reports it, without ever stopping a
    /// mutant. `enforce` also holds each mutant to a ceiling derived from that measurement, and
    /// reports a mutant that breaches it as killed, shown as `OUTOFMEM`.
    ///
    /// Needs a delegated cgroup v2 on Linux, or a job object on Windows. A run that asked for this
    /// explicitly says so and stops where the host cannot provide it, rather than pretend to be
    /// protected; a run that merely inherited the default drops to `off` and says so.
    #[arg(long, value_name = "MODE", help_heading = "Memory")]
    pub memory: Option<crate::exec::MemoryControl>,

    /// Multiple of a test binary's baseline peak memory a mutant of it may reach.
    #[arg(long, value_name = "FACTOR", value_parser = factor, help_heading = "Memory")]
    pub memory_multiplier: Option<f64>,

    /// Absolute headroom added to a test binary's baseline peak memory.
    ///
    /// The ceiling is the larger of this and the multiplier, so this is what governs a binary whose
    /// baseline peak is small enough that doubling it would still leave no room for a lazily
    /// initialized table or a randomized test that picked a larger input.
    #[arg(long, value_name = "SIZE", value_parser = size, help_heading = "Memory")]
    pub memory_headroom: Option<u64>,

    /// An explicit memory ceiling for every test binary, instead of one derived from the baseline.
    ///
    /// Implies `--memory enforce`, and is the only way to bound a run that skips the baseline,
    /// since there is then nothing to calibrate a ceiling from.
    #[arg(long, value_name = "SIZE", value_parser = size, help_heading = "Memory")]
    pub memory_limit: Option<u64>,

    /// A memory ceiling for the baseline runs themselves.
    ///
    /// A ceiling derived from the baseline cannot protect the machine from a baseline that is
    /// itself runaway, which is the risk the first time an unfamiliar suite is measured. Implies
    /// `--memory measure`.
    #[arg(long, value_name = "SIZE", value_parser = size, help_heading = "Memory")]
    pub baseline_memory_limit: Option<u64>,

    /// Do not re-run inside a systemd scope to obtain the cgroup memory control needs.
    ///
    /// Bounding a test subtree needs a cgroup this process may create children under, and a host
    /// that started cargo-gamma outside a systemd user session never handed it one. Rather than
    /// give up the ceiling, cargo-gamma asks the systemd user manager for a delegated scope and
    /// re-runs itself inside it, reporting that it did so.
    ///
    /// Pass this to keep the original process, accepting that memory control is then unavailable:
    /// a run that asked for it explicitly stops, and one that inherited the default continues
    /// unbounded. Worth doing where a new process is itself the problem — a supervisor tracking
    /// this pid, or a session that must not gain a scope.
    #[arg(long, help_heading = "Memory")]
    pub no_relaunch: bool,

    /// Which Cargo profile to build with.
    ///
    /// Worth more here than in a per-mutant tool: the build is paid once and then thousands of
    /// mutants run against it, so an optimized profile usually pays for itself many times over.
    ///
    /// `release` also turns `debug_assertions` off, so a mutant that only a `debug_assert!` would
    /// have caught survives instead. Scores from two profiles are not comparable for that reason.
    #[arg(long, value_name = "NAME", help_heading = "Building")]
    pub profile: Option<String>,

    /// Pass an argument through to every cargo invocation.
    #[arg(
        short = 'C',
        long = "cargo-arg",
        value_name = "ARG",
        allow_hyphen_values = true,
        help_heading = "Building"
    )]
    pub cargo_args: Vec<String>,

    /// Pass an argument through to every test binary.
    #[arg(long = "cargo-test-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub cargo_test_args: Vec<String>,

    /// Run the tests of these packages when deciding a verdict.
    ///
    /// Separate from `--package`, which chooses what to mutate. By default each mutant is judged
    /// only by tests from its own package, so this is how it gets judged by a suite living
    /// somewhere else — a workspace that keeps its integration tests in a package of their own
    /// wants naming that package here.
    #[arg(long = "test-package", value_name = "NAME")]
    pub test_packages: Vec<String>,

    /// Only let these test targets decide a verdict.
    ///
    /// Matches cargo target names — a package's unit tests take the name of the lib or bin they
    /// live in, and each file under `tests/` is a target named after the file. Globs use `*` and
    /// `?`. Finer than `--test-package`, which cannot separate a package's real tests from the
    /// conformance corpus sitting beside them.
    #[arg(long = "include-test", value_name = "GLOB")]
    pub include_tests: Vec<String>,

    /// Do not let these test targets decide a verdict.
    ///
    /// Applied after `--include-test`, so an exclusion always wins. The usual reason is a target
    /// that is slow, flaky, or not an oracle at all — a conformance or fuzz corpus whose failures
    /// say nothing about whether a mutant was noticed. A pattern matching no target is an error.
    #[arg(long = "exclude-test", value_name = "GLOB")]
    pub exclude_tests: Vec<String>,

    /// Run test binaries through `cargo nextest` for per-test process isolation.
    ///
    /// The default launches each compiled test binary directly, which is faster. Passing `--nextest`
    /// hands those same binaries to `cargo nextest`, which gives every test its own process. Use it
    /// for a suite that depends on that isolation: such a suite is not merely slower without it but
    /// red, and a red baseline stops the run before it judges anything.
    ///
    /// Nextest is given the binaries this run already built rather than asked to build its own, so
    /// it never invokes cargo and a mutant costs one extra process rather than one extra build.
    #[arg(long)]
    pub nextest: bool,

    /// Let every workspace package's tests decide a verdict.
    ///
    /// By default each mutant is judged by its package's own suite and not by its dependents'.
    /// This lifts that restriction and asks every workspace package that can reach the mutant,
    /// which is more thorough and costs a workspace-wide build and test run per mutant.
    #[arg(long, conflicts_with = "test_packages")]
    pub test_workspace: bool,

    /// Run every selected test in each reachable test binary.
    ///
    /// By default every mutation site doubles as a reachability probe: gamma measures which
    /// individual tests execute each site, then runs only those tests against its mutant. This
    /// disables that measurement and runs each reachable binary whole instead.
    ///
    /// Use it when test reachability is nondeterministic because control flow depends on threads,
    /// the clock, randomness or hash iteration order. It is usually much slower, especially for
    /// survivors, because every test in a linked binary is repeated for every mutant it can reach.
    ///
    /// This does not override `--include-test`, `--exclude-test`, `--test-package` or package
    /// reachability; it changes only whether selected binaries are filtered to specific test cases.
    #[arg(long)]
    pub whole_test_binaries: bool,

    /// Put cargo-gamma's reusable workspace and Cargo artifacts in this directory.
    ///
    /// Lets a read-only checkout be mutated and moves the copy off a slow or network filesystem. It
    /// is refused when relocating would hide VCS metadata a build script can see from the source
    /// checkout. The path is the cache itself, must be empty on first use, and becomes owned by this
    /// workspace; another workspace cannot share it. Build artifacts live here too, so reusing the
    /// directory across this workspace's runs keeps them incremental while a fresh one starts cold.
    #[arg(long, value_name = "PATH", help_heading = "Cache")]
    pub cache_dir: Option<Utf8PathBuf>,

    /// Copy files version control ignores into the cached workspace as well.
    ///
    /// Files git tracks are always copied, whatever an ignore rule says about them; this is for the
    /// untracked ones. Reach for it when the build reads something a shared `.gitignore` excludes —
    /// a generated module, a downloaded fixture — which would otherwise have to be fixed by editing
    /// an ignore file that is not this tool's to edit. Everything else the tree has ever built is
    /// copied too, so a run with it on can be markedly slower to start.
    #[arg(long, help_heading = "Cache")]
    pub copy_ignored: bool,

    /// Arguments passed to every test binary, after `--`.
    ///
    /// The natural place to name the tests a run should consider, as in `-- --skip slow_`.
    #[arg(last = true, value_name = "TEST_ARGS")]
    pub test_args: Vec<String>,
}

/// Arguments for `run`.
#[derive(Debug, Args, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is an independent command-line flag, and grouping them would only obscure that"
)]
pub struct RunArgs {
    /// Which mutants to consider.
    #[command(flatten)]
    pub select: SelectArgs,

    /// How the build and the baseline are measured.
    #[command(flatten)]
    pub measure: MeasureArgs,

    /// How long the build may take.
    #[command(flatten)]
    pub limits: BuildLimitArgs,

    /// Write all user-facing artifacts to this directory.
    ///
    /// The directory is created when it does not exist. It contains the JSON, HTML and SARIF
    /// reports, performance advice and diagnostics bundle.
    #[arg(long, value_name = "PATH", help_heading = "Reporting")]
    pub artifact_dir: Option<Utf8PathBuf>,

    /// Fail the run if the assertion-killed mutation score is below this percentage.
    ///
    /// Timeouts and out-of-memory mutants count against the score because no test assertion
    /// rejected them.
    #[arg(long, value_name = "PERCENT", value_parser = percentage, help_heading = "Run control")]
    pub min_score: Option<f64>,

    /// How an incremental run reuses the last run: `no` starts cold; `build` reuses compiler
    /// unviability and checked execution hints.
    #[arg(long, value_enum, help_heading = "Run control")]
    pub incremental: Option<crate::exec::IncrementalMode>,

    /// List the mutants the suite killed, not just the ones that survived.
    ///
    /// A run reports what escaped, because that is what needs acting on. This shows the other side:
    /// what the suite actually killed, which is how you confirm it is testing what you think it is.
    #[arg(long, help_heading = "Reporting")]
    pub show_killed: bool,

    /// List every mutant that could not be compiled, not just how many there were.
    ///
    /// A mutant that does not compile says nothing about the test suite, and a large workspace
    /// produces thousands of them, so the summary counts them instead. Ask for the list when the
    /// question is which constructs the encoding could not express.
    #[arg(long, help_heading = "Reporting")]
    pub show_unviable: bool,

    /// Keep an incomplete scratch workspace after errors so it can be inspected.
    #[arg(long, help_heading = "Run control")]
    pub leak_dirs: bool,

    /// Skip the baseline run.
    ///
    /// Faster, and strictly less trustworthy: without it there is no evidence that a failure was
    /// caused by the mutant rather than by the suite already being red.
    #[arg(long, help_heading = "Run control")]
    pub no_baseline: bool,

    /// Believe a failing test without re-running it with no mutant active.
    ///
    /// Saves one run per kill, and gives up the ability to tell a detection from a flaky test: a
    /// test that fails either way is counted as having caught every mutant it was run against.
    #[arg(long, help_heading = "Run control")]
    pub no_confirm: bool,

    /// Find and report mutants without building or running anything.
    #[arg(long, help_heading = "Run control")]
    pub dry_run: bool,

    /// Load the report viewer from a CDN instead of embedding it.
    ///
    /// Produces a much smaller file, at the cost of needing network access to read it.
    #[arg(long, help_heading = "Reporting")]
    pub html_external: bool,

    /// How loudly a survivor is reported to a SARIF consumer.
    ///
    /// A surviving mutant is an observation about the test suite rather than a defect in the code,
    /// and drowning the security tab is how a good signal gets turned off.
    #[arg(long, value_name = "LEVEL", default_value = "note", help_heading = "Reporting")]
    pub sarif_level: Level,

    /// Annotate the diff and write a job summary when running inside a CI system.
    #[arg(long, value_name = "WHEN", default_value = "auto", help_heading = "Reporting")]
    pub annotations: Annotations,

    /// What to do with package and binary names in the diagnostics bundle.
    ///
    /// Hashed by default. A timing profile needs to tell one row from another and to group rows
    /// that belong together, and a stable hash gives both without naming an unreleased codebase.
    #[arg(long, value_name = "POLICY", default_value = "hashed", help_heading = "Reporting")]
    pub diag_names: crate::diag::Redaction,

    /// Project what the rest of the run will cost, once the build and baseline have been measured.
    ///
    /// Printed at the only moment it is both possible and useful: everything before it was
    /// measured, and everything after it is the wait you are deciding whether to sit through. The
    /// range assumes a killed mutant gets through 60% of the tests that can reach it before one
    /// of them fails.
    #[arg(long, help_heading = "Run control")]
    pub estimate: bool,

    /// Dump what the run measured about itself, for people working on this tool.
    ///
    /// Hidden, unstable and undocumented on purpose: it exists so that a change to the scheduler,
    /// the build sequencing or the mutator catalog can be judged against numbers instead of
    /// against how the run felt. Nothing here is a promise, and none of it is meant to be parsed.
    #[arg(long, hide = true, help_heading = "Run control")]
    pub diag: bool,

    /// Wait out the whole budget for every mutant instead of cutting off one that has stopped
    /// making progress.
    ///
    /// A hung mutant is normally detected as soon as its test binary goes quiet for longer than
    /// the baseline ever did, which is usually far sooner than its timeout. Turn this off if a
    /// test legitimately goes silent for much longer under mutation than it ever does healthy.
    #[arg(long, help_heading = "Run control")]
    pub no_stall_detection: bool,
}

/// Arguments for `completions`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// The shell to generate a completion script for.
    #[arg(value_name = "SHELL")]
    pub shell: Shell,
}

/// What `list` can enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListKind {
    /// The mutants that would be generated.
    Mutants,

    /// The mutator registry.
    Mutators,

    /// The source files that would be analyzed.
    Files,

    /// The named mutator presets.
    #[value(alias = "profiles")]
    Presets,
}

/// Arguments for `list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// What to list.
    #[arg(value_enum, default_value = "mutants")]
    pub what: ListKind,

    /// Which mutants to consider.
    #[command(flatten)]
    pub select: SelectArgs,

    /// Emit machine-readable JSON instead of text.
    #[arg(long, help_heading = "Reporting")]
    pub json: bool,

    /// Write the population as a report document, for `merge` to withdraw retired mutants against.
    #[arg(long, value_name = "PATH", help_heading = "Reporting")]
    pub json_report: Option<Utf8PathBuf>,
}

/// Arguments for `explain`.
#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// A mutator name, family, preset, or mutant id.
    #[arg(value_name = "SUBJECT")]
    pub subject: String,
}

impl Default for SelectArgs {
    fn default() -> Self {
        Self {
            dir: Utf8PathBuf::from("."),
            mutators: None,
            files: Vec::new(),
            exclude_files: Vec::new(),
            shard_count: None,
            shard_index: None,
            in_diff: None,
            packages: Vec::new(),
            workspace: false,
            errors: Vec::new(),
            features: FeatureArgs::default(),
            config: ConfigArgs::default(),
        }
    }
}

impl SelectArgs {
    /// Resolves the `--mutators` selector list into a concrete set of mutators.
    pub fn selection(&self) -> crate::Result<Selection> {
        let mut selection = self
            .mutators
            .as_deref()
            .map_or_else(|| Ok(Selection::default_preset()), Selection::parse)?;

        // An explicit `--mutators` list is the whole set, so `--error` must not smuggle a mutator into
        // it. Dropping the values silently would be worse still, so say so.
        if self.mutators.is_some() && !self.errors.is_empty() && !selection.contains("fn_value.err_with") {
            selection.drop_errors();
            return Ok(selection);
        }

        selection.set_errors(self.errors.clone());
        Ok(selection)
    }

    /// Validates the effective sharding settings and names the shard to run.
    ///
    /// This is the only place the count and the index are checked against each other, and it runs
    /// on the values the configuration file and the command line have already been folded into.
    /// The pair is whole nowhere else: the count belongs in the committed file, because every job
    /// in the matrix has to agree on it, while the index differs per job and arrives on the command
    /// line — so a check made while the command line is parsed would reject the split the file
    /// exists to support, and a check made on the file alone would not see the index at all.
    ///
    /// A half pair is refused rather than rounded down to "no sharding". Silently running the whole
    /// population when a CI job asked for a slice of it is the failure that costs a night: the job
    /// passes, the report looks complete, and nothing says the run was eight times the size it was
    /// budgeted for.
    pub fn shard(&self) -> crate::Result<Option<(u32, u32)>> {
        match (self.shard_count, self.shard_index) {
            (None, None) => Ok(None),

            (Some(count), None) => Err(error!(
                "a shard count of {count} was set without a shard index; a shard is both, so pass `--shard-index` or set `index` in the `[shard]` table"
            )
            .usage()),

            (None, Some(index)) => Err(error!(
                "a shard index of {index} was set without a shard count; a shard is both, so pass `--shard-count` or set `count` in the `[shard]` table"
            )
            .usage()),

            (Some(count), Some(index)) => {
                if count == 0 {
                    return Err(error!("a shard count of 0 divides the population into nothing; it must be at least 1").usage());
                }

                if count > crate::merge::MAX_SHARDS {
                    return Err(error!(
                        "a shard count of {count} is more than the {} a merge will account for; a rotation that large cannot be merged back together",
                        crate::merge::MAX_SHARDS
                    )
                    .usage());
                }

                if index >= count {
                    return Err(error!(
                        "shard index {index} is out of range for a shard count of {count}; valid indices are 0..{}",
                        count - 1
                    )
                    .usage());
                }

                Ok(Some((count, index)))
            }
        }
    }
}

impl FeatureArgs {
    /// Renders the selection as the cargo arguments that express it.
    #[must_use]
    pub fn to_cargo_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        if self.all_features {
            args.push("--all-features".to_owned());
        }

        if self.no_default_features {
            args.push("--no-default-features".to_owned());
        }

        if !self.features.is_empty() {
            args.push("--features".to_owned());
            args.push(self.features.join(","));
        }

        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_index_must_be_in_range() {
        let args = SelectArgs {
            shard_count: Some(4),
            shard_index: Some(4),
            ..SelectArgs::default()
        };

        let error = args.shard().unwrap_err();

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("out of range"), "{error}");
        assert!(error.to_string().contains("0..3"), "{error}");
    }

    /// The last index is inside the range, and an off-by-one here would reject a whole shard of a
    /// matrix — the last job, which is the one nobody notices is missing.
    #[test]
    fn the_last_shard_index_is_in_range() {
        let args = SelectArgs {
            shard_count: Some(4),
            shard_index: Some(3),
            ..SelectArgs::default()
        };

        assert_eq!(args.shard().expect("the last index is valid"), Some((4, 3)));
    }

    #[test]
    fn a_zero_shard_count_is_rejected() {
        let args = SelectArgs {
            shard_count: Some(0),
            shard_index: Some(0),
            ..SelectArgs::default()
        };

        let error = args.shard().unwrap_err();

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("at least 1"), "{error}");
    }

    /// A rotation wider than a merge can account for is refused where it is asked for, not where it
    /// is discovered.
    ///
    /// The merge side carries a ceiling on how many shards it will track, and a count above it
    /// leaves a rotation whose slices each run fine and which can never be put back together. The
    /// producing run is the only place that failure is cheap: it costs one refused command instead
    /// of a whole matrix of green jobs and a merge that cannot report on them.
    #[test]
    fn a_shard_count_beyond_what_a_merge_can_account_for_is_rejected() {
        let args = SelectArgs {
            shard_count: Some(crate::merge::MAX_SHARDS + 1),
            shard_index: Some(0),
            ..SelectArgs::default()
        };

        let error = args.shard().unwrap_err();

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("cannot be merged back together"), "{error}");

        let at_ceiling = SelectArgs {
            shard_count: Some(crate::merge::MAX_SHARDS),
            shard_index: Some(0),
            ..SelectArgs::default()
        };

        assert_eq!(
            at_ceiling.shard().expect("the ceiling itself is a mergeable rotation"),
            Some((crate::merge::MAX_SHARDS, 0))
        );
    }

    /// A count with no index is the shape of a configuration file that names the matrix width and a
    /// job that forgot to say which slice it is. Running the whole population instead would pass,
    /// look complete, and cost the entire budget.
    #[test]
    fn a_count_without_an_index_is_rejected() {
        let args = SelectArgs {
            shard_count: Some(8),
            shard_index: None,
            ..SelectArgs::default()
        };

        let error = args.shard().unwrap_err();

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("--shard-index"), "{error}");
        assert!(error.to_string().contains("shard count of 8"), "{error}");
    }

    #[test]
    fn an_index_without_a_count_is_rejected() {
        let args = SelectArgs {
            shard_count: None,
            shard_index: Some(3),
            ..SelectArgs::default()
        };

        let error = args.shard().unwrap_err();

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("--shard-count"), "{error}");
        assert!(error.to_string().contains("shard index of 3"), "{error}");
    }

    /// Zero is a value like any other, so a lone zero is a half pair rather than an absence. The
    /// distinction matters because `Option::unwrap_or_default` on either field would turn one of
    /// these into a silent full-population run.
    #[test]
    fn a_lone_zero_is_still_a_half_pair() {
        let count_only = SelectArgs {
            shard_count: Some(0),
            shard_index: None,
            ..SelectArgs::default()
        };
        let index_only = SelectArgs {
            shard_index: Some(0),
            ..SelectArgs::default()
        };

        assert!(count_only.shard().unwrap_err().to_string().contains("--shard-index"));
        assert!(index_only.shard().unwrap_err().to_string().contains("--shard-count"));
    }

    /// One shard containing everything is a degenerate but legitimate matrix of one job.
    #[test]
    fn a_single_shard_is_accepted() {
        let args = SelectArgs {
            shard_count: Some(1),
            shard_index: Some(0),
            ..SelectArgs::default()
        };

        assert_eq!(args.shard().expect("one shard of one"), Some((1, 0)));
    }

    #[test]
    fn a_valid_shard_is_accepted() {
        let args = SelectArgs {
            shard_count: Some(4),
            shard_index: Some(3),
            ..SelectArgs::default()
        };

        assert_eq!(args.shard().unwrap(), Some((4, 3)));
    }

    #[test]
    fn no_sharding_arguments_means_no_shard() {
        assert_eq!(SelectArgs::default().shard().unwrap(), None);
    }

    /// The command line no longer requires the two together, because the file supplies one of them
    /// and is read after parsing. What must not follow is that a half pair typed on the command
    /// line becomes acceptable — it is refused later instead, on the effective values.
    #[test]
    fn half_a_pair_parses_and_is_refused_afterwards() {
        let cli = Cli::try_parse_from(["cargo-gamma", "list", "--shard-count", "4"]).expect("parsing must not reject the split");

        let Command::List(args) = cli.command else {
            panic!("the list subcommand was parsed as something else");
        };

        assert_eq!(args.select.shard_count, Some(4));
        assert!(args.select.shard().unwrap_err().is_usage());
    }

    #[test]
    fn no_ops_argument_selects_the_default_preset() {
        let selection = SelectArgs::default().selection().unwrap();

        assert!(selection.contains("fn_value.default"));
        assert!(selection.contains("stmt.delete_call"));
    }

    #[test]
    fn mutator_presets_are_listed_by_the_new_name_and_the_old_name_remains_an_alias() {
        for name in ["presets", "profiles"] {
            let cli = Cli::try_parse_from(["cargo-gamma", "list", name]).expect("list kind parses");
            let Command::List(args) = cli.command else {
                panic!("expected list");
            };

            assert_eq!(args.what, ListKind::Presets);
        }
    }

    #[test]
    fn naming_error_values_turns_the_error_mutator_on() {
        // The mutator is registered on like everything else, but it is inert until the user names
        // something for it to substitute, so supplying a value has to keep it on rather than being
        // the thing that enables it.
        let args = SelectArgs {
            errors: vec!["MyError::Io".to_owned()],
            ..SelectArgs::default()
        };

        let selection = args.selection().unwrap();

        assert!(selection.contains("fn_value.err_with"));
        assert_eq!(selection.errors(), ["MyError::Io".to_owned()]);
    }

    #[test]
    fn error_values_are_ignored_when_the_mutator_is_deselected() {
        let args = SelectArgs {
            mutators: Some("relational".to_owned()),
            errors: vec!["MyError::Io".to_owned()],
            ..SelectArgs::default()
        };

        assert!(args.selection().unwrap().errors().is_empty());
    }

    #[test]
    fn feature_arguments_render_as_cargo_spells_them() {
        let features = FeatureArgs {
            features: vec!["a,b".to_owned()],
            all_features: false,
            no_default_features: true,
        };

        assert_eq!(
            features.to_cargo_args(),
            vec!["--no-default-features".to_owned(), "--features".to_owned(), "a,b".to_owned()]
        );
    }

    #[test]
    fn no_feature_arguments_render_as_nothing() {
        assert!(FeatureArgs::default().to_cargo_args().is_empty());
    }

    #[test]
    fn all_features_render_before_named_features() {
        let features = FeatureArgs {
            features: vec!["serde".to_owned(), "cli".to_owned()],
            all_features: true,
            no_default_features: false,
        };

        assert_eq!(
            features.to_cargo_args(),
            vec!["--all-features".to_owned(), "--features".to_owned(), "serde,cli".to_owned()]
        );
    }

    #[test]
    fn the_cli_definition_is_valid() {
        use clap::CommandFactory as _;

        Cli::command().debug_assert();
    }

    /// The default eligibility is both ceilings, and it is a real parse rather than a string check.
    ///
    /// Asserting on the `default_value` literal alone would pass even if the token were misspelled,
    /// because nothing in clap requires the default to be a value `Eligible::parse` accepts — the
    /// failure would surface only when a user ran `suppress` with no `--eligible`.
    #[test]
    fn suppress_defaults_to_suppressing_both_timeouts_and_out_of_memory() {
        let cli = Cli::try_parse_from(["cargo-gamma", "suppress"]).expect("suppress parses with no arguments");

        let Command::Suppress(args) = cli.command else {
            panic!("expected the suppress subcommand");
        };

        let eligible = crate::fix::Eligible::parse(&args.eligible).expect("the default must be a value the parser accepts");

        assert_eq!(eligible, vec![crate::fix::Eligible::Timeout, crate::fix::Eligible::OutOfMemory]);
    }

    #[test]
    fn a_zero_merge_window_is_the_documented_disabled_value() {
        use clap::CommandFactory as _;

        let cli = Cli::try_parse_from(["cargo-gamma", "merge", "report.json", "--window", "0"]).expect("zero window");
        let Command::Merge(args) = cli.command else {
            panic!("expected merge");
        };

        assert_eq!(args.window, 0);
        let help = Cli::command()
            .find_subcommand_mut("merge")
            .expect("merge")
            .render_long_help()
            .to_string();
        assert!(help.contains("Zero disables the freshness window"), "{help}");
    }

    /// The `bounded!`-generated wrappers and the memory-size wrapper are what clap actually calls
    /// as `value_parser`s; `crate::bounds` has its own tests for the underlying range checks, but
    /// those never exercise the adapters here, and a wrapper that stopped forwarding its argument
    /// correctly would only show up once a user typed a flag, not in any test of `bounds` itself.
    #[test]
    fn the_bound_wrappers_forward_to_the_underlying_checks() {
        _ = seconds("1.5").expect("in range");
        _ = seconds("-5").expect_err("out of range");
        assert!(seconds("not-a-number").expect_err("not a number").contains("not a number"));

        _ = factor("1.2").expect("in range");
        _ = factor("-1").expect_err("out of range");

        _ = percentage("50").expect("in range");
        _ = percentage("150").expect_err("out of range");

        assert_eq!(size("1024"), Ok(1024));
        _ = size("not-a-size").expect_err("not a size");
    }

    /// `-V` is `--version` everywhere, and `-v` is not bound at all.
    ///
    /// Both short flags were once bound to `run` reporting options — `-V` to `--unviable` and `-v`
    /// to `--killed` — so `cargo gamma run -V` silently started a full run instead of printing a
    /// version. Nothing failed, which is why it survived: clap only rejects a collision when the
    /// flag it collides with exists on that subcommand, and `version` was not propagated then.
    #[test]
    fn short_v_is_version_on_a_subcommand_and_never_a_reporting_flag() {
        use clap::CommandFactory as _;

        for short in ['v', 'V'] {
            let error = Cli::try_parse_from(["cargo gamma", "run", &format!("-{short}")]).expect_err("neither short flag may start a run");

            let kind = error.kind();
            assert_ne!(kind, clap::error::ErrorKind::DisplayHelp, "-{short} must not be help");

            if short == 'V' {
                assert_eq!(
                    kind,
                    clap::error::ErrorKind::DisplayVersion,
                    "-V must print the version, not run: {error}"
                );
            } else {
                assert_eq!(
                    kind,
                    clap::error::ErrorKind::UnknownArgument,
                    "-v must stay free for a future --verbose: {error}"
                );
            }
        }

        let run = Cli::command()
            .get_subcommands()
            .find(|command| command.get_name() == "run")
            .expect("run exists")
            .clone();

        for argument in run.get_arguments() {
            assert_ne!(argument.get_short(), Some('v'), "{} claimed -v", argument.get_id());
        }
    }

    /// Artifact routing is directory-wide; individual report path flags are not accepted.
    #[test]
    fn artifact_dir_replaces_individual_report_paths() {
        let cli = Cli::try_parse_from(["cargo gamma", "run", "--artifact-dir", "out"]).expect("the directory parses");

        match cli.command {
            Command::Run(args) => assert_eq!(args.artifact_dir.unwrap(), "out"),
            _ => panic!("expected run"),
        }

        for removed in ["--html-report", "--json-report", "--sarif-report", "--advice", "--diag-bundle"] {
            _ = Cli::try_parse_from(["cargo gamma", "run", removed, "out"]).expect_err("individual report paths are gone");
        }

        let cli = Cli::try_parse_from(["cargo gamma", "run", "--cache-dir", "cache"]).expect("cache directory parses");

        match cli.command {
            Command::Run(args) => assert_eq!(args.measure.cache_dir.unwrap(), "cache"),
            _ => panic!("expected run"),
        }

        _ = Cli::try_parse_from(["cargo gamma", "run", "--scratch-dir", "cache"]).expect_err("the old cache option is gone");
    }

    /// Every flag taking a filesystem path presents the same `<PATH>` placeholder.
    ///
    /// `--config` once said `<FILE>` and `--cache-dir` said `<DIR>`, which reads as though they
    /// accept different things from the eleven other path-valued flags. They do not.
    #[test]
    fn path_valued_flags_all_use_the_same_placeholder() {
        use clap::CommandFactory as _;

        let command = Cli::command();
        let mut checked = 0;

        for subcommand in command.get_subcommands() {
            for argument in subcommand.get_arguments() {
                let id = argument.get_id().as_str();

                if !matches!(id, "config" | "cache_dir" | "artifact_dir" | "dir") {
                    continue;
                }

                let names = argument.get_value_names().unwrap_or_default();

                assert_eq!(
                    names.first().map(clap::builder::Str::as_str),
                    Some("PATH"),
                    "{}'s {id} does not say PATH",
                    subcommand.get_name()
                );

                checked += 1;
            }
        }

        assert!(checked >= 4, "the filter matched almost nothing: {checked}");
    }
}
