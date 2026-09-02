// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(all(test, miri)))]
#![doc(hidden)]
#![forbid(
    unsafe_code,
    reason = "every raw platform call in this workspace lives in `cargo-gamma-unsafe`, behind a safe interface; enforcing that here is what makes it a property the compiler checks rather than a convention people remember"
)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(
    clippy::redundant_pub_crate,
    reason = "this crate's modules are private, so the lint fires on every pub(crate) item in them; taking its advice and widening them to `pub` is exactly what the private module tree exists to prevent"
)]
#![cfg_attr(
    not(feature = "internals"),
    allow(
        unused_imports,
        reason = "a module's `pub use` lines are its surface for the integration tests, which reach it through the `internals` facade; without that feature they have no consumer and read as unused. CI lints with `--all-features`, where they are checked normally"
    ),
    allow(
        dead_code,
        reason = "a few internal entry points exist solely for integration tests reached through the `internals` facade; without that feature rustc cannot see those consumers. CI lints with `--all-features`, where they are checked normally"
    )
)]
#![cfg_attr(
    miri,
    allow(
        unused_imports,
        dead_code,
        reason = "Miri excludes the host-I/O test modules that consume these helpers because its isolation forbids their filesystem and subprocess operations; ordinary all-feature linting still checks every consumer"
    )
)]

//! Internal implementation library for [`cargo-gamma`](https://crates.io/crates/cargo-gamma).
//!
//! This crate is an implementation detail. Do not depend on it: it may change in incompatible
//! ways without warning, and it carries no semver commitment to anything it exposes.
//
// Everything cargo-gamma does, apart from talking to the real terminal.
//
// The `cargo-gamma` binary is a few dozen lines that implement `Host` and call `run`. Code in a
// `[[bin]]` target cannot be linked by an integration test, so putting anything here that could
// live in a library would be putting it somewhere no test can reach — an unusually bad trade for a
// tool whose subject is test quality.
//
// # What the tool does
//
// Conventional mutation testing rebuilds the crate under test once per mutant. A workspace with ten
// thousand mutants and a ninety-second build spends ten days building and a few minutes testing.
//
// cargo-gamma builds **once**. Every selected mutant is compiled into the same set of test binaries
// as a *guard* — a branch, taken only when that mutant's ordinal matches the one named by the
// `GAMMA_ACTIVE` environment variable:
//
//     original:     a < b
//     instrumented: (if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })
//
// This is the *mutant schema*: one artifact encoding the whole population, with the choice deferred
// from compile time to process start. Testing a mutant then costs one process launch instead of one
// build, and the guard itself costs a cached atomic load and a branch the CPU learns immediately.
//
// # The pipeline
//
// A run moves through these stages, in order. Each module names one of them.
//
// | Stage | Module | What it produces |
// |---|---|---|
// | Command line | `commands` | The parsed request, folded together with the config file |
// | Configuration | `config` | `gamma.toml`, with precedence against the command line decided in one place |
// | Enumeration | `discover` | Workspace packages, source files, the shard slice, and which package can reach which |
// | Parsing | `parse` | An AST with byte-accurate spans, plus the comment trivia suppression needs |
// | Definition | `ops` | Source-level mutant definitions from the mutator registry — the catalog of what can be changed |
// | Suppression | `suppress` | The mutants withdrawn by an attribute, a comment directive, or a config rule |
// | Identity | `model` | Content-addressed mutant IDs, outcomes, and the score they roll up into |
// | Instrumentation | `schema` | The rewritten sources, the guard for each mutant, and the rollback loop that withdraws whatever will not compile |
// | Execution | `exec` | The scratch tree with the guard runtime vendored into it, one build, a measured baseline, then every mutant run in parallel under a timeout and a stall detector |
// | Projection | `report`, `elements`, `html`, `ci` | Console output, the `mutation-testing-elements` document, a self-contained page, and SARIF plus CI annotations |
//
// The `vendor` directory beside these modules is not one: it holds the report viewer and the report
// schema, embedded so that an HTML report opens on a machine with no network at all.
//
// The rest stand beside the pipeline rather than inside it:
//
// - `estimate`: stops a run at the point it would stop measuring and start waiting, and projects
//   the rest — so a four-hour job is discovered in the first minute rather than the last.
// - `advise`: turns a finished run into findings, each with a measured symptom, a remedy, and what
//   the remedy costs in signal. This is what the advice artifact and CI job summary carry.
// - `fix`: plans and applies the source edits behind the `suppress` command.
// - `merge`: combines per-shard reports into one score, so a nightly job covering a slice at a time
//   still adds up to an answer about the whole workspace.
// - `bounds`: the timeout arithmetic — baseline, multiplier, floor — kept in one place so that
//   every command sizes a budget the same way.
// - `diag`: the hidden `--diag` dump, which reports where a run's wall clock actually went. It
//   exists for developing this tool, not for using it.
// - `error`: the error type, its cause chain, and the usage-versus-failure distinction that picks
//   the exit code.
//
// # Conventions
//
// - Every fallible path returns [`Result`], whose error carries a cause chain and knows whether it
//   is a usage error, because that distinction is what picks the process exit code.
// - Nothing writes to `stdout` or `stderr` directly; everything goes through [`Host`], which is
//   what makes the console UI, the color decisions and the exit codes ordinary assertions in a
//   test rather than things verified by eye.
// - Hash maps are `rustc_hash`, not the standard library's. The keys are mutant IDs, paths and
//   package names this run produced, and the cost of a DoS-resistant hash on several hundred
//   thousand of them is not worth paying for keys nobody outside the run chooses. The one exception
//   is `merge`, which decodes a document it did not write into a map keyed by the file names that
//   document states — a crafted report can make those collide, and the worst it buys is a slow
//   `merge` on a local CLI the user pointed at the file themselves. That is a trade the read path
//   makes knowingly rather than an invariant it upholds.

/// The result type used throughout the crate.
///
/// The error carries a cause chain and knows whether it is a usage error, which is what decides
/// the process exit code.
pub type Result<T, E = error::Error> = core::result::Result<T, E>;

use rustc_hash::{FxHashMap, FxHashSet};

pub(crate) type HashMap<K, V> = FxHashMap<K, V>;
pub(crate) type HashSet<V> = FxHashSet<V>;

// Every module below is declared twice, under opposite halves of the `internals` feature. Private
// is the shape that matters: this library has exactly one consumer, the `cargo-gamma` binary, so an
// item nothing here uses is genuinely dead, and only a private module tree lets rustc see that. The
// `internals` facade cannot name a private module — a `pub use` of one is a hard error rather than
// a lint — so the feature that opens the facade is also what widens these declarations, and the
// facade then re-exports them by name instead of by glob.
//
// Written out rather than generated, because rustfmt and every `syn`-based tool (cargo-gamma
// included) find sources by resolving file-bearing `mod` declarations, and a `mod` produced by a
// macro is invisible to them.
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod advise;
#[cfg(not(feature = "internals"))]
mod advise;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod bounds;
#[cfg(not(feature = "internals"))]
mod bounds;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod cfg;
#[cfg(not(feature = "internals"))]
mod cfg;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod ci;
#[cfg(not(feature = "internals"))]
mod ci;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod commands;
#[cfg(not(feature = "internals"))]
mod commands;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod config;
#[cfg(not(feature = "internals"))]
mod config;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod diag;
#[cfg(not(feature = "internals"))]
mod diag;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod discover;
#[cfg(not(feature = "internals"))]
mod discover;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod docs;
#[cfg(not(feature = "internals"))]
mod docs;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod elements;
#[cfg(not(feature = "internals"))]
mod elements;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod error;
#[cfg(not(feature = "internals"))]
mod error;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod estimate;
#[cfg(not(feature = "internals"))]
mod estimate;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod exec;
#[cfg(not(feature = "internals"))]
mod exec;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod fix;
#[cfg(not(feature = "internals"))]
mod fix;
#[cfg(test)]
mod fixtures;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod html;
#[cfg(not(feature = "internals"))]
mod html;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod merge;
#[cfg(not(feature = "internals"))]
mod merge;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod model;
#[cfg(not(feature = "internals"))]
mod model;
mod notes;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod ops;
#[cfg(not(feature = "internals"))]
mod ops;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod parse;
#[cfg(not(feature = "internals"))]
mod parse;
mod paths;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod report;
#[cfg(not(feature = "internals"))]
mod report;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod schema;
#[cfg(not(feature = "internals"))]
mod schema;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod suppress;
#[cfg(not(feature = "internals"))]
mod suppress;

/// Runs the deterministic concurrency models selected by the dedicated Loom test target.
#[cfg(loom)]
#[doc(hidden)]
pub fn run_loom_models() {
    exec::run_loom_models();
}

/// Re-exports the crate internals for the crate's own integration tests.
///
/// The modules above are declared privately whenever this feature is off, which is what keeps the
/// dead-code analysis honest. This library has exactly one consumer, the `cargo-gamma` binary, so
/// an item nothing here uses is genuinely dead — but a `pub` module makes it look like a deliberate
/// part of a public API some external crate might call, and the analysis goes quiet. Only a private
/// module tree lets rustc see the truth.
///
/// Integration tests reach directly into the internals and test them at the level they are designed
/// at, with no `pub(crate)` escape hatches and no `#[cfg(test)]` re-exports widening the API
/// surface. They compile as separate crates and so cannot see a private module; this facade opens
/// exactly the same paths for them, under `internals::`, and nothing else. The `internals` feature
/// is a required feature of those test targets, so Cargo skips them unless the test command
/// explicitly enables it.
///
/// Each module is named individually rather than glob-re-exported. A glob would take on whatever
/// the module gains next, silently and without review; naming the module itself re-exports the same
/// set of paths while keeping the list of what the facade offers readable in one place.
///
/// The facade declares no file-bearing modules, which is what lets it stay a macro. rustfmt and
/// every `syn`-based tool — cargo-gamma among them — find sources by resolving file-bearing `mod`
/// declarations to a path on disk, and a `mod` produced by a macro is invisible to them. There are
/// no such declarations here, so hiding this behind a macro costs those tools nothing.
///
/// It is gated on a feature rather than on `debug_assertions` because `cargo test --release` turns
/// `debug_assertions` off while still building the integration tests, which then fail to compile.
macro_rules! expose_internals {
    ($($name:ident),+ $(,)?) => {
        #[cfg(feature = "internals")]
        #[doc(hidden)]
        pub mod internals {
            $(
                #[doc(hidden)]
                pub use crate::$name;
            )+
        }
    };
}

expose_internals!(
    advise, bounds, cfg, ci, commands, config, diag, discover, docs, elements, error, estimate, exec, fix, html, merge, model, ops, parse,
    report, schema, suppress
);

/// Shared test fixtures, reachable from the integration tests as well as the unit tests.
///
/// Gated the same way as the modules above rather than on `cfg(test)` alone, because `tests/`
/// compiles as a separate crate and cannot see anything a `cfg(test)` gate creates. Without this
/// the integration tests would need their own copy of every fixture.
#[cfg(any(test, feature = "internals"))]
#[doc(hidden)]
pub mod testing;

#[doc(inline)]
pub use crate::commands::{Host, run};
