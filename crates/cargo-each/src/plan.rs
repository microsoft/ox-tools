// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Building the resolved [`Plan`] of command invocations from a selected,
//! filtered member set, a [`Mode`], and the command template.

use std::path::PathBuf;

use crate::error::{ChdirRequiresPerPackageError, EachError};
use crate::substitute::{Placeholders, substitute};
use crate::workspace::Member;

/// How the command is run over the selected set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Run the command once per selected member (default).
    PerPackage,
    /// Run the command exactly once for the whole set (`--once`).
    Once,
}

/// One fully-resolved command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// A short label for progress output (the member name in per-package
    /// mode; `None` in once mode).
    pub label: Option<String>,
    /// The program and arguments to spawn, placeholders already expanded.
    pub argv: Vec<String>,
    /// The working directory to run in, when `--chdir` is set (the member's
    /// crate root). `None` runs in the caller's current directory.
    pub work_dir: Option<PathBuf>,
}

/// The resolved list of invocations `cargo-each` will run.
///
/// An empty [`Plan::invocations`] means the selection resolved to nothing:
/// the caller treats that as a successful no-op (exit 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The invocations, in run order.
    pub invocations: Vec<Invocation>,
}

impl Plan {
    /// Build the plan.
    ///
    /// `chdir` runs each per-package invocation from the member's crate root
    /// (its `{manifest_dir}`); it is only valid in per-package mode — combined
    /// with [`Mode::Once`] it is a usage error.
    ///
    /// `whole_workspace` controls the `{packages}` expansion in once mode:
    /// when the resolved set is the entire workspace (see
    /// [`Selection::is_whole_workspace`]), `{packages}` becomes
    /// `--workspace`; otherwise it becomes an explicit `--package name@version`
    /// list.
    ///
    /// [`Selection::is_whole_workspace`]: crate::Selection::is_whole_workspace
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] if `chdir` is combined with [`Mode::Once`], or if
    /// a placeholder in `command` is used in the wrong mode.
    pub fn build(members: &[&Member], mode: Mode, chdir: bool, whole_workspace: bool, command: &[String]) -> Result<Self, EachError> {
        if chdir && mode == Mode::Once {
            return Err(ChdirRequiresPerPackageError::new().into());
        }
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
            Mode::Once => {
                let packages = packages_flags(members, whole_workspace);
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
fn packages_flags(members: &[&Member], whole_workspace: bool) -> Vec<String> {
    if whole_workspace {
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
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    fn member(name: &str) -> Member {
        Member {
            name: name.to_owned(),
            version: "1.2.3".to_owned(),
            manifest_path: PathBuf::from(format!("/ws/{name}/Cargo.toml")),
            has_lib: true,
            has_bin: false,
            dependencies: BTreeSet::new(),
            metadata: Value::Null,
        }
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_set_yields_empty_plan() {
        let plan = Plan::build(&[], Mode::PerPackage, false, false, &cmd(&["cargo", "test"])).expect("build");
        assert!(plan.invocations.is_empty());
    }

    #[test]
    fn per_package_builds_one_invocation_each() {
        let a = member("alpha");
        let b = member("beta");
        let plan = Plan::build(
            &[&a, &b],
            Mode::PerPackage,
            false,
            false,
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
        let plan = Plan::build(&[&a], Mode::PerPackage, true, false, &cmd(&["cargo", "fmt"])).expect("build");
        assert_eq!(plan.invocations[0].work_dir.as_deref(), Some(PathBuf::from("/ws/alpha").as_path()));
    }

    #[test]
    fn chdir_with_once_is_a_usage_error() {
        let a = member("alpha");
        let err = Plan::build(&[&a], Mode::Once, true, false, &cmd(&["cargo", "test", "{packages}"])).expect_err("chdir+once must error");
        let rendered = err.to_string();
        assert!(rendered.contains("--chdir"), "rendered: {rendered}");
        assert!(rendered.contains("--once"), "rendered: {rendered}");
    }

    #[test]
    fn once_whole_workspace_uses_workspace_flag() {
        let a = member("alpha");
        let plan = Plan::build(&[&a], Mode::Once, false, true, &cmd(&["cargo", "clippy", "{packages}"])).expect("build");
        assert_eq!(plan.invocations.len(), 1);
        assert_eq!(plan.invocations[0].argv, ["cargo", "clippy", "--workspace"]);
    }

    #[test]
    fn once_subset_uses_explicit_package_flags() {
        let a = member("alpha");
        let b = member("beta");
        let plan = Plan::build(&[&a, &b], Mode::Once, false, false, &cmd(&["cargo", "clippy", "{packages}"])).expect("build");
        assert_eq!(
            plan.invocations[0].argv,
            ["cargo", "clippy", "--package", "alpha@1.2.3", "--package", "beta@1.2.3"]
        );
    }
}
