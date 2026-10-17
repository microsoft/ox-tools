# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Correctness
- [COR1](#cor1) — Handle incompatible previous results before starting campaign work
- [COR2](#cor2) — Recover completed results without requiring unchanged execution artifacts
- [COR3](#cor3) — Separate invalid control-flow edits from guard-induced ownership conflicts
- [COR4](#cor4) — Preserve temporary lifetimes when guarding borrowed expressions
- [COR5](#cor5) — Preserve expression attributes and active configuration tails
- [COR6](#cor6) — Preserve expected types when mutations disrupt generic inference
- [COR7](#cor7) — Unify iterator types inside return containers and method replacements
- [COR8](#cor8) — Generate floating-point boundary adjustments with floating-point units
- [COR9](#cor9) — Bind mutation type evidence to lexical identities and call conventions
- [COR10](#cor10) — Validate reusable reference payloads against current bindings and lifetimes
- [COR11](#cor11) — Avoid introducing an unnecessary Debug bound in propagation mutations
- [COR12](#cor12) — Distinguish async call futures from their declared output types
- [COR13](#cor13) — Require causal evidence before blaming replacements inside broad compiler spans

### Performance
- [P3](#p3) — Amortize per-mutant process launch with a fork server
- [P4](#p4) — Extend exact negative Default evidence without excluding unknown types
- [P5](#p5) — Filter overloaded operator mutations using exact operand and result types
- [P6](#p6) — Resolve API identity before iterator and option rewrites
- [P7](#p7) — Prove cursor stalls through nested branches and lexical binding identities
- [P8](#p8) — Track owned collection length as loop progress
- [P9](#p9) — Prove stalled grouping boundaries over immutable collections
- [P10](#p10) — Fold typed arithmetic to prove scalar loop fixed points
- [P11](#p11) — Prove retry fixed points without mistaking cached calls for progress
- [P12](#p12) — Investigate retained-growth proofs for open-ended standard iterator loops
- [P13](#p13) — Prioritize function-local control-flow candidates for ownership failures
- [P14](#p14) — Amortize isolation searches across independent failures in one context
- [P15](#p15) — Investigate graph-equivalent target narrowing for isolation proofs

### Features
- [F3](#f3) — Native fork-server test harness as a `cargo test`/nextest replacement
- [F4](#f4) — Keep a running unviable-mutant counter visible throughout Excluding
- [F5](#f5) — Report hint coverage and separate probe costs from whole-suite fallback

## Correctness

<a id="cor1"></a>
### COR1 — Handle incompatible previous results before starting campaign work

**Area:** `cargo-gamma-lib::discover::record`, campaign startup/publication · **Priority:** High · **Effort:** Medium

A campaign can execute for days before completed-result publication discovers that an existing
`last-gamma-run.json` uses an unsupported format. The observed failure involved a version-10
record left in the campaign cache while the finishing executable expected version 12. Publication
reads the previous record to merge earlier evidence and refuses to overwrite the incompatible
file, leaving the new campaign's results unpublished despite completed testing. Preserving
unreadable evidence is correct; discovering this blocker only after expensive work is not.

- `crates/cargo-gamma-lib/src/discover/record.rs:1251` — `load_for_update` rejects a foreign record version.
- `crates/cargo-gamma-lib/src/discover/record.rs:1855` — `store_completed` propagates that refusal during publication.
- `crates/cargo-gamma-lib/src/commands/run.rs:813` — completed-result publication depends on this merge.

**Done when:** campaign startup checks previous-result compatibility before expensive work and
either safely archives incompatible evidence or refuses early with actionable instructions;
completed-result publication can recover from an incompatible predecessor without deleting
either generation's evidence. Regression coverage reproduces an old-format predecessor and
verifies that completed verdicts remain publishable without rerunning mutants.

**See also:** COR2 (publication recovery after testing has finished).

---

<a id="cor2"></a>
### COR2 — Recover completed results without requiring unchanged execution artifacts

**Area:** `cargo-gamma-lib::commands::run`, `cargo-gamma-lib::exec::testing_state` · **Priority:** High · **Effort:** Medium

Resume unconditionally validates prepared execution artifacts before publishing results, even
when the checkpoint is `FinishedTesting` and all verdicts are already settled. A modified
prepared workspace `Cargo.toml` blocks publication even with a checkpoint-compatible executable,
an intact snapshot and verdict journal, and no incompatible previous record (COR1). Files needed
only to execute tests therefore block publication of observations that no longer require
execution. This contradicts the documented ability to
recover interrupted completed-ledger publication without rerunning mutants.

- `crates/cargo-gamma-lib/src/commands/run.rs:1335` — `resumed_testing` unconditionally calls `validate_artifacts`.
- `crates/cargo-gamma-lib/src/exec/testing_state.rs:309` — checkpoint loading accepts `FinishedTesting`.
- `crates/cargo-gamma-lib/src/exec/testing_state.rs:419` — artifact validation requires the prepared files to match.
- `crates/cargo-gamma/docs/CMDLINE.md:99` — publication-only resume is documented.

**Done when:** a verified, fully settled checkpoint has a publication-only recovery path that
does not require execution artifacts or host execution capabilities. Snapshot integrity,
verdict checksums, campaign identity, and captured source provenance remain validated;
pending-test recovery still requires unchanged artifacts and the normal execution checks.
Regression coverage verifies publication after prepared artifacts change or disappear, without
launching tests or presenting current source bytes as the campaign's captured source.

**See also:** COR1 (an incompatible previous record can interrupt publication).

---

<a id="cor3"></a>
### COR3 — Separate invalid control-flow edits from guard-induced ownership conflicts

**Area:** `cargo-gamma-engine::schema`, collector control-flow analysis, build convergence · **Priority:** High · **Effort:** Large

The last campaign (`960645875da50c13`) has 134 unviable mutants in a control-flow/ownership
cohort: moved-value and conflicting-borrow errors, incompatible branch types, and three isolated
control-flow failures. These are not all evidence that the source replacement is invalid.
Guarding reinitialization and mandatory exits can make other, individually viable edits fail
in the combined schema. Examples include deleting `current_line.push_str(word)`, deleting
convergence progress notifications, and changing a literal in `spawn_patiently`. Conversely,
deleting `err.exit()` from a value-producing match really removes a required divergent exit.
Do not suppress every statement or condition in these functions based on a broad compiler span.

Mandatory `continue` exits need the same protection. In `LoopProof::proof_tokens`, deleting
a `continue;` from a value-producing match arm makes that arm return `()` instead of
diverging, conflicting with the other arms' `TokenTree` values. A compile-only reproduction
confirms this is an invalid deletion even without neighboring mutations. Screen such
source-visible type dependencies before generating the candidate, rather than paying for
convergence or suppressing every mutation inside the match.

- `crates/cargo-gamma-engine/src/schema.rs:502` — control-flow and statement guard encodings.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:942` — statement eligibility.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:967` — `continue` deletion eligibility.
- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:110` — a mandatory divergent match-arm exit.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3818` — local divergence protection.
- `crates/cargo-gamma-lib/src/exec/build.rs:655` — diagnostic blame precedes isolation.
- Campaign examples: `6abedf56eec9`, `9199a40f7b1b`, `e3a180f6840b`, `b7f7b1313cc4`.

**Done when:** source-visible mandatory initialization/exit dependencies identify edits that
are provably invalid, while guarded ownership interactions are distinguished from individual
unviability and handled without discarding independently viable mutants. Focused fixtures cover
move/reinitialize loops, partial moves, borrowed loop state, and divergent match arms, including
positive controls for ordinary deletions and mutations beside unrelated ownership dependencies.
Include value-producing match arms whose `continue`, `break`, or return supplies required
divergence, retaining deletions where the surrounding expression still has a valid type.

**See also:** COR13 (one invalid exit must not incriminate neighboring replacements).

---

<a id="cor4"></a>
### COR4 — Preserve temporary lifetimes when guarding borrowed expressions

**Area:** `cargo-gamma-engine::schema`, expression-site selection · **Priority:** High · **Effort:** Large

Nineteen campaign mutants belong to the temporary-lifetime cohort: 18 reported E0716 and one
isolated `Some` replacement borrows a temporary path buffer. Ordinary expression guards add
branch scopes that can drop an owned temporary before a returned reference is consumed. This
affects unrelated viable replacements: date slice bounds, `trim_end`/`trim_start`, `Some`/`None`,
and `?`/`unwrap` in temporary-backed method chains. Blacklisting these mutators or all borrowed
expressions would remove valid signal rather than fix the encoding.

- `crates/cargo-gamma-engine/src/schema.rs:469` — expression replacements introduce branch scopes.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3637` — existing reference/promotion protection.
- Campaign examples: `5f11dfa2e7d1` (date slicing), `8c1e59549dcf` (trim),
  `751cb2698bc1` (borrow from a temporary vector), `c5c737675625` (temporary path).

**Done when:** generation preserves the required evaluation and borrow extent for these shapes,
or rejects only sites proven impossible to encode safely. Regression fixtures compare
unmutated and activated behavior, evaluation count, and drop order, and retain otherwise
viable borrowed-expression mutations; no blanket lifetime-based exclusion is introduced.

---

<a id="cor5"></a>
### COR5 — Preserve expression attributes and active configuration tails

**Area:** `cargo-gamma-engine::ops::collect`, `cargo-gamma-engine::schema` · **Priority:** High · **Effort:** Medium

Eight campaign mutants expose attribute/configuration problems. Three boolean negations put
`#[cfg]` or `#[expect]` inside `!(...)`, producing E0658. Five more mutants in
`same_existing_path` report type mismatches although they change a boolean result or string
literal: the function has mutually exclusive platform-attributed tails. Skipping inactive
children is insufficient if parent wrapping relocates their attributes or changes which
expression supplies the active block's value. Keep viable mutations in attributed code.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1591` — boolean replacement copies the full attributed span.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:878` — whole-body replacement shape.
- `crates/cargo-gamma-engine/src/schema.rs:469` — attributed source copied into guard branches.
- Campaign examples: `498f5b6024c5`, `9a620201e616`, `0274bdca33ad`,
  `71158ba8e2e2`, `58506036ace0`.

**Done when:** attributes retain their original legal placement and configuration semantics,
and active tails keep their type after expression/body wrapping. Fixtures cover `cfg`,
`cfg_attr`, and lint attributes, including alternative Windows/Unix tails and viable mutations
within each active branch; inactive code must not acquire an active replacement.

---

<a id="cor6"></a>
### COR6 — Preserve expected types when mutations disrupt generic inference

**Area:** `cargo-gamma-engine::ops::collect`, replacement rendering · **Priority:** High · **Effort:** Medium

Fifteen campaign mutants lose inference that the original expression received from its
context. Ten increment/decrement replacements wrap `.sum()` with arithmetic; three
`call_result.default` replacements discard the result of generic `no_target_selection()`;
two `?`-to-`unwrap` replacements remove error-type constraints from `collect::<Result<_, _>>()`.
These are not grounds to exclude all generic calls or boundary mutations: their original
types can sometimes be established from a signature, annotation, or enclosing typed expression.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1685` — discarded call result loses its expected type.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:2049` — arithmetic perturbation wrapper.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:2510` — existing expected-type context.
- Campaign examples: `474157cc9965`, `7f2abc2e4aed`, `3b22d540bca4`,
  `035c77e1c78d`, `f79081365dc0`.

**Done when:** replacements preserve necessary generic constraints using only resolved,
source-visible types, and the affected viable mutations compile in regression fixtures.
Unknown generic arguments remain compiler-oracle cases rather than receiving guessed
annotations or a blanket exclusion.

---

<a id="cor7"></a>
### COR7 — Unify iterator types inside return containers and method replacements

**Area:** collector return shapes, parameter shadows, iterator guard encoding · **Priority:** High · **Effort:** Medium

Six campaign mutants need more than an ordinary same-type guard. Empty-iterator replacements
for two `Result<impl Iterator<...>>` functions use `Shape::Block`, whereas only a directly
returned iterator receives `IterBlock`. Parameter-shadow replacements in those functions also
produce incompatible guarded body types. Two character case conversions switch between
distinct iterator types (`to_uppercase` and `to_lowercase`). All are reasons to improve type
unification, not to drop viable iterator mutations indiscriminately.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:878` — iterator shape checks only the outer return type.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1607` — parameter-shadow generation.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1231` — same-type method rename path.
- `crates/cargo-gamma-engine/src/schema.rs:479` — existing `Either` guard encoding.
- Campaign examples: `b2ae8dcea46f`, `bb4e0c2e034f`, `aa0e8b9f731e`,
  `1d2c83017c34`, `530c4a1d6a59`, `99941354afa0`.

**Done when:** proven iterator-valued payloads and method results use compatible encodings,
and parameter shadows preserve the original opaque return contract. Fixtures cover nested
result/option payloads and character iterators with their required iterator traits; custom
same-named methods and unsupported opaque bounds are not assumed to be standard iterators.

---

<a id="cor8"></a>
### COR8 — Generate floating-point boundary adjustments with floating-point units

**Area:** collector numeric perturbations and range mutations · **Priority:** Medium · **Effort:** Small

Six campaign mutants add or subtract an integer `1` from a known float: two perturbations of
`f64::from(hits)` and four inclusive floating-point ranges. `perturb` recognizes numeric
associated calls but does not carry their float type into its unit spelling; range endpoint
adjustments always use an integer unit. Fix the replacement rather than removing floating-point
sites. Moving a float endpoint by one is also not generally equivalent to changing inclusivity,
so the intended range mutation semantics must be defined explicitly.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1741` — float detection during perturbation.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:2078` — existing floating-point unit spelling.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3975` — range endpoint arithmetic.
- Campaign examples: `c35f40af42b7`, `2bc9900dada8`, `01739c0bbc26`, `dd93a1576446`.

**Done when:** exact primitive float evidence selects a type-correct adjustment, and float
range mutations have documented, tested boundary semantics rather than inheriting the
integer-only equivalence claim. Integer mutations remain unchanged; overloaded or unresolved
endpoint types are not silently treated as primitive floats.

---

<a id="cor9"></a>
### COR9 — Bind mutation type evidence to lexical identities and call conventions

**Area:** collector indexes, closure/pattern bindings, expected call arguments · **Priority:** High · **Effort:** Medium

Nine campaign mutants expose lost binding or call-site identity. Six arithmetic perturbations
target a nonnumeric `file` binding, two boolean negations target a `TestBinary`, and removing
`!` from a destructured borrowed boolean produces a reference rather than a boolean.
The numeric-use index is keyed by file-wide names, closure inputs are not rebound by the
closure visitor, and expected call arguments omit method receivers even when a qualified
call passes the receiver explicitly. Broadly removing numeric-use inference would also lose
valid mutations on genuinely numeric unannotated locals.

- `crates/cargo-gamma-engine/src/ops/collect/collector/indexes.rs:47` — name-only numeric-use evidence.
- `crates/cargo-gamma-engine/src/ops/collect/collector/indexes.rs:336` — receiver omitted from parameter indexing.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3782` — closure traversal and missing input rebinding.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3804` — borrowed-bool replacement requires known operand type.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:4069` — expected positional argument types.
- Campaign examples: `5d4f375a11c7`, `15a2a009e944`, `528f10d94628`.

**Done when:** evidence follows lexical binding identities, nested/destructured closure
parameters, and the correct free-function/method/qualified receiver convention. Fixtures
retain numeric shadowed locals and borrowed-bool mutations while avoiding the observed
nonnumeric and nonboolean replacements; unresolved calls do not inherit unrelated signatures.

---

<a id="cor10"></a>
### COR10 — Validate reusable reference payloads against current bindings and lifetimes

**Area:** collector reusable payload selection · **Priority:** High · **Effort:** Medium

Five `option.none_to_some` mutants reuse reference parameters unsafely. Four produce
`Some(selected)` or `Some(root)` after those parameter names have been shadowed by owned
path buffers, so the replacement has the wrong type. The fifth returns `host_domain` from
`Endpoints::host_url`, although its return lifetime is tied to `self`, not that argument.
Copyability and matching type syntax do not establish that a currently visible binding has
the required type or outlives the returned reference.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3082` — parameter-map payload reuse.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3134` — immutable references considered copy-safe.
- `crates/cargo-gamma-lib/src/exec/workspace.rs:1155` — `selected` is rebound to an owned path.
- `crates/cargo-aprz-lib/src/facts/endpoints.rs:76` — return lifetime and argument lifetime differ.
- Campaign examples: `8977225b3ac2`, `d913eabdebfb`, `b518de8e5a95`.

**Done when:** reuse checks the current lexical binding and establishes the necessary lifetime
relation. Positive fixtures retain unshadowed references with an explicit shared lifetime;
negative fixtures cover owned shadows and separate elided input lifetimes. If compatibility
cannot be proved, omit only that synthesized payload, not other viable mutants at the site.

---

<a id="cor11"></a>
### COR11 — Avoid introducing an unnecessary Debug bound in propagation mutations

**Area:** collector `try.propagate_to_unwrap` replacements · **Priority:** Medium · **Effort:** Small

Two campaign mutants fail because replacing `?` with `.unwrap()` requires the error type
to implement `Debug`. Both operate on `Result<_, StartError>` in `spawn_patiently`.
The intended behavior change is to panic instead of propagating failure; formatting the
error is not necessary to ask that question. Excluding all unknown error types would remove
many valid panic mutations.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:4326` — propagation replacement generation.
- Campaign examples: `b1c6d696784a`, `72025eeee80b` (E0277, missing trait implementation).

**Done when:** propagation mutations can express the same success/panic distinction without
adding an avoidable `Debug` requirement, or use exact trait evidence to select the appropriate
encoding. Fixtures include non-`Debug` errors and generic errors, and preserve evaluation
count, success values, and failure-as-panic behavior.

---

<a id="cor12"></a>
### COR12 — Distinguish async call futures from their declared output types

**Area:** collector local call signatures and default replacements · **Priority:** High · **Effort:** Small

Twelve campaign mutants default the result of six calls that yield futures: `discover_with`,
`resolve_executable`, `refresh_task`, `stream_download`, `forward_download_stream`, and
`send_download_chunk`. The return index records the signature's output without whether the
function is async, so default eligibility is inferred from the awaited output while the
replacement is inserted before `.await` or inside `tokio::spawn`. Generic callable parameters
also must not inherit a same-named function's return annotation.

- `crates/cargo-gamma-engine/src/ops/collect/collector/indexes.rs:336` — signature index loses asyncness.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1651` — call-default eligibility uses that return index.
- Campaign examples: `5ca0cb87613b`, `2a43c4e6a70a`, `8fb90e3cdaca`,
  `41060d8b2d89`, `61c592822926`.

**Done when:** resolved async functions' declared outputs are not treated as pre-await call
types. Proven impossible future-default candidates are omitted or moved to a valid
post-await output site; ordinary async-body mutants and genuinely defaultable user-defined
futures remain eligible. Fixtures include named async calls, task spawning, and shadowing
generic callable parameters.

---

<a id="cor13"></a>
### COR13 — Require causal evidence before blaming replacements inside broad compiler spans

**Area:** `cargo-gamma-lib::exec::build::blame`, compiler verdict attribution · **Priority:** High · **Effort:** Medium

`secondary_attribution` treats any overlap between generated replacement text and a secondary
compiler span as exact blame. Rustc can report the entire enclosing match as a secondary
span when one guarded mandatory exit makes its arm return the wrong type. Every unrelated
replacement inside that match can then be withdrawn as unviable without an individual proof.
The existing overlap test does not distinguish a causal
replacement span from a broad enclosing diagnostic.

A compile-only reproduction guards the `continue` in `0 => continue` and independently
guards an integer literal in another arm. Rustc emits E0308 with a secondary span covering
the whole match. The literal mutation compiles when the mandatory continue is preserved,
but the overlap rule blames it for the other mutant's failure. The current compiled schema
has 82 absent guards inside `LoopProof::proof_tokens`' token-kind match, consistent with
this failure mode. Those are inferred exclusions, not proof that all 82 are independently
unviable or that every exclusion has this cause.

- `crates/cargo-gamma-lib/src/exec/build/blame.rs:272` — secondary-span overlap inserts exact blame.
- `crates/cargo-gamma-lib/src/exec/build/blame.rs:243` — exact blame takes precedence over deletion attribution for the batch.
- `crates/cargo-gamma-lib/src/exec/build/tests.rs:4128` — existing test accepts enclosing secondary-span overlap.
- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:99` — the affected value-producing token-kind match.

**Done when:** broad enclosing or consequence spans identify isolation candidates rather than
proving individual unviability. A regression fixture with one invalid mandatory-exit mutation
and independently compilable neighboring mutations withdraws only compiler-proven culprits;
genuinely causal replacement diagnostics still work. Reassess the affected `proof_tokens`
cohort and ensure reports and reusable compiler evidence do not preserve false verdicts.

**See also:** COR3 (screen provably invalid exit deletions), P13 (efficient candidate isolation).

---

## Performance

<a id="p3"></a>
### P3 — Amortize per-mutant process launch with a fork server

**Area:** `cargo-gamma-rt`, `cargo-gamma-lib::exec` (census, sweep) · **Priority:** Medium ·
**Effort:** Large

The design accepts per-mutant process launch as an unavoidable floor: "process launch remains
per mutant... activating several mutants together would confound causality" (the
"Costs and limitations" section of `docs/DESIGN.md`). For workspaces with many small, fast-running
tests, that launch/link/static-init cost dominates both the census walk (one subprocess per sampled
scope, including every subdivision) and the mutant sweep (one per selected binary or probe), and
currently cannot be amortized: no machinery in this crate reuses a warm process across launches.

A fork-server model can remove that floor without weakening isolation: install a pre-main
constructor (the existing hook point already used for guard installation) that, in the runtime
crate, forks the warm, fully-linked, single-threaded process fresh for every test or mutant
launch instead of paying `execve`/dynamic-link/static-init again. Each child still gets full
process isolation, matching today's guarantees; only the one-time setup cost is shared. The
mutant ordinal can be set by direct memory write in the child (inherited via copy-on-write, no
env/argv needed); libtest-driven test selection needs the fork server's constructor to run before
std's argv-capturing constructor so each child's argv can be patched before libtest reads it.

- `crates/cargo-gamma-rt/src/runtime.rs:1736` (`install()`) and the `.init_array`/
  `__DATA,__mod_init_func`/`.CRT$XCU` link-section statics around it —
  the existing pre-main constructor mechanism to extend
- `crates/cargo-gamma-lib/src/exec/census.rs` (`walk_with`) and
  `crates/cargo-gamma-lib/src/exec/sweep.rs` — the two call sites that currently launch one fresh
  process per sampled scope or mutant/binary attempt
- `crates/cargo-gamma/docs/DESIGN.md` — the documented limitation this would relax

**Done when:** census and sweep launches on Linux reuse a warm forked process instead of a fresh
`execve` per launch, a test proves the forked child observes no repeated dynamic-link or
static-init work (e.g. a constructor-run counter stays at one across many forked launches), and a
documented, tested fallback to today's spawn-per-launch behavior on platforms without `fork()`
(Windows) or without a dynamic loader step (statically linked binaries).

**See also:** F3 (shares the same fork-server engine). Landing this changes the per-launch cost
used by census admission and subdivision, assignment-time cost estimates, and learning-value
priority; revisit those heuristics' thresholds once this ships.

---

<a id="p4"></a>
### P4 — Extend exact negative Default evidence without excluding unknown types

**Area:** collector type/default indexes and replacement values · **Priority:** Medium · **Effort:** Medium

After separating the 12 async-call failures in COR12, 103 campaign mutants require an
unavailable `Default` implementation in a return value, nested payload, collection element,
loop break value, or wrapper. Current local evidence intentionally cannot resolve qualified
and renamed declarations; recursive default constructions therefore retain guesses such as
`crate::error::Error`, `Vec<AssignedMutant>`, and tuples containing a process output or channel.
Many other failures involve unresolved dependency types (`ProgressStyle`, `Version`, `Meta`,
and others), which must not become a name-based blacklist just because this campaign failed.

Same-crate re-exports now also disable useful negative evidence: `Defaults::visit_item_use`
records every public re-export name as indirect, and `lacks_error_default` refuses all such
names, including locally resolvable declarations. `catalog::Artifact` is re-exported from
its own declaration module and has no `Default`, yet unchanged artifact builders now receive
`Default::default()` and one-element `Vec<Artifact>` replacements. The empty-vector
replacement remains valid and must stay eligible. A comparison with the earlier snapshot
found 359 newly excluded function-value candidates, 296 with unchanged original bodies;
these are inferred schema exclusions, not a confirmed per-cause compiler census.

- `crates/cargo-gamma-engine/src/ops/collect/collector/types.rs:95` — qualified/renamed local paths remain unknown.
- `crates/cargo-gamma-engine/src/ops/collect/defaults.rs:542` — indirect names and wildcard re-exports veto negative evidence.
- `crates/cargo-gamma-engine/src/ops/collect/defaults.rs:714` — public re-export names are collected without declaration identity.
- `crates/cargo-anvil/src/catalog/mod.rs:20` — a same-crate re-export of `Artifact`.
- `crates/cargo-anvil/src/anvil/artifacts/ado.rs:122` — unchanged builder receiving an invalid default-valued replacement.
- `crates/cargo-gamma-engine/src/ops/collect/collector/types.rs:988` — existing negative default evidence.
- `crates/cargo-gamma-engine/src/ops/collect/collector/values.rs:103` — negative evidence vetoes unknown replacements.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:4001` — loop break default emitted without a type proof.
- Campaign examples: `c7472e99dfba`, `ee35ee80fa89`, `530c7a136bda`,
  `431768c762a4`, `b10d2fe3bb6d`.

**Done when:** exact module-qualified declaration and alias identity extends complete local
negative evidence, and proven unsupported nested/default constructor obligations veto only
the affected replacement. Exact standard-library negative facts may be added with verified
definitions. Unknown dependencies, configuration-sensitive/macro-generated impls, ambiguous
names, and valid whole-wrapper defaults remain eligible for the compiler oracle. Positive
fixtures cover separate same-named types, custom `Default` impls, empty containers, and
unsized wrapper payloads; reduction is measured against fresh compiler evidence.
Explicitly cover same-crate re-export chains: preserve negative evidence for the resolved
declaration without borrowing it for external or same-named unrelated types, and retain
valid empty collections while screening impossible element defaults.

**See also:** COR12 (async output is not the call's future type).

---

<a id="p5"></a>
### P5 — Filter overloaded operator mutations using exact operand and result types

**Area:** collector arithmetic/range selection and type inference · **Priority:** Medium · **Effort:** Medium

Twenty-two campaign mutants apply an unsupported operator to temporal, textual, or position
types: 19 arithmetic replacements and three nonnumeric range endpoint adjustments.
Examples include `Duration % u32`, `Duration * Duration`, string multiplication/subtraction,
date-time addition/division, and `Position + 1`. Temporal/textual recognition already exists,
but missing field, local-call, or intermediate expression types leave these candidates
optimistic. Recognition must describe operator support, not ban every operator on a temporal
type: scalar duration multiplication/division and duration addition/subtraction can be valid.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1842` — limited inferred call types.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1935` — temporal operand recognition.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3557` — existing textual/temporal operator veto.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3967` — range endpoint arithmetic.
- Campaign examples: `b81f41c76a90`, `42089bfe1f0e`, `e428c256e6df`,
  `504a2fe64455`, `401de9c5f7b2`.

**Done when:** resolved nominal operand and expected result types identify proven unsupported
operator combinations before insertion, including typed fields and propagated local results.
Fixtures retain supported temporal/scalar pairs and custom overloaded operators; unknown
types and ambiguous field names remain compiler-oracle cases, not name-based exclusions.

**See also:** COR8 (type-correct floating-point endpoint adjustments).

---

<a id="p6"></a>
### P6 — Resolve API identity before iterator and option rewrites

**Area:** collector method and constructor mutation selection · **Priority:** Medium · **Effort:** Medium

Seven campaign mutants confuse matching spellings with matching APIs. Six `iter.take_to_skip`
mutants target `std::io::Read::take` on files or readers; the result is not an iterator and has
no corresponding iterator `skip`. Another `option.some_to_none` mutant targets
`MetadataFeatureOption::Some`, a Cargo metadata enum variant rather than `Option::Some`.
Known mismatched APIs can be excluded without disabling valid iterator and option mutations.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:1350` — unconditional `take`/`skip` rename.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:4049` — constructor identified by final name.
- `crates/cargo-gamma-engine/src/ops/collect/collector/types.rs:770` — existing iterator trait evidence.
- Campaign examples: `ac75a61fa411`, `267358db589d`, `eac9376341d2`, `1d1a5b53ec43`.

**Done when:** exact receiver and constructor identity vetoes proven non-iterator/non-option
sites, including `Read` adapters and qualified enum variants. Positive fixtures retain
`Iterator::take`, standard `Option::Some`, imported aliases, and supported custom APIs;
unresolved/custom same-named methods are not rejected solely for lacking positive standard
library recognition.

---

<a id="p7"></a>
### P7 — Prove cursor stalls through nested branches and lexical binding identities

**Area:** collector resource proofs and binding/flow evidence · **Priority:** High · **Effort:** Large

Fourteen resource failures in campaign `960645875da50c13` have a stalled cursor:
ten glob-tokenizer mutants (eight OOM, two timeouts), an unchanged Tarjan edge cursor,
two unchanged diff diagonals, and a text-cluster scan that repeatedly reads the same
zero-width character. Besides deleting advancement or replacing its increment with zero,
changing the tokenizer's inner star predicate leaves the outer index unchanged and
repeatedly appends a token. The current proof accepts neither these nested matches and
pattern bindings nor the local reads, field updates, and dynamic branches in their bodies.

- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:190` — restricted block/branch proof.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3773` — match, for-loop, and pattern-context transparency gates.
- `crates/cargo-gamma-lib/src/discover/glob.rs:159` — outer index, inner star scan, and retained tokens.
- `crates/cargo-gamma-lib/src/discover/survey.rs:1649` — frame edge advancement.
- `crates/cargo-gamma-lib/src/fix/diff.rs:121`, `crates/cargo-gamma-lib/src/report/text.rs:104` — diagonal and character advancement.
- Campaign examples: `dd37da88403a`, `ca3b951bfdae`, `e34593b0c3b2`,
  `70a3b31d94ed`, `433a54249128`, `93d0937daca6`.

**Done when:** bounded, path-sensitive evidence can prove a reachable repeated cursor state
with no progress or escape, including an inner predicate made false by its enclosing match.
It distinguishes retained token growth from spinning and tracks exact bindings and field
aliases, not names. Fixtures retain finite scans, cursors advanced on another branch,
escaping/error paths, alternate writes, custom indexing, and unknown calls; unsupported
patterns and exhausted proof budgets remain eligible. Campaign examples are fixtures for
proof development, not a blanket list of exclusions.

**See also:** COR9 (lexical binding identity), P9 (derived grouping boundaries).

---

<a id="p8"></a>
### P8 — Track owned collection length as loop progress

**Area:** collector resource proofs for standard owned collections · **Priority:** Medium · **Effort:** Medium

Deleting `left.insert(...)` in `equalize_panels` timed out: the loop tests the vector's
length, but nothing left in its body changes that length. The proof currently recognizes
owned string appends of literals, not vector length and insertion. The companion OOM
mutant negating `right.len() < target` is an important counterexample: equal-length panels
grow forever, but a shorter right panel returns incorrectly without padding. An observed
OOM on one input must not erase that separately testable finite behavior.

- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:238` — supported preserving operations and string growth.
- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:666` — trusted resource binding kinds.
- `crates/cargo-gamma-lib/src/commands/dashboard.rs:866` — panel-length loops.
- Campaign examples: `dca3dfecad1d` (deleted progress), `2793bb89ebb1` (retain as a finite-behavior counterexample).

**Done when:** exact standard owned-vector identity and bounded effect summaries prove
deleted length progress without guessing from method names. Unknown helper effects,
possible escapes, clear/remove operations, and custom/interior-mutable receivers retain
candidates. Positive fixtures cover the deleted insertion; negative fixtures preserve the
negated condition's shorter-panel behavior rather than excluding every length mutation.

---

<a id="p9"></a>
### P9 — Prove stalled grouping boundaries over immutable collections

**Area:** collector derived-index and retained-collection resource proofs · **Priority:** Medium · **Effort:** Large

Two OOM mutants in `GroupedHints::from` repeatedly build the same group. Deleting
`start = end` leaves the group boundary unchanged; replacing `start + offset` with
`start * offset` leaves the initial boundary at zero when a later file boundary exists.
The latter produces an empty group without consuming input, while `files` keeps growing.
This needs a relation between the immutable input slice, its derived boundary, and the
outer cursor, not a blacklist of arithmetic or `position` mutations.

- `crates/cargo-gamma-lib/src/discover/hints.rs:973` — initial cursor and boundary derivation.
- `crates/cargo-gamma-lib/src/discover/hints.rs:1002` — retained group append and cursor update.
- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:238` — current proof has no derived slice/index relations.
- Campaign examples: `e0204c97c85a`, `d519b3c5914c`.

**Done when:** bounded evidence proves an unchanged boundary over stable input and retained
growth, including the zero-times-offset case, while summarizing intervening calls only
with verified nonescaping effects. Fixtures retain empty inputs, advancing boundaries,
normal zero offsets outside this relation, mutable inputs, side-effecting predicates,
custom collection APIs, and unknown aliases. No exclusion relies solely on the replacement
containing multiplication or the result being an empty group.

**See also:** P7 (direct cursor stalls).

---

<a id="p10"></a>
### P10 — Fold typed arithmetic to prove scalar loop fixed points

**Area:** collector scalar resource proofs and exact numeric type evidence · **Priority:** High · **Effort:** Medium

Eight of the 14 timed-out `shard_of` mutants force a zero-valued loop state: deleting
either cursor transfer, defaulting `next`, using a zero scale, or removing the positive
step keeps `next` at zero for shard counts above one. Fixed-width shifts matter:
`1_u32 >> 31` and `2_u32 << 31` both yield zero. The other six change the distribution
of progress, sometimes making it exceptionally slow, but are not established fixed
points and must not be discarded merely because they timed out.

- `crates/cargo-gamma-lib/src/discover/shard.rs:23` — initial state and loop transfers.
- `crates/cargo-gamma-lib/src/discover/shard.rs:31` — shift, scale, cast, and next-state computation.
- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:276` — scalar proof currently excludes arithmetic/casts.
- Campaign examples: `c482043e3d22`, `43141d21e376`, `527e5f88be07`,
  `fed6cf95d80c`, `0eb0d70388f8`, `5188fa2f9b5d`.

**Done when:** exact primitive widths, cast semantics, initial values, and standard scalar
defaults support bounded constant propagation/fixed-point proofs. Overflow, wrapping,
floating-point exceptional values, and checked arithmetic are modeled rather than
evaluated as mathematical integers. Fixtures preserve genuinely advancing variants,
including the six non-fixed-point campaign examples, and unknown/custom numeric types;
losing type or effect evidence keeps a candidate eligible.

**See also:** COR9 (do not borrow another binding's numeric evidence).

---

<a id="p11"></a>
### P11 — Prove retry fixed points without mistaking cached calls for progress

**Area:** collector resource proofs for bounded local helper effects · **Priority:** Medium · **Effort:** Large

Eleven timeouts occur in `minimize_interaction`. Eight edits specifically remove a
retry's progress or terminal escape: forcing either `reduced` check true, disabling the
granularity stop, changing `>=` to `>`, removing/replacing that break, or disabling
granularity growth. Repeating a cached proof does not consume the proof budget, so the
budget conditions alone do not guarantee termination. The two negated `reduced` checks
and deleted complement assignment also change control flow, but may produce different
finite outcomes; they are not justified exclusions just because they share this function.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:537` — retry state, budget guards, and partitions.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:550` — first restart.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:570` — second restart, terminal escape, and granularity growth.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:637` — cached results return before proof counters advance.
- Campaign examples: `f22a7b6da1a4`, `35326a00d297`, `5b33335c0b1c`,
  `fda02c1203e4`, `37ae5eb40807`, `d72ac26e7f21`.

**Done when:** bounded, resolved local effect summaries can certify a repeated retry state
on cache-hit paths without overlooking writes to counters, collections, or external
state. Positive fixtures exercise stagnation after warm-cache hits; negative fixtures
cover fresh proofs consuming budgets, changed populations, finite alternate results,
escaping errors, and unresolved helpers. Do not suppress all mutations in retry loops or
assume every call advances a budget.

---

<a id="p12"></a>
### P12 — Investigate retained-growth proofs for open-ended standard iterator loops

**Area:** collector for-loop resource proofs and standard collection effects · **Priority:** Low · **Effort:** Large

Removing or replacing the successful-name `break` in `disambiguate_artifact_directories`
produced two OOM mutants: the suffix search keeps retaining new names in `used`.
The collector currently installs no resource proof for a `for` loop. This is a candidate
for a richer growth proof, not an already justified exclusion: `2_u64..` has
overflow-sensitive stepping, suffix uniqueness depends on a local helper, and finite
allocation must not be confused with unbounded growth. Negating the insertion result
also has a finite wrong-result path when a suffix already exists and must stay eligible.

- `crates/cargo-gamma-engine/src/ops/collect/collector.rs:3874` — for-loop context has no resource proof.
- `crates/cargo-gamma-engine/src/ops/collect/collector/resource.rs:238` — retained-growth recognition is string-only.
- `crates/cargo-gamma-lib/src/exec/baseline.rs:627` — suffix construction.
- `crates/cargo-gamma-lib/src/exec/baseline.rs:673` — open-ended suffix search and retained set insertion.
- Campaign examples: `2b636daa72d9`, `043973128dcb`; counterexample `2628cbdf90df`.

**Done when:** a documented, bounded proof distinguishes genuinely nonterminating standard
iteration with retained growth from finite or overflow-terminating ranges, and certifies
element uniqueness and the absence of another escape. Explicitly determine whether the
two campaign examples meet that proof; retain them if they do not. Fixtures preserve
bounded searches, duplicate insertions, removals, finite wrong-result paths, custom
iterators/hashers, and unknown helper effects. Do not treat the spelling `RangeFrom` or
an OOM verdict as a proof.

---

<a id="p13"></a>
### P13 — Prioritize function-local control-flow candidates for ownership failures

**Area:** `cargo-gamma-lib::exec::build::isolation`, diagnostic candidate selection · **Priority:** High · **Effort:** Medium

Live observation of the exclusion phase found repeated E0506 failures in
`physical_with_links`: rustc reports the assignment to `pending` and its outstanding borrow,
while changing a later loop exit can cause the conflict. Isolation narrowed a 63-mutant
file population through multiple proof builds over several minutes. The current tiers
start with diagnostic files, and the diagnostic-code heuristic does not specialize
ownership failures. Smaller, function-local tiers prioritizing relevant exits and state
updates could reach the cause sooner without treating a broad borrow span as proof.

- `crates/cargo-gamma-lib/src/paths.rs:27` — borrowed component traversal and replacement of `pending` before a loop exit.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:219` — existing diagnostic-code prioritization.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:340` — file, package, and dependency candidate tiers.

**Done when:** ownership diagnostics can prioritize bounded function-local and control-flow
tiers before broader searches, with deterministic fixtures demonstrating fewer proof
invocations for the borrowed-loop case. Every unviable verdict still requires compiler
evidence; unsuccessful prioritization falls back to the broader tiers, retaining viable
neighbors and distinguishing guard-induced interactions from invalid individual edits.

**See also:** COR3 (ownership-conflict correctness), P14 (repeated isolation searches).

---

<a id="p14"></a>
### P14 — Amortize isolation searches across independent failures in one context

**Area:** `cargo-gamma-lib::exec::build::isolation`, convergence proof reuse · **Priority:** High · **Effort:** Large

Isolation returns after resolving one culprit or interaction for a diagnostic context.
If another independent mutant causes the same failure, a later convergence round starts
another context-local search with a fresh proof cache. The observed campaign repeatedly
compiled small subsets of `paths.rs` for the same E0506 diagnostic. Investigate continuing
the bounded search over remaining candidates and reusing identical proof results rather
than repeatedly paying for pristine controls and rediscovery. A matching error message
alone is not evidence that the cause or compilation context is unchanged.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:381` — a new context-local proof cache.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:402` — the first resolved result ends the tier search.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:667` — existing exact-active-subset memoization.
- `crates/cargo-gamma-lib/src/exec/build.rs:807` — a subsequent unattributed failure invokes isolation again.

**Done when:** a fixture with multiple independently failing edits in one function reaches
the same justified verdicts with fewer compiler invocations than repeated fresh searches.
Reuse is bounded and keyed by exact active schema, source generation, Cargo roots, target
mode, and build configuration; changed contexts invalidate it. Interaction-only failures,
pristine failures, timeouts, and exhausted budgets must not become individual unviable
verdicts, and cache hits must not permit non-progressing retry loops.

**See also:** P13 (candidate ordering), P11 (cache-hit retry progress).

---

<a id="p15"></a>
### P15 — Investigate graph-equivalent target narrowing for isolation proofs

**Area:** `cargo-gamma-lib::exec::build::isolation`, Cargo proof invocation planning · **Priority:** Medium · **Effort:** Large

The live campaign's proof builds retained the selected workspace package roots,
`--all-features`, and `--lib --bins --tests` even when only a small subset of `paths.rs`
mutants was active. The same borrow error appeared in ordinary library and test-harness
compilation. This is an intentional correctness safeguard: ambiguous compiler messages
do not identify the compilation mode, and dropping package roots can alter feature
unification. Investigate whether captured build-unit evidence can safely select less
target work without changing the graph under which unviability is judged.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:190` — ambiguous target modes retain the failed invocation's verb.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:374` — proof scopes preserve the original package roots.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:712` — exact-subset proofs use those roots and selectors.

**Done when:** representative library/test-harness and feature-unification fixtures establish
which target reductions preserve the original compilation semantics, and supported cases
show fewer rebuilt Cargo units with unchanged verdicts. Unsupported or ambiguous cases
retain the original invocation; no optimization infers the target mode from diagnostic
text or simply drops roots. Record measured unit counts and elapsed proof time before
and after, rather than assuming that a smaller mutant subset makes compilation cheaper.

**See also:** P13 (reduce search breadth), P14 (reduce proof invocation count).

---

## Features

<a id="f3"></a>
### F3 — Native fork-server test harness as a `cargo test`/nextest replacement

**Area:** new fork-server engine crate, `cargo-gamma-lib::exec` baseline/nextest integration ·
**Priority:** Low · **Effort:** Large

Today, "run tests under process isolation" is not something this crate provides itself: baseline
measurement launches the whole binary as one process with `Only::All`
(`crates/cargo-gamma-lib/src/exec/baseline.rs:215`) — exactly as fragile to a crashing or
state-leaking test as plain `cargo test`. Per-test isolation only exists when configured to
delegate to `cargo-nextest` as the harness (`crates/cargo-gamma-lib/src/exec/nextest.rs`), which
pays nextest's own exec-per-test cost; this crate is a client of nextest there, not a competitor
to it. Meanwhile, CI pipelines that run both a normal test runner and `cargo gamma` today execute
the test suite twice: once for real verification, once as this crate's own baseline calibration.

Once P3's fork-server engine exists, expose it as a first-class native test-execution mode (e.g.
`cargo gamma --no-mutants`) that gives per-test isolation intrinsically, at fork-server speed,
without invoking `cargo-nextest`. Let baseline optionally emit CI-grade output (JUnit, per-test
pass/fail, timing) as a byproduct of the run it already performs, so a pipeline can opt to drop
its separate test-runner invocation instead of running the suite twice. Because this crate's
instrumented tree is not guaranteed behavior-identical to a plain build in every case (guards
"alter size and layout" and "may expose stack or compiler limits in unusually deep code" per the
design's own limitations section), this must stay an explicit, documented opt-in for teams
willing to verify against the instrumented tree — not a silent replacement of their release gate.

- `crates/cargo-gamma-lib/src/exec/baseline.rs:215` — current single-process, whole-binary
  baseline launch
- `crates/cargo-gamma-lib/src/exec/nextest.rs` — current nextest delegation for opt-in isolation
- `crates/cargo-gamma/docs/DESIGN.md` — "Costs and limitations" section documenting the
  instrumented-tree behavioral caveats this mode must disclose

**Done when:** a `--no-mutants` (or equivalent) mode runs the workspace's tests with native
per-test isolation via the fork-server engine, can emit CI-consumable per-test results, and its
documentation explicitly discloses the instrumented-tree caveats a team must accept to use it as
their primary test runner.

**See also:** P3 (this mode's execution engine)

---

<a id="f4"></a>
### F4 — Keep a running unviable-mutant counter visible throughout Excluding

**Area:** `cargo-gamma-lib::commands::console_events`, compiler convergence progress · **Priority:** Medium · **Effort:** Small

Long exclusion phases need an immediately visible cumulative count of confirmed unviable
mutants so users can distinguish ongoing discovery from time spent investigating one failure.
The existing convergence-progress callback formats a count, but the display should show it
from phase entry and retain it throughout ordinary checks and isolation proof builds.

- `crates/cargo-gamma-lib/src/commands/console_events.rs:112` — existing unviable-count rendering.
- `crates/cargo-gamma-lib/src/exec/build.rs:907` — confirmed compiler blame publishes the cumulative census size.

**Done when:** both progress and dashboard displays show `Excluding unviable mutants (0 found)`
at phase entry, update the counter as unviability is confirmed, and retain the monotonic count
during proof builds. Temporary active-subset sizes, unresolved mutants, and interaction-only
exclusions must not inflate it; focused display tests cover these transitions.

---

<a id="f5"></a>
### F5 — Report hint coverage and separate probe costs from whole-suite fallback

**Area:** `cargo-gamma-lib::exec` sweep telemetry, checkpointing, and reporting · **Priority:** High · **Effort:** Medium

Current counters conflate the execution tier credited with whether exact knowledge was
available or reused. A campaign loaded 44,491 exact hints matching 34,603 of 43,164 testable
mutants, yet initially reported only 50 exact probes. In a later sample, 251 completed
mutants had exact hints and 245 were killed by the same recorded test, including executions
not counted as exact probes. Aggregate sweep duration and hit counts also cannot establish
whether speculative probes save time or merely add work before expensive whole-suite fallback.

- `crates/cargo-gamma-lib/src/exec/session.rs:243` — sweep duration is aggregate rather than split by execution tier.
- `crates/cargo-gamma-lib/src/exec/session.rs:252` — exact probe attempts and hits count a particular execution path.
- `crates/cargo-gamma-lib/src/exec/session.rs:261` — candidate counts are gathered at assignment rather than describing population-wide coverage.
- `crates/cargo-gamma-lib/src/exec/sweep.rs:2031` — selection attempts already carry tier and elapsed-time observations.
- `crates/cargo-gamma-lib/src/exec/testing_state.rs:500` — cumulative sweep costs are persisted with settled verdicts.

**Done when:** live diagnostics and completed reports distinguish population-wide exact-hint
coverage, attempted exact probes, and confirmation of the initially recorded killer regardless
of execution tier. Record launch counts and execution time separately for exact probes,
inferred-test probes, and whole-suite fallback, distinguishing unsuccessful or inconclusive
speculation and accounting explicitly for confirmation work and duplicate launches avoided.
Persist the measurements across resume, avoid double-counting reused executions, and label
aggregate concurrent execution time separately from campaign wall-clock time. Focused fixtures
cover the same test selected through different tiers, missed hints, fallback, and resumed
accounting; comparisons assess total campaign time rather than early completion rate.
