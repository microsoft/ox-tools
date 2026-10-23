# cargo-gamma-engine — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-engine`.

## Purpose

This crate owns Rust source parsing, mutation-site discovery, stable mutation
identity, mutator selection, and mutant-schema instrumentation.

## Discovery model

Discovery first performs a configuration-aware syntax-tree pre-pass. It audits
stated-value attributes and builds per-file indexes of source-visible type
evidence, aliases, local signatures, numeric uses, and imported paths. Candidate
collection then applies the selected mutators using those indexes and emits
stable, content-derived mutation identities. Each candidate is classified as
`Proven`, `Optimistic`, or `Explicit`: positive source evidence supports proven
candidates, unresolved semantic guesses are optimistic, and caller-supplied
replacement values are explicit. Confidence is diagnostic evidence rather than
an automatic filter. Site classes whose measured compile-failure cost is high
and whose killing tests are not exclusive may be withheld from the default;
a non-default selector that resolves to their mutator admits them without
changing their stable identity. The standalone `check_stated` API has no build
configuration and therefore audits the whole file.

## Boundaries

- Input is Rust source plus a mutator selection; output is deterministic
  mutation metadata and instrumented source.
- The engine does not invoke Cargo, run tests, supervise processes, or decide
  campaign verdicts.
- The pre-parse shape guard carries a cumulative expression-path budget across
  nested delimiters, including unary operators before block expressions.
  Sibling separators reset to their inherited cost; nested `else` ladders share
  that budget rather than receiving a fresh root-level allowance.
- Stable identities use scheme 7 and are content-derived so reports and incremental knowledge
  remain meaningful as unrelated source changes.
  Stated-value identities additionally include the token-normalized replacement
  expression: correcting only a `#[gamma::value(...)]` attribute invalidates its
  historical identity without changing unrelated mutants. Whitespace and comments
  in that expression do not change its identity: the parsed expression is rendered
  as tokens, preserving literal contents and distinct operators. Manually supplied
  stated candidates must contain valid Rust expressions; invalid ones are caller
  programming errors. Named error replacements retain their existing
  replacement-text discriminator.
- Expression mutation families do not descend into patterns, including literal
  and range patterns in `let ... else` declarations. Evaluated initializers,
  diverging `else` expressions, and later statements remain eligible for mutation.
  Dedicated match-arm and match-guard rewrites are handled separately.
- Mutation discovery uses source-visible type evidence and never identifier
  spelling to screen candidates. Unsigned, textual, and temporal evidence
  propagates through parameters, annotated locals, fields, casts, assignments,
  comparisons, index and slice positions, locally visible signatures,
  `map_or` results, and iterator sums whose source fixes the result. This
  includes backward constraints from a later local use to an earlier
  unsuffixed-zero initializer. Local-use evidence belongs to a lexical binding,
  not every binding with the same spelling. Closure parameters, destructured
  bindings, and local shadows establish their own evidence; a local initializer
  still sees the binding that precedes its declaration. Expected call arguments
  distinguish method syntax from qualified calls with an explicit receiver,
  account for configuration-disabled parameters, and do not borrow an
  unrelated signature for unresolved or shadowed callees. Method results retain
  their declaring receiver's `Self`, not the caller's enclosing `Self`.
  Indexed signature arguments and results resolve imports and aliases in the
  declaration scope. Callee generic parameters remain unknown rather than
  borrowing a same-named caller parameter's bounds. Associated `Self`
  parameters and results refer to the declaring impl, including explicit
  receiver reference types.
  Standard `Option`/`Result` patterns and callbacks propagate payload types;
  tuple destructuring preserves known members independently of unknown members
  and aligns bindings after a rest pattern with the tuple's trailing members.
  Proven unsigned zero is not decremented, proven
  signed zero is decremented in the default campaign, and an unsuffixed zero
  with no resolved signedness is an optimistic candidate available only through
  a non-default selector that includes `literal.int_decrement`. Likewise,
  associated function items and numeric wrappers without integer-literal
  arithmetic are not sufficient evidence for expression increment or
  decrement; focused non-default selection can still request those optimistic
  sites. A mutable reference to a numeric primitive is not itself a scalar
  arithmetic operand; dereferencing it retains numeric mutations.
  Proven text/time addition is not changed to any incompatible arithmetic
  operator; signed and unresolved arithmetic remains in the population.
- A match pattern is not evaluated, so its literals are not mutated. The `if`
  guard attached to a match arm is evaluated and receives ordinary expression
  mutations, including relational substitutions, alongside whole-guard
  `match_guard` mutations.
- A mutation that invents `Default::default()` for a known payload requires
  positive evidence. Primitive and supported standard types, package-local
  derives or impls whose concrete generic arguments satisfy their bounds,
  aliases, renamed imports, and generic parameters with an applicable
  `Default` bound qualify. Package indexes remain separate so same-named types
  in different packages cannot make each other's evidence ambiguous. Import and
  shadow evidence is also kept per lexical scope, so a wildcard import or local
  trait in one module or block does not weaken otherwise proven candidates in
  a sibling module or function.
  Generic aliases are instantiated before querying their payload's default
  obligations or synthesizing a return shape. Standard `Box<str>`, `Rc<str>`,
  and `Arc<str>` retain their specialized empty-string defaults even though
  `str` itself has no `Default`. Ordinary non-defaultable pointees still provide
  negative evidence for sized smart-pointer defaults.
  Type substitution checks both
  its input and projected output before cloning. Resolution shares a budget
  of 4,096 type, path-segment, expression, and attribute nodes, charging both
  input and projected output, with a maximum nesting depth of 64. This bounds
  acyclic generic expansion as well as cycles,
  including call-signature canonicalization. Over-budget evidence becomes an
  unknown type, not proof of a concrete type or its trait obligations.
  Alias bindings are lexical:
  unrelated nominal declarations and body-local import shadows do not change
  a declaration's target. Each target's nominal and imported names are frozen
  in its own declaration environment before sibling aliases are expanded;
  referenced aliases are then normalized in dependency order, and cycles and
  aliases depending on cycles become unknown. Incomplete or over-budget alias
  targets stay wholly unknown rather than preserving a partially expanded shape.
  Another alias's generic parameters
  cannot capture those names. Both passes
  retain the expansion and nesting budgets, sharing scope indexes rather than
  cloning them for each alias. Bare numeric primitives are fixed to their absolute
  primitive identities before inheritance only when imports do not make them
  ambiguous. Block-local aliases and nominal declarations shadow inherited
  imports without rebinding already normalized parent aliases. Declaration-only
  freezing does not resolve sibling wrapper aliases; payload extraction happens
  after the wrapper itself has been expanded.
  Operator-output incompatibility requires complete, canonicalized types on both
  sides. Equivalent primitive spellings compare equally, and nested unknown or
  over-budget outputs retain replacements for compiler validation.
  Nominal alias targets remain relative
  to the declaring file's module, including inline modules, rather than assuming
  that file is the crate root. Conditional or arbitrary bounded generic implementations are
  not proof that a concrete instantiation is defaultable: otherwise admissible
  whole-function fallbacks remain optimistic compiler-oracle candidates.
  A defaultable set element or map key does not itself establish the ordering
  or hashing traits required to collect a synthesized element. Supported
  primitive/string keys and recursively supported standard options, results,
  smart pointers, tuples, and arrays establish those obligations independently
  of `Default`, with bounded recursion. Such unresolved
  constructions remain admitted with optimistic confidence; empty standard
  collections need no element default or ordering/hash proof. Custom hasher
  defaults and element-collection obligations are classified separately.
  Package source labels bind files to unique active crate roots and module
  paths. Qualified local declarations, renamed imports, aliases, and same-crate
  re-export chains resolve by that identity, including literal `#[path]`
  modules. Separate same-named types do not borrow each other's evidence.
  Missing, conditional, duplicate, ambiguous, or unlabeled module evidence and
  unresolved dependency paths remain unknown. A single-source index does not
  infer a qualified path's negative evidence from an unrelated root declaration
  with the same terminal name. Structural construction checks
  distinguish an empty collection from an element default: a non-defaultable
  `Artifact` does not invalidate `Vec<Artifact>::new()`, but does invalidate a
  synthesized default element.
  A source-visible generic or lifetime-bearing declaration without a `Default`
  derive or implementation supplies negative evidence just as a nongeneric
  declaration does. Conditional generic derives and implementations are not
  unconditional positive evidence. Standard `std::io::Result<T>` is the
  standard result with an `std::io::Error` payload, not an unknown custom
  container. Result-valued blocks and matches retain the outer result identity
  when payload types cannot be inferred, so a loop break does not invent a
  blanket `Default` implementation for the container.
  A `Self::Assoc` projection is resolved
  through the enclosing impl when that impl declares the associated type;
  unresolved trait-associated types remain abstract. An unresolved concrete
  dependency type remains an optimistic fallback candidate unless source-visible
  evidence proves it non-defaultable. It remains in the default campaign because
  completed-run evidence shows exclusive killing tests for this class; confidence
  telemetry keeps its compilation cost measurable. Expected types flow
  through nested blocks, branches, closures, returns, local
  annotations, and assignment targets. When no default exists, a type-identical
  copy-safe parameter may be reused as the opposite payload only while that
  parameter remains the visible binding. Reference reuse additionally requires
  evidence that the input lifetime satisfies the output lifetime; unrelated
  elided input lifetimes are not interchangeable. Direct-default
  mutations of parameters, call results, and early returns additionally exclude
  `Result`, which has defaultable payloads but no blanket `Default`
  implementation of its own. A return already written as `bool::default()`,
  `Option::default()`, or `String::default()` is recognized as the same value
  as the proposed `Default::default()` replacement and produces no mutant.
  Arrays follow the standard library's implemented lengths and element bounds,
  and known standard types without `Default` are rejected only after their
  complete import-resolved path identifies them. A wrapper contributes proven
  default evidence only when its path resolves to a supported standard wrapper
  and its payload is also proven defaultable.
- Whole-function replacements preserve the written return shape. Alias
  parameters are substituted by their declared roles before `Option` or
  `Result` payload values are chosen. Renamed imports of standard `Option`
  and `Result` preserve those container shapes rather than falling back to a
  default of the entire container. Unresolved foreign imports remain optimistic.
  Collection
  aliases use `Default::default()` rather than rebuilding a standard
  constructor with different generic defaults. Imported aliases whose source
  name differs from the concrete collection type use the same trait
  constructor, avoiding assumptions about an inherent `new`. A local collection-shaped type
  that shadows a standard collection name and positively implements `Default`
  also uses the trait constructor instead of assuming an inherent `new`. If
  its concrete generic arguments do not satisfy that implementation's bounds,
  discovery emits no empty constructor: the standard type's inherent `new`
  cannot be borrowed as evidence for the local shadow.
  Unsized standard wrappers use shape-specific constructors, opaque iterators
  retain iterator-shaped replacements, and supported nested `Option`/`Result`
  iterator payloads are unified by mapping the payload into `Either` rather
  than wrapping the outer container as an iterator. Early returns participate
  in the same unification without rewriting returns in nested functions,
  methods, closures, or async blocks. A wrapped iterator shape carries a positive
  container depth; persisted positive depths keep their integer encoding, while
  zero is invalid rather than an alternate unwrapped shape. Tuple/collection
  products require evidence for every member. Shared empty slices may use `&[]`; arbitrary shared or mutable
  references are not fabricated with `Box::leak`, and mutable string slices
  receive no automatic replacement because the only source-independent
  construction would leak once per invocation.
- Statement and loop deletion is gated only for concrete intra-procedural
  compile hazards: deferred-binding initialization, explicit `drop` calls, and
  branch/loop type transitions already proven by local syntax. The divergent
  tail of a `let`-`else` remains intact, and a `continue` is not changed to a
  valueless `break` when the target loop must produce a value. Ordinary calls,
  assignments, and viable loop transitions remain candidates. Source-visible
  moves and restorations can require a wider guard even when each individual
  edit is viable: those edits lift to the nearest loop, or otherwise the
  enclosing block. Their replacement region applies only the selected edit
  to uninstrumented source; the original region retains nested guards. Guard
  incompatibility is not evidence that an individual replacement is invalid.
  Non-Copy evidence includes standard owning types, mutable references, and
  unconditional source-visible owning fields. A derive on an otherwise
  untransformed struct or an unrelated external module does not erase that
  field evidence. Conditional or transformed fields stay unknown, and call
  arguments that may implicitly reborrow a mutable reference are not treated
  as definite moves.
- Method renames require source-visible receiver compatibility when the
  replacement API is not shared by all same-named methods. In particular,
  `last` becomes `first` only for proven slices, arrays, or `Vec` values, not
  iterator chains. Removing `!` from a borrowed boolean dereferences the
  operand so the replacement remains a boolean. Slice, array, and standard
  `Vec::iter` callback evidence carries borrowed tuple elements into `any`
  and `all` closures. Each negation site emits one replacement: a proven
  borrowed boolean uses the dereferenced form at replacement index 1; other
  operands use the plain form at index 0, optimistic when their type is
  unresolved. Keeping these identities distinct prevents a historical failure
  of the plain form from suppressing a corrected borrowed-boolean replacement.
  Unknown operands never add a speculative dereference alternative.
  Double negation is not a substitute
  for removal: overloaded `Not` can have side effects and need not restore its
  operand when applied twice.
  Exact API identity also distinguishes standard reader `take` adapters from
  iterator `take`, and qualified custom `Some` variants from `Option::Some`.
  Reader bounds include `Read` and `BufRead`, their qualified forms, references,
  trait objects, and tracked generic parameters. Concrete reader methods still
  require the reader trait in scope; a source-visible custom `by_ref` prevents
  assuming that its output is another reader. Character values inferred from
  standard `str::chars`, `next`, loops, and callbacks retain casing mutations
  through iterator-shaped `Either` guards, preserving the distinct concrete
  uppercase and lowercase iterator types.
  Compatible custom variant rewrites preserve their qualifier. Unknown
  receivers and external aliases remain compiler-oracle cases rather than
  being rejected merely for lacking standard API evidence.
- Replacement rendering retains source-written expected types when changing an
  expression would otherwise remove generic inference constraints. Numeric
  boundary adjustments use floating-point units for proven primitive floats,
  including primitive associated conversions. Integer range adjustments retain
  their discrete endpoint semantics; floating-point endpoint adjustments are
  boundary shifts, not an equivalence claim about switching inclusivity.
  Arithmetic admission checks the resolved operand, right-hand side, and
  expected result together. Standard and supported chrono temporal APIs retain
  valid scalar multiplication/division and duration addition/subtraction while
  rejecting proven unsupported combinations. Field and propagated local-call
  evidence can supply these types; custom operators, ambiguous fields, and
  unresolved nominal identities never inherit an unrelated standard contract.
- Propagation-to-panic mutation preserves the successful value and evaluates the
  operand once without requiring the error to implement `Debug`. Error and
  payload type constraints supplied by the original propagation context remain
  available where source evidence establishes them. An async function's declared
  output is not treated as the type of its unawaited call.
- Rust-semantic mutation covers logical operand contribution, `Option` and
  `Result` predicates, `?` propagation, fallback values, collection order,
  additional literal classes, control-flow values, typed boolean values,
  calls, iterator pipelines, parameters, and parsed regular expressions.
  Different concrete iterator types are joined by the runtime `Either`
  adapter. Option-style `filter` chains are excluded, because replacing their
  `Option` result with an iterator adapter changes the type of later methods.
  Removing `filter` is enabled by default only when source-visible evidence
  identifies the receiver as an iterator; unresolved receivers remain
  available through a non-default selector containing `iter.remove_filter`.
  Boolean struct-field shorthand remains shorthand rather than becoming an
  expression-shaped guarded replacement. Fallback mutation requires positive
  evidence that the fallback result directly implements `Default`, and is
  withheld when that result is already an obvious primitive default such as
  `false` or numeric zero. Early-return defaulting applies the same no-op
  rule. Floating-point negation excludes zero because changing only its sign
  bit is not a useful behavioral mutation for ordinary numeric code. Call
  mutation additionally requires an unattributed, unqualified call whose local
  signature is unambiguous. Qualified calls never consume a same-named bare local signature;
  they retain only independently proven standard or associated inference.
  Array reversal is withheld when guarding would shorten the
  lifetime of a borrowed array temporary. Parameter shadowing supports opaque
  iterator returns through the same iterator/container unification; unsupported
  opaque contracts remain excluded. Regex mutation is confined to string
  literals passed to a path ending in `Regex::new`; both the original and each
  replacement must parse with `regex-syntax`.
- Guard placement preserves temporary-borrow lifetimes by lifting affected
  mutations to a consuming expression when a branch-local temporary would
  otherwise escape. Borrow provenance is retained through `Option`/`Result`
  adapters such as `ok_or`, `ok_or_else`, and `unwrap`, so a propagation guard
  cannot drop the owner before a later `to_owned` consumes its borrowed payload.
  A borrow in an `if let` or `while let` scrutinee lifts to the complete
  conditional or loop, keeping pattern bindings in scope throughout the body
  and retaining short-circuit evaluation in let chains.
  Lifted replacements retain the original evaluation order,
  single evaluation, and lazy boundaries such as short-circuit operands,
  closures, and async bodies. Source attributes remain outside generated
  expression operators, and configuration-selected function tails retain their
  original value type.
- Proven no-ops are omitted under every selector, including `all`. For
  `call_result.default`, a separate positive callee index recognizes only a
  directly named, non-async free function whose declaration is lexically
  visible and returns implicit unit or the literal type `()`. This does not
  reuse the optimistic file-wide return-type index as name-resolution proof.
  Duplicate free-function names, any same-named import, pattern binding,
  constant, static, struct constructor, or foreign item anywhere in the file,
  visible wildcard imports, and opaque statement/item
  macros or potentially transforming local attributes in an enclosing scope
  defeat the proof. Module boundaries do not
  inherit bare function names. Qualified calls, qualified-self paths,
  parenthesized callees, function pointers, return aliases, and functions with
  potentially transforming attributes remain outside this proof. The
  replacement's bare `Default` must also resolve to the standard trait.
  Disabled implicit preludes and generic parameters named `Default` do not
  supply that proof. Renamed external crates also participate in path resolution,
  so aliasing a dependency to `std` cannot manufacture standard-type evidence.
  Only result defaulting is omitted: deleting the unit call still changes its
  side effects and remains eligible.
- Early-return defaulting additionally recognizes zero-argument standard
  `new` and `default` constructors for `Vec`, `String`, `VecDeque`,
  `LinkedList`, `BinaryHeap`, `BTreeSet`, and `BTreeMap`, when both constructor
  and expected return type resolve to the same supported standard type.
  Standard imports, including renamed type/module imports, can supply this
  evidence; bare non-prelude collection names cannot. Explicit allocator
  parameters, lifetime/const/associated generic arguments, parenthesized path
  arguments, constructor type aliases, qualified-self calls, custom/shadowed
  paths, and unknown types do not supply proof. `bool::default()` and
  `Option::default()` also require standard type evidence, rather than terminal
  identifier spelling. Source-resolved trait-default spellings retain their
  existing recognition, with qualified-self calls left unknown.
  `HashMap` and `HashSet` constructors are deliberately excluded because
  hasher state is observable. Capacity and allocation mutations are unchanged:
  empty elements do not prove equal capacity, allocator state, or allocation
  failure behavior.
- Array and `vec!` reversal already require at least two parsed elements.
  Consequently singleton literals, even with effectful or destructor-bearing
  elements, have no reversal candidate; repeated-element lengths and arbitrary
  receiver lengths are not inferred. No operand commutativity, range, or
  dataflow-equivalence theorem is used. Semantic admission filters reserve
  identity occurrences before withholding candidates, preserving the
  identities of later same-text sites; existing token-identical replacements
  continue to be rejected before occurrence allocation.
- Mutations that introduce proven stalled loops or unbounded retained-memory
  growth are also omitted, under every selector, without a verdict, suppression
  count, or score contribution. These are resource-safety exclusions, not
  equivalence claims. The proof reconstructs the proposed loop and accepts only
  invariant conditions over source-resolved, owned primitive scalar bindings,
  scalar operations that cannot panic and preserve those bindings, and constant
  branches without an escape. Removing a sole `break`, deleting the only
  scalar advance, or changing that advance to zero can establish the proof.
  Standard owned `String` appends of character or string literals are additionally
  recognized: a nonempty append on every repeated iteration proves unbounded
  retained growth, without predicting whether allocation failure, capacity
  overflow, or a timeout ends execution first. Empty appends do not prove growth.
  Source-resolved `Peekable<Chars>` values (including uniquely borrowed ones and
  `.chars().peekable()` on proven string-slice bindings) support an additional
  exhaustion proof. A predicate that remains true at EOF, followed only by
  harmless `next()` calls and supported scalar/string operations, cannot restore
  progress at EOF. Pure character predicates over `peek()` also remain invariant
  if a mutation removes consumption. Generic or custom iterators, callback
  factories, effectful predicates, resets, and panicking unwraps remain unknown.
  Already-proven problematic original loops are not used to omit unrelated edits.
  Unsupported calls, macros, transforming attributes, unknown drops and aliases,
  overloaded operations, arithmetic whose overflow behavior is unresolved, unknown
  branches or nested loops, and other possible escapes defeat this narrow proof.
  Loops over 4096 source bytes remain unfiltered to bound proof work.
  Resource aliases resolve in their declaring lexical scope, not through a
  file-wide terminal name. Imported or ambiguous alias identities remain unknown,
  and recursive alias/reference queries stop at cycles. Proofs reuse the original
  AST and literal tokens; probing candidate text does not retain additional
  proc-macro source-map entries.
  Source identity indexes are built even for narrow selectors so shadows and
  opaque scopes cannot disappear merely because another mutator was deselected.
  Omitted candidates reserve their identity occurrences so remaining identities
  do not change.
  A bounded fixed-point interpreter additionally tracks lexical shadows,
  one-level primitive field projections, known character-match facts, exact
  standard-vector length invariance, and supported immutable derived boundaries.
  Match guards are not ignored: guarded arms remain eligible unless their
  effects are certified. Fixed-width arithmetic, shifts, casts, explicit wrapping
  operations, and concrete primitive floating-point behavior use Rust's typed
  semantics rather than unbounded mathematical integers. Resolved scalar helper
  bodies and certified standard-map warm-hit prefixes can establish repeated
  state without assuming every call advances a budget. Unknown effects and
  changing keys/populations retain candidates. The interpreter bounds bindings,
  steps, iterations, helper syntax, and provenance sizes; exhaustion is unknown,
  never a proof. Exclusions are not keyed by application function names or prior
  campaign verdicts.
  Open-ended integer ranges are not proof of infinite retained growth:
  fixed-width stepping can overflow, and wrapping permits only finitely many
  distinct scalar suffixes. Deleting the suffix-search exits in artifact-directory
  disambiguation therefore remains eligible. Neither source-visible
  `RangeFrom` syntax nor an observed OOM establishes unique insertions forever;
  custom hashers, filesystem-dependent naming, and unresolved helper effects
  provide additional reasons to retain these candidates.
- The discovery pre-pass is confined to code discovery would mutate: not
  configured out and not test-only, including field-level gates.
- A stated value is reported as an error where discovery would never read it —
  on a declaration, a `const fn`, or an empty body — matching the proc macro's
  compile-time rejections, so a hint that generates nothing is never silent.
  The public error retains a typed `StatedValueError` source until diagnostic
  rendering, including through the fused audit-and-collect entry point.
- The crate forbids unsafe code.

Replaceable traversal and test mechanics are recorded in the
[implementation guide](IMPLEMENTATION.md).

## Semantic oracle

rustc remains the final authority on whether the complete instrumented schema
compiles. Narrow compiler-assisted witnesses were evaluated for unresolved
questions such as whether a dependency type implements `Default`, but are not
an additional discovery stage: they would repeat Cargo setup under the same
features, target, and cfg values before the convergence build that already
answers the question exactly. Completed-run evidence also shows that unresolved
default construction can exercise killing tests no other candidate class
reaches, so dropping it wholesale would lose measured testing value. Confidence
telemetry keeps that cost visible while ordinary convergence retains the valid
candidates. A witness is warranted only if future diagnostics identify a
repeatable semantic question whose avoided proof cost exceeds the extra Cargo
invocation and whose answer can be reproduced under the exact build context.

Resource-analysis source-inspection regression tests read checked-in immutable fixtures, including
bounded snapshots of campaign source functions and a labeled helper-module graph.
They never parse or embed live production files: a campaign instruments those
files before compiling its test harnesses, changing both source shape and nesting
depth even when no mutant is active. The repository's mutation configuration
excludes these fixture directories and the compiled controls at
`src/ops/collect/collector/resource/acceptance/guarded.rs` and
`src/ops/collect/collector/resource/effects/fixture.rs` so their bytes remain
stable in baseline and mutant runs.

## Stability

The crate is published so `cargo-gamma` can be installed from crates.io. Its
Rust API is internal and carries no independent compatibility guarantee. Its
rustdoc is hidden, and its hand-written README warns downstream users not to
depend on it.
