# cargo-gamma-attrs-impl — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-attrs-impl`.

## Purpose

This ordinary library implements parsing and validation for the inert
attributes exported by `cargo-gamma-attrs`.

## Terminology

A **stated value** is the expression in `#[gamma::value(...)]`. A
**comment-form directive** is an attribute-shaped source comment such as
`// #[gamma::skip(...)]`. Timeout attributes and directives accept a timeout
multiplier either as a bare numeric argument or as a named setting. Their
other arguments are mutator selectors plus the named `reason` and `tag`
metadata.

## Boundaries

- This crate must remain a normal library, not a proc-macro crate. Keeping the
  logic outside rustc makes it directly testable and mutation-testable.
- It accepts exactly one Rust expression where an attribute promises an
  expression and rejects unsupported keys or malformed selectors.
- Delimiter depth and the combined chain of operators, casts, postfix links,
  and `else` arms are bounded before input reaches `syn`. The chain categories
  share one budget, matching the engine guard rather than allowing mixed syntax
  to evade each independent limit.
- A stated value is rejected on any function the tool would never mutate: a
  declaration with no body, a `const fn`, or a function whose body is empty.
  Accepting one there would leave a hint that reads as working and generates
  nothing.
- Argument lists are split on their top-level commas and each argument is then
  classified on its own, so an attribute accepts exactly the text the equivalent
  comment-form directive accepts. A positional timeout multiplier
  therefore carries no positional meaning: it may sit before, between, or after
  selectors, a `reason`, or a `tag`. Every comma-delimited argument must be
  non-empty, so leading, repeated, comma-only, and trailing commas are rejected
  instead of being confused with the intentionally bare all-mutator form. What a multiplier may not do is
  appear twice — a second multiplier, in any spelling and in either order, is
  refused rather than silently overriding the first, and the tool's directive
  parser refuses the same text.
- Mutation-control attributes return the original item unchanged after
  validation.
- A resource attribute accepts one portable string name on a test function or
  inline test module and emits an ignored marker test. Portable names start with
  an ASCII letter and otherwise contain only ASCII letters, digits, `.`, `_`,
  or `-`. Function annotations require a test attribute and apply to that test;
  module annotations require an inline module and apply to the containing test
  target. Resource arguments and annotated items pass the same bounded
  token-shape guard as stated values before either reaches `syn`. Distinct
  resource validation failures remain typed internally until the proc-macro
  boundary renders the final `compile_error!`.

## Resource marker protocol

The emitter and cargo-gamma's harness-listing decoder share a private marker
grammar. Marker names begin with `__cargo_gamma_resource_`, followed by the
lowercase hexadecimal UTF-8 bytes of the resource name. Function markers then
use `_test_` and the encoded Rust function name; module markers use `_binary`.
Libtest adds the enclosing module path, which distinguishes identical
binary-wide markers in sibling modules.

Function markers copy only `cfg` and `cfg_attr` attributes, so scheduling
metadata exists under the same conditional compilation as the annotated test.
Other attributes are intentionally not propagated. Any change to this grammar
must update both this crate's emitter and cargo-gamma's decoder.

## Stability

The crate is published only to support `cargo-gamma-attrs`. Its Rust API is an
implementation detail, so its rustdoc is hidden and its hand-written README
warns downstream users not to depend on it. The diagnostics and accepted
attribute syntax are the user-visible contract.
