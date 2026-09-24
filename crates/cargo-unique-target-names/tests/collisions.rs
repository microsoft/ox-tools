// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The collision rule, exercised through the library against synthetic cargo
//! workspaces. The rule is the crate's substance, so it is tested by calling
//! `check` directly rather than through the binary; `cli.rs` covers the
//! command-line contract.

// miri cannot sandbox the filesystem work these fixtures do, nor the `cargo
// metadata` subprocess that reads them.
#![cfg(not(miri))]

mod common;

use cargo_unique_target_names::{Collision, Outcome, check};
use common::{Member, workspace};

/// The collisions found in a workspace built from `members`, rendered as the
/// tool would print them, or the empty string when there are none.
fn report(members: &[Member]) -> String {
    let temp = workspace(members);
    match check(Some(&temp.path().join("Cargo.toml"))) {
        Outcome::Clean => String::new(),
        Outcome::Contended(collisions) => collisions.iter().map(Collision::render).collect::<Vec<_>>().join("\n"),
        other => panic!("the fixture workspace must be readable by cargo: {}", other.render()),
    }
}

/// A bin-crate-type example uplifts to `examples/<name>` and its debug-info
/// sibling, and both are reported together.
#[test]
fn reports_two_packages_sharing_an_uplifted_example_name() {
    let report = report(&[
        Member::new("alpha", "shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "shared", "beta_tool", "beta_lib_example"),
    ]);
    assert!(
        report.contains("declared by alpha (example 'shared'), beta (example 'shared')"),
        "{report}"
    );
    assert!(report.contains("target/<profile>/examples/shared[.exe]"), "{report}");
    assert!(
        report.contains("target/<profile>/examples/shared.pdb"),
        "every contested file must be listed, not just the primary one: {report}"
    );
    assert!(
        report.contains("2 targets uplift to the same files: "),
        "an example contends for two files, so the wording is plural: {report}"
    );
}

#[test]
fn reports_two_packages_sharing_an_uplifted_binary_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "tool", "beta_lib_example"),
    ]);
    assert!(
        report.contains("2 targets uplift to the same files: target/<profile>/tool.pdb, target/<profile>/tool[.exe]"),
        "{report}"
    );
    assert!(
        report.contains("declared by alpha (binary 'tool'), beta (binary 'tool')"),
        "{report}"
    );
}

/// The motivating accident: Cargo has already normalized `-` to `_` in the
/// default library target name, so two packages whose names differ only in that
/// separator collide with no configuration at all.
#[test]
fn reports_default_library_names_that_normalize_to_one_target() {
    let report = report(&[
        Member::new("foo-bar", "dash_example", "dash_tool", "dash_lib_example"),
        Member::new("foo_bar", "score_example", "score_tool", "score_lib_example"),
    ]);
    assert!(
        report.contains("declared by foo-bar (library 'foo_bar'), foo_bar (library 'foo_bar')"),
        "{report}"
    );
    assert!(report.contains("target/<profile>/libfoo_bar.rlib"), "{report}");
}

/// An ordinary library uplifts its rlib, so an explicit duplicate `[lib] name`
/// collides even with the default crate type.
#[test]
fn reports_two_packages_sharing_an_explicit_library_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "lib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "lib"),
    ]);
    assert!(report.contains("target/<profile>/libshared_lib.rlib"), "{report}");
    assert!(
        report.contains("2 targets uplift to the same file: "),
        "an rlib contends for exactly one file, so the wording is singular: {report}"
    );
}

/// A library-crate-type example uplifts its rlib into the examples directory.
#[test]
fn reports_two_packages_sharing_a_library_example_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "library_example"),
        Member::new("beta", "beta_shared", "beta_tool", "library_example"),
    ]);
    assert!(report.contains("target/<profile>/examples/liblibrary_example.rlib"), "{report}");
}

/// `cdylib` and `dylib` emit the same platform file, so the collision crosses
/// the two crate types.
#[test]
fn reports_a_cdylib_and_a_dylib_sharing_a_library_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "cdylib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "dylib"),
    ]);
    assert!(
        report.contains("alpha (shared library 'shared_lib'), beta (shared library 'shared_lib')"),
        "the emitted family is reported, not the crate type: {report}"
    );
    assert!(report.contains("target/<profile>/[lib]shared_lib[.so|.dll|.dylib]"), "{report}");
}

/// A proc-macro emits the same platform file as a `cdylib`.
#[test]
fn reports_a_proc_macro_and_a_cdylib_sharing_a_library_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "proc-macro"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "cdylib"),
    ]);
    assert!(report.contains("target/<profile>/[lib]shared_lib[.so|.dll|.dylib]"), "{report}");
}

/// Two static libraries of one name contend for the archive, which is what
/// gives the static-library family a reason to exist.
#[test]
fn reports_two_packages_sharing_a_static_library_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "staticlib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "staticlib"),
    ]);
    assert!(
        report.contains("alpha (static library 'shared_lib'), beta (static library 'shared_lib')"),
        "{report}"
    );
    assert!(report.contains("target/<profile>/[lib]shared_lib[.a|.lib]"), "{report}");
}

/// A binary and a shared library of one name emit different primary files but
/// the same Windows debug-info file, so the collision is visible only when
/// every emitted file is keyed.
#[test]
fn reports_a_binary_and_a_shared_library_sharing_a_debug_info_file() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("tool", "cdylib"),
    ]);
    assert!(report.contains("alpha (binary 'tool'), beta (shared library 'tool')"), "{report}");
    assert!(report.contains("target/<profile>/tool.pdb"), "{report}");
    assert!(
        !report.contains("target/<profile>/tool[.exe]"),
        "the primary files differ and must not be reported as contested: {report}"
    );
}

/// Owners are keyed by package name, so packages differing only in case must
/// both survive rather than one overwriting the other and hiding a collision.
#[test]
fn reports_both_owners_when_package_names_differ_only_by_case() {
    let report = report(&[
        Member::new("Shared", "upper_example", "tool", "upper_lib_example").in_directory("upper"),
        Member::new("shared", "lower_example", "tool", "lower_lib_example").in_directory("lower"),
    ]);
    assert!(report.contains("2 targets uplift to the same files"), "{report}");
    assert!(
        report.contains("declared by Shared (binary 'tool'), shared (binary 'tool')"),
        "{report}"
    );
}

/// An rlib emits neither the shared-library file nor a debug-info file, so it
/// shares a directory with a shared library of one name without contending.
#[test]
fn accepts_an_rlib_beside_a_cdylib_of_one_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "lib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "cdylib"),
    ]);
    assert!(report.is_empty(), "{report}");
}

/// A static library emits no debug-info file, so it does not contend with a
/// binary of one name.
#[test]
fn accepts_a_staticlib_beside_a_binary_of_one_name() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("tool", "staticlib"),
    ]);
    assert!(report.is_empty(), "{report}");
}

/// Cargo keys its own collision check by exact path, so names that differ only
/// in case are distinct targets and are not merged.
#[test]
fn accepts_library_names_differing_only_by_case() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("Shared_lib", "lib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "lib"),
    ]);
    assert!(report.is_empty(), "{report}");
}

#[test]
fn accepts_repeated_test_and_bench_names() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example"),
    ]);
    assert!(report.is_empty(), "{report}");
}

/// Cargo replaces `-` with `_` when deriving library and debug-info file
/// names, but not executable names. Two binaries named `foo-bar` and `foo_bar`
/// therefore produce distinct executables and a single `foo_bar.pdb`.
///
/// No single target name describes both owners here, which is why the report
/// leads with the contended file rather than a name: a headline naming either
/// spelling would be wrong for the other owner.
#[test]
fn reports_binaries_whose_names_differ_only_by_separator() {
    let report = report(&[
        Member::new("alpha", "alpha_shared", "foo-bar", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "foo_bar", "beta_lib_example"),
    ]);
    assert!(
        report.contains("2 targets uplift to the same file: target/<profile>/foo_bar.pdb"),
        "the contended file is the headline, and its name is normalized: {report}"
    );
    assert!(
        report.contains("declared by alpha (binary 'foo-bar'), beta (binary 'foo_bar')"),
        "each owner must name the target as it declares it, or the rename is not actionable: {report}"
    );
    assert!(
        !report.contains("targets uplift to the same file: target/<profile>/foo-bar"),
        "the report must not headline a name only one owner declares: {report}"
    );
    assert!(
        !report.contains("[.exe]"),
        "the executables keep their own names and must not be reported: {report}"
    );
}

/// Cargo permits a `[lib]` and a `[[bin]]` of one name in a single package, and
/// they contend for the debug-info file. Owners are keyed per target, not per
/// package, so the pair is reported rather than collapsing into one owner.
#[test]
fn reports_a_library_and_a_binary_of_one_name_in_a_single_package() {
    let report = report(&[Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example").with_library("tool", "cdylib")]);
    assert!(
        report.contains("2 targets uplift to the same file: target/<profile>/tool.pdb"),
        "{report}"
    );
    assert!(
        report.contains("declared by alpha (binary 'tool'), alpha (shared library 'tool')"),
        "both targets must be named, each with its own kind: {report}"
    );
}

/// Two target declarations may point at one source file, so target identity
/// cannot be the source path. A `[lib]` and a `[[bin]]` of one name both built
/// from `src/lib.rs` still contend for the debug-info file.
#[test]
fn reports_two_targets_that_share_one_source_file() {
    let report = report(&[Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example")
        .with_library("twin", "cdylib")
        .with_extra_manifest("[[bin]]\nname = \"twin\"\npath = \"src/lib.rs\"\n")]);
    assert!(
        report.contains("2 targets uplift to the same file: target/<profile>/twin.pdb"),
        "{report}"
    );
    assert!(
        report.contains("declared by alpha (binary 'twin'), alpha (shared library 'twin')"),
        "targets sharing a source file must still count separately: {report}"
    );
}
