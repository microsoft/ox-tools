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
- A stated value is rejected on any function the tool would never mutate: a
  declaration with no body, a `const fn`, or a function whose body is empty.
  Accepting one there would leave a hint that reads as working and generates
  nothing.
- Argument lists are split on their top-level commas and each argument is then
  classified on its own, so an attribute accepts exactly the text the equivalent
  comment-form directive accepts. A positional timeout multiplier
  therefore carries no positional meaning: it may sit before, between, or after
  selectors, a `reason`, a `tag`, or a trailing comma. What it may not do is
  appear twice — a second multiplier, in any spelling and in either order, is
  refused rather than silently overriding the first, and the tool's directive
  parser refuses the same text.
- It returns the original item unchanged after validation.

## Stability

The crate is published only to support `cargo-gamma-attrs`. Its Rust API is an
implementation detail, so its rustdoc is hidden and its hand-written README
warns downstream users not to depend on it. The diagnostics and accepted
attribute syntax are the user-visible contract.
