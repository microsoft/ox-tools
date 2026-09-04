<div align="center">
 <img src="./logo.png" alt="Cargo-Gamma-Rt Logo" width="96">

# Cargo-Gamma-Rt

[![crates.io](https://img.shields.io/crates/v/cargo-gamma-rt.svg)](https://crates.io/crates/cargo-gamma-rt)
[![docs.rs](https://docs.rs/cargo-gamma-rt/badge.svg)](https://docs.rs/cargo-gamma-rt)
[![MSRV](https://img.shields.io/crates/msrv/cargo-gamma-rt)](https://crates.io/crates/cargo-gamma-rt)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml/badge.svg?event=pull_request)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

Runtime support injected into crates under mutation test by `cargo-gamma`.

This crate is injected into the dependency graph of the crate under test while a mutation run
is in progress. You should never need to depend on it directly.

`cargo-gamma` rewrites the crate under test so that every mutation site carries a *guard*: a
cheap runtime check that activates exactly one mutant. That lets a whole population of mutants
live in a single compiled artifact — the *mutant schema*, after Untch, Offutt and Harrold, who
introduced the construction in 1993 — instead of requiring one build per mutant. Since a build
is by far the most expensive step in the loop, testing a mutant drops from minutes to the cost
of launching a process.

## What a guard looks like

[`a`][__link0] is the only function the instrumented source calls. Guard shape follows what Rust accepts
at the mutation site:

```text
// an expression, whose value the mutant replaces
(if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })

// a block, whose body the mutant replaces
{ if ::gamma_rt::a(12u32) { Default::default() } else { ..the real body.. } }

// a statement, which the mutant deletes
if !::gamma_rt::a(19u32) { self.entries.push(value); }
```

Sites nest — in `a + b < c` the `<` site contains the `+` site — and only the `else` arm carries
instrumented children. Exactly one mutant is live in a process, so if the `<` mutant is active
then no `+` mutant can be, and the taken arm can hold plain original text. That is what keeps
the encoding linear in the size of the source rather than exponential in nesting depth.

## You do not depend on this crate

`cargo-gamma` copies the workspace to a scratch tree, writes this crate into it, and adds the
dependency there. Nothing is added to your manifest, nothing is fetched from the network, and
your own build is never instrumented. The package is `cargo-gamma-rt` but its library is named
`gamma_rt`, which is why instrumented source can say `::gamma_rt::a` without a rename.

The copy embedded in the tool is this exact source, so the vendored runtime cannot drift from
the one the guards were generated against.

## Why this crate has no dependencies

It has zero dependencies, no build script and no `std`, by design. Its empty features expose
only coordinator or repository test plumbing; the vendored crate enables none of them.
Anything else would perturb feature unification in *the user’s* tree, which could change what
their code compiles to and therefore what their tests prove, or stop a `no_std` tree from
building once the shim is injected into it. Zero dependencies is a correctness requirement,
not a preference.

For the same reason [`a`][__link1] must stay trivial. It is called at every mutation site of every
execution of the suite, so its cost is multiplied by the whole population: a cached atomic load
and a comparison, behind a branch the predictor learns immediately.

## A worked example

Given this function, and a mutant that turns `<` into `<=`:

```rust
fn below(a: u32, b: u32) -> bool {
    a < b
}
```

`cargo-gamma` rewrites it in the scratch tree as:

```rust
fn below(a: u32, b: u32) -> bool {
    if ::gamma_rt::a(7u32) {
        (a) <= (b)
    } else {
        a < b
    }
}
```

The whole population lives in one binary, and the run launches it once per mutant:

```text
GAMMA_ACTIVE=7  ./target/debug/deps/my_crate-abc123   # mutant 7 is live
GAMMA_ACTIVE=8  ./target/debug/deps/my_crate-abc123   # mutant 8 is live
./target/debug/deps/my_crate-abc123                   # nothing is live: the baseline
```

## Selection protocol

The active mutant is named by the [`ACTIVE_VAR`][__link2] environment variable, captured exactly once
during process startup before user code can start threads. The value is a decimal mutant
ordinal. [`NONE`][__link3] means no mutant is active, which is how the baseline run and every ordinary
build behave — including builds of proc macros, where an active mutant could otherwise hang the
compiler.

An unset, empty, or unparsable value all mean [`NONE`][__link4]. A build that links this crate but is not
being driven by a mutation run must behave exactly as it did before, and the ordinals are
1-based precisely so that “absent” and “explicitly unmutated” are the same answer. Failure to
acquire the startup environment is different: the runtime emits [`ENVIRONMENT_ERROR_MARKER`][__link5]
and exits, so the parent cannot mistake a mutant that never activated for a survivor.

That distinction covers [`CENSUS_VAR`][__link6] as well as [`ACTIVE_VAR`][__link7]. An unset census variable is an
ordinary process, but a census variable this process could not *read* is a startup failure, not
an absent one: treating it as absence would run the mutant named by [`ACTIVE_VAR`][__link8], produce no
census file, and report a baseline failure the run would read as a verdict about that mutant. A
read interrupted by a signal is retried rather than counted as a failure, since an interruption
is not evidence of anything.

Two further failure shapes exist because “captured, but wrong” is worse than either of the
above:

* If some other native constructor — a loader or C-runtime startup hook that runs before
  `main` — runs instrumented code before this crate’s own constructor installs the captured
  selection, a guard reached in that window emits a fixed diagnostic and terminates immediately
  without unwinding rather than silently reporting the baseline. This applies on a hosted target
  outside a Miri execution, where that installation is expected. `NONE` would otherwise be
  ambiguous between “genuinely unmutated” and “asked too early to know”, and only the first may
  ever be reported as a passing mutant.
* On a Unix with no immutable startup environment image, [`ACTIVE_VAR`][__link9] is read through
  `getenv` under the POSIX process-wide precondition that no native environment mutation runs
  concurrently. This capture happens before Rust `main`, so safe Rust has not had an opportunity
  to start a thread that violates the precondition; Rust environment mutation is unsafe for the
  same reason. A foreign native constructor that starts concurrent `setenv`, `putenv`,
  `unsetenv`, or equivalent mutation is outside this abstraction. The runtime still performs a
  second independent read and rejects a disagreement through [`ENVIRONMENT_ERROR_MARKER`][__link10] and
  immediate exit. That double-read is integrity detection for a visibly inconsistent result,
  not a proof of memory safety or proof that forbidden foreign mutation did not occur.

```rust
use gamma_rt::{ACTIVE_VAR, CENSUS_VAR, NONE, a, active, any};

// Stated for an ordinary process. A census is its own mode: it activates no mutant, so
// `active` reports `NONE`, and every guard answers `false` while recording the site it stands
// at — including the sites this example would walk past.
if std::env::var_os(CENSUS_VAR).is_none_or(|path| path.is_empty()) {
    // In an ordinary process nothing is selected, so every guard takes its original arm.
    if active() == NONE {
        assert!(!any());
        assert!(!a(NONE));
        assert!(!a(1));
        assert!(!a(9_999));
    }

    // Only a positive ordinal can select a mutant.
    if active() != NONE {
        assert!(a(active()));
    }
}

assert_eq!(ACTIVE_VAR, "GAMMA_ACTIVE");
```

Reading it once, rather than per call, is what makes the guard cheap; it also means a test that
sets the variable on itself changes nothing, which is the honest behavior. The run drives
selection by launching a fresh process per mutant.

## Runtime entry points

[`a`][__link11] is what the guards call, and the only runtime entry point instrumented source contains.
[`active`][__link12] and [`any`][__link13] are there for the tool’s own diagnostics and for anyone inspecting a
scratch tree by hand:

```rust
use gamma_rt::{active, any};

// Useful in a scratch tree when you are trying to work out which mutant a failing
// reproduction actually ran.
if any() {
    println!("mutant {} is live", active());
} else {
    println!("baseline");
}
```

## Making two iterators one type

[`Either`][__link14] is the one other thing instrumented source mentions, and it exists because the guard
is an `if`. A function returning `impl Iterator<Item = T>` returns a single concrete type that
its body picks, so `if a(n) { core::iter::empty() } else { ..the real body.. }` has arms of two
different types and will not compile. Wrapping each arm in a variant makes them one type:

```text
{ if ::gamma_rt::a(4u32) { ::gamma_rt::Either::L(core::iter::empty()) }
  else { ::gamma_rt::Either::R({ ..the real body.. }) } }
```

See [`Either`][__link15] for why this is not a `Box<dyn Iterator>`.

## `no_std`, and what it does not buy

This crate is `#![no_std]`. It has to be: it is injected into the dependency graph of every
crate the tool instruments, so a shim that linked `std` could not be instrumented into a tree
whose target has no `std` in its sysroot — and that failure is not attributable to any mutant,
so the rollback loop cannot withdraw anything to rescue it. The whole tree simply stops
building. The tests link `std`, which costs nothing because they are never compiled into a
user’s build.

What `no_std` does not buy is a mutation run on a target with no environment. A mutant is
selected by reading `GAMMA_ACTIVE`, which needs POSIX `getenv` or the Win32 equivalent; a
target with neither gets the arm that reports no mutant at all, so the instrumented code
compiles and runs the original everywhere. That is deliberately the safe direction — every
mutant is reported as surviving rather than a mutated program being reported as correct — but
it means the useful case is a `no_std` *crate* whose tests are run on a hosted target, which is
how nearly every `no_std` library is tested anyway.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma-rt">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbhFwoyicofFob0JM_5SGotNcb-qKodur5pWUbrijzXi7ixeFhZIGDbmNhcmdvLWdhbW1hLXJ0ZTAuMi4wbmNhcmdvX2dhbW1hX3J0
 [__link0]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=a
 [__link1]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=a
 [__link10]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=ENVIRONMENT_ERROR_MARKER
 [__link11]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=a
 [__link12]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=active
 [__link13]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=any
 [__link14]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=Either
 [__link15]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=Either
 [__link2]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=ACTIVE_VAR
 [__link3]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=NONE
 [__link4]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=NONE
 [__link5]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=ENVIRONMENT_ERROR_MARKER
 [__link6]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=CENSUS_VAR
 [__link7]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=ACTIVE_VAR
 [__link8]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=ACTIVE_VAR
 [__link9]: https://docs.rs/cargo-gamma-rt/0.2.0/cargo_gamma_rt/?search=ACTIVE_VAR
