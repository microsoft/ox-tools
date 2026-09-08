// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolving engine cfg evidence from the compiler selected by Cargo policy.

use std::process::Command;

use super::{Build, CfgSet};
use crate::Result;
use crate::error::error;

pub(crate) fn for_build(build: &Build) -> Result<CfgSet> {
    if build.several_targets {
        return Ok(CfgSet::unconditional());
    }

    let program = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    for_build_with(build, &program)
}

fn for_build_with(build: &Build, program: impl AsRef<std::ffi::OsStr>) -> Result<CfgSet> {
    let mut command = Command::new(program);
    let _builder = command.args(build.probe_args());
    let output = command
        .output()
        .map_err(|cause| error!("could not run `rustc --print cfg`").caused_by(cause))?;

    if !output.status.success() {
        return Err(error!(
            "`rustc --print cfg` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut set = CfgSet::parse(&String::from_utf8_lossy(&output.stdout));

    if build.debug_assertions.is_none() {
        set = set.with_undecided(["debug_assertions".to_owned()]);
    }

    Ok(set.with_undecided(build.undecided.iter().cloned()))
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    #[test]
    fn a_rustc_probe_failure_is_reported() {
        let executable = std::env::current_exe().expect("the test executable has a path");
        let error = for_build_with(&Build::default(), executable).expect_err("libtest rejects rustc's arguments");

        assert!(error.to_string().contains("`rustc --print cfg` failed"), "{error}");
    }

    #[test]
    fn the_host_answers_about_itself() {
        let set = for_build(&Build::default()).expect("the compiler that built the tests is available");

        assert!(set.holds_str("target_pointer_width = \"64\"") || set.holds_str("target_pointer_width = \"32\""));
        assert!(!set.holds_str("target_arch = \"there-is-no-such-architecture\""));
    }

    #[test]
    fn a_build_for_another_target_answers_about_that_target() {
        let host = for_build(&Build::default()).expect("the host answers");
        let (elsewhere, family) = if host.holds_str("windows") {
            ("x86_64-unknown-linux-gnu", "unix")
        } else {
            ("x86_64-pc-windows-msvc", "windows")
        };
        let build = Build {
            target: Some(elsewhere.to_owned()),
            debug_assertions: Some(true),
            ..Build::default()
        };
        let set = for_build(&build).expect("a built-in triple answers");

        assert!(set.holds_str(family), "{elsewhere} is a {family} target");
        assert!(!set.holds_str(if family == "unix" { "windows" } else { "unix" }));
        assert!(set.holds_str("target_arch = \"x86_64\""));
        assert_ne!(host.holds_str("windows"), set.holds_str("windows"));
    }

    #[test]
    fn a_profile_without_debug_assertions_strips_the_other_half() {
        let release = for_build(&Build {
            debug_assertions: Some(false),
            ..Build::default()
        })
        .expect("the host answers");
        let debug = for_build(&Build {
            debug_assertions: Some(true),
            ..Build::default()
        })
        .expect("the host answers");

        assert!(!release.holds_str("debug_assertions"));
        assert!(release.holds_str("not(debug_assertions)"));
        assert!(debug.holds_str("debug_assertions"));
        assert!(!debug.holds_str("not(debug_assertions)"));
    }

    #[test]
    fn an_unanswered_profile_leaves_both_halves_mutable() {
        let set = for_build(&Build::default()).expect("the host answers");

        assert!(set.holds_str("debug_assertions"));
        assert!(set.holds_str("not(debug_assertions)"));
        assert!(!set.holds_str("all(debug_assertions, there_is_no_such_predicate)"));
    }

    #[test]
    fn a_custom_predicate_the_build_passes_holds() {
        let set = for_build(&Build {
            cfgs: vec!["loom".to_owned(), "flavor=\"strawberry\"".to_owned()],
            debug_assertions: Some(true),
            ..Build::default()
        })
        .expect("the host answers");

        assert!(set.holds_str("loom"));
        assert!(set.holds_str("flavor = \"strawberry\""));
        assert!(!set.holds_str("flavor = \"vanilla\""));
        assert!(!set.holds_str("kani"));
    }

    #[test]
    fn a_name_the_build_cannot_settle_is_unanswerable() {
        let set = for_build(&Build {
            undecided: vec!["sometimes".to_owned()],
            debug_assertions: Some(true),
            ..Build::default()
        })
        .expect("the host answers");

        assert!(set.holds_str("sometimes"));
        assert!(set.holds_str("not(sometimes)"));
        assert!(!set.holds_str("all(sometimes, there_is_no_such_predicate)"));
    }

    #[test]
    fn a_valued_predicate_the_build_cannot_settle_is_unanswerable() {
        let set = for_build(&Build {
            undecided: vec!["flavor".to_owned()],
            debug_assertions: Some(true),
            ..Build::default()
        })
        .expect("the host answers");

        assert!(set.holds_str("flavor = \"strawberry\""));
        assert!(set.holds_str("not(flavor = \"strawberry\")"));
        assert!(!set.holds_str("all(flavor = \"strawberry\", there_is_no_such_predicate)"));
    }

    #[test]
    fn a_build_of_several_targets_strips_nothing() {
        let build = Build {
            several_targets: true,
            ..Build::default()
        };
        let set = for_build(&build).expect("no probe is needed");

        assert!(set.holds_str("windows"));
        assert!(set.holds_str("unix"));
    }
}
