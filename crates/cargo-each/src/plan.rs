// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Building the resolved [`Plan`] of command invocations from a selected,
//! filtered member set, an execution [`Mode`], and the command template.

use std::collections::BTreeSet;
use std::path::PathBuf;

use cargo_metadata::TargetKind;

use crate::error::{ChdirConflictsWithOnceError, EachError};
use crate::substitute::{Placeholders, substitute, validate_placeholders};
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

impl Plan {
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
    pub(crate) fn build(
        members: &[&Member],
        mode: Mode,
        chdir: bool,
        packages: PackagesExpansion,
        target_kinds: &BTreeSet<TargetKind>,
        target_required_features: &BTreeSet<String>,
        command: &[String],
    ) -> Result<Self, EachError> {
        if chdir && mode == Mode::Once {
            return Err(ChdirConflictsWithOnceError::new().into());
        }
        // Validate placeholder/mode consistency up front — before the
        // empty-set short-circuit — so a misused template (e.g. `{name}` under
        // `--once`) is a usage error even when the selection resolves to no
        // members, rather than silently passing until some tier is non-empty.
        validate_placeholders(command, mode)?;
        if members.is_empty() {
            return Ok(Self { invocations: Vec::new() });
        }

        let invocations = match mode {
            Mode::PerPackage => members
                .iter()
                .map(|m| {
                    let placeholders = Placeholders::Package {
                        name: m.name.clone(),
                        spec: m.spec(),
                        version: m.version.clone(),
                        manifest: m.manifest_path.display().to_string(),
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
                    member
                        .targets
                        .iter()
                        .filter(|target| {
                            target.kinds.iter().any(|kind| target_kinds.contains(kind))
                                && target_required_features
                                    .iter()
                                    .all(|feature| target.required_features.contains(feature))
                        })
                        .map(|target| {
                            let placeholders = Placeholders::Target {
                                name: member.name.clone(),
                                spec: member.spec(),
                                version: member.version.clone(),
                                manifest: member.manifest_path.display().to_string(),
                                target: target.name.clone(),
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
                let placeholders = Placeholders::Once { packages };
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

/// The `{packages}` expansion: `--workspace` for the whole workspace, else an
/// explicit `--package name@version` per member.
fn packages_flags(members: &[&Member], packages: PackagesExpansion) -> Vec<String> {
    if packages == PackagesExpansion::Workspace {
        return vec!["--workspace".to_owned()];
    }
    let mut flags = Vec::with_capacity(members.len() * 2);
    for m in members {
        flags.push("--package".to_owned());
        flags.push(m.spec());
    }
    flags
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::workspace::MemberTarget;

    fn member(name: &str) -> Member {
        Member {
            name: name.to_owned(),
            version: "1.2.3".to_owned(),
            manifest_path: PathBuf::from(format!("/ws/{name}/Cargo.toml")),
            publishable: true,
            features: BTreeSet::new(),
            targets: Vec::new(),
            has_lib: true,
            has_bin: false,
            dependencies: BTreeSet::new(),
            metadata: Value::Null,
        }
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn build(members: &[&Member], mode: Mode, chdir: bool, packages: PackagesExpansion, command: &[String]) -> Result<Plan, EachError> {
        Plan::build(members, mode, chdir, packages, &BTreeSet::new(), &BTreeSet::new(), command)
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
            ["cargo", "check-external-types", "--manifest-path", "/ws/alpha/Cargo.toml"]
        );
        assert!(plan.invocations[0].work_dir.is_none());
        assert_eq!(plan.invocations[1].argv[3], "/ws/beta/Cargo.toml");
    }

    #[test]
    fn chdir_sets_work_dir_to_crate_root() {
        let a = member("alpha");
        let plan = build(&[&a], Mode::PerPackage, true, PackagesExpansion::Explicit, &cmd(&["cargo", "fmt"])).expect("build");
        assert_eq!(plan.invocations[0].work_dir.as_deref(), Some(PathBuf::from("/ws/alpha").as_path()));
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
        let required = std::iter::once("loom".to_owned()).collect();
        let plan = Plan::build(
            &[&a],
            Mode::PerTarget,
            false,
            PackagesExpansion::Explicit,
            &kinds,
            &required,
            &cmd(&["cargo", "test", "-p", "{name}", "--test", "{target}"]),
        )
        .expect("build target plan");
        assert_eq!(plan.invocations.len(), 1);
        assert_eq!(plan.invocations[0].label.as_deref(), Some("alpha::loom"));
        assert_eq!(plan.invocations[0].argv, ["cargo", "test", "-p", "alpha", "--test", "loom"]);
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
            Mode::PerTarget,
            true,
            PackagesExpansion::Explicit,
            &kinds,
            &BTreeSet::new(),
            &cmd(&["echo", "{target}"]),
        )
        .expect("build target plan");
        assert_eq!(plan.invocations[0].work_dir.as_deref(), Some(PathBuf::from("/ws/alpha").as_path()));
    }
}
