# cargo-anvil contributor instructions

## Documentation

- Keep reader documentation focused on supported behavior, configuration,
  failure recovery, and limitations. Every paragraph must help a reader use
  the tool or understand its contract.
- Put contributor and agent instructions here, not in design prose or rustdoc.
  Keep user setup and configuration instructions in the reader documentation.
- Do not duplicate source algorithms, API declarations, emitted workflows, or
  recipe bodies in prose. Use small examples only when they explain a choice
  the reader must make.
- Document the current contract, not implementation history, speculative
  features, or compatibility promises inferred from incidental code paths.
  Preserve real refusal and data-preservation guarantees.
- Distinguish the generated recipes' Rust/Cargo support floor (1.95) from
  this crate's build MSRV in the workspace manifest. Do not imply that a
  documented support limit is enforced unless the code enforces it.
- Keep `src/lib.rs` as the crate README source and regenerate with
  `just anvil-readme --fix`. Do not hand-edit `README.md`.

## Source ownership

- Edit canonical templates under `templates/`, not their generated copies.
  Follow the root version-bump policy for catalog changes, then regenerate
  repository artifacts, snapshots, and `.anvil.lock` together.
- Keep the root-MSRV scanners in `versions.just` and `tools.just` aligned.
  They run before Cargo metadata is available and must agree on accepted
  syntax and workspace-versus-package precedence.
- Keep check scope and tier routing consistent across local, GitHub, and ADO
  entry points. Establish impact mode before Just evaluates dependencies.
- Update PR-title patterns and allowed types at their shared source so
  validation and corrective diagnostics continue to agree.
- Update backend publishers explicitly when adding advisory checks; a new
  report file alone does not publish a PR comment.
- Review Miri exclusions at the narrowest applicable scope. Revalidate
  nightly-dependent protocols when changing pins.

## Validation

- Use the generated Anvil recipes as required by the root instructions.
  Select focused regressions for the changed contract; do not treat snapshot
  acceptance as proof that an emitted script executes correctly.
- Integration targets declared with `required-features = ["test-util"]`
  need that feature enabled. It is internal test support, not a downstream API.
- Inspect validator availability when using `tests/schemas.rs`; missing
  executables skip checks. Report skipped checks rather than claiming coverage.
- For a Miri nightly update, validate the compile/run protocol with executable
  contracts, including package working directories, environment restoration,
  filters, and aggregate failures.
- For backend changes, distinguish local rendering/schema evidence from a
  successful current-commit CI run. ADO compliance composition requires the
  target environment; GitHub dogfooding cannot establish it.
- `scripts/test-anvil-dogfood.ps1 -GitHubToken` requires an explicitly supplied
  host `GITHUB_TOKEN`. Without the switch its container checks are
  unauthenticated. Do not obtain host GitHub CLI credentials for the script.
