# cargo-coverage-gate implementation

This guide describes how the crate turns LCOV records into deterministic
failure diagnostics. The user-visible behavior is defined by the
[design](design/README.md).

## Collection pipeline

The optional `run` mode keeps collection and evaluation separated internally:

1. Cargo metadata resolves every selector and package-file entry to a concrete
   workspace member. Exact `name@version` specs are passed to collection tools,
   while bare names are passed to the evaluator.
2. Each feature configuration gets an isolated clean, instrumented
   `cargo llvm-cov nextest --no-report` run. Cargo's JSON messages provide the
   executable object paths; no target-directory scan or diagnostic parsing is
   needed.
3. `llvm-profdata merge -f` consumes an atomically written profile list.
   `llvm-cov export` receives every `-object` pair through an atomically written
   response file on every operating system.
4. LCOV output is first written to a same-directory temporary file and renamed
   to its stable per-configuration name only after a successful export. An
   existing stable file is never removed before collection, so clean, test,
   merge, and export failures preserve the last completed artifact byte for
   byte. Response files, profile lists, merged profiles, and partial LCOV files
   are removed on both success and failure.
5. The completed LCOV files are passed directly to the same in-process
   evaluation path used by the legacy bare command.

An empty package file is represented as an explicit empty selection rather
than as the absence of selection. This distinction lets automation request a
successful no-op without accidentally expanding back to the whole workspace.

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
