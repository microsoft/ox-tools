// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Deciding which conditionally compiled code is actually in the build.
//!
//! Rust strips `#[cfg(...)]` items before anything else runs, so code behind a predicate that does
//! not hold is not in the compiled artifact at all. A mutant placed there is therefore unkillable
//! by construction: activating it changes nothing, every test passes, and the tool reports a
//! survivor no test could ever have caught.
//!
//! That is not merely a wrong number. Each such mutant costs a full run of every test binary that
//! links its package, so a workspace with a lot of platform-specific or feature-gated code spends
//! most of its time proving things about code it did not build. On one real workspace, 378 of
//! 2,290 survivors — 16.5% — sat behind a gate that did not hold.
//!
//! # What this module decides
//!
//! [`CfgSet`] holds the configuration predicates that are true for the build, expands active
//! `cfg_attr` attributes, and answers whether the resulting `#[cfg(...)]` attributes hold:
//!
//! ```rust
//! # #[cfg(feature = "internals")]
//! # fn example() {
//! # use cargo_gamma_lib::internals::cfg::CfgSet;
//! let set = CfgSet::parse("unix\ntarget_arch=\"x86_64\"\n").with_features(["std".to_owned()]);
//!
//! assert!(set.holds_str("unix"));
//! assert!(set.holds_str("feature = \"std\""));
//! assert!(!set.holds_str("windows"));
//! assert!(!set.holds_str("feature = \"stats\""));
//! assert!(set.holds_str("any(unix, windows)"));
//! assert!(set.holds_str("not(windows)"));
//! assert!(!set.holds_str("all(unix, feature = \"stats\")"));
//! # }
//! # #[cfg(feature = "internals")]
//! # example();
//! ```
//!
//! The names and values come from `rustc --print cfg`, asked about the build cargo will actually
//! run rather than about the compiler's own defaults: [`Build`] resolves the target, the profile's
//! `debug_assertions` and any custom `--cfg` from the same places cargo reads them — the
//! command line, `CARGO_BUILD_TARGET`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, the
//! `.cargo/config.toml` files and the manifest's profile tables — and hands them to the probe as
//! flags. They have to be passed rather than inherited: `RUSTFLAGS` is a Cargo-facing variable
//! that `rustc` does not interpret, so a probe that merely inherited it would answer about a
//! different compilation from the one the run builds.
//!
//! Features are the one thing `rustc` cannot answer, because they are Cargo's concept. They are
//! resolved separately, per package, by [`features`].
//!
//! # Erring toward keeping a mutant
//!
//! Every uncertainty resolves toward the predicate holding, which keeps the mutant. A mutant that
//! should not exist is visible and annoying; a mutant silently missing from the population is a
//! hole in the measurement that nobody can see. So an unparsable attribute, an unrecognised
//! predicate function, a build whose profile could not be followed, a `--cfg` a configuration file
//! sets under a predicate of its own, and a package whose features could not be resolved all leave
//! the code mutable:
//!
//! ```rust
//! # #[cfg(feature = "internals")]
//! # fn example() {
//! # use cargo_gamma_lib::internals::cfg::CfgSet;
//! let set = CfgSet::parse("unix\n");
//!
//! // `version` is a predicate this module does not model, so it is assumed to hold.
//! assert!(set.holds_str("version(\"1.80\")"));
//!
//! // And a set that was never resolved holds everything.
//! assert!(CfgSet::unconditional().holds_str("windows"));
//! # }
//! # #[cfg(feature = "internals")]
//! # example();
//! ```

mod build;
mod cfgs;
mod probe;

pub mod features;

#[doc(inline)]
pub use build::Build;
pub use cargo_gamma_engine::cfg::CfgSet;
pub(crate) use cargo_gamma_engine::cfg::test_gated_for;
#[doc(inline)]
pub use cfgs::Cfgs;
pub(crate) use probe::for_build;
