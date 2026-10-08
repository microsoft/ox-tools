# Gamma real-toolchain system tests

## Purpose and boundary

These tests observe interfaces that scripted process results cannot prove:
whether an embedded Rust project actually compiles, whether Cargo builds it
without a registry cache, whether public proc-macros produce compiler-hosted
diagnostics and harness markers, and whether those markers reach the real
campaign scheduler.

They are a provisioned system-test boundary, not ordinary algorithm tests.
Command construction, configuration resolution, flag precedence, and
scheduler policy remain covered by in-process tests with controlled inputs.
Replacing compiler acceptance with a successful scripted runner would remove
the assertion these system tests own.

The boundary comprises:

- `cargo-gamma-lib/tests/offline_builds.rs`: library/binary acceptance, a
  local package graph, and configured plus inherited compiler flags.
- The existing real-toolchain cases in `cargo-gamma-lib/tests/cli.rs` and
  `tests/session.rs`: command/campaign acceptance, including the public
  function/module resource-marker-to-scheduler path.
- `cargo-gamma-attrs/tests/diagnostics.rs`: a valid consumer and the exact
  macro identity/reason fragments for rejected consumers.
- Existing Cargo metadata/build adapter acceptance cases in the
  coordinator's configuration, discovery, and execution test modules.
  This designation covers only cases whose subject is the real tool
  boundary; their in-process graph, parsing, and policy cases remain
  ordinary tests.

The generator's ordinary regressions remain in `tests/project.rs`. Their
subject is materialization itself: the temporary directory is the output
of `write_fixture` or `write_project`, and assertions inspect the written
bytes, configuration, or rejected paths. It is not unrelated input setup.

## Provisioning and execution

The system jobs require a native Cargo/rustc pair for the repository's
selected toolchain and its standard library. The parent test artifacts and
normal product dependencies must already be built or available in the
parent Cargo cache. Public macro checks bootstrap the actual product macro
offline and locked, select its library from Cargo's JSON artifact stream,
and copy it into the owned consumer project.

No service, developer credential, browser, or network access is a consumer
prerequisite. An empty registry cache is a guarantee for the generated
consumer, not for bootstrapping the whole product from an empty cache.
Unavailable compiler infrastructure fails the system check; it is not
accepted as a diagnostic rejection or silently replaced by a mock.

The new build-acceptance target requires the non-default `system-tests`
feature, which enables the unsupported `internals` test facade:

```powershell
cargo test --offline --locked -p cargo-gamma-lib --features system-tests --test offline_builds
```

Pure materialization/flag checks can be selected independently:

```powershell
cargo test --offline --locked -p cargo-gamma-lib --features internals --test project
```

The existing Anvil native tests/coverage and runtime-analysis jobs provision
the compiler and run all features. They therefore retain the real acceptance
cases after this target split. No ordinary production/default feature gains
test helpers or a compiler dependency.

## Immutable inputs and owned outputs

Canonical sample inputs are checked-in Rust source literals, manifests,
configuration, and embedded runtime assets. The materializer copies source
bytes unchanged; source-location assertions keep their exact line/column
inputs. Specialized cases retain explicit local package/feature graphs.

Each system case owns its generated workspace, Cargo home where tested,
target directory, copied macro library, and campaign outputs. A writable
workspace is necessary to observe Cargo builds and source mutation. Tests
never use the developer's repository as the subject workspace or modify
checked-in inputs. RAII retains the directories through all synchronous
child work and removes them afterward.

Consumers have no registry or repository path dependencies; local graphs
and the explicit local directory-source metadata fixture remain controlled
exceptions. Valid Cargo configurations retain `net.offline = true` after
updates. Deliberately malformed raw inputs are not repaired.

Compiler flags use Cargo's source precedence. Child-only environments retain
the effective analysis flags, select only the applicable target setting,
and append fixture or `--extern` arguments with intact boundaries. Tests
do not mutate process-global environment or working directory.

## Interpreter boundary

The new `offline_builds` cases carry individual reasoned Miri ignores for
their compiler subprocesses. `project` has no crate-wide exclusion: its
in-process flag/configuration/path invariants run under the interpreter.
The existing campaign and public-macro targets keep their established
native-only boundaries; this split does not convert those targets to
interpreter tests.

Filesystem-output checks are separate from compiler checks. A pinned Windows
Miri run confirmed that host temporary-directory setup is unsupported with
isolation enabled. Those output checks have individual explanatory ignores
and retain native coverage. Pure path validation, offline configuration
transforms, and scripted flag selection run under Miri without disabling
isolation.

Miri is not a substitute for loading a real proc-macro in rustc or observing
a Cargo build. Its in-process coverage and the native system acceptance
coverage are complementary.

## Runtime budget and failure semantics

After compilation/provisioning, the build probes target tens of seconds per
case; a small mutation/resource campaign targets at most two minutes. These
are operational budgets, not wall-clock correctness assertions or promises
about an arbitrary developer machine. CI job limits provide an outer safety
bound, and campaign subprocesses retain the tool's configured supervision.

Projects are intentionally tiny, graphs and worker counts are fixed, and
resource campaigns use one relational mutation with capacity one. Resource
readiness deadlines are safety bounds; exclusive ownership and the overlap
marker are the observations. Deterministic scheduler invariants continue to
have separate in-process coverage rather than treating one observed schedule
as exhaustive proof.

Spawn/build/artifact/copy failures remain explicit. Negative macro cases
require their expected public diagnostic fragments, and a positive consumer
prevents an unrelated compiler failure from being counted as correct
validation. Re-executed checks require exactly one passing child test; a
successful zero-test selection is a harness failure. Cold build and resource
exit/overlap assertions are not weakened by retries, skips, relaxed thresholds,
or success-shaped fallback behavior.
