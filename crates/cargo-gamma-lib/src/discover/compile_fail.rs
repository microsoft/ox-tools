// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Recognizing test targets that may contain compiler-driving test cases.

use cargo_metadata::{DependencyKind, Metadata};
use proc_macro2::{TokenStream, TokenTree};

use crate::parse::exceeds_nesting_limit;

/// The dev-dependencies that give a compile-fail harness away.
///
/// Both drive rustc once per case and assert on what it says. Neither is detectable from the target
/// list alone, since such a target is an ordinary integration test as far as cargo is concerned.
const HARNESSES: [&str; 2] = ["trybuild", "compiletest_rs"];

/// A test target that may contain compiler-driving test cases.
///
/// Detection is static: it does not establish which cases run or measure their cost.
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

/// Whether the root source names the harness as a Rust identifier, not in prose or a literal.
fn mentions(source: &str, harness: &str) -> bool {
    !exceeds_nesting_limit(source) && source.parse::<TokenStream>().is_ok_and(|tokens| names_harness(tokens, harness))
}

fn names_harness(tokens: TokenStream, harness: &str) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(ident) => ident == harness,
        TokenTree::Group(group) => names_harness(group.stream(), harness),
        TokenTree::Punct(_) | TokenTree::Literal(_) => false,
    })
}

/// Renders the warning a run shows when it finds one of these.
///
/// Detection is heuristic, so advice describes a possible cost and suggests opting individual
/// cases out rather than excluding entire test binaries.
#[must_use]
pub fn advice(targets: &[CompileFailTarget]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }

    let named: Vec<String> = targets
        .iter()
        .map(|target| format!("  `{}` in {} ({})", target.target, target.package, target.harness))
        .collect();

    Some(format!(
        "{} test {} may have test cases that invoke the Rust compiler, which may lead to very long mutation test runs:\n{}\n\
         Consider omitting the specific test cases using `--skip` harness filters.",
        targets.len(),
        if targets.len() == 1 { "binary" } else { "binaries" },
        named.join("\n"),
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::{Value, from_value, json};

    use super::*;

    #[test]
    #[cfg_attr(
        miri,
        ignore = "native source reader requires filesystem access; injected-reader cases remain enabled"
    )]
    fn native_discovery_does_not_mistake_harness_names_in_its_own_strings_for_usage() {
        let root = camino::Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = root.join("src").join("discover").join("compile_fail.rs");
        let metadata: Metadata = from_value(json!({
            "packages":[{
                "name":"subject","version":"0.0.0","id":"subject","source":null,
                "dependencies":[{
                    "name":"trybuild","source":null,"req":"*","kind":"dev","rename":null,
                    "optional":false,"uses_default_features":true,"features":[],"target":null,"registry":null
                }],
                "targets":[{
                    "name":"checked-in","kind":["test"],"crate_types":["bin"],"src_path":source,
                    "edition":"2024","test":true,"doc":false,"doctest":false,"required-features":[]
                }],
                "features":{},"manifest_path":root.join("Cargo.toml"),"metadata":null,
                "authors":[],"categories":[],"keywords":[],"edition":"2024","publish":null,
                "license":null,"license_file":null,"readme":null,"repository":null,"homepage":null,
                "documentation":null,"description":null,"links":null,"default_run":null,"rust_version":null
            }],
            "workspace_members":["subject"],"workspace_default_members":["subject"],
            "workspace_root":root,"target_directory":root.join("unused"),"resolve":null,"metadata":null,"version":1
        }))
        .unwrap();
        assert_eq!(compile_fail_targets(&metadata).len(), 0);
    }

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
        let package = |name: &str, dependencies: Vec<Value>, targets: Vec<Value>| {
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
        let metadata: Metadata = from_value(json!({
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
        assert!(mentions("use trybuild::TestCases;", "trybuild"));
        assert!(mentions("mod nested { fn check() { trybuild::TestCases::new(); } }", "trybuild"));
    }

    #[test]
    fn overly_nested_source_is_not_parsed_for_optional_harness_advice() {
        let source = format!("{}trybuild{}", "(".repeat(65), ")".repeat(65));
        assert!(exceeds_nesting_limit(&source));
        assert!(!mentions(&source, "trybuild"));
        assert!(mentions(&format!("{}trybuild{}", "(".repeat(64), ")".repeat(64)), "trybuild"));
    }

    #[test]
    fn comments_literals_and_longer_identifiers_do_not_count_as_harness_usage() {
        for source in [
            "//! The macro's only other use is the trybuild compile-test.\n#[test] fn runtime() {}",
            "// trybuild::TestCases::new()\n#[test] fn runtime() {}",
            "/* compiletest_rs /* nested trybuild comment */ */ fn runtime() {}",
            r#"#[doc = "trybuild"] fn runtime() { let _ = "trybuild::TestCases"; }"#,
            r##"fn runtime() { let _ = r#"trybuild"#; }"##,
            "fn trybuild_helpers() {}",
        ] {
            assert!(!mentions(source, "trybuild"), "{source}");
            assert!(!mentions(source, "compiletest_rs"), "{source}");
        }
        assert!(!mentions("fn broken() {", "trybuild"));
    }

    #[test]
    fn six_test_targets_with_one_comment_only_mention_report_five_compiler_harnesses() {
        let sources = [
            ("performables", "compile_fail", "fn check() { trybuild::TestCases::new(); }"),
            ("internity", "compile_fail", "fn check() { trybuild::TestCases::new(); }"),
            ("templated_uri", "compile_tests", "use trybuild::TestCases;"),
            ("ohno", "display_diagnostics", "fn check() { trybuild::TestCases::new(); }"),
            ("fundle", "mod", "use trybuild::TestCases;"),
            (
                "fundle",
                "newtype_basic",
                "//! The macro's only other use is the trybuild compile-test in `tests/proc`.\n\
                 #[test] fn newtype_from_deref_and_deref_mut() {}",
            ),
        ];
        let targets: Vec<_> = sources
            .into_iter()
            .filter(|(_, _, source)| mentions(source, "trybuild"))
            .map(|(package, name, _)| CompileFailTarget {
                package: package.into(),
                target: name.into(),
                harness: "trybuild".into(),
            })
            .collect();
        let warning = advice(&targets).unwrap();
        assert!(
            warning.starts_with("5 test binaries may have test cases that invoke the Rust compiler"),
            "{warning}"
        );
        assert_eq!(warning.lines().filter(|line| line.starts_with("  ")).count(), 5);
        assert!(!warning.contains("newtype_basic"), "{warning}");
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

    #[test]
    fn the_advice_names_the_binary_and_suggests_ignoring_specific_cases() {
        let text = advice(&[target("router_compile_fail")]).expect("one target is enough to advise about");

        assert_eq!(
            text,
            concat!(
                "1 test binary may have test cases that invoke the Rust compiler, which may lead to very long mutation test runs:\n",
                "  `router_compile_fail` in routerama (trybuild)\n",
                "Consider omitting the specific test cases using `--skip` harness filters."
            )
        );
    }

    #[test]
    fn several_targets_are_advised_about_together() {
        let text = advice(&[target("router_compile_fail"), target("feature_gates")]).expect("two targets are advised about");

        assert!(
            text.starts_with("2 test binaries may have test cases that invoke the Rust compiler"),
            "{text}"
        );
        assert!(text.contains("`feature_gates` in routerama (trybuild)"), "{text}");
        assert!(text.contains("`router_compile_fail` in routerama (trybuild)"), "{text}");
    }

    #[test]
    fn each_binary_has_its_own_line_even_when_target_names_are_shared() {
        let mut other_package = target("compile_fail");
        other_package.package = "other".into();
        let text = advice(&[target("compile_fail"), other_package, target("ui")]).unwrap();
        assert_eq!(
            text.lines().filter(|line| line.starts_with("  ")).collect::<Vec<_>>(),
            [
                "  `compile_fail` in routerama (trybuild)",
                "  `compile_fail` in other (trybuild)",
                "  `ui` in routerama (trybuild)",
            ]
        );
        assert_eq!(text.matches("Consider omitting").count(), 1, "{text}");
        assert!(!text.contains("--exclude-test"), "{text}");
        assert!(!text.contains("primary oracle"), "{text}");
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

        assert!(
            text.contains("Consider omitting the specific test cases using `--skip` harness filters"),
            "{text}"
        );
    }
}
