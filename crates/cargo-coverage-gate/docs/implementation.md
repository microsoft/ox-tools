# cargo-coverage-gate implementation

This guide describes how the crate turns LCOV records into deterministic
failure diagnostics. The user-visible behavior is defined by the
[design](design/README.md).

## Collection pipeline

The optional `run` mode keeps collection and evaluation separated internally:

1. Cargo metadata resolves every repeated `--package` selector to a concrete
   workspace member. Exact `name@version` specs are passed to collection tools,
   while bare names are passed to the evaluator. With no selectors, both
   collection and evaluation cover the workspace.
2. Collection uses the invoking environment's Cargo and rustc, honoring
   inherited `CARGO`, `RUSTC`, `RUSTUP_TOOLCHAIN`, `PATH`, and related
   variables. Instrumented runs validate that the effective Cargo and rustc are
   nightly and that cargo-llvm-cov is 0.9.0 or newer. Version 0.9.0 is required
   because it introduced `--workspace` support for the `report` subcommand
   used by the default selection.
3. Each feature configuration gets an isolated clean, instrumented
   `cargo llvm-cov nextest --no-report --locked` run. Both instrumented and
   plain-nextest no-gate paths pass `--no-tests=pass`, allowing packages with a
   valid zero-test harness to continue to report generation, and both pass
   `--locked`. Nextest and Cargo retain their ordinary output, including
   rendered compiler diagnostics; the collector does not request or filter
   machine-readable Cargo messages.
   The instrumented target is unique per invocation beneath
   `target/coverage-gate/` and is removed by an RAII guard on every return
   path. Concurrent runs therefore cannot clean or merge each other's
   profiles.
   After evaluation, a pure combiner gives evaluation errors precedence over
   cleanup errors, preserves nonzero rendered verdicts with an explicit cleanup
   warning, and converts a passing evaluation plus cleanup failure into an
   operational error. Explicit cleanup disarms the guard after one attempt, so
   `Drop` never retries the same failed deletion.
4. `cargo llvm-cov report --lcov` receives the same package and target
   selection. cargo-llvm-cov therefore owns raw-profile merging, ordinary and
   nested trybuild object discovery, build-directory handling, and its full
   default filename filter. On Windows command-line overflow only, the
   collector replays cargo-llvm-cov's complete failed export arguments through
   an LLVM response file.
5. Each report is written directly to its stable per-configuration path under
   `--coverage-dir`, then that same path is passed to the ordinary in-process
   evaluator. `no-default-features` maps to `lcov-no-default.info`.

Coverage policy never changes collection routing: all-zero, mixed, and
positive-threshold selections all use the same instrumented path. A successful
empty LCOV export is valid and reaches evaluation. LLVM instead exits with its
specific `no coverage data found` / `could not load coverage information`
diagnostic when every discovered object lacks a coverage map; the collector
converts only that complete diagnostic to a stable empty LCOV file and
evaluates it normally. Zero-threshold and `expect-no-coverable-lines` packages
then pass, while positive-threshold packages report `NO DATA`. Missing raw
profiles, failed object discovery, malformed output, and other report failures
remain operational errors.

Plain nextest is used only when a caller explicitly lists the effective target
with repeatable `--no-coverage-target`. If the list is empty, no target is
resolved for this routing decision. Otherwise an explicit `--target` is matched
directly, or the rustc host is resolved with `rustc -vV`. A match applies to all
selected packages and feature configurations, emits explicit no-coverage and
no-gate diagnostics, and creates no LCOV files.

All child stderr is inherited or captured and immediately forwarded when
Windows overflow detection requires inspection. Normal collection stdout is
inherited. `--quiet` redirects or drops each stdout channel while leaving
errors and summary-file rendering intact.

## Diagnostic pipeline

1. The LCOV parser merges reports by source path and line number. A line is
   coverable when it has a distinct `DA:` record. Hit counts determine the
   covered subset, while sorted coverable and uncovered line numbers remain
   available for diagnostics.
2. Attribution maps each source file to the most specific workspace member
   whose manifest directory contains it. Aggregation computes exact package
   counters from the attributed files.
3. Verdict evaluation selects diagnostic locations by status. Numeric failures
   select uncovered lines, unexpected-coverable-lines failures select all
   coverable lines, and passing or no-data outcomes select none. Paths are made
   relative to the package manifest directory when possible, then diagnostics
   are ordered by package, path, and line.
4. The terminal and Markdown renderers share the same detail and range
   formatting. Each renderer emits at most 100 locations per package and
   computes the omitted count from the complete diagnostic set, so truncation
   does not alter aggregate counts or conceal how much output was omitted.

## Invariants

- Coverable line numbers are unique because the parser merges records by line
  number, and ascending because `FileReport` construction explicitly sorts
  them.
- Uncovered lines are a subset of coverable lines.
- Package counters describe the complete attributed input, independent of the
  rendered location limit.
- Both renderers consume the same status-specific diagnostics and preserve
  deterministic ordering.
- A no-data outcome is explanatory rather than location-bearing because there
  are no attributed LCOV records to name.

## Display bound

The 100-location limit keeps a package's failure detail to a few kilobytes
while retaining enough context to show multiple clusters of missed code. The
limit is a presentation bound rather than a coverage-data bound: exact totals
and the omitted count still describe the full report. Reevaluate it when real
failure reports show that useful first clusters are routinely omitted or that
the resulting CI summaries are still too large.
