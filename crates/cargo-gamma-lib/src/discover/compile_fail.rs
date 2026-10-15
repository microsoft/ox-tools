// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Recognizing test targets that run the compiler rather than the code.

use cargo_metadata::{DependencyKind, Metadata};

/// The dev-dependencies that give a compile-fail harness away.
///
/// Both drive rustc once per case and assert on what it says. Neither is detectable from the target
/// list alone, since such a target is an ordinary integration test as far as cargo is concerned.
const HARNESSES: [&str; 2] = ["trybuild", "compiletest_rs"];

/// A test target that asserts about compiler output rather than exercising the code under test.
///
/// Under mutation testing these are ruinous: the harness invokes rustc once per case, every mutant
/// pays that in full, and the oracle almost never gains anything, because what the target checks is
/// that some code fails to compile — which a mutated function body several crates away does not
/// change. A target of this shape has been measured putting a single mutant at one to two minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileFailTarget {
    /// The workspace package declaring it.
    pub package: String,

    /// The cargo target name, which is what `--exclude-test` is written against.
    pub target: String,

    /// The harness crate found in the package's dev-dependencies.
    pub harness: String,
}

/// Finds every test target that appears to assert about compiler output.
///
/// Detection is two-stage on purpose. The dev-dependency is what makes the search cheap — almost no
/// package declares one, so almost no package is looked at further. Reading the target's root
/// source is what makes the answer specific: a package with a compile-fail target usually has
/// ordinary test targets beside it, and naming those too would send the reader to exclude tests that
/// cost nothing.
///
/// A target whose source cannot be read is left out rather than guessed at. The whole value of this
/// is that it names a target precisely enough to act on, and a warning about the wrong one is worse
/// than none: `--exclude-test` takes a target out of the oracle, so acting on a bad name silently
/// narrows what can convict a mutant.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn compile_fail_targets(metadata: &Metadata) -> Vec<CompileFailTarget> {
    compile_fail_targets_with(metadata, |path| std::fs::read_to_string(path))
}

fn compile_fail_targets_with(
    metadata: &Metadata,
    mut read: impl FnMut(&camino::Utf8Path) -> std::io::Result<String>,
) -> Vec<CompileFailTarget> {
    let mut found = Vec::new();

    for package in metadata.workspace_packages() {
        let Some(harness) = package
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Development)
            .map(|dependency| dependency.name.as_str())
            .find(|name| HARNESSES.contains(name))
        else {
            continue;
        };

        for target in package.targets.iter().filter(|target| target.test) {
            let Ok(source) = read(&target.src_path) else {
                continue;
            };

            if mentions(&source, harness) {
                found.push(CompileFailTarget {
                    package: package.name.as_str().to_owned(),
                    target: target.name.clone(),
                    harness: harness.to_owned(),
                });
            }
        }
    }

    normalized(found)
}

fn normalized(mut found: Vec<CompileFailTarget>) -> Vec<CompileFailTarget> {
    found.sort_by(|left, right| left.target.cmp(&right.target));
    found.dedup();
    found
}

/// Whether a source file names the harness crate.
///
/// A substring search over the root source, because the shapes in use are `trybuild::TestCases`,
/// `use trybuild;` and `compiletest_rs::run_tests`, and distinguishing those from the same word in a
/// comment would need the file parsed for no gain: a file that mentions the harness at all is a file
/// this warning is right about.
fn mentions(source: &str, harness: &str) -> bool {
    source.contains(harness)
}

/// Renders the warning a run shows when it finds one of these.
///
/// The flag is spelled out in full because the value of this is that it can be acted on without
/// first learning that `--exclude-test` exists. Nothing is excluded automatically: `trybuild`
/// asserts exact compiler output, so on a proc-macro crate it is often the *primary* oracle — a
/// mutant that corrupts a diagnostic message is caught there and nowhere else. Excluding by default
/// would gut the oracle for the code the technique suits best, and the mutants would come back as
/// survivors rather than as anything visibly wrong.
#[must_use]
pub fn advice(targets: &[CompileFailTarget]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }

    let named: Vec<String> = targets
        .iter()
        .map(|target| format!("`{}` in {} ({})", target.target, target.package, target.harness))
        .collect();

    let flags: Vec<String> = targets.iter().map(|target| format!("--exclude-test {}", target.target)).collect();

    Some(format!(
        "{} {} the compiler once per case, so every mutant pays for a full rustc run and almost none are convicted by it: {}.\n\
         If {} is not part of what should be judging these mutants, exclude it with `{}`.\n\
         Left in, it is likely to make this run take hours.",
        crate::report::quantity(targets.len(), "test target"),
        if targets.len() == 1 { "invokes" } else { "invoke" },
        named.join(", "),
        if targets.len() == 1 { "it" } else { "any of them" },
        flags.join(" "),
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn survivor_compile_fail_discovery_continues_after_unrelated_packages_and_missing_targets() {
        let root = camino::Utf8Path::new("workspace");
        let target = |name: &str, source: &str| {
            json!({
                "name": name, "kind": ["test"], "crate_types": ["bin"],
                "src_path": root.join(source), "edition": "2024", "test": true,
                "doc": false, "doctest": false, "required-features": []
            })
        };
        let dependency = |kind: &str| {
            json!({
                "name": "trybuild", "source": null, "req": "*", "kind": kind,
                "rename": null, "optional": false, "uses_default_features": true,
                "features": [], "target": null, "registry": null
            })
        };
        let package = |name: &str, dependencies: Vec<serde_json::Value>, targets: Vec<serde_json::Value>| {
            json!({
                "name": name, "version": "0.0.0", "id": name, "source": null,
                "dependencies": dependencies, "targets": targets, "features": {},
                "manifest_path": root.join(name).join("Cargo.toml"), "metadata": null,
                "authors": [], "categories": [], "keywords": [], "edition": "2024",
                "publish": null, "license": null, "license_file": null, "readme": null,
                "repository": null, "homepage": null, "documentation": null, "description": null,
                "links": null, "default_run": null, "rust_version": null
            })
        };
        let metadata: Metadata = serde_json::from_value(json!({
            "packages": [
                package("unrelated", Vec::new(), vec![target("unrelated", "harness.rs")]),
                package("normal-dependency", vec![dependency("normal")], vec![target("normal", "harness.rs")]),
                package("subject", vec![dependency("dev")], vec![
                    target("missing", "missing.rs"), target("ordinary", "ordinary.rs"),
                    target("zeta", "harness.rs"), target("alpha", "harness.rs"),
                    target("alpha", "harness.rs")
                ])
            ],
            "workspace_members": ["unrelated", "normal-dependency", "subject"],
            "workspace_default_members": ["subject"], "workspace_root": root,
            "target_directory": root.join("target"), "resolve": null, "metadata": null, "version": 1
        }))
        .unwrap();
        let mut reads = Vec::new();
        let found = compile_fail_targets_with(&metadata, |path| {
            reads.push(path.to_owned());
            if path == root.join("missing.rs") {
                Err(std::io::ErrorKind::NotFound.into())
            } else if path == root.join("ordinary.rs") {
                Ok("#[test] fn ordinary() {}".into())
            } else if path == root.join("harness.rs") {
                Ok("fn main() { trybuild::TestCases::new(); }".into())
            } else {
                panic!("unexpected source read: {path}");
            }
        });
        assert_eq!(
            reads,
            ["missing.rs", "ordinary.rs", "harness.rs", "harness.rs", "harness.rs"].map(|path| root.join(path))
        );
        assert_eq!(
            found,
            [
                CompileFailTarget {
                    package: "subject".into(),
                    target: "alpha".into(),
                    harness: "trybuild".into()
                },
                CompileFailTarget {
                    package: "subject".into(),
                    target: "zeta".into(),
                    harness: "trybuild".into()
                },
            ]
        );
    }

    fn target(name: &str) -> CompileFailTarget {
        CompileFailTarget {
            package: "routerama".to_owned(),
            target: name.to_owned(),
            harness: "trybuild".to_owned(),
        }
    }

    #[test]
    fn a_source_naming_the_harness_is_recognized() {
        assert!(mentions("fn main() { trybuild::TestCases::new(); }", "trybuild"));
        assert!(mentions("use compiletest_rs as compiletest;", "compiletest_rs"));
    }

    /// An ordinary integration test in the same package must not be named, or the reader is sent to
    /// exclude a target that costs nothing and can convict.
    #[test]
    fn an_ordinary_test_source_is_not_recognized() {
        assert!(!mentions("#[test]\nfn routes_are_matched() {}", "trybuild"));
    }

    /// Nothing found means nothing said. A warning with no target in it cannot be acted on.
    #[test]
    fn no_compile_fail_target_produces_no_advice() {
        assert_eq!(advice(&[]), None);
    }

    /// The whole point is that the flag can be copied out of the message, so it has to be there in
    /// full — including the target name, which is what the pattern matches on.
    #[test]
    fn the_advice_names_the_target_and_the_flag_that_removes_it() {
        let text = advice(&[target("router_compile_fail")]).expect("one target is enough to advise about");

        assert!(text.contains("`router_compile_fail` in routerama (trybuild)"), "{text}");
        assert!(text.contains("--exclude-test router_compile_fail"), "{text}");
        assert!(text.contains("1 test target invokes"), "{text}");
    }

    /// Several targets are one warning with one flag string, since a reader who has to run the
    /// command twice will run it once and wonder why the run is still slow.
    #[test]
    fn several_targets_are_advised_about_together() {
        let text = advice(&[target("router_compile_fail"), target("feature_gates")]).expect("two targets are advised about");

        assert!(text.contains("2 test targets invoke"), "{text}");
        assert!(
            text.contains("--exclude-test router_compile_fail --exclude-test feature_gates"),
            "{text}"
        );
    }

    #[test]
    fn discovered_targets_are_sorted_and_deduplicated() {
        let found = normalized(vec![target("zeta"), target("alpha"), target("zeta")]);

        assert_eq!(found, [target("alpha"), target("zeta")]);
    }

    /// The warning must not exclude anything, only say what would.
    #[test]
    fn the_advice_says_it_is_the_callers_decision() {
        let text = advice(&[target("router_compile_fail")]).expect("one target is enough to advise about");

        assert!(text.contains("If it is not part of what should be judging"), "{text}");
    }
}
