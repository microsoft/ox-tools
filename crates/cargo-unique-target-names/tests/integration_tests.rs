// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Drives the built `cargo-unique-target-names` binary against synthetic cargo
//! workspaces, so the rule is verified end to end rather than as a unit.

// miri cannot sandbox the filesystem and subprocess work these tests do.
#![cfg(not(miri))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fs};

use tempfile::{Builder, TempDir};

/// A workspace member: its directory and package name, plus the names of the
/// uplifted targets it declares.
struct Member {
    directory: String,
    package: String,
    example: String,
    binary: String,
    library_example: String,
    library: Option<(String, String)>,
}

impl Member {
    /// A member whose uplifted target names are all distinct, so nothing
    /// collides unless a test asks for it. Its library keeps the default
    /// (unique) package target name and the default `lib` crate type.
    fn new(package: &str, example: &str, binary: &str, library_example: &str) -> Self {
        Self {
            directory: package.to_owned(),
            package: package.to_owned(),
            example: example.to_owned(),
            binary: binary.to_owned(),
            library_example: library_example.to_owned(),
            library: None,
        }
    }

    /// The same, with an explicit library name and crate type so a
    /// library-level collision can be provoked.
    fn with_library(mut self, name: &str, crate_type: &str) -> Self {
        self.library = Some((name.to_owned(), crate_type.to_owned()));
        self
    }

    /// The same, in a directory that does not match the package name. Needed to
    /// give two members package names differing only in case, which their
    /// directories cannot do on a case-insensitive filesystem.
    fn in_directory(mut self, directory: &str) -> Self {
        directory.clone_into(&mut self.directory);
        self
    }
}

/// Builds a workspace whose members all declare the same duplicated `test` and
/// `bench` names -- the only kinds Cargo never uplifts -- plus the per-member
/// uplifted names given by `members`.
fn fixture(members: &[Member]) -> TempDir {
    let temp = Builder::new()
        .prefix("unique target names repo's [copy] (fork) ")
        .tempdir()
        .expect("temporary repository must be creatable");
    let root = temp.path();

    let list = members
        .iter()
        .map(|member| format!("\"{}\"", member.directory))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"3\"\nmembers = [{list}]\n"),
    )
    .expect("workspace manifest must be writable");

    for member in members {
        let package = root.join(&member.directory);
        for directory in ["src", "src/bin", "examples", "tests", "benches"] {
            fs::create_dir_all(package.join(directory)).expect("member directories must be creatable");
        }
        let library = match &member.library {
            Some((name, crate_type)) => {
                format!("[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\ncrate-type = [\"{crate_type}\"]\n")
            }
            None => "[lib]\npath = \"src/lib.rs\"\n".to_owned(),
        };
        fs::write(
            package.join("Cargo.toml"),
            format!(
                concat!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
                    "{library}\n",
                    "[[example]]\nname = \"{library_example}\"\npath = \"examples/{library_example}.rs\"\ncrate-type = [\"lib\"]\n"
                ),
                name = member.package,
                library = library,
                library_example = member.library_example
            ),
        )
        .expect("member manifest must be writable");
        fs::write(package.join("src/lib.rs"), "").expect("member library must be writable");
        fs::write(package.join(format!("src/bin/{}.rs", member.binary)), "fn main() {}\n").expect("member binary must be writable");
        fs::write(package.join(format!("examples/{}.rs", member.example)), "fn main() {}\n").expect("member example must be writable");
        fs::write(package.join(format!("examples/{}.rs", member.library_example)), "").expect("library example must be writable");
        // Duplicated across members on purpose: Cargo keeps a metadata hash on
        // these, so they must never be reported.
        fs::write(package.join("tests/shared.rs"), "").expect("member test must be writable");
        fs::write(package.join("benches/shared.rs"), "fn main() {}\n").expect("member bench must be writable");
    }
    temp
}

/// The binary under test, as cargo built it next to the integration test.
fn binary() -> PathBuf {
    let mut path = env::current_exe().expect("the test binary must have a path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("cargo-unique-target-names{}", env::consts::EXE_SUFFIX))
}

fn run(root: &Path) -> Output {
    Command::new(binary())
        .args(["unique-target-names"])
        .current_dir(root)
        .output()
        .expect("the built binary must be runnable")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A bin-crate-type example uplifts to `examples/<name>` and its debug-info
/// sibling, and both are reported together.
#[test]
fn fails_when_two_packages_share_an_uplifted_example_name() {
    let temp = fixture(&[
        Member::new("alpha", "shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "shared", "beta_tool", "beta_lib_example"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(!output.status.success(), "collision must fail the check: {diagnostic}");
    assert!(
        diagnostic.contains("target 'shared' is declared by 2 workspace packages: alpha (example), beta (example)"),
        "diagnostic must name the target, every owning package, and how each spells it: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/examples/shared[.exe]"),
        "diagnostic must name the contested file: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/examples/shared.pdb"),
        "every contested file must be listed, not just the primary one: {diagnostic}"
    );
    assert!(
        diagnostic.contains("they uplift to the same files: "),
        "an example contends for two files, so the wording is plural: {diagnostic}"
    );
}

#[test]
fn fails_when_two_packages_share_an_uplifted_binary_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "tool", "beta_lib_example"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(!output.status.success(), "collision must fail the check: {diagnostic}");
    assert!(
        diagnostic.contains("target 'tool' is declared by 2 workspace packages: alpha (binary), beta (binary)"),
        "duplicate binaries must be reported: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/tool[.exe]"),
        "diagnostic must name the contested file: {diagnostic}"
    );
}

/// The motivating accident: Cargo has already normalized `-` to `_` in the
/// default library target name, so two packages whose names differ only in that
/// separator collide with no configuration at all.
#[test]
fn fails_when_default_library_names_normalize_to_one_target() {
    let temp = fixture(&[
        Member::new("foo-bar", "dash_example", "dash_tool", "dash_lib_example"),
        Member::new("foo_bar", "score_example", "score_tool", "score_lib_example"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "packages differing only by the name separator collide and must fail the check: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target 'foo_bar' is declared by 2 workspace packages: foo-bar (library), foo_bar (library)"),
        "diagnostic must report the normalized target name against both packages: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/libfoo_bar.rlib"),
        "diagnostic must name the contested file: {diagnostic}"
    );
}

/// An ordinary library uplifts its rlib, so an explicit duplicate `[lib] name`
/// collides even with the default crate type.
#[test]
fn fails_when_two_packages_share_an_explicit_library_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "lib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "lib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "an ordinary library uplifts its rlib and must be guarded: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/libshared_lib.rlib"),
        "diagnostic must name the contested file: {diagnostic}"
    );
}

/// A library-crate-type example uplifts its rlib into the examples directory.
#[test]
fn fails_when_two_packages_share_a_library_example_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "library_example"),
        Member::new("beta", "beta_shared", "beta_tool", "library_example"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "a library-crate-type example still uplifts and must be guarded: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/examples/liblibrary_example.rlib"),
        "diagnostic must name the contested file: {diagnostic}"
    );
}

/// `cdylib` and `dylib` emit the same platform file, so the collision crosses
/// the two crate types.
#[test]
fn fails_when_a_cdylib_and_a_dylib_share_a_library_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "cdylib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "dylib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "a cdylib and a dylib of the same name collide and must fail the check: {diagnostic}"
    );
    assert!(
        diagnostic.contains("alpha (shared library), beta (shared library)"),
        "diagnostic must report the emitted family, not the crate type: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/[lib]shared_lib[.so|.dll|.dylib]"),
        "diagnostic must name the platform-specific contested file: {diagnostic}"
    );
}

/// A proc-macro emits the same platform file as a `cdylib`.
#[test]
fn fails_when_a_proc_macro_and_a_cdylib_share_a_library_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "proc-macro"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "cdylib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "a proc-macro and a cdylib of the same name collide and must fail the check: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/[lib]shared_lib[.so|.dll|.dylib]"),
        "diagnostic must name the platform-specific contested file: {diagnostic}"
    );
}

/// A binary and a shared library of one name emit different primary files but
/// the same Windows debug-info file, so the collision is visible only when
/// every emitted file is keyed.
#[test]
fn fails_when_a_binary_and_a_shared_library_share_a_debug_info_file() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("tool", "cdylib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "a binary and a shared library of one name share a pdb and must fail the check: {diagnostic}"
    );
    assert!(
        diagnostic.contains("alpha (binary), beta (shared library)"),
        "diagnostic must describe how each package spells the target: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/tool.pdb"),
        "the debug-info file is the contested one and must be named: {diagnostic}"
    );
    assert!(
        !diagnostic.contains("target/<profile>/tool[.exe]"),
        "the primary files differ and must not be reported as contested: {diagnostic}"
    );
}

/// An rlib emits no uplifted debug-info file, so it shares a directory with a
/// shared library of the same name without colliding.
#[test]
fn passes_when_an_rlib_and_a_cdylib_share_a_library_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "lib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "cdylib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        output.status.success(),
        "families that emit different file names must not be reported: {diagnostic}"
    );
}

/// The static-library / shared-library boundary.
#[test]
fn passes_when_a_staticlib_and_a_cdylib_share_a_library_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "staticlib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "cdylib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        output.status.success(),
        "a static library and a shared library of one name emit different files: {diagnostic}"
    );
}

/// A static library likewise emits no uplifted debug-info file.
#[test]
fn passes_when_a_staticlib_and_a_binary_share_a_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("tool", "staticlib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        output.status.success(),
        "a static library emits no pdb and must not contend with a binary: {diagnostic}"
    );
}

/// Cargo keys its own collision check by exact path, so names that differ only
/// in case are distinct targets and must not be merged.
#[test]
fn passes_when_library_names_differ_only_by_case() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("Shared_lib", "lib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "lib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        output.status.success(),
        "names differing only by case are distinct to Cargo and must not be merged: {diagnostic}"
    );
}

/// Owners are keyed by package name, so packages differing only in case must
/// both survive rather than one overwriting the other and hiding a collision.
#[test]
fn fails_when_packages_differing_only_by_case_share_a_binary_name() {
    let temp = fixture(&[
        Member::new("Shared", "upper_example", "tool", "upper_lib_example").in_directory("upper"),
        Member::new("shared", "lower_example", "tool", "lower_lib_example").in_directory("lower"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "two packages owning one binary name collide however their own names are cased: {diagnostic}"
    );
    assert!(
        diagnostic.contains("is declared by 2 workspace packages: Shared (binary), shared (binary)"),
        "both owners must survive: {diagnostic}"
    );
}

/// Two static libraries of one name contend for the archive, which is what
/// gives the static-library family a reason to exist.
#[test]
fn fails_when_two_packages_share_a_static_library_name() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example").with_library("shared_lib", "staticlib"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example").with_library("shared_lib", "staticlib"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "two static libraries of one name collide and must fail the check: {diagnostic}"
    );
    assert!(
        diagnostic.contains("alpha (static library), beta (static library)"),
        "diagnostic must report the static-library family: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target/<profile>/[lib]shared_lib[.a|.lib]"),
        "diagnostic must name the contested archive: {diagnostic}"
    );
    assert!(
        diagnostic.contains("they uplift to the same file: "),
        "a static library contends for exactly one file, so the wording is singular: {diagnostic}"
    );
}

/// The manifest can be named explicitly instead of discovered from the
/// working directory.
#[test]
fn accepts_an_explicit_manifest_path() {
    let temp = fixture(&[
        Member::new("alpha", "shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "shared", "beta_tool", "beta_lib_example"),
    ]);
    let output = Command::new(binary())
        .args(["unique-target-names", "--manifest-path"])
        .arg(temp.path().join("Cargo.toml"))
        .current_dir(env::temp_dir())
        .output()
        .expect("the built binary must be runnable");
    let diagnostic = combined(&output);
    assert!(
        !output.status.success(),
        "the workspace named by --manifest-path must be the one inspected: {diagnostic}"
    );
    assert!(
        diagnostic.contains("target 'shared' is declared by 2 workspace packages"),
        "diagnostic must describe the named workspace: {diagnostic}"
    );
}

#[test]
fn passes_when_only_hashed_target_kinds_repeat() {
    let temp = fixture(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example"),
    ]);
    let output = run(temp.path());
    let diagnostic = combined(&output);
    assert!(
        output.status.success(),
        "duplicate test and bench names must not fail the check: {diagnostic}"
    );
    assert!(
        diagnostic.contains("All workspace targets uplift to their own path"),
        "a clean workspace must say so: {diagnostic}"
    );
}
