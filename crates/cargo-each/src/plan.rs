// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Building the resolved [`Plan`] of command invocations from a selected,
//! filtered member set, an execution [`Mode`], and the command template.

use std::collections::BTreeSet;
use std::path::PathBuf;

use cargo_metadata::TargetKind;

use crate::error::{ChdirConflictsWithOnceError, EachError, MissingTargetPerPackageError};
use crate::filter::TargetPredicate;
use crate::json_lines::JsonRecord;
use crate::substitute::{Placeholders, substitute, substitute_json_prevalidated, validate_json_placeholders, validate_placeholders};
use crate::workspace::Member;

/// How the command is run over the selected set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Run the command once per selected member (default).
    PerPackage,
    /// Run the command once per matching Cargo target.
    PerTarget,
    /// Run the command exactly once for the whole set (`--once`).
    Once,
}

/// How the `{packages}` placeholder expands in [`Mode::Once`].
///
/// A dedicated two-variant type rather than a bare `bool` so the caller states
/// its intent by name and it cannot be transposed with the adjacent `chdir`
/// flag at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackagesExpansion {
    /// The resolved set is the whole workspace with no narrowing: expand to a
    /// bare `--workspace`.
    Workspace,
    /// A narrowed set: expand to an explicit `--package name@version` per
    /// member.
    Explicit,
}

/// One fully-resolved command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    /// A short label for progress output.
    ///
    /// The member name in per-package mode; `None` in once mode.
    pub(crate) label: Option<String>,
    /// The program and arguments to spawn, placeholders already expanded.
    pub(crate) argv: Vec<String>,
    /// The working directory to run in, when `--chdir` is set (the member's
    /// crate root). `None` runs in the caller's current directory.
    pub(crate) work_dir: Option<PathBuf>,
}

/// The resolved list of invocations `cargo-each` will run.
///
/// An empty [`Plan::invocations`] means the selection resolved to nothing:
/// the caller treats that as a successful no-op (exit 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The invocations, in run order.
    pub(crate) invocations: Vec<Invocation>,
}

/// Inputs that control how a selected member set becomes invocations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuildOptions<'a> {
    pub(crate) mode: Mode,
    pub(crate) chdir: bool,
    pub(crate) packages: PackagesExpansion,
    pub(crate) target_kinds: &'a BTreeSet<TargetKind>,
    pub(crate) target_filters: &'a [TargetPredicate],
    pub(crate) require_target_per_package: bool,
    pub(crate) workspace_rust_version: Option<&'a str>,
}

impl Plan {
    /// Build one invocation per JSON object in input order.
    pub(crate) fn build_json(records: &[JsonRecord], command: &[String]) -> Result<Self, EachError> {
        validate_json_placeholders(command)?;
        let invocations = records
            .iter()
            .map(|record| {
                Ok(Invocation {
                    label: Some(record.label()),
                    argv: substitute_json_prevalidated(command, &record.fields, &record.source, record.line)?,
                    work_dir: None,
                })
            })
            .collect::<Result<Vec<_>, EachError>>()?;
        Ok(Self { invocations })
    }

    /// Validate plan-wide configuration and report whether it produces no
    /// invocations without expanding placeholders.
    ///
    /// This lets callers preserve usage validation while deferring lazy
    /// workspace-scoped values until the resolved selection is known to
    /// produce work.
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] under the same invalid `chdir` and placeholder
    /// combinations as [`Self::build`].
    pub(crate) fn is_empty(members: &[&Member], command: &[String], options: BuildOptions<'_>) -> Result<bool, EachError> {
        validate_build(command, options)?;
        Ok(match options.mode {
            Mode::PerPackage | Mode::Once => members.is_empty(),
            Mode::PerTarget => {
                validate_target_filters(members, options.target_kinds, options.target_filters)?;
                validate_target_requirement(members, options)?;
                !members.iter().any(|member| {
                    member
                        .targets
                        .iter()
                        .any(|target| target_matches(member, target, options.target_kinds, options.target_filters))
                })
            }
        })
    }

    /// Build the plan.
    ///
    /// `chdir` runs each per-package or per-target invocation from the member's
    /// crate root. Combined with [`Mode::Once`] it is a usage error.
    ///
    /// `packages` controls the `{packages}` expansion in once mode:
    /// [`PackagesExpansion::Workspace`] (the resolved set is the entire
    /// workspace — see [`Selection::is_whole_workspace`]) becomes `--workspace`;
    /// [`PackagesExpansion::Explicit`] becomes an explicit
    /// `--package name@version` list.
    ///
    /// [`Selection::is_whole_workspace`]: crate::select::Selection::is_whole_workspace
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] if `chdir` is combined with [`Mode::Once`], or if
    /// a placeholder in `command` is used in the wrong mode.
    pub(crate) fn build(members: &[&Member], command: &[String], options: BuildOptions<'_>) -> Result<Self, EachError> {
        if Self::is_empty(members, command, options)? {
            return Ok(Self { invocations: Vec::new() });
        }

        let BuildOptions {
            mode,
            chdir,
            packages,
            target_kinds,
            target_filters,
            workspace_rust_version,
            ..
        } = options;

        let uses_cargo_spec = command.iter().any(|arg| arg.contains("{cargo-spec}"));

        // #[gamma::skip(try.propagate_to_unwrap, reason = "is_empty already ran validate_build on this same immutable command and mode; every substitute repeats only that validated placeholder check, so all collected results and direct substitutions are Ok")]
        let invocations = match mode {
            Mode::PerPackage => members
                .iter()
                .map(|m| {
                    let placeholders = Placeholders::Package {
                        name: m.name.clone(),
                        spec: m.spec(),
                        cargo_spec: if uses_cargo_spec { m.cargo_spec() } else { String::new() },
                        version: m.version.clone(),
                        manifest: m.manifest_path.display().to_string(),
                        workspace_rust_version: workspace_rust_version.map(str::to_owned),
                    };
                    Ok(Invocation {
                        label: Some(m.name.clone()),
                        argv: substitute(command, &placeholders)?,
                        work_dir: chdir.then(|| m.manifest_dir().to_path_buf()),
                    })
                })
                .collect::<Result<Vec<_>, EachError>>()?,
            Mode::PerTarget => members
                .iter()
                .flat_map(|member| {
                    let mut cargo_spec = None;
                    member
                        .targets
                        .iter()
                        .filter(|target| target_matches(member, target, target_kinds, target_filters))
                        .map(move |target| {
                            let placeholders = Placeholders::Target {
                                name: member.name.clone(),
                                spec: member.spec(),
                                cargo_spec: cargo_spec
                                    .get_or_insert_with(|| if uses_cargo_spec { member.cargo_spec() } else { String::new() })
                                    .clone(),
                                version: member.version.clone(),
                                manifest: member.manifest_path.display().to_string(),
                                target: target.name.clone(),
                                workspace_rust_version: workspace_rust_version.map(str::to_owned),
                            };
                            Ok(Invocation {
                                label: Some(format!("{}::{}", member.name, target.name)),
                                argv: substitute(command, &placeholders)?,
                                work_dir: chdir.then(|| member.manifest_dir().to_path_buf()),
                            })
                        })
                })
                .collect::<Result<Vec<_>, EachError>>()?,
            Mode::Once => {
                let packages = packages_flags(members, packages);
                let placeholders = Placeholders::Once {
                    packages,
                    workspace_rust_version: workspace_rust_version.map(str::to_owned),
                };
                vec![Invocation {
                    label: None,
                    argv: substitute(command, &placeholders)?,
                    work_dir: None,
                }]
            }
        };

        Ok(Self { invocations })
    }
}

fn validate_build(command: &[String], options: BuildOptions<'_>) -> Result<(), EachError> {
    if options.chdir && options.mode == Mode::Once {
        return Err(ChdirConflictsWithOnceError::new().into());
    }
    // Validate placeholder/mode consistency before any empty-set short-circuit
    // so an invalid template never depends on whether a computed tier has work.
    validate_placeholders(command, options.mode)
}

fn target_matches(
    member: &Member,
    target: &crate::workspace::MemberTarget,
    target_kinds: &BTreeSet<TargetKind>,
    target_filters: &[TargetPredicate],
) -> bool {
    target.kinds.iter().any(|kind| target_kinds.contains(kind)) && target_filters.iter().all(|filter| filter.matches(member, target))
}

fn validate_target_filters(
    members: &[&Member],
    target_kinds: &BTreeSet<TargetKind>,
    target_filters: &[TargetPredicate],
) -> Result<(), EachError> {
    // Target filtering follows kind selection, so metadata on members without
    // any requested-kind target cannot affect the plan and is outside this
    // preflight.
    for member in members.iter().filter(|member| {
        member
            .targets
            .iter()
            .any(|target| target.kinds.iter().any(|kind| target_kinds.contains(kind)))
    }) {
        for filter in target_filters {
            filter.validate(member)?;
        }
    }
    Ok(())
}

fn validate_target_requirement(members: &[&Member], options: BuildOptions<'_>) -> Result<(), EachError> {
    if !options.require_target_per_package {
        return Ok(());
    }

    let missing = members
        .iter()
        .filter(|member| {
            !member
                .targets
                .iter()
                .any(|target| target_matches(member, target, options.target_kinds, options.target_filters))
        })
        .map(|member| member.spec())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(MissingTargetPerPackageError::new(missing.join(", ")).into())
    }
}

/// The `{packages}` expansion: `--workspace` for the whole workspace, else an
/// explicit `--package name@version` per member.
fn packages_flags(members: &[&Member], packages: PackagesExpansion) -> Vec<String> {
    if packages == PackagesExpansion::Workspace {
        return vec!["--workspace".to_owned()];
    }
    members.iter().flat_map(|member| ["--package".to_owned(), member.spec()]).collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::workspace::MemberTarget;

    fn member(name: &str) -> Member {
        Member {
            name: name.to_owned(),
            version: "1.2.3".to_owned(),
            rust_version: Some("1.70.0".parse().expect("valid Rust version")),
            manifest_path: PathBuf::from(if cfg!(windows) { r"C:\ws" } else { "/ws" })
                .join(name)
                .join("Cargo.toml"),
            publishable: true,
            doctest: true,
            features: BTreeSet::new(),
            targets: Vec::new(),
            dependencies: BTreeSet::new(),
            metadata: Value::Null,
        }
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn build(members: &[&Member], mode: Mode, chdir: bool, packages: PackagesExpansion, command: &[String]) -> Result<Plan, EachError> {
        Plan::build(
            members,
            command,
            BuildOptions {
                mode,
                chdir,
                packages,
                target_kinds: &BTreeSet::new(),
                target_filters: &[],
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
    }

    #[test]
    fn json_plan_preserves_record_order_duplicates_and_labels() {
        let record = |source: &str, line: usize, value: &str| JsonRecord {
            fields: serde_json::json!({"value": value}).as_object().expect("object").clone(),
            source: std::sync::Arc::from(source),
            line,
        };
        let records = [
            record("inline", 1, "alpha"),
            record("file.jsonl", 3, "beta"),
            record("inline", 1, "alpha"),
        ];
        let plan = Plan::build_json(&records, &cmd(&["echo", "{json:value}"])).expect("build JSON plan");
        assert_eq!(plan.invocations.len(), 3);
        assert_eq!(plan.invocations[0].argv, ["echo", "alpha"]);
        assert_eq!(plan.invocations[1].argv, ["echo", "beta"]);
        assert_eq!(plan.invocations[2].argv, ["echo", "alpha"]);
        assert_eq!(plan.invocations[1].label.as_deref(), Some("file.jsonl:3"));
        assert!(plan.invocations.iter().all(|invocation| invocation.work_dir.is_none()));
    }

    #[test]
    fn empty_set_yields_empty_plan() {
        let plan = build(&[], Mode::PerPackage, false, PackagesExpansion::Explicit, &cmd(&["cargo", "test"])).expect("build");
        assert!(plan.invocations.is_empty());
    }

    #[test]
    fn empty_set_still_validates_placeholders() {
        // A per-package token under --once is a usage error even when the
        // selection is empty (rather than a silent no-op).
        let err = build(
            &[],
            Mode::Once,
            false,
            PackagesExpansion::Explicit,
            &cmd(&["cargo", "test", "{name}"]),
        )
        .expect_err("misused placeholder must error");
        assert!(err.to_string().contains("{name}"));
        // A valid template over an empty set is still an empty plan.
        let plan = build(
            &[],
            Mode::Once,
            false,
            PackagesExpansion::Explicit,
            &cmd(&["cargo", "test", "{packages}"]),
        )
        .expect("build");
        assert!(plan.invocations.is_empty());
    }

    #[test]
    fn unused_cargo_spec_does_not_require_a_source_url() {
        let mut member = member("alpha");
        // A relative sentinel cannot become a file URL, exposing eager selector construction.
        member.manifest_path = PathBuf::from("alpha").join("Cargo.toml");
        member.targets.push(MemberTarget {
            name: "custom_library".to_owned(),
            kinds: BTreeSet::from([TargetKind::Lib]),
            required_features: BTreeSet::new(),
        });
        for mode in [Mode::PerPackage, Mode::PerTarget] {
            let plan = Plan::build(
                &[&member],
                &cmd(&["cargo", "fmt", "--manifest-path", "{manifest}"]),
                BuildOptions {
                    mode,
                    chdir: false,
                    packages: PackagesExpansion::Explicit,
                    target_kinds: &BTreeSet::from([TargetKind::Lib]),
                    target_filters: &[],
                    require_target_per_package: false,
                    workspace_rust_version: None,
                },
            )
            .expect("a template without cargo-spec does not need a source URL");
            assert_eq!(plan.invocations.len(), 1);
            assert_eq!(
                plan.invocations[0].argv,
                ["cargo", "fmt", "--manifest-path", &member.manifest_path.display().to_string()]
            );
        }
    }

    #[test]
    fn per_package_builds_one_invocation_each() {
        let a = member("alpha");
        let b = member("beta");
        let plan = build(
            &[&a, &b],
            Mode::PerPackage,
            false,
            PackagesExpansion::Explicit,
            &cmd(&["cargo", "check-external-types", "--manifest-path", "{manifest}"]),
        )
        .expect("build");
        assert_eq!(plan.invocations.len(), 2);
        assert_eq!(plan.invocations[0].label.as_deref(), Some("alpha"));
        assert_eq!(
            plan.invocations[0].argv,
            [
                "cargo",
                "check-external-types",
                "--manifest-path",
                &a.manifest_path.display().to_string()
            ]
        );
        assert!(plan.invocations[0].work_dir.is_none());
        assert_eq!(plan.invocations[1].argv[3], b.manifest_path.display().to_string());
    }

    #[test]
    fn chdir_sets_work_dir_to_crate_root() {
        let a = member("alpha");
        let plan = build(&[&a], Mode::PerPackage, true, PackagesExpansion::Explicit, &cmd(&["cargo", "fmt"])).expect("build");
        assert_eq!(plan.invocations[0].work_dir.as_deref(), Some(a.manifest_dir()));
    }

    #[test]
    fn chdir_with_once_is_a_usage_error() {
        let a = member("alpha");
        let err = build(
            &[&a],
            Mode::Once,
            true,
            PackagesExpansion::Explicit,
            &cmd(&["cargo", "test", "{packages}"]),
        )
        .expect_err("chdir+once must error");
        let rendered = err.to_string();
        assert!(rendered.contains("--chdir"), "rendered: {rendered}");
        assert!(rendered.contains("--once"), "rendered: {rendered}");
    }

    #[test]
    fn once_whole_workspace_uses_workspace_flag() {
        let a = member("alpha");
        let plan = build(
            &[&a],
            Mode::Once,
            false,
            PackagesExpansion::Workspace,
            &cmd(&["cargo", "clippy", "{packages}"]),
        )
        .expect("build");
        assert_eq!(plan.invocations.len(), 1);
        assert_eq!(plan.invocations[0].argv, ["cargo", "clippy", "--workspace"]);
    }

    #[test]
    fn once_subset_uses_explicit_package_flags() {
        let a = member("alpha");
        let b = member("beta");
        let plan = build(
            &[&a, &b],
            Mode::Once,
            false,
            PackagesExpansion::Explicit,
            &cmd(&["cargo", "clippy", "{packages}"]),
        )
        .expect("build");
        assert_eq!(
            plan.invocations[0].argv,
            ["cargo", "clippy", "--package", "alpha@1.2.3", "--package", "beta@1.2.3"]
        );
    }

    #[test]
    fn per_target_filters_and_substitutes_targets() {
        let mut a = member("alpha");
        a.targets = vec![
            MemberTarget {
                name: "ordinary".to_owned(),
                kinds: std::iter::once(TargetKind::Test).collect(),
                required_features: BTreeSet::new(),
            },
            MemberTarget {
                name: "loom".to_owned(),
                kinds: std::iter::once(TargetKind::Test).collect(),
                required_features: std::iter::once("loom".to_owned()).collect(),
            },
        ];
        let kinds = std::iter::once(TargetKind::Test).collect();
        let required = [TargetPredicate::parse("required-feature:loom").expect("target filter")];
        let plan = Plan::build(
            &[&a],
            &cmd(&["cargo", "test", "-p", "{name}", "--test", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &required,
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect("build target plan");
        assert_eq!(plan.invocations.len(), 1);
        assert_eq!(plan.invocations[0].label.as_deref(), Some("alpha::loom"));
        assert_eq!(plan.invocations[0].argv, ["cargo", "test", "-p", "alpha", "--test", "loom"]);
    }

    #[test]
    fn per_target_boolean_and_repeated_filters_compose() {
        let mut a = member("alpha");
        a.metadata = serde_json::json!({"cargo-each": {"excluded-targets": ["ordinary-test"]}});
        a.targets = vec![
            MemberTarget {
                name: "loom-test".to_owned(),
                kinds: std::iter::once(TargetKind::Test).collect(),
                required_features: std::iter::once("loom".to_owned()).collect(),
            },
            MemberTarget {
                name: "ordinary-test".to_owned(),
                kinds: std::iter::once(TargetKind::Test).collect(),
                required_features: BTreeSet::new(),
            },
            MemberTarget {
                name: "demo".to_owned(),
                kinds: std::iter::once(TargetKind::Example).collect(),
                required_features: BTreeSet::new(),
            },
        ];
        let kinds = [TargetKind::Example, TargetKind::Test].into_iter().collect();
        let filters = [
            TargetPredicate::parse("(required-feature:loom and name:loom-*) or name:demo").expect("Boolean target filter"),
            TargetPredicate::parse("not name-in-metadata:cargo-each.excluded-targets").expect("metadata exclusion filter"),
        ];
        let plan = Plan::build(
            &[&a],
            &cmd(&["echo", "{name}::{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &filters,
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect("build target plan");
        assert_eq!(
            plan.invocations
                .iter()
                .map(|invocation| invocation.argv.clone())
                .collect::<Vec<_>>(),
            [cmd(&["echo", "alpha::loom-test"]), cmd(&["echo", "alpha::demo"])]
        );
    }

    fn example_member() -> Member {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "demo".to_owned(),
            kinds: std::iter::once(TargetKind::Example).collect(),
            required_features: BTreeSet::new(),
        });
        a
    }

    #[test]
    fn missing_target_metadata_matches_under_not() {
        let a = example_member();
        let kinds = std::iter::once(TargetKind::Example).collect();
        let include_missing = [TargetPredicate::parse("not name-in-metadata:cargo-each.excluded-targets").expect("target filter")];
        let plan = Plan::build(
            &[&a],
            &cmd(&["echo", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &include_missing,
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect("missing metadata does not exclude the target");
        assert_eq!(plan.invocations[0].argv, ["echo", "demo"]);
    }

    #[test]
    fn empty_target_result_is_valid() {
        let a = example_member();
        let kinds = std::iter::once(TargetKind::Example).collect();
        let exclude_all = [TargetPredicate::parse("name:no-such-target").expect("target filter")];
        assert!(
            Plan::is_empty(
                &[&a],
                &cmd(&["echo", "{target}"]),
                BuildOptions {
                    mode: Mode::PerTarget,
                    chdir: false,
                    packages: PackagesExpansion::Explicit,
                    target_kinds: &kinds,
                    target_filters: &exclude_all,
                    require_target_per_package: false,
                    workspace_rust_version: None,
                },
            )
            .expect("an empty target result is valid")
        );
    }

    #[test]
    fn malformed_target_metadata_fails_preflight_for_relevant_members_only() {
        let filter = [TargetPredicate::parse("name-in-metadata:cargo-each.excluded-targets").expect("target filter")];
        let test_kinds = std::iter::once(TargetKind::Test).collect();

        let mut relevant = member("relevant");
        relevant.metadata = json!({"cargo-each": {"excluded-targets": "test"}});
        relevant.targets.push(MemberTarget {
            name: "test".to_owned(),
            kinds: std::iter::once(TargetKind::Test).collect(),
            required_features: BTreeSet::new(),
        });
        let error = Plan::is_empty(
            &[&relevant],
            &cmd(&["echo", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &test_kinds,
                target_filters: &filter,
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect_err("malformed metadata must fail before matching");
        assert_eq!(
            error.to_string(),
            "package `relevant` metadata `cargo-each.excluded-targets` must be an array of strings when used by `name-in-metadata:cargo-each.excluded-targets`"
        );

        let mut irrelevant = member("irrelevant");
        irrelevant.metadata = relevant.metadata.clone();
        irrelevant.targets.push(MemberTarget {
            name: "example".to_owned(),
            kinds: std::iter::once(TargetKind::Example).collect(),
            required_features: BTreeSet::new(),
        });
        assert!(
            Plan::is_empty(
                &[&irrelevant],
                &cmd(&["echo", "{target}"]),
                BuildOptions {
                    mode: Mode::PerTarget,
                    chdir: false,
                    packages: PackagesExpansion::Explicit,
                    target_kinds: &test_kinds,
                    target_filters: &filter,
                    require_target_per_package: false,
                    workspace_rust_version: None,
                },
            )
            .expect("members outside kind selection are outside metadata preflight")
        );
    }

    #[test]
    fn target_metadata_preflight_rejects_invalid_metadata_for_a_later_matching_kind() {
        let mut member = member("mixed");
        member.metadata = serde_json::json!({"checks": {"names": false}});
        member.targets = vec![
            MemberTarget {
                name: "example".to_owned(),
                kinds: BTreeSet::from([TargetKind::Example]),
                required_features: BTreeSet::new(),
            },
            MemberTarget {
                name: "library".to_owned(),
                kinds: BTreeSet::from([TargetKind::Lib, TargetKind::RLib]),
                required_features: BTreeSet::new(),
            },
        ];
        let filters = [TargetPredicate::parse("name-in-metadata:checks.names").unwrap()];
        let error = validate_target_filters(&[&member], &BTreeSet::from([TargetKind::Lib]), &filters).unwrap_err();
        assert_eq!(
            error.to_string(),
            "package `mixed` metadata `checks.names` must be an array of strings when used by `name-in-metadata:checks.names`"
        );
    }

    #[test]
    fn per_target_reuses_member_selectors_without_resolving_unmatched_members() {
        let mut a = member("alpha");
        a.targets = ["first", "second"]
            .into_iter()
            .map(|name| MemberTarget {
                name: name.to_owned(),
                kinds: BTreeSet::from([TargetKind::Example]),
                required_features: BTreeSet::new(),
            })
            .collect();
        let mut b = a.clone();
        b.name = "beta".to_owned();
        b.manifest_path = member("beta").manifest_path;
        let mut unmatched = member("unmatched");
        // This sentinel cannot become a file URL and must remain unresolved.
        unmatched.manifest_path = PathBuf::from("unmatched").join("Cargo.toml");
        unmatched.targets.push(MemberTarget {
            name: "library".to_owned(),
            kinds: BTreeSet::from([TargetKind::Lib]),
            required_features: BTreeSet::new(),
        });

        let plan = Plan::build(
            &[&unmatched, &a, &b],
            &cmd(&["cargo", "run", "-p", "{cargo-spec}", "--example", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &BTreeSet::from([TargetKind::Example]),
                target_filters: &[],
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect("unmatched members do not need a source URL");

        let root = if cfg!(windows) { "file:///C:/ws" } else { "file:///ws" };
        let expected = ["alpha", "beta"]
            .into_iter()
            .flat_map(|name| {
                ["first", "second"].map(|target| Invocation {
                    label: Some(format!("{name}::{target}")),
                    argv: cmd(&["cargo", "run", "-p", &format!("{root}/{name}#{name}@1.2.3"), "--example", target]),
                    work_dir: None,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(plan.invocations, expected);
    }

    #[test]
    fn per_target_matches_when_any_target_kind_is_requested() {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "multi-kind".to_owned(),
            kinds: [TargetKind::Bin, TargetKind::Example].into_iter().collect(),
            required_features: BTreeSet::new(),
        });
        let kinds = [TargetKind::Example, TargetKind::Test].into_iter().collect();
        let required = [];
        let plan = Plan::build(
            &[&a],
            &cmd(&["echo", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &required,
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect("one matching kind is sufficient");
        assert_eq!(
            plan.invocations,
            [Invocation {
                label: Some("alpha::multi-kind".to_owned()),
                argv: cmd(&["echo", "multi-kind"]),
                work_dir: None,
            }]
        );
    }

    #[test]
    fn per_target_requires_both_kind_and_required_features() {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "example".to_owned(),
            kinds: std::iter::once(TargetKind::Example).collect(),
            required_features: std::iter::once("loom".to_owned()).collect(),
        });
        let kinds = std::iter::once(TargetKind::Test).collect();
        let required = [TargetPredicate::parse("required-feature:loom").expect("target filter")];
        assert!(
            Plan::is_empty(
                &[&a],
                &cmd(&["echo", "{target}"]),
                BuildOptions {
                    mode: Mode::PerTarget,
                    chdir: false,
                    packages: PackagesExpansion::Explicit,
                    target_kinds: &kinds,
                    target_filters: &required,
                    require_target_per_package: false,
                    workspace_rust_version: None,
                },
            )
            .expect("valid target selection")
        );
    }

    #[test]
    fn target_requirement_accepts_every_member_with_a_match() {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "alpha-test".to_owned(),
            kinds: std::iter::once(TargetKind::Test).collect(),
            required_features: BTreeSet::new(),
        });
        let mut b = member("beta");
        b.targets.extend([
            MemberTarget {
                name: "beta-first".to_owned(),
                kinds: std::iter::once(TargetKind::Test).collect(),
                required_features: BTreeSet::new(),
            },
            MemberTarget {
                name: "beta-second".to_owned(),
                kinds: std::iter::once(TargetKind::Test).collect(),
                required_features: BTreeSet::new(),
            },
        ]);
        let kinds = std::iter::once(TargetKind::Test).collect();
        let plan = Plan::build(
            &[&a, &b],
            &cmd(&["echo", "{name}::{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &[],
                require_target_per_package: true,
                workspace_rust_version: None,
            },
        )
        .expect("every selected member has at least one matching target");
        assert_eq!(
            plan.invocations
                .iter()
                .map(|invocation| invocation.argv[1].as_str())
                .collect::<Vec<_>>(),
            ["alpha::alpha-test", "beta::beta-first", "beta::beta-second"]
        );
    }

    #[test]
    fn target_requirement_reports_every_missing_member_in_input_order() {
        let a = member("alpha");
        let b = member("beta");
        let kinds = std::iter::once(TargetKind::Test).collect();
        let error = Plan::is_empty(
            &[&b, &a],
            &cmd(&["echo", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &[],
                require_target_per_package: true,
                workspace_rust_version: None,
            },
        )
        .expect_err("both selected members lack matching targets");
        assert_eq!(
            error.to_string(),
            "`--require-target-per-package` found selected packages without a matching target: beta@1.2.3, alpha@1.2.3"
        );
    }

    #[test]
    fn target_filter_can_make_an_otherwise_matching_member_fail_the_requirement() {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "alpha-test".to_owned(),
            kinds: std::iter::once(TargetKind::Test).collect(),
            required_features: BTreeSet::new(),
        });
        let kinds = std::iter::once(TargetKind::Test).collect();
        let filters = [TargetPredicate::parse("name:no-such-target").expect("target filter")];
        let error = Plan::is_empty(
            &[&a],
            &cmd(&["echo", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: false,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &filters,
                require_target_per_package: true,
                workspace_rust_version: None,
            },
        )
        .expect_err("target filtering removes the otherwise matching target");
        assert_eq!(
            error.to_string(),
            "`--require-target-per-package` found selected packages without a matching target: alpha@1.2.3"
        );
    }

    #[test]
    fn target_requirement_keeps_an_empty_selection_as_a_noop() {
        let kinds = std::iter::once(TargetKind::Test).collect();
        assert!(
            Plan::is_empty(
                &[],
                &cmd(&["echo", "{target}"]),
                BuildOptions {
                    mode: Mode::PerTarget,
                    chdir: false,
                    packages: PackagesExpansion::Explicit,
                    target_kinds: &kinds,
                    target_filters: &[],
                    require_target_per_package: true,
                    workspace_rust_version: None,
                },
            )
            .expect("an empty selected package set remains valid")
        );
    }

    #[test]
    fn absent_workspace_value_expands_to_empty_in_every_nonempty_mode() {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "example".to_owned(),
            kinds: std::iter::once(TargetKind::Example).collect(),
            required_features: BTreeSet::new(),
        });
        let command = cmd(&["echo", "{workspace-rust-version}"]);
        for (mode, kinds) in [
            (Mode::PerPackage, BTreeSet::new()),
            (Mode::PerTarget, std::iter::once(TargetKind::Example).collect()),
            (Mode::Once, BTreeSet::new()),
        ] {
            let plan = Plan::build(
                &[&a],
                &command,
                BuildOptions {
                    mode,
                    chdir: false,
                    packages: PackagesExpansion::Explicit,
                    target_kinds: &kinds,
                    target_filters: &[],
                    require_target_per_package: false,
                    workspace_rust_version: None,
                },
            )
            .expect("an absent optional workspace value is valid");
            assert_eq!(plan.invocations[0].argv, ["echo", ""]);
        }
    }

    #[test]
    fn explicit_package_flags_preserve_member_order() {
        let a = member("alpha");
        let b = member("beta");
        assert_eq!(
            packages_flags(&[&b, &a], PackagesExpansion::Explicit),
            ["--package", "beta@1.2.3", "--package", "alpha@1.2.3"]
        );
    }

    #[test]
    fn per_target_supports_chdir() {
        let mut a = member("alpha");
        a.targets.push(MemberTarget {
            name: "demo".to_owned(),
            kinds: std::iter::once(TargetKind::Example).collect(),
            required_features: BTreeSet::new(),
        });
        let kinds = std::iter::once(TargetKind::Example).collect();
        let plan = Plan::build(
            &[&a],
            &cmd(&["echo", "{target}"]),
            BuildOptions {
                mode: Mode::PerTarget,
                chdir: true,
                packages: PackagesExpansion::Explicit,
                target_kinds: &kinds,
                target_filters: &[],
                require_target_per_package: false,
                workspace_rust_version: None,
            },
        )
        .expect("build target plan");
        assert_eq!(plan.invocations[0].work_dir.as_deref(), Some(a.manifest_dir()));
    }

    #[test]
    fn workspace_rust_version_is_available_in_once_mode() {
        let a = member("alpha");
        let plan = Plan::build(
            &[&a],
            &cmd(&["rustup", "toolchain", "install", "{workspace-rust-version}"]),
            BuildOptions {
                mode: Mode::Once,
                chdir: false,
                packages: PackagesExpansion::Workspace,
                target_kinds: &BTreeSet::new(),
                target_filters: &[],
                require_target_per_package: false,
                workspace_rust_version: Some("1.80"),
            },
        )
        .expect("build");
        assert_eq!(plan.invocations[0].argv, ["rustup", "toolchain", "install", "1.80"]);
    }
}
