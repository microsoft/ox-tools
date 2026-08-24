# Changelog

## [Unreleased]

### Fixed

- Advisories that RustSec has withdrawn are no longer counted, so crates cleared by a retracted
  advisory (for example `directories` and `humantime`) are no longer flagged as unmaintained

### Changed

- `MPL-2.0` is now part of the default allowed-license list, matching the file-level copyleft
  licenses commonly accepted alongside permissive ones

## 1.1.0 - 2026-08-07

### Added

- `bug_labels` configuration option (default `["bug", "crash", "defect", "regression"]`) classifying
  issues as bugs. Patterns are case-insensitive regular expressions matched anywhere in the label,
  so `bug` also matches `C-bug` and `type: bug`; anchor a pattern to require an exact match
- A parallel family of `activity.*bug*` metrics counting only issues classified as bugs. These are a
  strict subset of the issue metrics, so existing `activity.*issue*` metrics and any expressions
  written against them are unchanged
- `activity.labeled_issue_ratio`, the percentage of issues carrying at least one label, to
  distinguish a repository with no bugs from one that does not label issues
- A dedicated README section on acknowledging and suppressing findings

### Changed

- The hosting cache stores raw issue records instead of precomputed statistics, so changing
  `bug_labels` no longer requires re-fetching. Existing hosting cache entries are re-fetched once
- Cache locking uses the standard library's file locking, dropping the `fs4` dependency
- Updated dependencies

### Performance

- The `cargo-aprz` binary uses the mimalloc allocator

### Fixed

- Complete the throttler lost-wakeup fix by pinning and enabling the `Notify` future before checking
  the pause flag; registration previously happened on first poll, so a pause lifted in between left
  collection stalled until a later pause happened to wake it
- Honor `Retry-After` headers expressed as an HTTP-date. Only delta-seconds were understood, so a
  date-valued header turned a transient rate limit into an immediate failure or a one-hour wait
- Quote the appraisal status cell in CSV reports; its commas split the row into extra columns
- Query coverage only for forges Codecov hosts. Every repository was looked up under Codecov's
  GitHub namespace, so a same-named GitHub repository could supply coverage for an unrelated crate
- Evaluate `cfg` predicates when counting test lines instead of matching substrings, so
  `#[cfg(not(test))]`, `#[cfg(any(test, feature = "x"))]`, `#[cfg(feature = "test-util")]`, and
  `#[cfg_attr(test, ...)]` items count as production code
- Skip yanked and pre-release versions when resolving a crate's latest version, unless it has no
  other release
- Compute `usage.total_downloads_last_90_days` and `usage.version_downloads_last_90_days` over the
  last 90 days rather than the most recent months present in the data, which reported long-past
  traffic for crates that are no longer downloaded

## 1.0.0 - 2026-04-19

### Changed

- Updated dependencies
- Explain required-check failures with their high-risk label and without displaying
  a misleading zero-out-of-zero score
- Count inconclusive positive-weight checks as unawarded configured points, and
  fail closed without a score when every positive-weight check is inconclusive
- Include policy descriptions in expression output and list blocking crates in risk-check errors
- Neutralize formula-like crate headers, metric names, and textual metric values
  in CSV reports; this prefixes an apostrophe for any text whose first
  non-whitespace character is `=`, `+`, `-`, or `@`, including benign
  dash-prefixed descriptions
- Flat report reasons now include descriptions for failed or inconclusive checks; passing-check output remains name-only
- JSON appraisal output adds structured risk, nullable score/point, required-check,
  and expression-outcome fields while preserving legacy `reasons`, including
  evaluation-failure diagnostics
- Risk-check errors include full required-check diagnostics when console output is suppressed and distinguish inconclusive evaluations from policy failures
- Rejection details label outcomes as `FAILED` or `INCONCLUSIVE` and identify
  policy descriptions as expected or unevaluated conditions

### Performance

- Replace quadruple `Vec` clone of queryable specs with `Arc<[CrateSpec]>` sharing
- Single-pass accumulation for time-windowed issue/PR age statistics
- Single-pass ordered dedup of crate refs preserving insertion order
- Switch cache serialization from JSON to MessagePack
- Single-pass byte scan for CSV escaping
- Pre-compute shared `ReportContext` across report format generators
- Eliminate `Vec`+`join` allocation in progress callback
- Accept `&RepoSpec` to avoid cloning on retry
- Eliminate double-parse of rustdoc JSON: deserialize directly into typed struct
  instead of going through an intermediate `serde_json::Value` tree, halving peak
  memory per crate during docs analysis
- Run docs provider sequentially after other providers to isolate its memory-intensive
  rustdoc JSON parsing from the codebase provider's source analysis

### Fixed

- Fix throttler lost-wakeup race by registering `Notify` before checking pause state
- Fix throttler pause leak where work could slip through during semaphore acquisition
- Ensure throttler `paused` flag and `resume_at` are updated atomically under the mutex
- Fix request tracker counter visibility by upgrading atomic orderings from `Relaxed`
  to `Acquire`/`Release`

## 0.14.0 - 2026-03-06

### Fixed

- Fix handling of features when walking transitive dependencies.

## 0.13.0 - 2026-03-01

### Added

- Added support for an allow list in configuration to bypass the --error-if-high-risk and
  --error-if-medium-risk checks.

### Changed

- More improvements in HTML report rendering.

## 0.12.0 - 2026-02-28

### Changed

- Improved HTML report, much more user-friendly and visually appealing.

- Improved default config.

### Fixed

- Handle crates with - in their names properly, things were getting
  messed up since in certain cases the - is normalized to a _.

## 0.11.0 - 2026-02-24

### Fixed

- More work on the rate limits. Getting better...

## 0.10.0 - 2026-02-23

### Fixed

- Fixed rate-limit backoff in the hosting provider, it was getting confused about what time it was.

- Improved logging in the hosting provider to show how many individual calls are being made to the hosting
  provider for each repo.

## 0.9.0 - 2026-02-21

### Added

- Better rendering of the progress bar when we're rate limited

### Fixed

- Fixed hosting provider's retry loop to correctly handle rate limit errors and avoid infinite retries on non-rate-limit failures.
- Respect --color "never" in the progress bar rendering

## 0.8.0 - 2026-02-18

### Added

- New `Throttler` type for concurrency control, limiting each provider to a fixed number of
  outstanding requests and supporting temporary pause-and-resume on rate limit backpressure.

### Changed

- All providers (coverage, docs, hosting, codebase, advisories) now use the `Throttler` for
  concurrency limiting instead of ad-hoc approaches.
- Hosting provider: revamped scheduling with per-repo retry loop and automatic rate-limit
  pause/resume across all concurrent tasks.
- Hosting provider: consolidated issue/PR statistics computation into a single-pass design,
  eliminating intermediate `Vec` allocations and redundant iterations.
- Hosting provider: unified duplicated age-stats logic (`compute_age`, `compute_merged_pr_age`)
  into a single `compute_age_stats` function.
- Hosting provider: corrected `ISSUE_PAGE_SIZE` from 255 to 100 to match GitHub API maximum.
- Codebase provider: consolidated 6 sequential git calls into a single `get_commit_stats` invocation
  that computes all commit statistics in one pass.
- Codebase provider: switched contributor counting from `git log --format=%ae` with client-side
  deduplication to `git shortlog -sne --all`.
- Docs provider: inlined `download_zst` into `fetch_docs_for_crate_core`, eliminating the
  `DownloadError` enum and reducing `Provider` clones.
- Docs provider: deferred backtick-string allocation in broken-link detection using short-circuit evaluation.
- Improved logging consistency across all providers: use `{:#}` for error formatting, use host
  display names (GitHub/Codeberg) instead of generic "hosting API" in messages, and consolidated
  redundant debug log calls.

### Fixed

- Hosting provider: closed issues with missing `closed_at` no longer count as "0 day" age,
  fixing downward skew in age statistics.
- Coverage provider: distinguished 4xx (permanent, cached as unavailable) from 5xx (transient,
  returned as error) HTTP responses.
- Coverage provider: tightened SVG "unknown" detection from `contains("unknown")` to
  `contains(">unknown<")` to reduce false positives.
- Removed duplicate warn-level logging for unsupported hosts in the hosting provider.
- Removed double error logging for sync failures in the codebase provider.

## 0.7.0 - 2026-02-16

### Fixed

- Fixed codebase data cache not surviving process interruption. Previously, killing the tool during data 
  collection caused all codebase analysis to be re-downloaded on restart. Cache files are now written
  incrementally as each repository completes.
- Fixed repository URL path stripping so that URLs containing paths like `/tree/master/subdir` are
  correctly normalized to the base repository URL for hosting and codebase queries.

### Changed

- Added support for negative caching to avoid repeated 
- requests for permanently unavailable data (e.g., missing docs, unsupported hosts, parse errors).

## 0.6.1 - 2026-02-16

### Fixed

- Fixed `--version` displaying incorrect program name.

## 0.6.0 - 2026-02-13

### Added

- Added the `--ignore-cached` option to force re-downloading crate data even if it's already cached locally.
- Added the `--error-if-medium-risk` option to exit with status code 1 if any crate is appraised as medium or high risk.
- Expanded the `--console` option to provide control over what gets output to the console.

### Changed

- Renamed the `--check` option to `--error-if-high-risk`.
- Improve crate table download perf.

## 0.5.0 - 2026-02-12

### Added

- Make all network communication resilient with retries and timeouts

### Changed

- Substantially improved perf of "identification" phase
- Improved error messages

### Fixed

- Fixed point calculation around failed expressions
- Failed expressions now produce cleaner output in the reports

## 0.4.0 2026-02-11

### Added

- New metrics:
  - `stability.versions_last_180_days`
  - `stability.versions_last_365_days`
  - `activity.commits_last_180_days`
  - `activity.first_commit_at`
  - `activity.open_issues`
  - `activity.open_issue_age_avg`
  - `activity.open_issue_age_p50`
  - `activity.open_issue_age_p75`
  - `activity.open_issue_age_p90`
  - `activity.open_issue_age_p95`
  - `activity.issues_opened_last_90_days`
  - `activity.issues_opened_last_180_days`
  - `activity.issues_opened_last_365_days`
  - `activity.issues_opened_total`
  - `activity.issues_closed_last_90_days`
  - `activity.issues_closed_last_180_days`
  - `activity.issues_closed_last_365_days`
  - `activity.issues_closed_total`
  - `activity.closed_issue_age_avg`
  - `activity.closed_issue_age_p50`
  - `activity.closed_issue_age_p75`
  - `activity.closed_issue_age_p90`
  - `activity.closed_issue_age_p95`
  - `activity.closed_issue_age_last_90_days_avg`
  - `activity.closed_issue_age_last_90_days_p50`
  - `activity.closed_issue_age_last_90_days_p75`
  - `activity.closed_issue_age_last_90_days_p90`
  - `activity.closed_issue_age_last_90_days_p95`
  - `activity.closed_issue_age_last_180_days_avg`
  - `activity.closed_issue_age_last_180_days_p50`
  - `activity.closed_issue_age_last_180_days_p75`
  - `activity.closed_issue_age_last_180_days_p90`
  - `activity.closed_issue_age_last_180_days_p95`
  - `activity.closed_issue_age_last_365_days_avg`
  - `activity.closed_issue_age_last_365_days_p50`
  - `activity.closed_issue_age_last_365_days_p75`
  - `activity.closed_issue_age_last_365_days_p90`
  - `activity.closed_issue_age_last_365_days_p95`
  - `activity.open_prs`
  - `activity.open_pr_age_avg`
  - `activity.open_pr_age_p50`
  - `activity.open_pr_age_p75`
  - `activity.open_pr_age_p90`
  - `activity.open_pr_age_p95`
  - `activity.prs_opened_last_90_days`
  - `activity.prs_opened_last_180_days`
  - `activity.prs_opened_last_365_days`
  - `activity.prs_opened_total`
  - `activity.prs_merged_last_90_days`
  - `activity.prs_merged_last_180_days`
  - `activity.prs_merged_last_365_days`
  - `activity.prs_merged_total`
  - `activity.prs_closed_last_90_days`
  - `activity.prs_closed_last_180_days`
  - `activity.prs_closed_last_365_days`
  - `activity.prs_closed_total`
  - `activity.merged_pr_age_avg`
  - `activity.merged_pr_age_p50`
  - `activity.merged_pr_age_p75`
  - `activity.merged_pr_age_p90`
  - `activity.merged_pr_age_p95`
  - `activity.merged_pr_age_last_90_days_avg`
  - `activity.merged_pr_age_last_90_days_p50`
  - `activity.merged_pr_age_last_90_days_p75`
  - `activity.merged_pr_age_last_90_days_p90`
  - `activity.merged_pr_age_last_90_days_p95`
  - `activity.merged_pr_age_last_180_days_avg`
  - `activity.merged_pr_age_last_180_days_p50`
  - `activity.merged_pr_age_last_180_days_p75`
  - `activity.merged_pr_age_last_180_days_p90`
  - `activity.merged_pr_age_last_180_days_p95`
  - `activity.merged_pr_age_last_365_days_avg`
  - `activity.merged_pr_age_last_365_days_p50`
  - `activity.merged_pr_age_last_365_days_p75`
  - `activity.merged_pr_age_last_365_days_p90`
  - `activity.merged_pr_age_last_365_days_p95`

- Expression evaluation errors are now reported as such instead of failing overall appraisal.

### Removed

- Removed metrics (replaced by new naming convention above):
  - `activity.closed_issues`
  - `activity.closed_pull_requests`
  - `activity.open_pull_requests`
  - `activity.avg_open_issue_age_days`
  - `activity.median_open_issue_age_days`
  - `activity.p90_open_issue_age_days`
  - `activity.avg_open_pull_request_age_days`
  - `activity.median_open_pull_request_age_days`
  - `activity.p90_open_pull_request_age_days`

## 0.3.0 - 2026-02-10

### Changed

- Replaced binary accept/deny evaluation model with three-level risk assessment (`Low`, `Medium`, `High`).
- Renamed the `deny_if_any` expressions in config to `high_risk_if_any`.
- Replaced the `accept_if_all' and `accept_if_any` expressions in config with `eval'

## 0.2.0 - 2026-02-09

### Added

- New metrics: `activity.commits_last_365_days`, `activity.commit_count`, and `activity.last_commit_at`.
- Commit data (`commits_last_90_days`, `commits_last_365_days`, `commit_count`, `last_commit_at`) is now collected from the local git repository instead of the hosting API, reducing API usage.

### Fixed

- Clamp negative timestamps and epoch days to 0 before unsigned conversion, preventing wraparound for pre-epoch dates.
- Fixed `deps` command not handling multiple instances of the same crate with different versions.
- CSV escaping now handles all edge cases correctly.
- Fixed `--dependency-types` to only apply to the first level of dependencies.
- Fixed inconsistencies between CWD and standard Cargo path semantics.
- Fixed encoding issues in generated reports.
- Fixed evaluation to pass when no expressions are defined, matching documented behavior.

### Changed

- Eliminated `--eval` command-line option; expressions in configuration are now always evaluated.

## 0.1.0 2026-02-08

- Initial release.
