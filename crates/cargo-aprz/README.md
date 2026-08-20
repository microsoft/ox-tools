<div align="center">
 <img src="./logo.png" alt="Cargo-Aprz Logo" width="96">

# Cargo-Aprz

[![crates.io](https://img.shields.io/crates/v/cargo-aprz.svg)](https://crates.io/crates/cargo-aprz)
[![docs.rs](https://docs.rs/cargo-aprz/badge.svg)](https://docs.rs/cargo-aprz)
[![MSRV](https://img.shields.io/crates/msrv/cargo-aprz)](https://crates.io/crates/cargo-aprz)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

A cargo tool to appraise the quality of Rust dependencies.

* [Background](#background)
* [Installation](#installation)
* [Quick Start](#quick-start)
* [Data Sources](#data-sources)
* [Crates and Dependencies](#crates-and-dependencies)
  * [Dependency Types](#dependency-types)
  * [Package & Feature Selection](#package--feature-selection)
  * [Tokens](#tokens)
* [Reports](#reports)
* [Configuration and Expressions](#configuration-and-expressions)
  * [Expression Checks in CI](#expression-checks-in-ci)
* [Troubleshooting](#troubleshooting)
* [Collected Metrics](#collected-metrics)
  * [Metadata Metrics](#metadata-metrics)
  * [Usage Metrics](#usage-metrics)
  * [Stability Metrics](#stability-metrics)
  * [Community Metrics](#community-metrics)
  * [Activity Metrics](#activity-metrics)
  * [Documentation Metrics](#documentation-metrics)
  * [Advisory Metrics](#advisory-metrics)
  * [Code Metrics](#code-metrics)
  * [Trustworthiness Metrics](#trustworthiness-metrics)

### Background

Building modern applications usually involves integrating a large number of third-party dependencies.
While these dependencies can provide valuable functionality and accelerate development, they also
introduce risks related to quality, security vulnerabilities, future compatibility, and more.

Before taking a dependency in your project, it’s useful to vet whether that dependency meets some baseline
quality standards. For example, maybe you believe in having excellent unit test coverage for your projects,
but if you pull in some dependency which has no tests, it can undermine the overall quality of your application.

`cargo-aprz` lets you appraise the quality of dependencies. For any given crate, it collects a large number
of metrics, such as the number of open issues, the frequency of releases, the existence of security advisories,
the number of examples, the code coverage percentage, and many more. You can view nice reports showing you
all of these metrics in an easy to consume form.

You can also use `cargo-aprz` to automatically evaluate whether a crate meets your quality standards. You
do this by writing a set of expressions that operate on the collected metrics. For example, you can have an
expression that says “if code coverage is below 20 percent, treat this crate as not being acceptable as a dependency”.

You can run `cargo-aprz` by specifying a set of crates to evaluate, or you can run it on the full transitive set of
dependencies of an existing Rust project.

### Installation

```bash
cargo install --locked cargo-aprz
```

### Quick Start

1. Generate a default configuration file:
   
   ```bash
   cargo aprz init
   ```
   
   This creates `aprz.toml` which lets you control various options. This is where you define expressions that let you
   evaluate the relative quality of a crate by inspecting its metrics.

1. Get the metrics associated with the latest version of a crate:
   
   ```bash
   cargo aprz crates tokio
   ```
   
   The first time you run this command, it will take a while as it needs to download
   a large database from crates.io along with the `RustSec` advisory database. This
   data is cached such that subsequent runs will be much faster.

1. Get the metrics associated with the dependencies of a Rust project:
   
   ```bash
   cargo aprz deps
   ```

1. Get the metrics for specific versions of crates:
   
   ```bash
   cargo aprz crates tokio@1.40.0 serde@1.0.0
   ```

1. Get the metrics for a crate and produce an HTML report instead of outputting to the console:
   
   ```bash
   cargo aprz crates tokio@1.40.0 --html report.html
   ```

1. If a crate gets flagged and you want to acknowledge the finding so it stops failing your build,
   see [Acknowledging and Suppressing Findings](#acknowledging-and-suppressing-findings).

### Data Sources

`cargo-aprz` collects data from these sources:

* **crates.io**: Provides metadata and download statistics for each crate.

* **GitHub** or **Codeberg**: Provide information about the popularity of a crate, the
  number of issues and pull requests, the frequency of commits, and more. This is also
  where `cargo-aprz` gets source code in order to analyze the code quality of a crate.

* **`RustSec` Advisory Database**: Provides information about known vulnerabilities in Rust crates.

* **docs.rs**: Provides information about the quality of documentation for a crate, such as the presence of examples,
  the number of items with documentation comments, and more.

* **codecov.io**: Provides code coverage information.

### Crates and Dependencies

`cargo-aprz` can be used to appraise the quality of specific crates, or the quality of the dependencies of an existing Rust project.

When you run `cargo-aprz crates`, you specify a set of crates to appraise with or without a version number. For example:

```bash
cargo aprz crates tokio serde@1.0.1
```

When you run `cargo-aprz deps`, it will appraise the quality of the dependencies of the Rust project in the current directory.

```bash
cargo aprz deps --dependency-types standard
```

#### Dependency Types

The `--dependency-types` option accepts a comma-separated list of dependency types to include in the appraisal. Possible values are:

* `standard`: Only include the standard dependencies of the project.
* `dev`: Only include the development dependencies of the project.
* `build`: Only include the build dependencies of the project.

#### Package & Feature Selection

When using the `deps` command, you can use the usual cargo options to control precisely which package and feature to consider. The available options include:

* `--manifest-path <PATH>`: Path to the `Cargo.toml` file of the project to analyze. By default, it looks for `Cargo.toml` in the current directory.
* `--features`: A comma-separated list of features to activate.
* `--no-default-features`: Do not activate the `default` feature.
* `--all-features`: Activate all available features.
* `--package`: Appraise the dependencies of a specific package in a workspace.
* `--workspace`: Appraise the dependencies of all packages in a workspace.

#### Tokens

`cargo-aprz` accesses the GitHub or Codeberg API to collect data about a crate. Although these APIs can be used without any form of authentication, this
results in very low rate limits. If `cargo-aprz` detects it is being throttled by the API, it will enter a retry loop where it will wait until it is safe
to try the operation again.

When using the `deps` command on a large project, it’s likely you’ll hit these rate limits, which can make the process take hours to complete fully.
In such a case, you can provide a GitHub or Codeberg token on the command-line or through environment variables, which gives you substantially higher
rate limits.

```bash
cargo aprz deps --github-token <GITHUB_TOKEN> --codeberg-token <CODEBERG_TOKEN>
```

You can also set the `GITHUB_TOKEN` and `CODEBERG_TOKEN` environment variables, which `cargo-aprz` will automatically pick up.

### Reports

When you run `cargo-aprz`, it collects the many metrics listed below and then proceeds to generate a report
that shows all the collected metrics. The report can be in a variety of formats including HTML and JSON.
By default, the report is simply printed to the console.

```bash
cargo aprz crates tokio                     # Terminal output (default)
cargo aprz crates tokio --console           # Terminal output (explicit)
cargo aprz crates tokio --html report.html  # HTML report
cargo aprz crates tokio --json report.json  # JSON data
cargo aprz crates tokio --csv report.csv    # CSV file
cargo aprz crates tokio --excel report.xlsx # Excel spreadsheet
```

### Configuration and Expressions

You can configure `cargo-aprz` by creating an `aprz.toml` file in the current directory. This file lets you define the set of expressions that the tool uses in order
to assess whether a crate is acceptable or not acceptable to use as a dependency. The `--config` option lets you specify an arbitrary path to the configuration file
instead of the default.

`cargo-aprz` uses the [CEL expression language][__link0]. This is a flexible, general-purpose expression
language that allows you to write potentially complex boolean expressions that operate on the value of collected metrics. Expressions are divided into two buckets:

* `high_risk`: All expressions must evaluate to `true`. If any evaluates to `false`, the crate is flagged as high risk.

* eval: Each expression has a point value (default 1). All expressions are evaluated and a score is
  computed as `granted_points / configured_points * 100`. A check that cannot be evaluated remains
  in the configured-points denominator but earns no points. The score is compared against configurable
  thresholds (`medium_risk_threshold` and `low_risk_threshold`) to determine whether the crate is
  low, medium, or high risk.

These buckets are evaluated in order. If no expressions are defined, then all crates are considered low risk.
When a `high_risk` check fails, weighted expressions are skipped and the report says that the
score was not calculated rather than displaying a misleading zero-out-of-zero score. Expression
output includes descriptions for failed or inconclusive checks to explain the policy requirement;
passing checks remain name-only.
If every positive-weight `eval` check is inconclusive, evaluation also fails closed as high risk
without a score. Empty weighted policies and policies containing only zero-weight checks retain
the neutral low-risk score of 100.

Within these expressions, you can refer to any of the collected metrics. For example, you could write an expression that says
“the crate must have 100 or fewer open issues to avoid being flagged as high risk”:

```toml
[[high_risk]]
name = "Open Issues"
description = "Crate must not have too many open issues."
expression = "activity.open_issues <= 100"
```

Any of the metric listed in [Collected Metrics](#collected-metrics) below can be used in these expressions, which gives you a lot of flexibility in
defining what you consider to be an acceptable or unacceptable crate.

You can also use `duration()` in expressions for time-based comparisons. You can assign higher point values to more
important expressions using the `points` field:

```toml
[[eval]]
name = "Established Crate"
description = "Accepts if the crate version was created more than 6 months ago."
expression = "stability.version_created_at < (stability.version_updated_at - duration('4320h'))"  # 4320 hours = 180 days
points = 5
```

By default, crates scoring below 30 are high risk, between 30 and 70 are medium risk, and 70 or above are low risk.
You can customize these thresholds:

```toml
medium_risk_threshold = 30.0
low_risk_threshold = 70.0
```

#### Bugs vs. Issues

GitHub issues are used both for genuine defects and for ideas, questions, and discussion. A crate with
200 open feature requests is in a very different position from one with 200 open bugs, so `cargo-aprz`
tracks the two separately.

An issue is classified as a bug when one of its labels matches a configured regular expression:

```toml
bug_labels = ["bug", "crash", "defect", "regression"]
```

Matching uses case-insensitive regular expressions that match anywhere within the label, so the
default `bug` pattern also matches the conventional prefixed labels used by many Rust projects,
such as `C-bug`, `type: bug`, and `kind/bug`. Anchor a pattern with `^` and `$` when you want an
exact match instead:

```toml
bug_labels = ["^(c|kind)[-/](bug|defect)$", "^regression$"]
```

An invalid regular expression is reported when the configuration is loaded.

**Bug metrics are a strict subset of issue metrics.** Every bug is also counted as an issue, so
`activity.open_issues` remains a complete count of open issues and `activity.open_bugs` counts only
the bug-labeled ones. Nothing changes about the existing issue metrics. If you want the count of
non-bug issues, subtract:

```toml
[[high_risk]]
name = "Open Non-Bug Issues"
description = "Crate must not have too many open non-bug issues."
expression = "activity.open_issues - activity.open_bugs <= 100"
```

**Unlabeled issues are never counted as bugs.** This means a repository that does not label its
issues reports zero bugs, which would make a bug-based check pass trivially. Use
`activity.labeled_issue_ratio` — the percentage of issues carrying at least one label — to detect
that case and avoid being misled:

```toml
[[eval]]
name = "Few Open Bugs"
description = "Accepts if the crate has few open bugs, or if the repository does not label issues."
expression = "activity.labeled_issue_ratio < 25 || activity.open_bugs <= 25"
```

Setting `bug_labels = []` disables bug classification entirely, leaving all bug metrics at zero.

Because bug classification happens when metrics are computed rather than when data is fetched,
changing `bug_labels` takes effect immediately without re-fetching anything from the network.

#### Expression Checks in CI

If you want to use `cargo-aprz` in a CI pipeline to detect if any unsavory dependencies are being added to your project, you
can use the `--error-if-high-risk` option to make `cargo-aprz` return a non-zero exit code if any of the crates being appraised are
flagged as high risk based on the configured expressions. Similarly, `--error-if-medium-risk` returns a non-zero exit code
if any crate is flagged as medium or high risk.

To stop a specific finding from failing your build, see
[Acknowledging and Suppressing Findings](#acknowledging-and-suppressing-findings) below.

When a risk flag rejects the command, the final error includes an item-limited set of blocking
crates and failed or inconclusive checks unless the console already rendered complete appraisal
reasons. Up to 20 crates and 10 non-passing checks per crate are shown; when anything is omitted,
use `--console appraisal,reasons` or `--json <path>` for the complete appraisal.

### Acknowledging and Suppressing Findings

When `cargo-aprz` flags a crate and you need to unblock your build while the risk is evaluated, you
have three options. They are listed in order of preference:

#### 1. Remediate, upgrade, or replace

Fixing the underlying problem is always the best outcome. Upgrading to a newer version of the crate
often resolves findings on its own, since many metrics (advisory status, release recency, maintenance
activity) improve with newer releases. Where that is not possible, replacing the dependency may be
warranted.

#### 2. Acknowledge a specific crate with the allow list

If you have reviewed a finding and accepted the risk, add the crate to the allow list in your
`aprz.toml`. Each entry specifies a crate name and a semver version requirement:

```toml
[[allow_list]]
name = "some-crate"
version = "=1.2.3"     # exact version: the exemption lapses on the next upgrade

[[allow_list]]
name = "another-crate"
version = "^2.0"
```

Version requirements use standard semver syntax such as `"*"` (any version), `"=1.2.3"` (exact),
`"^1.2"` (compatible), `"~1.2"` (patch-level), or `">=1.0, <2.0"` (range).

**What the allow list does and does not do.** An allow-list entry only suppresses the non-zero exit
code from `--error-if-high-risk` and `--error-if-medium-risk`. The crate is still appraised, still
assigned a risk level, and still appears in console, HTML, CSV, Excel, and JSON reports exactly as
before. Nothing is hidden; the finding simply stops failing your build.

Prefer an exact version requirement (`"=1.2.3"`) over a wildcard. The exemption then applies only to
the version you actually reviewed, and a future upgrade re-raises the finding for a fresh look. A
wildcard silently carries your acknowledgment forward to versions you have never seen.

Because `aprz.toml` is a normal TOML file, record why an exemption exists so the next reader can
re-evaluate it:

```toml
# Reviewed 2026-01-15: unmaintained upstream, but the affected code path is not reachable
# from our usage. Re-check when 2.0 ships.
[[allow_list]]
name = "some-crate"
version = "=1.2.3"
```

#### 3. Change or remove the check itself

The allow list exempts one crate at a time. If a check is producing findings you never intend to act
on across your whole dependency graph, the check itself is the problem. Edit its `expression` to a
threshold you will enforce, or delete the `[[high_risk]]` or `[[eval]]` block entirely.

Removing an `[[eval]]` block also removes its points from the score denominator, so scores rise for
every crate. Removing a `[[high_risk]]` block stops that condition from forcing a high-risk
classification. Prefer adjusting a threshold over deleting a check outright, so you keep the signal
while lowering the bar to something you will actually act on.

### Troubleshooting

The `crates` and `deps` commands both let you specify a logging level using the `--log-level` option. Turning on logging can be useful
to troubleshooting connectivity problems. When logging is enabled, then normal console output is suspended.

### Collected Metrics

The sections below show the full set of metrics collected.

#### Metadata Metrics

|Metric|Description|
|------|-----------|
|`crate.name`|Name of the crate|
|`crate.version`|Semantic version of the crate|
|`crate.description`|Description of the crate’s purpose and use|
|`crate.license`|SPDX license identifier constraining use of the crate|
|`crate.categories`|Crate categories|
|`crate.keywords`|Crate keywords|
|`crate.features`|Available crate features|
|`crate.repository`|URL to the crate’s source code repository|
|`crate.homepage`|URL to the crate’s homepage|
|`crate.minimum_rust`|Minimum Rust version (MSRV) required to compile this crate|
|`crate.rust_edition`|Rust edition this crate targets|
|`crate.owners`|List of owner usernames|

#### Usage Metrics

|Metric|Description|
|------|-----------|
|`usage.total_downloads`|Crate downloads across all versions|
|`usage.total_downloads_last_90_days`|Crate downloads across all versions in the last 90 days|
|`usage.version_downloads`|Crate downloads of this specific version|
|`usage.version_downloads_last_90_days`|Crate downloads of this specific version in the last 90 days|
|`usage.dependent_crates`|Number of unique crates that depend on this crate|

#### Stability Metrics

|Metric|Description|
|------|-----------|
|`stability.crate_created_at`|When the crate was first published to crates.io|
|`stability.crate_updated_at`|When the crate’s metadata was last updated on crates.io|
|`stability.version_created_at`|When this version was first published to crates.io|
|`stability.version_updated_at`|When this version’s metadata was last updated on crates.io|
|`stability.yanked`|Whether this version has been yanked from crates.io|
|`stability.versions_last_90_days`|Number of versions published in the last 90 days|
|`stability.versions_last_180_days`|Number of versions published in the last 180 days|
|`stability.versions_last_365_days`|Number of versions published in the last 365 days|

#### Community Metrics

|Metric|Description|
|------|-----------|
|`community.repo_stars`|Number of stars on the repository|
|`community.repo_forks`|Number of forks of the repository|
|`community.repo_subscribers`|Number of users watching/subscribing to the repository|
|`community.repo_contributors`|Number of contributors to the repository|

#### Activity Metrics

|Metric|Description|
|------|-----------|
|`activity.commits_last_90_days`|Number of commits to the repository in the last 90 days|
|`activity.commits_last_180_days`|Number of commits to the repository in the last 180 days|
|`activity.commits_last_365_days`|Number of commits to the repository in the last 365 days|
|`activity.commit_count`|Total number of commits in the repository|
|`activity.first_commit_at`|Timestamp of the first commit in the repository|
|`activity.last_commit_at`|Timestamp of the most recent commit in the repository|
|`activity.open_issues`|Number of currently open issues|
|`activity.open_issue_age_avg`|Average age in days of open issues|
|`activity.open_issue_age_p50`|Median age in days of open issues|
|`activity.open_issue_age_p75`|75th percentile age in days of open issues|
|`activity.open_issue_age_p90`|90th percentile age in days of open issues|
|`activity.open_issue_age_p95`|95th percentile age in days of open issues|
|`activity.issues_opened_last_90_days`|Number of issues opened in the last 90 days|
|`activity.issues_opened_last_180_days`|Number of issues opened in the last 180 days|
|`activity.issues_opened_last_365_days`|Number of issues opened in the last 365 days|
|`activity.issues_opened_total`|Total number of issues opened (all time)|
|`activity.issues_closed_last_90_days`|Number of issues closed in the last 90 days|
|`activity.issues_closed_last_180_days`|Number of issues closed in the last 180 days|
|`activity.issues_closed_last_365_days`|Number of issues closed in the last 365 days|
|`activity.issues_closed_total`|Total number of issues closed (all time)|
|`activity.closed_issue_age_avg`|Average age in days of closed issues|
|`activity.closed_issue_age_p50`|Median age in days of closed issues|
|`activity.closed_issue_age_p75`|75th percentile age in days of closed issues|
|`activity.closed_issue_age_p90`|90th percentile age in days of closed issues|
|`activity.closed_issue_age_p95`|95th percentile age in days of closed issues|
|`activity.closed_issue_age_last_90_days_avg`|Average age in days of issues closed in the last 90 days|
|`activity.closed_issue_age_last_90_days_p50`|Median age in days of issues closed in the last 90 days|
|`activity.closed_issue_age_last_90_days_p75`|75th percentile age in days of issues closed in the last 90 days|
|`activity.closed_issue_age_last_90_days_p90`|90th percentile age in days of issues closed in the last 90 days|
|`activity.closed_issue_age_last_90_days_p95`|95th percentile age in days of issues closed in the last 90 days|
|`activity.closed_issue_age_last_180_days_avg`|Average age in days of issues closed in the last 180 days|
|`activity.closed_issue_age_last_180_days_p50`|Median age in days of issues closed in the last 180 days|
|`activity.closed_issue_age_last_180_days_p75`|75th percentile age in days of issues closed in the last 180 days|
|`activity.closed_issue_age_last_180_days_p90`|90th percentile age in days of issues closed in the last 180 days|
|`activity.closed_issue_age_last_180_days_p95`|95th percentile age in days of issues closed in the last 180 days|
|`activity.closed_issue_age_last_365_days_avg`|Average age in days of issues closed in the last 365 days|
|`activity.closed_issue_age_last_365_days_p50`|Median age in days of issues closed in the last 365 days|
|`activity.closed_issue_age_last_365_days_p75`|75th percentile age in days of issues closed in the last 365 days|
|`activity.closed_issue_age_last_365_days_p90`|90th percentile age in days of issues closed in the last 365 days|
|`activity.closed_issue_age_last_365_days_p95`|95th percentile age in days of issues closed in the last 365 days|
|`activity.open_bugs`|Number of currently open bugs|
|`activity.open_bug_age_avg`|Average age in days of open bugs|
|`activity.open_bug_age_p50`|Median age in days of open bugs|
|`activity.open_bug_age_p75`|75th percentile age in days of open bugs|
|`activity.open_bug_age_p90`|90th percentile age in days of open bugs|
|`activity.open_bug_age_p95`|95th percentile age in days of open bugs|
|`activity.bugs_opened_last_90_days`|Number of bugs opened in the last 90 days|
|`activity.bugs_opened_last_180_days`|Number of bugs opened in the last 180 days|
|`activity.bugs_opened_last_365_days`|Number of bugs opened in the last 365 days|
|`activity.bugs_opened_total`|Total number of bugs opened (all time)|
|`activity.bugs_closed_last_90_days`|Number of bugs closed in the last 90 days|
|`activity.bugs_closed_last_180_days`|Number of bugs closed in the last 180 days|
|`activity.bugs_closed_last_365_days`|Number of bugs closed in the last 365 days|
|`activity.bugs_closed_total`|Total number of bugs closed (all time)|
|`activity.closed_bug_age_avg`|Average age in days of closed bugs|
|`activity.closed_bug_age_p50`|Median age in days of closed bugs|
|`activity.closed_bug_age_p75`|75th percentile age in days of closed bugs|
|`activity.closed_bug_age_p90`|90th percentile age in days of closed bugs|
|`activity.closed_bug_age_p95`|95th percentile age in days of closed bugs|
|`activity.closed_bug_age_last_90_days_avg`|Average age in days of bugs closed in the last 90 days|
|`activity.closed_bug_age_last_90_days_p50`|Median age in days of bugs closed in the last 90 days|
|`activity.closed_bug_age_last_90_days_p75`|75th percentile age in days of bugs closed in the last 90 days|
|`activity.closed_bug_age_last_90_days_p90`|90th percentile age in days of bugs closed in the last 90 days|
|`activity.closed_bug_age_last_90_days_p95`|95th percentile age in days of bugs closed in the last 90 days|
|`activity.closed_bug_age_last_180_days_avg`|Average age in days of bugs closed in the last 180 days|
|`activity.closed_bug_age_last_180_days_p50`|Median age in days of bugs closed in the last 180 days|
|`activity.closed_bug_age_last_180_days_p75`|75th percentile age in days of bugs closed in the last 180 days|
|`activity.closed_bug_age_last_180_days_p90`|90th percentile age in days of bugs closed in the last 180 days|
|`activity.closed_bug_age_last_180_days_p95`|95th percentile age in days of bugs closed in the last 180 days|
|`activity.closed_bug_age_last_365_days_avg`|Average age in days of bugs closed in the last 365 days|
|`activity.closed_bug_age_last_365_days_p50`|Median age in days of bugs closed in the last 365 days|
|`activity.closed_bug_age_last_365_days_p75`|75th percentile age in days of bugs closed in the last 365 days|
|`activity.closed_bug_age_last_365_days_p90`|90th percentile age in days of bugs closed in the last 365 days|
|`activity.closed_bug_age_last_365_days_p95`|95th percentile age in days of bugs closed in the last 365 days|
|`activity.labeled_issue_ratio`|Percentage of issues carrying at least one label|
|`activity.open_prs`|Number of currently open pull requests|
|`activity.open_pr_age_avg`|Average age in days of open pull requests|
|`activity.open_pr_age_p50`|Median age in days of open pull requests|
|`activity.open_pr_age_p75`|75th percentile age in days of open pull requests|
|`activity.open_pr_age_p90`|90th percentile age in days of open pull requests|
|`activity.open_pr_age_p95`|95th percentile age in days of open pull requests|
|`activity.prs_opened_last_90_days`|Number of pull requests opened in the last 90 days|
|`activity.prs_opened_last_180_days`|Number of pull requests opened in the last 180 days|
|`activity.prs_opened_last_365_days`|Number of pull requests opened in the last 365 days|
|`activity.prs_opened_total`|Total number of pull requests opened (all time)|
|`activity.prs_merged_last_90_days`|Number of pull requests merged in the last 90 days|
|`activity.prs_merged_last_180_days`|Number of pull requests merged in the last 180 days|
|`activity.prs_merged_last_365_days`|Number of pull requests merged in the last 365 days|
|`activity.prs_merged_total`|Total number of pull requests merged (all time)|
|`activity.prs_closed_last_90_days`|Number of pull requests closed in the last 90 days|
|`activity.prs_closed_last_180_days`|Number of pull requests closed in the last 180 days|
|`activity.prs_closed_last_365_days`|Number of pull requests closed in the last 365 days|
|`activity.prs_closed_total`|Total number of pull requests closed (all time)|
|`activity.merged_pr_age_avg`|Average age in days of merged pull requests|
|`activity.merged_pr_age_p50`|Median age in days of merged pull requests|
|`activity.merged_pr_age_p75`|75th percentile age in days of merged pull requests|
|`activity.merged_pr_age_p90`|90th percentile age in days of merged pull requests|
|`activity.merged_pr_age_p95`|95th percentile age in days of merged pull requests|
|`activity.merged_pr_age_last_90_days_avg`|Average age in days of pull requests merged in the last 90 days|
|`activity.merged_pr_age_last_90_days_p50`|Median age in days of pull requests merged in the last 90 days|
|`activity.merged_pr_age_last_90_days_p75`|75th percentile age in days of pull requests merged in the last 90 days|
|`activity.merged_pr_age_last_90_days_p90`|90th percentile age in days of pull requests merged in the last 90 days|
|`activity.merged_pr_age_last_90_days_p95`|95th percentile age in days of pull requests merged in the last 90 days|
|`activity.merged_pr_age_last_180_days_avg`|Average age in days of pull requests merged in the last 180 days|
|`activity.merged_pr_age_last_180_days_p50`|Median age in days of pull requests merged in the last 180 days|
|`activity.merged_pr_age_last_180_days_p75`|75th percentile age in days of pull requests merged in the last 180 days|
|`activity.merged_pr_age_last_180_days_p90`|90th percentile age in days of pull requests merged in the last 180 days|
|`activity.merged_pr_age_last_180_days_p95`|95th percentile age in days of pull requests merged in the last 180 days|
|`activity.merged_pr_age_last_365_days_avg`|Average age in days of pull requests merged in the last 365 days|
|`activity.merged_pr_age_last_365_days_p50`|Median age in days of pull requests merged in the last 365 days|
|`activity.merged_pr_age_last_365_days_p75`|75th percentile age in days of pull requests merged in the last 365 days|
|`activity.merged_pr_age_last_365_days_p90`|90th percentile age in days of pull requests merged in the last 365 days|
|`activity.merged_pr_age_last_365_days_p95`|95th percentile age in days of pull requests merged in the last 365 days|

#### Documentation Metrics

|Metric|Description|
|------|-----------|
|`docs.documentation`|URL to the crate’s documentation|
|`docs.public_api_elements`|Number of public API elements (functions, structs, etc.)|
|`docs.undocumented_public_api_elements`|Number of public API elements without documentation|
|`docs.public_api_coverage_percentage`|Percentage of public API elements with documentation|
|`docs.crate_level_docs_present`|Whether crate-level documentation exists|
|`docs.broken_links`|Number of broken links in documentation|
|`docs.examples_in_docs`|Number of code examples in documentation|
|`docs.standalone_examples`|Number of standalone example programs in the codebase|

#### Advisory Metrics

|Metric|Description|
|------|-----------|
|`advisories.total_low_severity_vulnerabilities`|Number of low severity vulnerabilities across all versions|
|`advisories.total_medium_severity_vulnerabilities`|Number of medium severity vulnerabilities across all versions|
|`advisories.total_high_severity_vulnerabilities`|Number of high severity vulnerabilities across all versions|
|`advisories.total_critical_severity_vulnerabilities`|Number of critical severity vulnerabilities across all versions|
|`advisories.total_notice_warnings`|Number of notice warnings across all versions|
|`advisories.total_unmaintained_warnings`|Number of unmaintained warnings across all versions|
|`advisories.total_unsound_warnings`|Number of unsound warnings across all versions|
|`advisories.version_low_severity_vulnerabilities`|Number of low severity vulnerabilities in this version|
|`advisories.version_medium_severity_vulnerabilities`|Number of medium severity vulnerabilities in this version|
|`advisories.version_high_severity_vulnerabilities`|Number of high severity vulnerabilities in this version|
|`advisories.version_critical_severity_vulnerabilities`|Number of critical severity vulnerabilities in this version|
|`advisories.version_notice_warnings`|Number of notice warnings for this version|
|`advisories.version_unmaintained_warnings`|Number of unmaintained warnings for this version|
|`advisories.version_unsound_warnings`|Number of unsound warnings for this version|

#### Code Metrics

|Metric|Description|
|------|-----------|
|`code.source_files`|Number of source files|
|`code.source_files_with_errors`|Number of source files that had analysis errors|
|`code.code_lines`|Number of lines of production code (excluding tests)|
|`code.test_lines`|Number of lines of test code|
|`code.comment_lines`|Number of comment lines in the codebase|
|`code.transitive_dependencies`|Number of transitive dependencies|

#### Trustworthiness Metrics

|Metric|Description|
|------|-----------|
|`trust.unsafe_blocks`|Number of unsafe blocks in the codebase|
|`trust.ci_workflows`|Whether CI/CD workflows were detected in the repository|
|`trust.miri_usage`|Whether Miri is used in CI|
|`trust.clippy_usage`|Whether Clippy is used in CI|
|`trust.code_coverage_percentage`|Percentage of code covered by tests|


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-aprz">source code</a>.
</sub>

 [__link0]: https://github.com/google/cel-spec/blob/master/doc/langdef.md
