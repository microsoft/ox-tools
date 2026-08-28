# Command-line reference

Every subcommand and every option `cargo-gamma` accepts.

## Contents

* [Invoking the tool](#invoking-the-tool)
* [The subcommands](#the-subcommands)
* [The option categories](#the-option-categories)
* [Where settings come from](#where-settings-come-from)
* [Passing arguments through](#passing-arguments-through)
* [Understanding uncovered mutants](#understanding-uncovered-mutants)
* [Full option reference](#full-option-reference)
  * [Accepted by every subcommand](#accepted-by-every-subcommand)
  * [`gamma run`](#gamma-run)
  * [`gamma list`](#gamma-list)
  * [`gamma explain`](#gamma-explain)
  * [`gamma suppress`](#gamma-suppress)
  * [`gamma unsuppress`](#gamma-unsuppress)
  * [`gamma merge`](#gamma-merge)
  * [`gamma completions`](#gamma-completions)

## Invoking the tool

`cargo-gamma` is a cargo subcommand, so it is spelled with a space:

```bash
cargo gamma run
```

With no subcommand, `run` is implied, so these are the same:

```bash
cargo gamma
cargo gamma run
```

That shorthand only applies when the first argument is not itself a subcommand. `cargo gamma --workspace`
is a run over the whole workspace; `cargo gamma list` is the `list` subcommand and not a run.

## The subcommands

<!-- begin generated: commands -->

| Command | What it does |
| --- | --- |
| [`gamma run`](#gamma-run) | Run mutation testing |
| [`gamma list`](#gamma-list) | List what would be done, without doing it |
| [`gamma explain`](#gamma-explain) | Explain a mutator, a mutant, or a suppression |
| [`gamma suppress`](#gamma-suppress) | Write suppressions into the source for mutants that cannot usefully be tested |
| [`gamma unsuppress`](#gamma-unsuppress) | Remove skip directives that no longer suppress anything |
| [`gamma merge`](#gamma-merge) | Combine per-shard reports into one answer |
| [`gamma hints`](#gamma-hints) | Promote what a run learned about speed into a file the workspace can check in |
| [`gamma clean`](#gamma-clean) | Delete cargo-gamma's cached data for a workspace |
| [`gamma completions`](#gamma-completions) | Print a shell completion script |

<!-- end generated -->

## The option categories

The same categories appear in `--help` and in the reference below. Knowing which category a setting
lives in is usually enough to find it:

| Category | What it governs |
| --- | --- |
| **Selecting what to mutate** | Which files, packages, lines and mutators make up the population. Applied before anything is built. |
| **Cargo features** | The feature set to compile under. Discovery and the build must agree, so these apply to both. |
| **Configuration** | Where settings are read from, and whether the file is read at all. |
| **Building** | The one build the whole run depends on: profile, arguments to cargo, timeouts, and how many rollback rounds are allowed. |
| **Running tests** | How the suite is executed per mutant: parallelism, timeouts, which test targets may decide a verdict, and which runner. |
| **Memory** | The ceiling each test binary runs under, and how it is derived. A mutant can turn bounded allocation into unbounded allocation, which a timeout catches only slowly. |
| **Scratch tree** | Where the instrumented copy of the workspace lives, and what is copied into it. |
| **Run control** | What the run does as a whole: gate on a score, resume from a previous run, stop early, or only estimate. |
| **Reporting** | What is written where, and how much detail the console prints. |
| **Global options** | Color and progress, accepted by every subcommand. |

Two categories are specific to one subcommand: **Suppressing** (`suppress`, `unsuppress`) and
**Merging** (`merge`).

## Where settings come from

A setting can be given in three places. Later beats earlier:

1. **The built-in default.**
2. **The configuration file**, `gamma.toml` by default — see [CONFIG.md](CONFIG.md) for every
   key, and [`gamma.toml`](gamma.toml) for a documented file to copy.
3. **The command line.**

So a flag always wins over the file, and `--no-config` ignores the file entirely — which is what
makes a scripted run reproducible regardless of what the project happens to have committed.

Not every option has a configuration key, and not every key has an option. A key with no flag is
noted in [CONFIG.md](CONFIG.md); an option with no key is generally one that makes no sense to
persist, such as `--dry-run`.

## Passing arguments through

Three options forward arguments to something else, and they are not interchangeable:

| Option | Reaches | Use it for |
| --- | --- | --- |
| `--cargo-arg`, `-C` | every **cargo** invocation | `--offline`, `--locked`, `--target`, and other build arguments (not Cargo `--config`; use a configuration file gamma can inspect) |
| `--cargo-test-arg` | every **test binary** | harness arguments, in a form a configuration file can hold |
| `--` (trailing) | every **test binary** | the same thing, typed at the shell |

`--cargo-test-arg` and `--` mean the same thing to the harness and are concatenated. Both exist
because a configuration file cannot express a positional `--`, and because `--` must come last, so
nothing can follow it:

```bash
cargo gamma run --cargo-arg --offline -- --test-threads=1
```

## Understanding uncovered mutants

By default, `uncovered` means no selected runtime test activated that mutation site's gamma guard in
the reachability census. It does not mean that every external coverage report must show the source
line as missed. With `--whole-test-binaries`, no census runs and this case-level classification is
unavailable.

For example, code annotated `#[coverage(off)]` is removed from a coverage tool's denominator but
remains production code that gamma mutates. A project can therefore report 100% line coverage while
gamma correctly reports an uncovered mutant in that function. Coverage combined across other
platforms, features or test targets can create the same difference, as can cfg-disabled or
compile-time proc-macro code. Line coverage can also mark a line while a particular expression or
branch on it never ran.

Respond according to intent: add a runtime test, align the run's features/platform/test-targets with
the coverage job, or use a reviewed [suppression](../README.md#suppressing-mutations) for code that
is intentionally outside the mutation oracle. Gamma does not automatically exclude
`#[coverage(off)]`, because deliberately uncounted code can still contain behavior worth testing.

## Full option reference

`--help` and `--version` are accepted everywhere and are omitted below. Options marked with a
default take that value when neither the command line nor the configuration file sets one.

<!-- begin generated: options -->

### Accepted by every subcommand

**Global options**

| Option | Value | What it does |
| --- | --- | --- |
| `--color` | `<WHEN>` | When to use color in output. Defaults to `auto`. |
| `--progress` | `<WHEN>` | When to show the progress display. Defaults to `auto`. |

### `gamma run`

Run mutation testing

```text
cargo gamma run [OPTIONS] [-- <TEST_ARGS>...]
```

**Selecting what to mutate**

| Option | Value | What it does |
| --- | --- | --- |
| `-d`, `--dir` | `<PATH>` | Path to the workspace or package to analyze. Defaults to `.`. |
| `--mutators` | `<SELECTORS>` | Mutators to apply, as a comma-separated selector list. |
| `--file` | `<GLOB>` | Only mutate files matching these glob patterns. |
| `--exclude-file` | `<GLOB>` | Skip files matching these glob patterns. |
| `--shard-count` | `<COUNT>` | Number of shards to divide the mutants into. |
| `--shard-index` | `<INDEX>` | Which shard to run, from 0. |
| `-D`, `--in-diff` | `<PATH>` | Only mutate lines added or changed by this unified diff, or `-` for standard input. |
| `-p`, `--package` | `<NAME>` | Only mutate these packages. Defaults to Cargo's package selection for the current directory. |
| `--workspace` |  | Mutate every package in the workspace. |
| `--error` | `<EXPR>` | Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`. |

**Cargo features**

| Option | Value | What it does |
| --- | --- | --- |
| `--features` | `<FEATURES>` | Cargo features to activate, comma-separated or repeated. |
| `--all-features` |  | Activate every feature of every selected package. |
| `--no-default-features` |  | Do not activate the `default` feature. |

**Configuration**

| Option | Value | What it does |
| --- | --- | --- |
| `--config` | `<PATH>` | Read configuration from this file instead of `gamma.toml`. |
| `--no-config` |  | Ignore the configuration file entirely. |

**Running tests**

| Option | Value | What it does |
| --- | --- | --- |
| `--show-build` |  | Let cargo's own build output through, instead of only its progress bar. |
| `-j`, `--jobs` | `<N>` | How many mutants to test at once. Defaults to one more than the available parallelism. |
| `--test-timeout-multiplier` | `<FACTOR>` | Multiple of each test binary's baseline duration that a mutant is allowed. |
| `--minimum-test-timeout` | `<SECONDS>` | Lower bound on a test binary's timeout, however fast the baseline was. |
| `--cargo-test-arg` | `<ARG>` | Pass an argument through to every test binary. |
| `--test-package` | `<NAME>` | Run the tests of these packages when deciding a verdict. |
| `--include-test` | `<GLOB>` | Only let these test targets decide a verdict. |
| `--exclude-test` | `<GLOB>` | Do not let these test targets decide a verdict. |
| `--nextest` |  | Run test binaries through `cargo nextest` for per-test process isolation. |
| `--test-workspace` |  | Let every workspace package's tests decide a verdict. |
| `--whole-test-binaries` |  | Run every selected test in each reachable test binary. |

**Memory**

| Option | Value | What it does |
| --- | --- | --- |
| `--memory` | `<MODE>` | How much memory control to place around each test binary. `enforce` by default. |
| `--memory-multiplier` | `<FACTOR>` | Multiple of a test binary's baseline peak memory a mutant of it may reach. |
| `--memory-headroom` | `<SIZE>` | Absolute headroom added to a test binary's baseline peak memory. |
| `--memory-limit` | `<SIZE>` | An explicit memory ceiling for every test binary, instead of one derived from the baseline. |
| `--baseline-memory-limit` | `<SIZE>` | A memory ceiling for the baseline runs themselves. |
| `--no-relaunch` |  | Do not re-run inside a systemd scope to obtain the cgroup memory control needs. |

**Building**

| Option | Value | What it does |
| --- | --- | --- |
| `--profile` | `<NAME>` | Which Cargo profile to build with. |
| `-C`, `--cargo-arg` | `<ARG>` | Pass an argument through to every cargo invocation. |
| `--build-timeout` | `<SECONDS>` | Seconds the build may take before the run is abandoned. |
| `--build-timeout-multiplier` | `<FACTOR>` | Multiple of the first successful build's duration that a later build round is allowed. |
| `--rollback-rounds` | `<ROUNDS>` | How many times the tree may be rebuilt while withdrawing mutants that do not compile. Defaults to `256`. |

**Cache**

| Option | Value | What it does |
| --- | --- | --- |
| `--cache-dir` | `<PATH>` | Put cargo-gamma's reusable workspace and Cargo artifacts in this directory. |
| `--copy-ignored` |  | Copy files version control ignores into the cached workspace as well. |

**Arguments**

| Option | Value | What it does |
| --- | --- | --- |
| `<TEST_ARGS>` | `<TEST_ARGS>` | Arguments passed to every test binary, after `--`. |

**Reporting**

| Option | Value | What it does |
| --- | --- | --- |
| `--artifact-dir` | `<PATH>` | Write all user-facing artifacts to this directory. |
| `--show-killed` |  | List the mutants the suite killed, not just the ones that survived. |
| `--show-unviable` |  | List every mutant that could not be compiled, not just how many there were. |
| `--html-external` |  | Load the report viewer from a CDN instead of embedding it. |
| `--sarif-level` | `<LEVEL>` | How loudly a survivor is reported to a SARIF consumer. Defaults to `note`. |
| `--annotations` | `<WHEN>` | Annotate the diff and write a job summary when running inside a CI system. Defaults to `auto`. |
| `--diag-names` | `<POLICY>` | What to do with package and binary names in the diagnostics bundle. Defaults to `hashed`. |

**Run control**

| Option | Value | What it does |
| --- | --- | --- |
| `--min-score` | `<PERCENT>` | Fail the run if the assertion-killed mutation score is below this percentage. |
| `--incremental` | `<INCREMENTAL>` | How an incremental run reuses the last run: `no` starts cold; `build` reuses compiler unviability and checked execution hints. |
| `--leak-dirs` |  | Keep an incomplete scratch workspace after errors so it can be inspected. |
| `--no-baseline` |  | Skip the baseline run. |
| `--no-confirm` |  | Believe a failing test without re-running it with no mutant active. |
| `--dry-run` |  | Find and report mutants without building or running anything. |
| `--estimate` |  | Project what the rest of the run will cost, once the build and baseline have been measured. |
| `--no-stall-detection` |  | Wait out the whole budget for every mutant instead of cutting off one that has stopped making progress. |

### `gamma list`

List what would be done, without doing it

```text
cargo gamma list [OPTIONS] [WHAT]
```

**Arguments**

| Option | Value | What it does |
| --- | --- | --- |
| `<WHAT>` | `<WHAT>` | What to list. Defaults to `mutants`. |

**Selecting what to mutate**

| Option | Value | What it does |
| --- | --- | --- |
| `-d`, `--dir` | `<PATH>` | Path to the workspace or package to analyze. Defaults to `.`. |
| `--mutators` | `<SELECTORS>` | Mutators to apply, as a comma-separated selector list. |
| `--file` | `<GLOB>` | Only mutate files matching these glob patterns. |
| `--exclude-file` | `<GLOB>` | Skip files matching these glob patterns. |
| `--shard-count` | `<COUNT>` | Number of shards to divide the mutants into. |
| `--shard-index` | `<INDEX>` | Which shard to run, from 0. |
| `-D`, `--in-diff` | `<PATH>` | Only mutate lines added or changed by this unified diff, or `-` for standard input. |
| `-p`, `--package` | `<NAME>` | Only mutate these packages. Defaults to Cargo's package selection for the current directory. |
| `--workspace` |  | Mutate every package in the workspace. |
| `--error` | `<EXPR>` | Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`. |

**Cargo features**

| Option | Value | What it does |
| --- | --- | --- |
| `--features` | `<FEATURES>` | Cargo features to activate, comma-separated or repeated. |
| `--all-features` |  | Activate every feature of every selected package. |
| `--no-default-features` |  | Do not activate the `default` feature. |

**Configuration**

| Option | Value | What it does |
| --- | --- | --- |
| `--config` | `<PATH>` | Read configuration from this file instead of `gamma.toml`. |
| `--no-config` |  | Ignore the configuration file entirely. |

**Reporting**

| Option | Value | What it does |
| --- | --- | --- |
| `--json` |  | Emit machine-readable JSON instead of text. |
| `--json-report` | `<PATH>` | Write the population as a report document, for `merge` to withdraw retired mutants against. |

### `gamma explain`

Explain a mutator, a mutant, or a suppression

```text
cargo gamma explain <SUBJECT>
```

**Arguments**

| Option | Value | What it does |
| --- | --- | --- |
| `<SUBJECT>` | `<SUBJECT>` | A mutator name, family, preset, or mutant id. |

### `gamma suppress`

Write suppressions into the source for mutants that cannot usefully be tested

```text
cargo gamma suppress [OPTIONS] [-- <TEST_ARGS>...]
```

**Selecting what to mutate**

| Option | Value | What it does |
| --- | --- | --- |
| `-d`, `--dir` | `<PATH>` | Path to the workspace or package to analyze. Defaults to `.`. |
| `--mutators` | `<SELECTORS>` | Mutators to apply, as a comma-separated selector list. |
| `--file` | `<GLOB>` | Only mutate files matching these glob patterns. |
| `--exclude-file` | `<GLOB>` | Skip files matching these glob patterns. |
| `--shard-count` | `<COUNT>` | Number of shards to divide the mutants into. |
| `--shard-index` | `<INDEX>` | Which shard to run, from 0. |
| `-D`, `--in-diff` | `<PATH>` | Only mutate lines added or changed by this unified diff, or `-` for standard input. |
| `-p`, `--package` | `<NAME>` | Only mutate these packages. Defaults to Cargo's package selection for the current directory. |
| `--workspace` |  | Mutate every package in the workspace. |
| `--error` | `<EXPR>` | Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`. |

**Cargo features**

| Option | Value | What it does |
| --- | --- | --- |
| `--features` | `<FEATURES>` | Cargo features to activate, comma-separated or repeated. |
| `--all-features` |  | Activate every feature of every selected package. |
| `--no-default-features` |  | Do not activate the `default` feature. |

**Configuration**

| Option | Value | What it does |
| --- | --- | --- |
| `--config` | `<PATH>` | Read configuration from this file instead of `gamma.toml`. |
| `--no-config` |  | Ignore the configuration file entirely. |

**Running tests**

| Option | Value | What it does |
| --- | --- | --- |
| `--show-build` |  | Let cargo's own build output through, instead of only its progress bar. |
| `-j`, `--jobs` | `<N>` | How many mutants to test at once. Defaults to one more than the available parallelism. |
| `--test-timeout-multiplier` | `<FACTOR>` | Multiple of each test binary's baseline duration that a mutant is allowed. |
| `--minimum-test-timeout` | `<SECONDS>` | Lower bound on a test binary's timeout, however fast the baseline was. |
| `--cargo-test-arg` | `<ARG>` | Pass an argument through to every test binary. |
| `--test-package` | `<NAME>` | Run the tests of these packages when deciding a verdict. |
| `--include-test` | `<GLOB>` | Only let these test targets decide a verdict. |
| `--exclude-test` | `<GLOB>` | Do not let these test targets decide a verdict. |
| `--nextest` |  | Run test binaries through `cargo nextest` for per-test process isolation. |
| `--test-workspace` |  | Let every workspace package's tests decide a verdict. |
| `--whole-test-binaries` |  | Run every selected test in each reachable test binary. |

**Memory**

| Option | Value | What it does |
| --- | --- | --- |
| `--memory` | `<MODE>` | How much memory control to place around each test binary. `enforce` by default. |
| `--memory-multiplier` | `<FACTOR>` | Multiple of a test binary's baseline peak memory a mutant of it may reach. |
| `--memory-headroom` | `<SIZE>` | Absolute headroom added to a test binary's baseline peak memory. |
| `--memory-limit` | `<SIZE>` | An explicit memory ceiling for every test binary, instead of one derived from the baseline. |
| `--baseline-memory-limit` | `<SIZE>` | A memory ceiling for the baseline runs themselves. |
| `--no-relaunch` |  | Do not re-run inside a systemd scope to obtain the cgroup memory control needs. |

**Building**

| Option | Value | What it does |
| --- | --- | --- |
| `--profile` | `<NAME>` | Which Cargo profile to build with. |
| `-C`, `--cargo-arg` | `<ARG>` | Pass an argument through to every cargo invocation. |
| `--build-timeout` | `<SECONDS>` | Seconds the build may take before the run is abandoned. |
| `--build-timeout-multiplier` | `<FACTOR>` | Multiple of the first successful build's duration that a later build round is allowed. |
| `--rollback-rounds` | `<ROUNDS>` | How many times the tree may be rebuilt while withdrawing mutants that do not compile. Defaults to `256`. |

**Cache**

| Option | Value | What it does |
| --- | --- | --- |
| `--cache-dir` | `<PATH>` | Put cargo-gamma's reusable workspace and Cargo artifacts in this directory. |
| `--copy-ignored` |  | Copy files version control ignores into the cached workspace as well. |

**Arguments**

| Option | Value | What it does |
| --- | --- | --- |
| `<TEST_ARGS>` | `<TEST_ARGS>` | Arguments passed to every test binary, after `--`. |

**Reporting**

| Option | Value | What it does |
| --- | --- | --- |
| `--artifact-dir` | `<PATH>` | Write all user-facing artifacts to this directory. |
| `--show-killed` |  | List the mutants the suite killed, not just the ones that survived. |
| `--show-unviable` |  | List every mutant that could not be compiled, not just how many there were. |
| `--html-external` |  | Load the report viewer from a CDN instead of embedding it. |
| `--sarif-level` | `<LEVEL>` | How loudly a survivor is reported to a SARIF consumer. Defaults to `note`. |
| `--annotations` | `<WHEN>` | Annotate the diff and write a job summary when running inside a CI system. Defaults to `auto`. |
| `--diag-names` | `<POLICY>` | What to do with package and binary names in the diagnostics bundle. Defaults to `hashed`. |

**Run control**

| Option | Value | What it does |
| --- | --- | --- |
| `--min-score` | `<PERCENT>` | Fail the run if the assertion-killed mutation score is below this percentage. |
| `--incremental` | `<INCREMENTAL>` | How an incremental run reuses the last run: `no` starts cold; `build` reuses compiler unviability and checked execution hints. |
| `--leak-dirs` |  | Keep an incomplete scratch workspace after errors so it can be inspected. |
| `--no-baseline` |  | Skip the baseline run. |
| `--no-confirm` |  | Believe a failing test without re-running it with no mutant active. |
| `--dry-run` |  | Find and report mutants without building or running anything. |
| `--estimate` |  | Project what the rest of the run will cost, once the build and baseline have been measured. |
| `--no-stall-detection` |  | Wait out the whole budget for every mutant instead of cutting off one that has stopped making progress. |

**Suppressing**

| Option | Value | What it does |
| --- | --- | --- |
| `--dry-run-suppress` |  | Print the diff without changing anything. |
| `--eligible` | `<LIST>` | Which verdicts may be suppressed. Defaults to `timeout,outofmem`. |
| `--allow-dirty` |  | Edit source files that have uncommitted changes. |

### `gamma unsuppress`

Remove skip directives that no longer suppress anything

```text
cargo gamma unsuppress [OPTIONS]
```

**Selecting what to mutate**

| Option | Value | What it does |
| --- | --- | --- |
| `-d`, `--dir` | `<PATH>` | Path to the workspace or package to analyze. Defaults to `.`. |
| `--mutators` | `<SELECTORS>` | Mutators to apply, as a comma-separated selector list. |
| `--file` | `<GLOB>` | Only mutate files matching these glob patterns. |
| `--exclude-file` | `<GLOB>` | Skip files matching these glob patterns. |
| `--shard-count` | `<COUNT>` | Number of shards to divide the mutants into. |
| `--shard-index` | `<INDEX>` | Which shard to run, from 0. |
| `-D`, `--in-diff` | `<PATH>` | Only mutate lines added or changed by this unified diff, or `-` for standard input. |
| `-p`, `--package` | `<NAME>` | Only mutate these packages. Defaults to Cargo's package selection for the current directory. |
| `--workspace` |  | Mutate every package in the workspace. |
| `--error` | `<EXPR>` | Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`. |

**Cargo features**

| Option | Value | What it does |
| --- | --- | --- |
| `--features` | `<FEATURES>` | Cargo features to activate, comma-separated or repeated. |
| `--all-features` |  | Activate every feature of every selected package. |
| `--no-default-features` |  | Do not activate the `default` feature. |

**Configuration**

| Option | Value | What it does |
| --- | --- | --- |
| `--config` | `<PATH>` | Read configuration from this file instead of `gamma.toml`. |
| `--no-config` |  | Ignore the configuration file entirely. |

**Suppressing**

| Option | Value | What it does |
| --- | --- | --- |
| `--apply` |  | Remove the directives instead of printing what would be removed. |
| `--allow-dirty` |  | Remove directives from source files that have uncommitted changes. |

### `gamma merge`

Combine per-shard reports into one answer

```text
cargo gamma merge [OPTIONS] <REPORTS>...
```

**Arguments**

| Option | Value | What it does |
| --- | --- | --- |
| `<INPUTS>` | `<REPORTS>` | The reports to merge. A directory is read for its `*.json` files. |

**Reporting**

| Option | Value | What it does |
| --- | --- | --- |
| `--json-report` | `<PATH>` | Write the merged `mutation-testing-elements` document here. |
| `--html-report` | `<PATH>` | Write a self-contained merged HTML report here. |

**Merging**

| Option | Value | What it does |
| --- | --- | --- |
| `--window` | `<DAYS>` | Days after which a verdict is reported as stale. Zero disables the freshness window. Defaults to `30`. |

**Run control**

| Option | Value | What it does |
| --- | --- | --- |
| `--min-score` | `<PERCENT>` | Fail if the merged assertion-killed score is below this percentage. |

### `gamma hints`

Promote what a run learned about speed into a file the workspace can check in

```text
cargo gamma hints [OPTIONS]
```

**Selecting what to mutate**

| Option | Value | What it does |
| --- | --- | --- |
| `-d`, `--dir` | `<PATH>` | Path to the workspace or package to analyze. Defaults to `.`. |
| `--mutators` | `<SELECTORS>` | Mutators to apply, as a comma-separated selector list. |
| `--file` | `<GLOB>` | Only mutate files matching these glob patterns. |
| `--exclude-file` | `<GLOB>` | Skip files matching these glob patterns. |
| `--shard-count` | `<COUNT>` | Number of shards to divide the mutants into. |
| `--shard-index` | `<INDEX>` | Which shard to run, from 0. |
| `-D`, `--in-diff` | `<PATH>` | Only mutate lines added or changed by this unified diff, or `-` for standard input. |
| `-p`, `--package` | `<NAME>` | Only mutate these packages. Defaults to Cargo's package selection for the current directory. |
| `--workspace` |  | Mutate every package in the workspace. |
| `--error` | `<EXPR>` | Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`. |

**Cargo features**

| Option | Value | What it does |
| --- | --- | --- |
| `--features` | `<FEATURES>` | Cargo features to activate, comma-separated or repeated. |
| `--all-features` |  | Activate every feature of every selected package. |
| `--no-default-features` |  | Do not activate the `default` feature. |

**Configuration**

| Option | Value | What it does |
| --- | --- | --- |
| `--config` | `<PATH>` | Read configuration from this file instead of `gamma.toml`. |
| `--no-config` |  | Ignore the configuration file entirely. |

**Cache**

| Option | Value | What it does |
| --- | --- | --- |
| `--cache-dir` | `<PATH>` | Read the run record from this cache directory instead of cargo-gamma's default. |

**Run control**

| Option | Value | What it does |
| --- | --- | --- |
| `--dry-run` |  | Report what would be promoted without writing anything. |

### `gamma clean`

Delete cargo-gamma's cached data for a workspace

```text
cargo gamma clean [OPTIONS]
```

**Options**

| Option | Value | What it does |
| --- | --- | --- |
| `-d`, `--dir` | `<PATH>` | Path to the workspace or package whose cache should be deleted. Defaults to `.`. |

### `gamma completions`

Print a shell completion script

```text
cargo gamma completions <SHELL>
```

**Arguments**

| Option | Value | What it does |
| --- | --- | --- |
| `<SHELL>` | `<SHELL>` | The shell to generate a completion script for. |

<!-- end generated -->
