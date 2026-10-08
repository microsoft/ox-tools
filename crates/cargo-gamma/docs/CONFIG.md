# Configuration reference

Every key `cargo-gamma` reads from a configuration file.

A configuration file is optional. It exists so that the settings a project has *decided on* — the
mutators it runs, the files it excludes, the score it holds itself to — live in version control and
apply to everyone, rather than being retyped on each command line and drifting between developers
and CI.

For a file you can copy and edit down, see [`gamma.toml`](gamma.toml), which lists every key below
with its default. For the command-line flags these mirror, see [CMDLINE.md](CMDLINE.md).

## Contents

* [Where the file lives](#where-the-file-lives)
* [How settings combine](#how-settings-combine)
* [Unknown keys are an error](#unknown-keys-are-an-error)
* [Selecting what to mutate](#selecting-what-to-mutate)
* [Cargo features](#cargo-features)
* [Building](#building)
* [Running tests](#running-tests)
* [Baseline](#baseline)
* [Memory](#memory)
* [Run control](#run-control)
* [`artifact-dir`](#artifact-dir)
* [`[shard]`](#shard)

## Where the file lives

`gamma.toml` in the directory being analyzed — so the workspace root for a workspace run, and
`--dir` decides it otherwise.

Two flags change this:

* `--config <PATH>` reads a specific file instead. An explicit path **must exist** — asking for a
  file and silently getting the defaults because the name was misspelled is precisely the failure
  this guards against. A missing *conventional* file is the ordinary case and is not an error.
* `--no-config` reads no file at all, which is what makes a scripted run reproducible regardless of
  what the project happens to have committed.

`.cargo/mutants.toml` is noticed and deliberately not read. Its keys overlap only partly, and the
ones that look identical do not always mean the same thing, so reading it would produce a run that
silently differs from what either tool would do. It is unsupported; configure Gamma independently
in `gamma.toml`. When it is present and `gamma.toml` is not, the tool says so.

`gamma-hints.yaml` may sit beside it. It is not configuration and has no user-editable settings: it
is a generated artifact written by `cargo gamma hints`, holding the parts of previous runs that
cannot move a score. Ordinary promotion merges exact killers, generalized item/file candidates,
reaching-test sets, and compiler-unviability ordering into the existing workspace artifact.
Knowledge outside a package, file, diff, feature, or mutator selection is preserved; pass
`--replace` only to rebuild the artifact from the selected population intentionally.

Candidate tests are executed again rather than trusted as verdicts, so a fresh checkout starts warm
without carrying a score forward. Format version 4 groups mutants by workspace-relative source
file and stores each repeated killer identity once in that file's killer table; mutant entries
refer to the table by index. Its `context` contains only the full `repo_sha` at generation time and
the UTC `generated_on` date. These fields describe artifact provenance and do not gate hints or
claim that retained entries originated at that revision. No-op promotion preserves the date and
bytes.

The independently versioned generalized section is schema version 3. It separates seed
observations from transfer hits and misses, interns repeated test and binary identities, and
stores kind-qualified target identities. Versions 1 and 2 are migrated on read. Version 1 seed
counts are retained with a minimum of one while hit, miss, measured-time, and sample counters are
reset; version 2 observations are preserved. Other generalized schema versions are unsupported.

Runs read the artifact automatically and it needs no setting here. Malformed artifacts, unsupported
versions, and artifacts whose producer is not cargo-gamma are ignored safely. An unsupported
generalized section is recognized but its contents are not decoded; the supported envelope and exact
hints remain reusable for automatic scheduling. Ordinary promotion leaves the original artifact
untouched because it cannot round-trip that section. See
[checking in the hints file](../README.md#checking-in-the-hints-file).

Explicit promotion is stricter than automatic reading. Ordinary promotion refuses a malformed,
foreign, or unsupported existing generation rather than silently replacing knowledge it cannot
round-trip. `--replace` permits discarding that unsupported knowledge, but it does not disable
publication safety: the command still compares the exact YAML bytes it read, leaves a concurrently
written generation alone, writes atomically, and verifies the published artifact. An unreadable or
over-limit existing file is never overwritten implicitly, even with `--replace`.

## How settings combine

Three sources, later beating earlier:

1. The built-in default.
2. The configuration file.
3. The command line.

The interesting part is *how* the command line beats the file, which depends on the key's type:

| Key type | Behavior | Example |
| --- | --- | --- |
| Scalar — number, string, boolean | **Overridden.** The flag replaces the file's value. | `jobs = 4` in the file, `--jobs 8` on the command line, run uses 8. |
| List — `files`, `cargo-args`, `packages`, … | **Extended.** The flag adds to the file's value. | `exclude-files = ["a/**"]` plus `--exclude-file 'b/**'` excludes both. |
| `mutators` | **Overridden**, unusually for a list. | A selector list is one decision, not an accumulation. |

There is deliberately no syntax for subtracting from a list the file set. If you need to ignore the
file, ignore all of it with `--no-config`; a per-key escape hatch would make the effective
configuration something you have to compute rather than read.

## Unknown keys are an error

Every table is parsed with `deny_unknown_fields`, so a misspelled key stops the run instead of
doing nothing. A configuration file has no `--help` and no completion, so a typo that parsed
successfully would be discovered only by noticing that a setting never took effect — which in
practice means never.

Numeric keys are range-checked when the file is read, with the same bounds the command-line parsers
apply, so a bad value is reported the same way from either source.

## Selecting what to mutate

```toml
# Which mutators to apply. A selector is a mutator name, a family, an `@preset`, or `all`;
# `!` subtracts, and selectors apply left to right.
mutators = [
    "@default",
    "!literal",
]

# Globs limiting which files are mutated. Empty means every file in the selected packages.
files = ["src/**/*.rs"]

# Globs excluding files, applied after `files`.
exclude-files = ["src/generated/**", "**/build.rs"]

# Packages to mutate. Empty follows Cargo: the owning package below a package root, or the
# workspace's default members at the workspace root.
packages = ["my-core", "my-api"]

# Additional error values for the `fn_value.err_with` mutator.
errors = ["MyError::Timeout"]

# Debug output is diagnostic text without a stable formatting contract.
exclude-trait-impls = ["Debug", "Display"]
```

`mutators` is a list here but a comma-separated string on the command line. The entries are joined with
commas and handed to exactly the same parser, so the two forms accept the same selectors — the list
exists only so each entry can carry a comment saying why it is there, which is the thing most worth
recording about a mutator you have switched off. See [MUTATORS.md](MUTATORS.md) for the catalog.
`@pedantic` contains valid but commonly low-yield mutations that are not enabled by default. Select
it alone for a focused run, or add it to the normal selection with
`mutators = ["@default", "@pedantic"]`.

A non-default selector can also include source locations that an ordinary run skips because their
types are unclear and the generated mutation often does not compile. This currently applies to
uncertain `literal.int_decrement`, `expr.increment`, and `expr.decrement` locations. Naming the
mutator, its family, another preset that contains it, or `all` includes those locations. The same
rule applies to `iter.remove_filter` when the source does not establish that the receiver is an
iterator. See
[MUTATORS.md](MUTATORS.md#choosing-what-to-run) for the exact behavior.

Each `exclude-trait-impls` entry is an unqualified Rust identifier compared with the final written
segment of an implementation's trait path.
Qualification does not matter: `impl Debug`, `impl fmt::Debug`, and `impl core::fmt::Debug` all
have the final identifier `Debug`. An alias keeps its written identifier, so `impl Diagnostic` is
matched by the `Diagnostic` entry.

This is lexical matching, not Rust name resolution. Gamma cannot semantically distinguish two
imported traits that are both written with the same terminal name, and it does not guess which
declaration an alias denotes without rustc resolution. Every configured name must match at least
one implementation in the discovered source population; an unmatched entry is a usage error rather
than a silent no-op, so a misspelling cannot quietly change selection. These project-wide rules are
for cross-cutting policy. Prefer a `#[gamma::skip(...)]` directive beside the source for a single
equivalent mutant, where its reason can be reviewed with the code.

## Cargo features

```toml
features = ["postgres", "tracing"]
all-features = false
no-default-features = false
```

Discovery and the build always agree on these. Finding mutants under one feature set and compiling
under another would produce mutants that cannot exist, which is not a failure a user could diagnose.

## Building

```toml
# The cargo profile to build with.
profile = "test"

# Extra arguments for every cargo invocation. These reach cargo, not the test binaries.
cargo-args = ["--offline", "--locked"]

# Seconds one compiler invocation may take before the run is abandoned. Default: unlimited.
build-timeout = 1800.0

# The multiple of the first compiler round's duration a later round is allowed.
build-timeout-multiplier = 3.0
```

`cargo-args` does not support Cargo's `--config` option. Put that setting in a Cargo configuration
file gamma can inspect, so discovery and cache provenance describe the same build Cargo runs.
When `test-lib = true`, `cargo-args` must not contain Cargo target selectors such as `--tests`,
`--test`, `--bins`, or `--all-targets`: those selectors would widen or replace the library-only
oracle, so the effective configuration is rejected.

A run establishes a compiler-viable mutant schema before it executes any mutant. It checks the
instrumented packages, withdraws compiler-rejected mutants, and repeats until the schema checks;
then it generates the test binaries. A compiler invocation that never finishes therefore blocks
the whole campaign rather than one mutant, which is why compiler work has its own timeout.
`build-timeout-multiplier` covers later convergence rounds: they check the same warm tree with fewer
admitted mutants, so a round taking far longer than the first is evidence of a problem rather than
of a slow machine.

An optimized profile can pay when mutant execution dominates a CPU-heavy run: the slower build is
paid once, while the faster suite is paid once per mutant. It is less useful for build-heavy narrow
runs and I/O-bound suites. The
[`gamma` profile example](../README.md#optimizing-compute-heavy-suites) retains debug assertions
and overflow checks; select it explicitly with `cargo gamma run --profile gamma`.

Changing profiles invalidates compiler-unviability reuse and may change verdicts through different
code generation. Scores from different profiles are not directly comparable, so choose the
profile before a long campaign and keep it fixed across runs and shards that will be merged.

## Running tests

```toml
# How many mutants to test at once. Default: one more than the available parallelism (cores + 1).
jobs = 8

# The multiple of each test binary's baseline duration a mutant is allowed.
test-timeout-multiplier = 1.5

# A lower bound on the test binary timeout, however fast the baseline was.
minimum-test-timeout = 20.0

# Extra arguments for every test binary. These reach the harness, not cargo.
cargo-test-args = ["--test-threads=1"]

# Which packages' tests may decide a verdict. Empty means each mutant's own package.
test-packages = ["my-integration-tests"]

# Let every workspace package's tests judge mutants they can reach.
# Default: false.
test-workspace = false

# Compile and run only library unit-test harnesses when deciding verdicts.
# Default: false.
test-lib = false

# Experimentally measure case-level reachability before testing mutants.
# Default: false.
optimize-test-execution = false

# Explicitly suppress case-level selection and run each reachable test binary whole.
# This conflicts with optimize-test-execution.
whole-test-binaries = false

# Test target name globs that may or may not decide a verdict.
include-tests = ["unit_*"]
exclude-tests = ["*_slow", "e2e_*"]

# Run tests with nextest for per-test process isolation. Default: false.
nextest = false

# Capacities for shared resources declared beside tests. A declared resource omitted here has a
# conservative capacity of one.
[resources]
cargo-subprocess = 2
powershell = 1
```

A test declares only the stable resource identity:

```rust
#[gamma::resource("cargo-subprocess")]
#[test]
fn resolves_metadata() {
    // ...
}
```

An annotation on a test function applies to that case. An annotation on an inline test module
applies to every execution of the containing test target, which is useful when a whole
integration-test binary shares the same expensive fixture. An integration target may use an empty
inline module solely to declare that binary-wide resource. Command-line
`--resource-concurrency NAME=N` settings override this table. Every admission atomically reserves
all resources needed by the selected tests, so tests requiring more than one resource cannot
deadlock by acquiring them in different orders. A capacity whose resource is not active in the
selected packages is ignored, allowing workspace-wide configuration to apply to package-scoped
campaigns. Each resource name may appear at most once on the command line; duplicate capacities are
rejected rather than resolved by argument order.

A derived budget adapts to the machine it runs on and to each specific test binary.
`minimum-test-timeout` exists because a test binary finishing in milliseconds would otherwise get a budget of
just over that duration, which a loaded machine can exceed for reasons having nothing to do with the
mutant.

`cargo-test-args` is the file's equivalent of the trailing `-- …`, which TOML has no way to express.
The two are concatenated rather than one overriding the other.

Set `nextest = true` (or pass `--nextest` on the command line) when the suite depends on per-test process isolation — tests that set
environment variables, install process-wide handlers, or share a global singleton. Such a suite is
not merely slower under a threaded harness; it is red, and a red baseline stops the run. Mutants are
still judged against binaries this run built itself, so nextest never invokes cargo and a mutant
costs one extra process rather than one extra build.

By default gamma performs no case-level census: exact and generalized probes still run first, and
anything they do not kill falls back to the complete reachable test binary. Set
`optimize-test-execution = true` (or pass `--optimize-test-execution`) to enable the experimental
census. The opt-in still passes through an economic gate: gamma measures listing startup cost and
skips sampling when its projected process-launch cost cannot repay the maximum test work it could
save. A census that proceeds is bounded by that same maximum saving, and sampling stops early when
every relevant site already reaches more than half the binary's tests.

Only a complete census may exclude tests or establish that a site is uncovered. Positive reach
observations from a budget-limited census are checked hints: the named cases run first, and any result
other than a kill falls back to the whole binary. Failed samples are discarded.
`whole-test-binaries = true` (or `--whole-test-binaries`) explicitly forbids case-level selection
and conflicts with the census opt-in. Both modes keep target and package filters intact.

## Baseline

```toml
# Skip the baseline run entirely.
no-baseline = false

# Believe a failing test without re-running it with no mutant active.
no-confirm = false
```

Both trade trustworthiness for speed, and both defaults are the trustworthy choice. Without a
baseline there is no evidence that a failing test was caused by the mutant rather than by a suite
that was already red, and no measurement to derive a timeout or a memory ceiling from — so a run
with `no-baseline = true` and `memory = "enforce"` must also set `memory-limit` explicitly.

## Memory

```toml
# "off", "measure", or "enforce".
memory = "enforce"

# A ceiling derived from each test binary's baseline peak.
memory-multiplier = 2.0
memory-headroom = "128MiB"

# Or an explicit ceiling, instead of a derived one.
memory-limit = "2GiB"
baseline-memory-limit = "4GiB"
```

| Value | Measures | Enforces |
| --- | --- | --- |
| `off` | no | no |
| `measure` | yes | no |
| `enforce` | yes | yes |

The command-line size flags select their documented modes even when the file names another one:
`--memory-limit` implies `enforce`, and `--baseline-memory-limit` implies `measure`. An explicit
command-line `--memory` remains more specific than either implication.

The default is `enforce`, on the same reasoning as the wall-clock timeout: a mutation can turn
bounded allocation into unbounded allocation, and the person who most needs protecting from that is
the one who never thought to ask for it. A timeout does eventually catch it, but only after the
machine has spent minutes swapping.

Where the host cannot provide the accounting, a run that merely *defaulted* into `enforce` drops to
`off` and says so, rather than refusing to start. A run that was *asked* for `enforce` is an error
instead — someone who passed `--memory` did so because an unbounded mutant would cost them a wedged
laptop or a CI runner that takes the rest of the job down with it, and quietly giving them a run
without that protection would be discovered only by the thing they were trying to prevent.

`measure` is the honest starting point for a project that does not yet know what its suite
allocates: it costs one accounting boundary per invocation and gives you the numbers a ceiling has
to be chosen from.

## Run control

```toml
# Caching and incremental mode: "no" or "build". Default: "build".
incremental = "build"

# Fail the run if the assertion-killed mutation score is below this percentage.
min-score = 70.0

# Fail independently if more than this many flaky outcomes remain.
# Requires confirmation to stay enabled.
max-flaky = 0
```

| Mode | Reuses unviability | Reuses killer hints | Reuses test verdicts |
| --- | --- | --- | --- |
| `no` | no | no | no |
| `build` | yes, under matching compilation inputs and context | yes, after checking the hinted test | no |

`incremental` defaults to `build`. It skips compiler-unviable mutants only when cryptographic input
digests and the compilation context match. It may try a previous killer first, but every score-bearing
outcome is established again: unchanged inputs cannot prove that a test result is deterministic. Set
`incremental = "no"` or pass `--incremental no` for a completely cold run.

Set it below where you are today and ratchet upwards. A gate set above the current score turns every
build red on the day it lands, and a gate that is red by default gets switched off within a week.
Only mutants rejected by a failing test assertion enter the score's numerator. Survivors,
uncovered mutants, timeouts, and out-of-memory mutants remain in its denominator, so
`min-score = 100.0` fails closed on any of those outcomes.
If any selected mutant remains pending, either gate fails as incomplete rather than evaluating a
score or flaky count over only the completed subset.
`max-flaky` is independent of the score because flaky outcomes are inconclusive and excluded from
its denominator. `max-flaky = 0` requires a fully conclusive population; it cannot be combined with
`no-confirm = true`, which removes the observation needed to identify a flaky outcome.

## `[shard]`

```toml
[shard]
count = 8
index = 0
```

Splits the population across parallel CI jobs; combine the resulting reports with `cargo gamma
merge`. Both keys are required together — a count without an index does not describe a shard.

Usually these come from the command line, since the index differs per job while the file is shared.
Setting `count` here and passing `--shard-index` per job is a reasonable split.

## `artifact-dir`

```toml
artifact-dir = "target/cargo-gamma"
```

Moves all five user-facing artifacts as one set. The directory is created when necessary. Omitting
the key writes `gamma-report.json`, `gamma-report.html`, `gamma-report.sarif`,
`gamma-perf-advice.md`, and `gamma-diagnostics.json` under the original workspace's
`target/cargo-gamma`.
