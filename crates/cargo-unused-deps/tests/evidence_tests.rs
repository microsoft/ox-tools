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
    let test = std::env::current_exe().expect("failed to locate the test executable");
    test.parent()
        .and_then(Path::parent)
        .expect("integration tests run from the profile's deps directory")
        .join(format!("cargo-unused-deps{}", std::env::consts::EXE_SUFFIX))
}

#[test]
fn a_bin_only_finding_does_not_try_to_compile_doctests() {
    let dir = TempDir::new().expect("failed to create temp dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"dead\"]\nresolver = \"2\"\n",
    )
    .expect("failed to write workspace manifest");

    for (name, body, source) in [
        ("dead", "", "pub fn f() {}\n"),
        ("app", "[dependencies]\ndead = { path = \"../dead\" }\n", "fn main() {}\n"),
    ] {
        let crate_dir = dir.path().join(name);
        fs::create_dir_all(crate_dir.join("src")).expect("failed to create source dir");
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{body}"),
        )
        .expect("failed to write manifest");
        let source_name = if name == "app" { "main.rs" } else { "lib.rs" };
        fs::write(crate_dir.join("src").join(source_name), source).expect("failed to write source");
    }

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"))
        .args(["--package", "app"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "the bin has an unused dependency");
    assert!(String::from_utf8_lossy(&output.stderr).contains("dead: no compiled unit loaded it"));
}

/// Command the binary under test with nightly-only rustdoc options enabled.
///
/// `RUSTC_BOOTSTRAP` is confined to synthetic fixture builds. It lets the same
/// behavioral tests run under cargo-mutants' stable compiler.
fn command() -> Command {
    let mut command = Command::new(binary());
    command.env("RUSTC_BOOTSTRAP", "1");
    command
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
        let output = command()
            .arg("unused-deps")
            .arg("--manifest-path")
            .arg(self.dir.path().join("Cargo.toml"))
            .arg("--workspace")
            .output()
            .expect("failed to execute the binary");

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            stderr.into_owned()
        }
    }
}

/// A dependency reference in the manifest body, pointing at a leaf crate.
fn dep(name: &str) -> String {
    format!("{name} = {{ path = \"../{name}\" }}\n")
}

#[test]
fn a_dependency_no_unit_loads_is_unused() {
    let fixture = Fixture::new(
        &["dead"],
        &format!("[dependencies]\n{}", dep("dead")),
        "#![allow(unused_crate_dependencies)]\npub fn go() {}\n",
    );

    let report = fixture.report();

    assert!(report.contains("dead: no compiled unit loaded it"), "unexpected report: {report}");
}

#[test]
fn encoded_rustflags_are_preserved_by_the_compiler_wrapper() {
    let fixture = Fixture::new(
        &["used"],
        &format!("[dependencies]\n{}", dep("used")),
        "#[cfg(custom_evidence)]\npub fn go() { used::f(); }\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .env("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}custom_evidence")
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "the wrapper must preserve encoded flags that select dependency-using code: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inactive_target_dependencies_are_not_reported() {
    let fixture = Fixture::new(
        &["inactive"],
        &format!("[target.'cfg(any())'.dependencies]\n{}", dep("inactive")),
        "pub fn go() {}\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "an inactive target dependency is outside the evidence scope: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn dependencies_required_by_features_are_not_reported() {
    let fixture = Fixture::new(
        &["forwarded", "required", "bare"],
        concat!(
            "[dependencies]\n",
            "forwarded = { path = \"../forwarded\", optional = true }\n",
            "required = { path = \"../required\" }\n",
            "bare = { path = \"../bare\", optional = true }\n\n",
            "[features]\napi = [\"forwarded/forwarded-feature\", \"required/required-feature\", \"bare\"]\n",
        ),
        "pub fn go() {}\n",
    );
    for (name, feature) in [("forwarded", "forwarded-feature"), ("required", "required-feature")] {
        fs::write(
            fixture.dir.path().join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\n{feature} = []\n"),
        )
        .expect("failed to add the forwarded feature");
    }

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "dependencies referenced by features are load-bearing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_specific_findings_name_the_manifest_table() {
    let fixture = Fixture::new(
        &["dead"],
        &format!("[target.'cfg(all())'.dependencies]\n{}", dep("dead")),
        "pub fn go() {}\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the active target dependency is unused");
    assert!(
        stderr.contains("main [target.'cfg(all())'.dependencies] dead"),
        "the report must identify the target table: {stderr}"
    );
    assert!(
        stderr.contains("remove it from [target.'cfg(all())'.dependencies]"),
        "the remediation must preserve the target predicate: {stderr}"
    );
}

#[test]
fn target_specific_misplaced_remediation_preserves_the_predicate() {
    let fixture = Fixture::new(
        &["helper"],
        &format!("[target.'cfg(all())'.dependencies]\n{}", dep("helper")),
        "pub fn go() {}\n",
    )
    .with_file("tests/it.rs", "#[test]\nfn t() { helper::f(); }\n");

    let report = fixture.report();

    assert!(
        report.contains("main [target.'cfg(all())'.dependencies] helper: only development units load it"),
        "the finding must identify the source target table: {report}"
    );
    assert!(
        report.contains("move it to [target.'cfg(all())'.dev-dependencies]"),
        "the remediation must preserve the target predicate: {report}"
    );
}

#[test]
fn duplicate_target_declarations_are_not_judged_from_shared_extern_evidence() {
    let fixture = Fixture::new(
        &["dead"],
        &format!(
            "[target.'cfg(all())'.dependencies]\n{}\n[target.'cfg(any())'.dependencies]\n{}",
            dep("dead"),
            dep("dead")
        ),
        "pub fn go() {}\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "rustc evidence cannot distinguish duplicate target declarations: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn filtered_success_does_not_claim_every_dependency_is_used() {
    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");
    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main", "--check", "misplaced"])
        .output()
        .expect("failed to execute the binary");

    assert!(output.status.success(), "the unused-only finding was filtered out");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No selected dependency problems found"),
        "unexpected stdout: {stdout}"
    );
    assert!(!stdout.contains("Every declared dependency"), "unexpected stdout: {stdout}");
}

#[test]
fn a_dependency_only_tests_use_is_misplaced() {
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
    let repeated = fixture.report();
    assert!(
        !repeated.contains("usedonce"),
        "a repeated run must compile fresh doctest evidence: {repeated}"
    );
}

#[test]
fn a_dev_dependency_only_a_doctest_uses_is_not_reported() {
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
fn a_proc_macro_doctest_can_spare_a_dependency() {
    let dir = TempDir::new().expect("failed to create temp dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"macro-fixture\", \"doctestdep\"]\nresolver = \"2\"\n",
    )
    .expect("failed to write workspace manifest");

    let dependency = dir.path().join("doctestdep");
    fs::create_dir_all(dependency.join("src")).expect("failed to create dependency source dir");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"doctestdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("failed to write dependency manifest");
    fs::write(dependency.join("src/lib.rs"), "pub fn used() {}\n").expect("failed to write dependency source");

    let proc_macro = dir.path().join("macro-fixture");
    fs::create_dir_all(proc_macro.join("src")).expect("failed to create proc-macro source dir");
    fs::write(
        proc_macro.join("Cargo.toml"),
        concat!(
            "[package]\nname = \"macro-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
            "[lib]\nproc-macro = true\n\n",
            "[dependencies]\ndoctestdep = { path = \"../doctestdep\" }\n",
        ),
    )
    .expect("failed to write proc-macro manifest");
    fs::write(
        proc_macro.join("src/lib.rs"),
        concat!(
            "extern crate proc_macro;\nuse proc_macro::TokenStream;\n\n",
            "/// ```rust\n/// doctestdep::used();\n/// ```\n",
            "#[proc_macro]\npub fn fixture(_input: TokenStream) -> TokenStream { TokenStream::new() }\n",
        ),
    )
    .expect("failed to write proc-macro source");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"))
        .args(["--package", "macro-fixture"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "the normal dependency is used only for development");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("doctestdep: only development units load it"),
        "unexpected report: {stderr}"
    );
    assert!(
        !stderr.contains("doctestdep: no compiled unit loaded it"),
        "the proc-macro doctest used it: {stderr}"
    );
}

#[test]
fn the_allow_list_suppresses_a_finding() {
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

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"))
        .arg("--workspace")
        .output()
        .expect("failed to execute the binary");

    let report = String::from_utf8_lossy(&output.stderr);
    assert!(
        !report.contains("dead: no compiled unit loaded"),
        "an allowed name is not reported: {report}"
    );
    assert!(
        !report.contains("allow-list entry can be removed"),
        "a declared source suppression is not stale: {report}"
    );
}

#[test]
fn no_package_selector_runs_only_the_catalog_check() {
    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");

    let output = command()
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

#[test]
fn exclude_requires_workspace_selection() {
    let fixture = Fixture::new(&[], "", "pub fn go() {}\n");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--exclude", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "--exclude without --workspace must be rejected");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--workspace"));
}

#[test]
fn package_and_workspace_selection_conflict() {
    let fixture = Fixture::new(&[], "", "pub fn go() {}\n");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--workspace", "--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "--workspace and --package must be rejected together");
}

#[test]
fn unknown_package_selection_fails_loudly() {
    let fixture = Fixture::new(&[], "", "pub fn go() {}\n");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "missing"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "an unknown package selector must be rejected");
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not match any workspace member"));
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

    let output = command()
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
    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");
    let manifest = fixture.dir.path().join("Cargo.toml");

    let leaf = command()
        .args(["unused-deps", "--manifest-path"])
        .arg(&manifest)
        .args(["--package", "dead"])
        .output()
        .expect("failed to execute the binary");
    assert!(leaf.status.success(), "the unselected main package must not be judged");
    assert!(!String::from_utf8_lossy(&leaf.stderr).contains("dead: no compiled unit loaded"));
    assert!(String::from_utf8_lossy(&leaf.stdout).contains("in 1 package"));

    let main = command()
        .args(["unused-deps", "--manifest-path"])
        .arg(&manifest)
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");
    assert!(!main.status.success(), "the selected main package contains an unused dependency");
    assert!(String::from_utf8_lossy(&main.stderr).contains("dead: no compiled unit loaded"));

    let repeated = command()
        .args(["unused-deps", "--manifest-path"])
        .arg(&manifest)
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary again");
    assert!(!repeated.status.success(), "a repeated analysis must retain the finding");
}

#[test]
fn transitive_workspace_packages_are_not_treated_as_selected_roots() {
    let fixture = Fixture::new(
        &["middle", "leaf"],
        &format!("[dependencies]\n{}", dep("middle")),
        "pub fn go() { middle::f(); }\n",
    );
    fs::write(
        fixture.dir.path().join("middle/Cargo.toml"),
        concat!(
            "[package]\nname = \"middle\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
            "[dependencies]\nleaf = { path = \"../leaf\" }\n",
        ),
    )
    .expect("failed to add the middle package dependency");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "the transitive middle package is outside the selected roots: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("middle [dependencies] leaf"));
}

#[test]
fn workspace_exclude_limits_compile_evidence() {
    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n");
    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--workspace", "--exclude", "main", "--exclude", "optional"])
        .output()
        .expect("failed to execute the binary");

    assert!(output.status.success(), "the excluded main package must not be judged");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("dead: no compiled unit loaded"));
}

#[test]
fn package_checks_support_a_non_workspace_manifest() {
    let dir = TempDir::new().expect("failed to create temp dir");
    fs::create_dir_all(dir.path().join("src")).expect("failed to create source dir");
    fs::create_dir_all(dir.path().join("side/src")).expect("failed to create dependency source dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        concat!(
            "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
            "[package.metadata.unused-deps]\nallowed = [\"side\"]\n\n",
            "[dependencies]\nside = { path = \"side\" }\n",
        ),
    )
    .expect("failed to write manifest");
    fs::write(dir.path().join("src/lib.rs"), "pub fn go() {}\n").expect("failed to write source");
    fs::write(
        dir.path().join("side/Cargo.toml"),
        "[package]\nname = \"side\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("failed to write dependency manifest");
    fs::write(dir.path().join("side/src/lib.rs"), "pub fn side_effect() {}\n").expect("failed to write dependency source");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"))
        .args(["--package", "solo"])
        .output()
        .expect("failed to execute the binary");

    assert!(output.status.success(), "package analysis should support a standalone crate");
    assert!(String::from_utf8_lossy(&output.stdout).contains("in 1 package"));
}

#[test]
fn a_failing_doctest_fails_evidence_collection() {
    let fixture = Fixture::new(
        &["dead"],
        &format!("[dependencies]\n{}", dep("dead")),
        "/// ```rust\n/// this is not rust\n/// ```\npub fn go() {}\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "invalid doctests must fail the check");
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed while collecting doctest evidence"));
}

#[test]
fn misplaced_only_skips_doctests_for_unselected_unused_dev_findings() {
    let fixture = Fixture::new(
        &["dead"],
        &format!("[dev-dependencies]\n{}", dep("dead")),
        "/// ```rust\n/// this is not rust\n/// ```\npub fn go() {}\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main", "--check", "misplaced"])
        .output()
        .expect("failed to execute the binary");

    assert!(
        output.status.success(),
        "an unselected unused dev finding must not trigger doctest collection: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn misplaced_finding_does_not_collect_unneeded_doctest_evidence() {
    let fixture = Fixture::new(
        &["helper"],
        &format!("[dependencies]\n{}", dep("helper")),
        "/// ```rust\n/// this is not rust\n/// ```\npub fn go() {}\n",
    )
    .with_file("tests/it.rs", "#[test]\nfn t() { helper::f(); }\n");

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main", "--check", "misplaced"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "the selected misplaced finding fails the check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("helper: only development units load it"),
        "unexpected report: {stderr}"
    );
    assert!(
        !stderr.contains("failed while collecting doctest evidence"),
        "doctests cannot change this verdict"
    );
}

#[test]
fn a_configured_workspace_wrapper_is_not_silently_replaced() {
    let fixture = Fixture::new(&["dead"], &format!("[dependencies]\n{}", dep("dead")), "pub fn go() {}\n").with_file(
        "../.cargo/config.toml",
        "[build]\nrustc-workspace-wrapper = \"workspace-wrapper\"\n",
    );

    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "an unchainable configured wrapper must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot safely interpose without bypassing it"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_package_that_does_not_compile_fails_evidence_collection() {
    let fixture = Fixture::new(&[], "", "this is not rust\n");
    let output = command()
        .arg("unused-deps")
        .arg("--manifest-path")
        .arg(fixture.dir.path().join("Cargo.toml"))
        .args(["--package", "main"])
        .output()
        .expect("failed to execute the binary");

    assert!(!output.status.success(), "an unbuildable package must fail the check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`cargo check` failed while collecting compile evidence"));
    assert!(
        stderr.contains("this is not rust"),
        "the compiler diagnostic must be rendered: {stderr}"
    );
}
