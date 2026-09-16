# cargo-coverage-gate implementation

This guide describes how the crate turns LCOV records into deterministic
failure diagnostics. The user-visible behavior is defined by the
[design](design/README.md).

## Collection pipeline

The optional `run` mode keeps collection and evaluation separated internally:

1. Cargo metadata resolves every selector and package-file entry to a concrete
   workspace member. Exact `name@version` specs are passed to collection tools,
   while bare names are passed to the evaluator.
2. The selected toolchain comes from `--toolchain`,
   `COVERAGE_GATE_TOOLCHAIN`, or the active Rustup toolchain, in that order.
   Instrumented runs validate a nightly Cargo release and cargo-llvm-cov
   0.9.0 or newer. Version 0.9.0 is required because it introduced
   `--workspace` support for the `report` subcommand used by the default
   selection. The same Rustup
   selection is applied to metadata,
   target-policy rustc queries, and collection commands.
   The rustup executable comes from a validated absolute `RUSTUP` override or
   a manual search of nonempty `PATH` entries, with Windows `PATHEXT`
   expansion. Every candidate is converted to an absolute path before
   `Command` is created, so Windows cannot inject a project-root
   `rustup.exe` through implicit current-directory lookup. Cargo and rustc
   paths returned by `rustup which` are likewise rejected unless absolute,
   before metadata or collection can resolve them from different directories.
3. Each feature configuration gets an isolated clean, instrumented
   `cargo llvm-cov nextest --no-report --locked` run. Plain-nextest no-gate
   paths also pass `--locked`. Nextest and Cargo retain their ordinary output,
   including rendered compiler diagnostics; the collector does not request or
   filter machine-readable Cargo messages.
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
5. LCOV output remains at an invocation-private path for evaluation. A copy is
   first written to a same-directory temporary file and renamed to its stable
   per-configuration consumer name only after a successful report. An
   existing stable file is never removed before collection, so clean, test,
   report, and publication failures preserve the last completed artifact byte
   for byte. Temporary response and LCOV files are removed on success and
   failure. Unix uses atomic rename replacement. Windows uses `MoveFileExW` to
   add write-through and bounded retries for sharing/access failures to the
   replacement behavior already provided by `std::fs::rename`.
6. The invocation-private LCOV files are passed directly to the same
   in-process evaluation path used by the legacy bare command. Shared stable
   publication paths therefore cannot race an in-flight verdict.

An empty package file is represented as an explicit empty selection rather
than as the absence of selection. This distinction lets automation request a
successful no-op without accidentally expanding back to the whole workspace.

Before tool validation, an empty-data policy probe distinguishes all-zero
threshold selections from packages that need instrumentation. All-zero
selections run plain nextest and stop with an explicit successful no-gate
diagnostic. The same plain path handles `aarch64-pc-windows-msvc`, where
cargo-llvm-cov is unsupported. Mixed selections continue through the
instrumented path without dropping zero-threshold test packages.

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
