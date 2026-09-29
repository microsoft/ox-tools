# Layer 5: external compiler artifacts with unchanged source handling

[Stack root](README.md) | [Previous layer](04-library-targets.md) |
[Next layer](06-mutant-replay.md)

## Outcome and scope

Place heavy reusable Cargo artifacts outside the checkout without changing
Gamma's existing synchronized-source or Git-metadata arrangement. Use the
existing split layout through `CARGO_TARGET_DIR` or Cargo's `build.target-dir`.
Do not add another placement flag.

The comparison is between equivalent Gamma campaigns with local and external
artifact directories, not between the instrumented scratch tree and an ordinary
unmodified source-checkout build. Changing artifact placement must not introduce
different source/Git handling.

Explicit Gamma `--cache-dir` retains its all-in-one meaning and its safety refusal
when relocation would hide metadata. Document the artifact-only alternative
rather than weakening that refusal.

Git proxying, executable interception, Git environment redirection, copying Git
internals, and changing source-checkout metadata are outside this layer. Do not
promise that scratch-root or scratch-dirty-state queries describe the original
checkout, and do not impose byte-identical Git-index preservation on commands
whose normal behavior includes index maintenance. Those are separate concerns,
not artifact-placement requirements.

## Entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-lib\src\discover\survey.rs` | Cargo metadata's resolved target directory |
| `cargo-gamma-lib\src\exec\workspace.rs` | Split paths, Cargo invocation, existing VCS exposure, locator, ownership and cleanup |
| `cargo-gamma-lib\src\exec\copy.rs`, `sync.rs` | Existing synchronized-source behavior |
| `cargo-gamma-lib\src\discover\record.rs` | Conservative build knowledge and campaign state |
| `cargo-gamma-lib\src\commands\clean.rs`, `hints.rs`, `suppress.rs` | Owned state lookup and postprocessing |
| `cargo-gamma\tests\external_artifacts.rs` | Owned executable fixtures exercising the production split layout rather than the library test harness's private all-in-one cache |

## Implementation sequence

### Prove artifact placement independently of source handling

Create owned temporary ordinary and linked-worktree repositories with small
Cargo packages. Use a real build script to read Git revision and branch/tag
metadata so the test proves visibility, not merely `.git` pointer formatting.

Run equivalent Gamma campaigns against the same selected checkout with local
and external Cargo target directories. Assert:

- Compiled artifacts actually reside under the selected external target root.
- The synchronized-source/Git arrangement is unchanged by target relocation.
- Build-script metadata agrees between the equivalent Gamma runs, including
  selecting a linked worktree whose branch differs from the main checkout.
- Source files are not instrumented in place and repository refs/staged entries
  are not deliberately modified by Gamma.

Use queries that exercise the supported metadata path without claiming broad
Git virtualization. If root/dirty-state queries are included as additional
regressions, compare the two Gamma arrangements; do not require either to look
like an uninstrumented checkout.

Exercise both environment and Cargo-configuration target-directory settings,
paths with spaces, and relative `.git` pointers. Use command-local environment
values rather than changing the test process's global environment.

### Reuse the existing split layout

Follow Cargo metadata's resolved target directory into `campaign_base`, the
artifact `target` subdirectory, and every Cargo build command. Keep
`gamma_base` source/runtime placement and the stable workspace lock independent
of that artifact setting.

If the existing route passes, keep production changes limited to documentation
and the actionable cache-relocation diagnostic. Fix only concrete wiring gaps
shown by the fixtures; do not redesign source copying or VCS exposure.

Retain workspace ownership checks. A target change must not create a second
lock domain for shared mutable source state or adopt unrelated nonempty data.

### Verify reuse and state-consuming commands

Run an unchanged campaign again and establish warm artifact reuse from
observable Cargo/build-script evidence, not elapsed-time thresholds.
Keep existing conservative compiler-unviability reuse rules, particularly
around build scripts with undeclared inputs. Do not reuse test verdicts.

Verify completed record publication precedes the validated `campaign-location`
locator, and `hints`/`suppress` resolve the completed campaign after relocation.
Published report placement remains a separate `--artifact-dir` concern.

Check cleanup against the exact owned campaign directory. Never delete the
shared Cargo target root or unrelated artifacts simply because the target
location is external.

### Document the supported route

Explain `CARGO_TARGET_DIR` and `build.target-dir` as artifact-only controls,
distinguishing them from Gamma's all-in-one `--cache-dir`.
Update the latter's VCS-visibility refusal to name the supported alternative.
Do not describe Git transparency beyond the existing scratch-build contract.

## Acceptance matrix

| Scenario | Required observation |
| --- | --- |
| Ordinary checkout, external `CARGO_TARGET_DIR` | Artifacts are external; source/Git handling matches the local-target Gamma run. |
| Cargo `build.target-dir` | Same behavior and consistent campaign-state lookup. |
| Linked worktree on another branch | Revision/branch metadata identifies the same selected worktree in both arrangements. |
| Relative Git pointer and paths with spaces | Existing metadata remains visible after artifact relocation. |
| Repeated unchanged run | Warm artifacts remain reusable; no timing-based success claim. |
| Hints/suppression after relocation | Latest completed campaign is found through validated state. |
| Explicit all-in-one `--cache-dir` hides metadata | Safety refusal remains and names the artifact-only alternative. |
| Cleanup beside unrelated target contents | Only this workspace's owned state is eligible for removal. |
| Root/dirty-state behavior, if exercised | No new placement-induced difference or promise of original-checkout virtualization. |

Run relevant platform fixtures on Windows and Linux where supported. Preserve
the remaining layers' contracts, especially library target selection and the
separate source/artifact generations needed for const execution.

Complete when the supported external artifact route is demonstrated and
documented without changing or proxying Git behavior.
