// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the checks that read compile evidence.
//!
//! These compile real fixtures, so they are slower than the manifest-only
//! tests and live in their own file to keep that cost visible.

// Miri cannot run these tests because they spawn subprocesses and use temp directories.
#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the binary under test.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-unused-deps"))
}

/// Whether the toolchain running the tests can compile doctests without running
/// them, which the tool needs and only nightly offers.
fn nightly() -> bool {
    Command::new("rustc")
        .arg("-vV")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("nightly"))
}

/// A fixture workspace: one `main` package plus a leaf crate per dependency.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// Build a workspace whose `main` package has `body` as its manifest tables
    /// and `source` as its library, with a leaf crate for each name in `leaves`.
    fn new(leaves: &[&str], body: &str, source: &str) -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let members = leaves.iter().map(|leaf| format!("\"{leaf}\"")).collect::<Vec<_>>().join(", ");

        fs::write(
            dir.path().join("Cargo.toml"),
            format!("[workspace]\nmembers = [\"main\", {members}]\nresolver = \"2\"\n"),
        )
        .expect("failed to write workspace manifest");

        for leaf in leaves {
            let crate_dir = dir.path().join(leaf);
            fs::create_dir_all(crate_dir.join("src")).expect("failed to create leaf dir");
            fs::write(
                crate_dir.join("Cargo.toml"),
                format!("[package]\nname = \"{leaf}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .expect("failed to write leaf manifest");
            fs::write(crate_dir.join("src").join("lib.rs"), "pub fn f() {}\n").expect("failed to write leaf source");
        }

        let main = dir.path().join("main");
        fs::create_dir_all(main.join("src")).expect("failed to create main dir");
        fs::write(
            main.join("Cargo.toml"),
            format!("[package]\nname = \"main\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{body}"),
        )
        .expect("failed to write main manifest");
        fs::write(main.join("src").join("lib.rs"), source).expect("failed to write main source");

        Self { dir }
    }

    /// Add a file under the `main` package.
    fn with_file(self, relative: &str, contents: &str) -> Self {
        let path = self.dir.path().join("main").join(relative);
        fs::create_dir_all(path.parent().expect("a file always has a parent")).expect("failed to create dir");
        fs::write(path, contents).expect("failed to write file");
        self
    }

    /// Run the tool over the fixture and return its stderr.
    fn report(&self) -> String {
        let output = Command::new(binary())
            .arg("unused-deps")
            .arg("--manifest-path")
            .arg(self.dir.path().join("Cargo.toml"))
            .arg("--workspace")
            .output()
            .expect("failed to execute the binary");

        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

/// A dependency reference in the manifest body, pointing at a leaf crate.
fn dep(name: &str) -> String {
    format!("{name} = {{ path = \"../{name}\" }}\n")
}

#[test]
fn a_dependency_no_unit_loads_is_unused() {
    if !nightly() {
        return;
    }

    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");

    let report = fixture.report();

    assert!(report.contains("dead: no compiled unit loaded it"), "unexpected report: {report}");
}

#[test]
fn a_dependency_only_tests_use_is_misplaced() {
    if !nightly() {
        return;
    }

    // Used from an integration test, never from the library.
    let fixture = Fixture::new(&["helper"], &format!("[dependencies]\n{}", dep("helper")), "pub fn go() {}\n")
        .with_file("tests/it.rs", "#[test]\nfn t() { helper::f(); }\n");

    let report = fixture.report();

    assert!(
        report.contains("helper: only development units load it"),
        "a dependency used only by tests belongs in [dev-dependencies]: {report}"
    );
}

#[test]
fn a_dependency_only_cfg_test_code_uses_is_misplaced() {
    if !nightly() {
        return;
    }

    // The `cfg(test)` unit of the library is the only user. Distinguishing it
    // from the plain library unit is what the report depends on.
    let fixture = Fixture::new(
        &["helper"],
        &format!("[dependencies]\n{}", dep("helper")),
        "pub fn go() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { helper::f(); }\n}\n",
    );

    let report = fixture.report();

    assert!(
        report.contains("helper: only development units load it"),
        "cfg(test) use is development use: {report}"
    );
}

#[test]
fn evidence_is_counted_per_target_not_per_package() {
    if !nightly() {
        return;
    }

    // A package with a library and two binaries. Each dependency is used in
    // production by exactly one target, so the *other* targets all report it
    // unused. Counting per package would convict them; counting per target and
    // taking the union does not.
    let fixture = Fixture::new(
        &["libonly", "binonly", "bintest"],
        &format!("[dependencies]\n{}{}{}", dep("libonly"), dep("binonly"), dep("bintest")),
        "pub fn go() { libonly::f(); }\n",
    )
    .with_file(
        "src/bin/one.rs",
        "fn main() { binonly::f(); }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { bintest::f(); }\n}\n",
    )
    .with_file("src/bin/two.rs", "fn main() {}\n");

    let report = fixture.report();

    assert!(
        !report.contains("libonly"),
        "used by the library, reported unused by both binaries: {report}"
    );
    assert!(
        !report.contains("binonly"),
        "used by one binary, reported unused by the library and the other binary: {report}"
    );
    assert!(
        report.contains("bintest: only development units load it"),
        "used only by one binary's cfg(test) code: {report}"
    );
}

#[test]
fn several_development_targets_each_testify_separately() {
    if !nightly() {
        return;
    }

    // Each dependency is used by exactly one development target, so every other
    // target reports it unused. Only the one nothing uses should be reported.
    let fixture = Fixture::new(
        &["bya", "byb", "bybench", "byexample", "dead"],
        &format!(
            "[dev-dependencies]\n{}{}{}{}{}",
            dep("bya"),
            dep("byb"),
            dep("bybench"),
            dep("byexample"),
            dep("dead")
        ),
        "pub fn go() {}\n",
    )
    .with_file("tests/a.rs", "#[test]\nfn t() { bya::f(); }\n")
    .with_file("tests/b.rs", "#[test]\nfn t() { byb::f(); }\n")
    .with_file("benches/bench.rs", "fn main() { bybench::f(); }\n")
    .with_file("examples/ex.rs", "fn main() { byexample::f(); }\n");

    let report = fixture.report();

    for used in ["bya", "byb", "bybench", "byexample"] {
        assert!(!report.contains(used), "{used} is used by one development target: {report}");
    }
    assert!(report.contains("dead: no compiled unit loaded it"), "unexpected report: {report}");
}

#[test]
fn a_development_target_compiled_twice_is_counted_like_a_library() {
    if !nightly() {
        return;
    }

    // An example declared `test = true` is compiled twice, exactly as a library
    // is. Treating any report at all as "unused" would convict a dependency its
    // `cfg(test)` code uses, because the plain unit reports while the
    // test-profile unit does not.
    let fixture = Fixture::new(
        &["excfg"],
        &format!("[dev-dependencies]\n{}\n[[example]]\nname = \"ex\"\ntest = true\n", dep("excfg")),
        "pub fn go() {}\n",
    )
    .with_file(
        "examples/ex.rs",
        "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { excfg::f(); }\n}\n",
    );

    let report = fixture.report();

    assert!(!report.contains("excfg"), "the example's cfg(test) unit loaded it: {report}");
}

#[test]
fn a_dependency_used_only_outside_cfg_test_is_not_misplaced() {
    if !nightly() {
        return;
    }

    // The `cfg(test)` unit is *not* a superset of the plain one: this
    // dependency is used from `#[cfg(not(test))]` code, so the plain unit uses
    // it and the `cfg(test)` unit reports it. Inferring "one report must be the
    // plain unit's" would move a production dependency to dev-dependencies.
    let fixture = Fixture::new(
        &["prodonly"],
        &format!("[dependencies]\n{}", dep("prodonly")),
        "#[cfg(not(test))]\npub fn go() { prodonly::f(); }\n\n#[cfg(test)]\npub fn go() {}\n",
    );

    let report = fixture.report();

    assert!(
        !report.contains("prodonly"),
        "the library itself uses it, outside cfg(test): {report}"
    );
}

#[test]
fn a_dependency_the_library_uses_is_not_reported() {
    if !nightly() {
        return;
    }

    let fixture = Fixture::new(
        &["helper"],
        &format!("[dependencies]\n{}", dep("helper")),
        "pub fn go() { helper::f(); }\n",
    );

    let report = fixture.report();

    assert!(!report.contains("helper"), "a used dependency must not be reported: {report}");
}

#[test]
fn an_unused_build_dependency_is_reported() {
    if !nightly() {
        return;
    }

    let fixture = Fixture::new(
        &["used", "dead"],
        &format!("[build-dependencies]\n{}{}", dep("used"), dep("dead")),
        "pub fn go() {}\n",
    )
    .with_file("build.rs", "fn main() { used::f(); }\n");

    let report = fixture.report();

    assert!(report.contains("dead: no compiled unit loaded it"), "unexpected report: {report}");
    assert!(!report.contains("used:"), "the build script's dependency is used: {report}");
}

#[test]
fn one_doctest_among_many_is_enough_to_spare_a_dependency() {
    if !nightly() {
        return;
    }

    // Every doctest is its own compilation with the dev-dependency in scope, so
    // the ones that do not mention it each report it unused. Treating any report
    // as proof of disuse would convict a dependency a single example needs --
    // which is what a real 175-doctest crate looked like.
    let fixture = Fixture::new(
        &["usedonce"],
        &format!("[dev-dependencies]\n{}", dep("usedonce")),
        concat!(
            "//! Lib.\n\n",
            "/// One.\n///\n/// ```\n/// assert_eq!(1 + 1, 2);\n/// ```\npub fn a() {}\n\n",
            "/// Two.\n///\n/// ```\n/// assert_eq!(2 + 2, 4);\n/// ```\npub fn b() {}\n\n",
            "/// Three, the only one that needs it.\n///\n/// ```\n/// usedonce::f();\n/// ```\npub fn c() {}\n",
        ),
    );

    let report = fixture.report();

    assert!(
        !report.contains("usedonce"),
        "one doctest out of three uses it, which is enough: {report}"
    );
}

#[test]
fn a_dev_dependency_only_a_doctest_uses_is_not_reported() {
    if !nightly() {
        return;
    }

    // The case no other tool reaches: `--all-targets` never builds doctests, so
    // without the shim this dependency looks dead.
    let fixture = Fixture::new(
        &["doconly", "deaddev"],
        &format!("[dev-dependencies]\n{}{}", dep("doconly"), dep("deaddev")),
        "//! Lib.\n\n/// Example.\n///\n/// ```\n/// doconly::f();\n/// ```\npub fn go() {}\n",
    );

    let report = fixture.report();

    assert!(
        !report.contains("doconly"),
        "a dependency a doctest uses must not be reported: {report}"
    );
    assert!(
        report.contains("deaddev: no compiled unit loaded it"),
        "a dev-dependency nothing uses is still reported: {report}"
    );
}

#[test]
fn the_allow_list_suppresses_a_finding() {
    if !nightly() {
        return;
    }

    let dir = TempDir::new().expect("failed to create temp dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"main\", \"dead\"]\nresolver = \"2\"\n\n[workspace.metadata.unused-deps]\nallowed = [\"dead\"]\n",
    )
    .expect("failed to write workspace manifest");

    for (name, body) in [("dead", String::new()), ("main", format!("[dependencies]\n{}", dep("dead")))] {
        let crate_dir = dir.path().join(name);
        fs::create_dir_all(crate_dir.join("src")).expect("failed to create dir");
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{body}"),
        )
        .expect("failed to write manifest");
        fs::write(crate_dir.join("src").join("lib.rs"), "pub fn f() {}\n").expect("failed to write source");
    }

    let output = Command::new(binary())
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .expect("failed to execute the binary");

    let report = String::from_utf8_lossy(&output.stderr);
    assert!(
        !report.contains("dead: no compiled unit loaded"),
        "an allowed name is not reported: {report}"
    );
}

#[test]
fn no_package_selector_runs_only_the_catalog_check() {
    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");

    let output = Command::new(binary())
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .output()
        .expect("failed to execute the binary");

    // `dead` is unused, but only the manifest-only check was asked for, and it
    // has nothing to say about a member's own declarations.
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(!report.contains("dead:"), "the catalog check does not judge declarations: {report}");
    assert!(output.status.success(), "the catalog is clean here");
}

/// The tool's own workspace is the acceptance test: it must not accuse a
/// dependency this repository legitimately uses.
#[test]
fn the_tools_own_catalog_is_clean() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives two levels below the workspace root")
        .join("Cargo.toml");

    let output = Command::new(binary())
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "this repository's catalog should be clean: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn package_selection_limits_compile_evidence() {
    if !nightly() {
        return;
    }

    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");
    let manifest = fixture.dir.path().join("Cargo.toml");

    let leaf = Command::new(binary())
        .args(["unused-deps", "--manifest-path"])
        .arg(&manifest)
        .args(["--package", "dead"])
        .output()
        .expect("failed to execute the binary");
    assert!(leaf.status.success(), "the unselected main package must not be judged");
    assert!(!String::from_utf8_lossy(&leaf.stderr).contains("dead: no compiled unit loaded"));

    let main = Command::new(binary())
        .args(["unused-deps", "--manifest-path"])
        .arg(&manifest)
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");
    assert!(!main.status.success(), "the selected main package contains an unused dependency");
    assert!(String::from_utf8_lossy(&main.stderr).contains("dead: no compiled unit loaded"));
}

#[test]
fn workspace_exclude_limits_compile_evidence() {
    if !nightly() {
        return;
    }

    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");
    let output = Command::new(binary())
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--workspace", "--exclude", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(output.status.success(), "the excluded main package must not be judged");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("dead: no compiled unit loaded"));
}
