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
   variables, with the report-only Windows `MSYSTEM` exception described
   below. One effective target is resolved from explicit `--target` or the
   rustc host. The resolved target is supplied explicitly to collection and
   evaluation, so `CARGO_BUILD_TARGET` and Cargo `build.target` cannot select a
   different collection target. Instrumented runs validate that the effective
   Cargo and rustc are nightly and that cargo-llvm-cov is 0.9.0 or newer. Host
   discovery output is reused for rustc validation. Version 0.9.0 is required
   because it introduced `--workspace` support for the `report` subcommand used
   by the default selection.
3. Each feature configuration gets a fresh private, instrumented
   `cargo llvm-cov nextest --no-report --locked` run. Both instrumented and
   plain-nextest no-gate paths pass `--no-tests=pass`, allowing packages with a
   valid zero-test harness to continue to report generation, and both pass
   `--locked`. Nextest and Cargo retain their ordinary output, including
   rendered compiler diagnostics; the collector does not request or filter
   machine-readable Cargo messages.
   The instrumented target is unique per configuration beneath a
   per-invocation `target/coverage-gate/` scratch directory and is removed by
   an RAII guard on every return path. Because every target starts empty, the
   collector does not run `cargo llvm-cov clean`; it never deletes shared
   cargo-llvm-cov reports, trybuild targets, UI test targets, or ordinary Cargo
   target state. The child also receives `CARGO_TARGET_DIR` pointing at that
   private directory, so metadata-derived trybuild, UI, and report paths do not
   cross configuration or invocation boundaries. Configurations and concurrent
   invocations therefore cannot merge each other's profiles.
   After evaluation, a pure combiner gives evaluation errors precedence over
   cleanup errors, preserves nonzero rendered verdicts with an explicit cleanup
   warning, and converts a passing evaluation plus cleanup failure into an
   operational error. Explicit cleanup disarms the guard after one attempt, so
   `Drop` never retries the same failed deletion.
4. `cargo llvm-cov report --lcov` receives the same package and target
   selection. cargo-llvm-cov therefore owns raw-profile merging, ordinary and
   nested trybuild object discovery, build-directory handling, and its full
   default filename filter. On Windows, the report child has `MSYSTEM` removed
   so any command-line-overflow diagnostic uses deterministic Windows
   argument quoting. On error 206 only, the collector parses that diagnostic,
   validates the `llvm-cov export` shape, and re-quotes the arguments for an
   LLVM response file without invoking a shell.
5. Each raw report is written inside per-invocation scratch. The released
   source surface is then reconstructed from Cargo target roots and Rust
   out-of-line module declarations. Sources reachable only from tests,
   examples, benches, build scripts, or other non-release targets are removed.
   Within released files, syntax ranges are removed only when their attributes
   prove they require `test`; `cfg(any(test, feature = "test-util"))`,
   feature-only gates, and `cfg(not(test))` remain.
6. The filtered LCOV is staged beside its stable per-configuration path under
   `--coverage-dir` and renamed into place only after complete filtering. The
   evaluator reads those exact stable files, so callers can give Codecov and
   Azure DevOps byte-identical released-surface reports.
   `no-default-features` maps to `lcov-no-default.info`.

The Windows response-file retry also writes only to scratch. Successful retry
output and the valid paired no-data conversion pass through the same source
filter before publication. Other retry failures preserve a previously
completed stable artifact.

Instrumented target isolation does not serialize or duplicate stable
`--coverage-dir` artifacts. Concurrent invocations that select the same
coverage directory are unsupported; callers give them distinct directories
when they need simultaneous collection.

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

Plain nextest is used only when a caller explicitly lists the resolved
effective target with repeatable `--no-coverage-target`. A match applies to all
selected packages and feature configurations, passes that target explicitly,
emits no-coverage and no-gate diagnostics, and creates no LCOV files. This
routing decision precedes nightly Cargo, nightly rustc, and cargo-llvm-cov
validation; an omitted target still requires one `rustc -vV` call to discover
the host, while an explicit target needs none.

Child stderr is inherited or captured and forwarded. The recognized paired
no-data diagnostic is consumed and replaced by the collector's empty-report
message on both ordinary and response-file report paths. Normal collection
stdout is inherited. `--quiet` redirects or drops each stdout channel while
leaving errors and summary-file rendering intact.

## Diagnostic pipeline

The CLI and `evaluate_released_surface*` use the complete pipeline below. Raw
`evaluate*` compatibility APIs skip source discovery/filtering and begin at
the filtered-report parsing/attribution stages.

1. Each LCOV input is parsed before source discovery. Malformed LCOV therefore
   fails without touching source state.
2. One Cargo metadata snapshot drives both source ownership and package policy.
   It enumerates every workspace target and classifies released
   libraries (including proc macros and explicit library crate types) and bins
   separately from every other target kind. A module walk follows plain
   `mod name;`, `name/mod.rs`, inline modules, and `#[path = "..."]` edges
   using Rust's distinct crate-root, plain-module, and path-module child
   directories. Conditional `cfg_attr(..., path = "...")` edges fail
   explicitly because they require the complete Cargo build configuration.
   A file is retained when at least one released path reaches it; release
   reachability wins when a test target also shares the file.
3. rust-analyzer syntax identifies test-only item ranges. A `cfg` is test-only
   only when it cannot hold with `test = false`: `all(...)` requires just one
   test-only operand, `any(...)` requires every operand, and `not(...)` is
   retained. Line, branch, and function records inside excluded ranges are
   removed together. Unreadable/unparseable sources, paths with no Cargo target
   owner, stale line numbers, ambiguous modules, and unsupported path
   attributes are operational errors rather than reasons to drop counters.
   Distinct `SF:` spellings that normalize to the same LCOV section key are
   also rejected instead of replacing one section's counters.
4. The LCOV parser merges filtered reports by source path and line number. A line is
   coverable when it has a distinct `DA:` record. Hit counts determine the
   covered subset, while sorted coverable and uncovered line numbers remain
   available for diagnostics.
5. Attribution maps each source file to the most specific workspace member
   whose manifest directory contains it. Aggregation computes exact package
   counters from the attributed files.
6. Verdict evaluation selects diagnostic locations by status. Numeric failures
   select uncovered lines, unexpected-coverable-lines failures select all
   coverable lines, and passing or no-data outcomes select none. Paths are made
   relative to the package manifest directory when possible, then diagnostics
   are ordered by package, path, and line.
7. The terminal and Markdown renderers share the same detail and range
   formatting. Each renderer emits at most 100 locations per package and
   computes the omitted count from the complete diagnostic set, so truncation
   does not alter aggregate counts or conceal how much output was omitted.

## Invariants

- Coverable line numbers are unique because the parser merges records by line
  number, and ascending because `FileReport` construction explicitly sorts
  them.
- Uncovered lines are a subset of coverable lines.
- Every successful input source is reachable from a Cargo target, and every
  retained source is attributable to one workspace member.
- Retained LCOV source paths are rewritten into the owning member's
  cargo-metadata path namespace before package attribution.
- Published `run` artifacts are already filtered; evaluation never gates a
  different in-memory view from the file made available to upload steps.
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
