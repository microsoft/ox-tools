# Layer 5: external compiler artifacts with preserved Git context

[Stack root](README.md) | [Previous layer](04-library-targets.md) |
[Next layer](06-mutant-replay.md)

## Outcome and supported arrangement

Use an external Cargo target directory for heavy reusable artifacts while
preserving the selected source checkout's build-time Git context. This applies
to an ordinary checkout and a linked worktree.

Use the existing default split layout, configured through `CARGO_TARGET_DIR` or
Cargo's `build.target-dir`. Do not add another placement flag. Explicit Gamma
`--cache-dir` retains its all-in-one meaning and its refusal when relocation
would lose required metadata.

The supported Git use is read-only build-time metadata acquisition: revision,
branch/tag description, selected worktree root, and dirty state. The latter must
describe the source checkout, not Gamma's instrumented copy. This is not a
sandbox against build scripts deliberately writing to repositories, and it does
not promise arbitrary Git mutations.

## Entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-lib\src\discover\survey.rs` | Cargo metadata's resolved target directory |
| `cargo-gamma-lib\src\exec\workspace.rs` | `gamma_base`, `campaign_base`, preparation, `cargo`, VCS exposure, state locator and cleanup |
| `cargo-gamma-lib\src\exec\copy.rs`, `sync.rs`, `manifest.rs` | Source synchronization, configuration/dependency anchoring |
| `cargo-gamma-lib\src\discover\record.rs`, `workspace_snapshot.rs` | Build context and conservative reuse |
| `cargo-gamma-lib\src\commands\clean.rs`, `hints.rs`, `suppress.rs` | Owned state lookup and postprocessing |
| `cargo-gamma-lib\tests\session.rs` | Real-Cargo end-to-end fixture infrastructure |

The existing `.git` exposure test verifies a pointer's text. A pointer alone is
not the proof needed here: a query may find the correct objects but infer the
wrong working tree or refresh the source repository's index.

## Implementation sequence

### Establish executable contract fixtures first

Create owned temporary repositories using local Git commands, with fixed author
metadata, a tracked file, a tag, and a small Cargo package whose build script
queries Git. Do not use the developer's actual checkout as a fixture.

Compare query results from the selected source directory with values embedded
by the scratch build. Exercise an ordinary checkout and a linked worktree whose
selected branch differs from the main checkout. Include clean and dirty sources,
relative `.git` pointers, and path spellings with spaces.

Snapshot source files, refs, and index bytes before the campaign and compare them
afterward. Disable optional Git lock/index refreshes in the fixture's read-only
queries. The fixture must fail if source metadata is accidentally changed.

Set an external target location and assert actual artifact placement, not only
the value of a configuration variable. Run again and verify reuse through
observable Cargo artifacts/build-script state rather than elapsed-time claims.

### Retain the split-cache design

Keep source/runtime/lock placement separate from the campaign artifact base.
Ensure an explicit Cargo target setting reaches metadata, Gamma's derived
campaign base, and every Cargo invocation consistently.

Keep ownership markers and stable lock-domain derivation. A target change must
not let two commands manipulate the same source workspace without the same lock.
Do not adopt an unrelated nonempty directory or follow an unsafe redirected
cache path.

Retain the `campaign-location` publication order: completed record first,
validated locator second. `hints` and `suppress` must find the latest completed
campaign without guessing that all reusable state lives under the source
checkout's `target`.

### Correct Git query context only where required

If the existing supported arrangement already passes the fixtures, retain it.
Otherwise derive the source repository's actual Git directory, common directory,
and worktree root with read-only Git queries, and associate that identity with
the synchronized workspace.

Do not rely on a global or command-scope `core.worktree` overlay to change
repository setup through a scratch gitfile. Reading the desired config value
does not establish that Git used it to resolve the working tree. The proof must
query `--show-toplevel` and actual dirty state, not just `git config --get`.

Before implementation is committed to a mechanism, prototype a repository-aware
Git command proxy in owned scratch state:

1. Resolve the real Git executable before altering the Cargo child's executable
   search path. Retain an absolute executable path to prevent proxy recursion.
2. Route ordinary Git CLI queries from the Cargo build through that proxy.
   Resolve the command's effective repository using real Git and its global
   repository-selection arguments, not merely the process's directory prefix.
3. For a query against the selected scratch repository, launch real Git with
   source Git-directory/worktree values on that invocation only and map the
   working directory to its corresponding source location. Relative pathspecs
   and `--show-prefix` must retain their intended meaning.
4. Forward commands for unrelated dependency repositories without the source
   override. Do not place a global `GIT_WORK_TREE`/`GIT_DIR` override on the Cargo
   process, where it would affect those repositories too.
5. Preserve caller configuration and argument boundaries. Treat conflicting
   explicit Git-directory, working-tree, common-directory, and index overrides
   as explicit compatibility decisions, not settings to discard.
6. Use `GIT_OPTIONAL_LOCKS=0` for the read-only child invocation to suppress
   optional index refreshes. This is not a sandbox or protection against a
   deliberately writing command.

This is a mechanism feasibility checkpoint, not an assertion that a command
proxy transparently supports every Git consumer. Test Git global-option forms,
relative `-C`, aliases, nested Git invocations, executable lookup, and dependency
repositories. Absolute executable invocation and in-process Git libraries can
bypass such a proxy. If the motivating build-time metadata path uses either,
this mechanism alone does not satisfy the requirement.

The checkpoint must establish a supported arrangement for the actual metadata
consumer before broader cache integration proceeds. If satisfying it requires
new restrictions on supported consumers, copying Git internals, changing source
metadata, or globally redirecting dependency queries, seek approval for that
specific contract change. Do not quietly claim general Git compatibility.

Keep proxy/configuration files in owned scratch state. Never edit the source
`.git`, common config, linked-worktree metadata, or index to install the
arrangement. Probe path/case behavior rather than inferring it from the OS.

If a safe scoped arrangement cannot be established for a requested topology,
stop with an actionable unsupported-arrangement diagnostic. Do not remove the
VCS refusal or fall back to silently different build inputs.

### Preserve cache and dependency semantics

Feed effective build-context changes through existing invalidation logic.
Verify the fixture's declared Git input dependencies actually refresh embedded
metadata after source revision/tag/dirty-state changes. Do not promise to repair
arbitrary build scripts that misdeclare Cargo dependencies.

Keep the existing conservative refusal to reuse compiler-unviability claims when
build-script inputs are not fully known. Warm Cargo artifacts and reusable
mutation verdicts are different things; this layer introduces no test-verdict
reuse.

Verify external artifact cleanup uses workspace ownership, never a broad deletion
of the shared Cargo target root. Layer 7's additional source/artifact generations
must fit this ownership model.

## Acceptance matrix

| Scenario | Required observation |
| --- | --- |
| Normal checkout, external `CARGO_TARGET_DIR` | Artifacts external; build-time Git values match source. |
| Cargo-configured `build.target-dir` | Same supported behavior and consistent state lookup. |
| Linked worktree on another branch | Worktree revision, branch, root, and dirty state are correct. |
| Relative Git pointer; spaces/pattern characters in paths | Correct scoping or explicit unsupported diagnostic. |
| Dirty tracked source and staged/unstaged changes | Metadata reflects the source state; index and refs remain unchanged. |
| Independent local dependency repository | Its queries are not redirected to the workspace repository. |
| Relative Git arguments and nested invocation | Correct source query meaning without overrides leaking to other repositories. |
| Actual metadata consumer bypasses executable proxying | Feasibility checkpoint blocks completion; no unsupported compatibility claim. |
| Inherited Git configuration/overrides | Preserved or explicitly diagnosed, never silently overwritten. |
| Repeated run and declared Git-input changes | Warm reuse works without stale embedded metadata. |
| Hints/suppression after target relocation | Latest completed record is resolved through validated state. |
| Unsupported all-in-one external `--cache-dir` | Existing safety refusal remains, with the artifact-only alternative named. |
| Cleanup | Only this workspace's owned state is removed. |

Exercise Windows and Linux. Use temporary repository fixtures and command-local
environment values; do not mutate the test process's global environment.

## Documentation and completion

Document the artifact-only configuration and distinguish published artifacts,
scratch sources, and reusable compiler state. Update the `--cache-dir` refusal
to recommend the supported alternative.

Complete only after real Git-aware Cargo builds demonstrate context equivalence
for both checkout forms. Passing a pointer-format test or successfully compiling
a fixture that never invokes Git is insufficient.
