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

`gamma-hints.json` may sit beside it. It is not configuration and has no keys: it is a
generated artifact written by `cargo gamma hints`, holding the parts of a previous run that cannot
move a score — which test caught which mutant, and which mutants failed to compile — so that a fresh
checkout starts warm instead of cold. Runs read it automatically and it needs no setting here. See
[checking in the hints file](../README.md#checking-in-the-hints-file).

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

Each `exclude-trait-impls` entry is compared with the lexical terminal identifier written in an implementation path.
Qualification does not matter: `impl Debug`, `impl fmt::Debug`, and `impl core::fmt::Debug` all
have the terminal name `Debug`. An alias keeps its written name, so `impl Diagnostic` is matched by
the `Diagnostic` entry.

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

# Seconds the build may take before the run is abandoned. Default: unlimited.
build-timeout = 1800.0

# The multiple of the first successful build's duration a later build round is allowed.
build-timeout-multiplier = 3.0
```

`cargo-args` does not support Cargo's `--config` option. Put that setting in a Cargo configuration
file gamma can inspect, so discovery and cache provenance describe the same build Cargo runs.

A run builds once and then runs the suite once per mutant, so a build that never finishes costs the
whole run rather than one mutant — which is why the build gets its own timeout rather than sharing
the per-mutant one. `build-timeout-multiplier` covers the rollback rounds: those rebuild the same
tree with fewer mutants, so a round taking far longer than the first is evidence of a problem rather
than of a slow machine.

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

# Disable the default case-level reachability census and run each reachable test binary whole.
# Default: false.
whole-test-binaries = false

# Test target name globs that may or may not decide a verdict.
include-tests = ["unit_*"]
exclude-tests = ["*_slow", "e2e_*"]

# Run tests with nextest for per-test process isolation. Default: false.
nextest = false
```

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

By default gamma considers a case-level census for test binaries that can reach selected pending
mutants. It measures listing startup cost and skips the census when its projected process-launch cost
cannot repay the maximum test work it could save. A census that proceeds is bounded by that same
maximum saving, and sampling stops early when every relevant site already reaches more than half the
binary's tests and would therefore use the whole binary.

Only a complete census may exclude tests or establish that a site is uncovered. Positive reach
observations from a budget-limited census are checked hints: the named cases run first, and any result
other than a kill falls back to the whole binary. Failed samples are discarded. Set
`whole-test-binaries = true` (or pass `--whole-test-binaries`) when reachability depends on threads,
clocks, randomness or hash iteration order. The opt-out keeps target and package filters intact; it
only stops filtering reachable binaries down to individual cases.

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
If any selected mutant remains pending, the gate fails as incomplete rather than evaluating a
score over only the completed subset.

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
