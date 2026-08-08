# cargo-aprz — Design

> Status: **Adopted**.
> Crate name: `cargo-aprz` (binary) plus `cargo-aprz-lib` (implementation detail).
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## Overview

`cargo-aprz` turns crate identities into appraisals through a staged pipeline:

```text
crate selection
    -> fact collection
    -> metric extraction
    -> policy evaluation
    -> report rendering and optional rejection
```

The command layer selects explicit crates or resolves a Cargo dependency graph,
loads configuration, and coordinates the remaining stages. Each later stage
operates on typed data produced by the previous stage rather than reading source
systems directly.

## Fact collection

Fact providers retrieve independent views of each crate from crates.io, source
hosting services, the RustSec advisory database, docs.rs, coverage services, and
local source analysis. Providers return either typed data or an explicit
unavailable result so downstream evaluation can distinguish missing information
from a legitimate zero value.

Collection is asynchronous where providers can run independently. Source-code
analysis is grouped by repository so crates from the same repository share the
clone and repository-level work.

## Cache storage

Provider data is stored beneath a platform-specific cache root, partitioned by
source:

```text
cache root/
    crates/
    hosting/
    codebase/
    coverage/
    advisories/
    docs/
```

Most provider entries are MessagePack files containing a timestamp and either
typed data or a negative-cache result. Each provider has its own configurable
time-to-live. Expired, corrupt, or explicitly ignored entries are treated as
cache misses. Repository clones and the RustSec database are kept in their
provider partitions alongside synchronization metadata.

The process takes an advisory lock on the cache root before collection so two
instances do not concurrently mutate shared cache state.

## Metrics

Collected facts are normalized into named, typed metrics. Metric values may be
unsigned integers, floating-point values, booleans, strings, timestamps, or
lists. Policy expressions and report generators consume this common metric
representation, keeping source-specific response formats out of those stages.

Unavailable provider data normally becomes a missing or null metric. Expression
evaluation can therefore report that a policy was inconclusive instead of
silently converting unavailable information into a passing value.

## Policy evaluation

Each crate is evaluated in two phases:

1. Required `high_risk` expressions run first. A false or inconclusive result
   immediately produces a high-risk appraisal without a weighted score.
2. If the required phase passes, weighted `eval` expressions contribute awarded
   and available points. The percentage score is mapped to low, medium, or high
   risk using configured thresholds.

An expression that cannot be evaluated produces an inconclusive outcome. Its
configured positive weight remains in the available-points denominator but it
earns no points, so partial provider failure cannot inflate the score by
silently shrinking the policy. If positive-weight expressions are configured
but every one is inconclusive, evaluation fails closed as high risk without a
score. An empty weighted policy, or a policy containing only zero-weight
expressions, remains low risk with the neutral score of 100.

An appraisal stores every expression outcome alongside a private state enum.
The enum distinguishes a scored appraisal, required-gate failure, and total
weighted-evaluation failure. Risk, point totals, and score availability are
derived from that state, preventing invalid combinations and ensuring the two
unscored states remain distinct.

## Reporting and rejection

Report generators consume the same crate, metric, and appraisal model to produce
console, JSON, HTML, CSV, or Excel output. Structured formats retain typed metric
values. JSON also exposes structured appraisal state and individual outcomes. The
appraisal object has this contract:

```json
{
  "result": "HIGH RISK (...)",
  "risk": "low | medium | high",
  "required_check_failure": false,
  "weighted_evaluation_failure": false,
  "score": 75.0,
  "awarded_points": 3,
  "available_points": 4,
  "reasons": ["legacy display strings"],
  "outcomes": [{
    "name": "check name",
    "description": "expected policy condition",
    "disposition": "passed | failed | inconclusive",
    "evaluation_error": null
  }]
}
```

Scored appraisals have numeric score and point fields with both failure flags
false. Required-gate failures set only `required_check_failure`; total weighted
evaluation failures set only `weighted_evaluation_failure`. Both unscored states
use `null` for score and point fields. `evaluation_error` is non-null only for
an `inconclusive` outcome. Legacy `result` and `reasons` remain for compatibility,
while machine consumers should prefer the structured fields.

CSV neutralizes formula-like crate headers, metric names, and textual metric
values before escaping them. Excel emits textual values as string cells.
Numeric values remain numeric in both formats.

Risk thresholds can turn an appraisal into a command failure. The rejection
error identifies the affected crates and distinguishes failed policy requirements
from inconclusive evaluation. Its detail list is bounded to keep errors usable:
up to 20 crates and 10 non-passing outcomes per crate. When details are omitted,
the output points users to complete console or JSON reports.

Policy descriptions state the condition an expression expects. Rejection output
labels that text as `expected` rather than presenting the desired condition as
though it were the reason for rejection. Inconclusive output separately identifies
the expected condition and the evaluation error.

Allow-list entries are applied after appraisal. An allowed crate remains visible
with its computed metrics and risk, but it does not cause the command to fail.

Issue activity is reported in two families. The `issue` metrics count every issue
in the repository. The `bug` metrics count the subset whose labels match the
configured `bug_labels` patterns, so bug counts are always bounded by the
corresponding issue counts. Modelling bugs as a subset rather than partitioning
issues into bug and non-bug families preserves the established meaning of the
existing issue metrics, so expressions written against them keep measuring what
they measured before bug metrics existed.

Label matching uses case-insensitive regular expressions matched anywhere within
the label, so conventional prefixed labels such as `C-bug` and `type: bug` match
the default patterns without per-repository configuration, while a repository
with an unusual labelling scheme can be described precisely with an anchored
pattern such as `^(c|kind)[-/]bug$`. The patterns are compiled once per run into
a single regex set: a repository can carry thousands of issues, each with several
labels, so compiling per comparison would be wasteful. Invalid patterns are
rejected when the configuration is loaded rather than when issues are classified.
Issues with no labels are not treated as bugs: inferring defects
from unlabeled issues would overcount discussion-heavy repositories. The cost of
that choice is that repositories which do not label issues report no bugs at all,
which a bug-based check would read as healthy. The `labeled_issue_ratio` metric
exposes labelling coverage so expressions can distinguish an absence of bugs from
an absence of labels.

The hosting cache stores raw issue records rather than computed statistics.
Because bug classification is user-configurable, aggregating at fetch time would
bake the configuration into the cache and serve stale statistics after a
configuration change. Deriving statistics on load keeps configuration changes free
of network cost and keeps the cache independent of policy.
