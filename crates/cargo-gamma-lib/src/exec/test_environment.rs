// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use super::test_binary::TestBinary;
use crate::HashMap;
use crate::error::{Error, error};

/// Cargo's runtime environment for the test tree built by one run.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct TestEnvironment {
    #[serde(with = "environment_pairs")]
    common: BTreeMap<OsString, OsString>,
    packages: HashMap<String, PackageEnvironment>,
    integration_tests: crate::HashSet<Utf8PathBuf>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PackageEnvironment {
    #[serde(with = "environment_pairs")]
    variables: BTreeMap<OsString, OsString>,
    #[serde(with = "environment_pairs")]
    binary_executables: BTreeMap<OsString, OsString>,
}

mod environment_pairs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{BTreeMap, OsString};

    pub(super) fn serialize<S: Serializer>(values: &BTreeMap<OsString, OsString>, serializer: S) -> Result<S::Ok, S::Error> {
        values.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<OsString, OsString>, D::Error> {
        Vec::<(OsString, OsString)>::deserialize(deserializer).map(|values| values.into_iter().collect())
    }
}

impl TestEnvironment {
    pub(super) fn testing_paths(&self) -> Result<Vec<Utf8PathBuf>, Error> {
        self.packages
            .values()
            .flat_map(|package| {
                package
                    .variables
                    .get(OsStr::new("OUT_DIR"))
                    .into_iter()
                    .chain(package.binary_executables.values())
            })
            .map(|path| {
                Utf8PathBuf::from_path_buf(path.clone().into()).map_err(|_path| error!("a prepared test artifact has a non-UTF-8 path"))
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn fake(package: &str, name: &str, value: &str) -> Self {
        let variables = [(OsString::from(name), OsString::from(value))].into();
        let packages = core::iter::once((
            package.to_owned(),
            PackageEnvironment {
                variables,
                binary_executables: BTreeMap::new(),
            },
        ))
        .collect();

        Self {
            common: BTreeMap::new(),
            packages,
            integration_tests: crate::HashSet::default(),
        }
    }

    pub(super) fn from_cargo(
        metadata: &str,
        artifacts: &str,
        binaries: &[TestBinary],
        root: &Utf8Path,
        cargo: &OsStr,
    ) -> Result<Self, Error> {
        Self::from_cargo_with(metadata, artifacts, binaries, || {
            common_environment(
                root,
                cargo,
                cfg!(windows),
                |name| env::var_os(name),
                quiet,
                || env::current_dir().ok(),
                Path::is_file,
            )
        })
    }

    fn from_cargo_with(
        metadata: &str,
        artifacts: &str,
        binaries: &[TestBinary],
        common: impl FnOnce() -> BTreeMap<OsString, OsString>,
    ) -> Result<Self, Error> {
        let metadata: Value = serde_json::from_str(metadata)
            .map_err(|cause| error!("could not decode Cargo metadata for the test environment").caused_by(cause))?;
        let mut packages = package_environments(&metadata);
        let mut integration_tests = crate::HashSet::default();

        // #[gamma::skip(call_result.default, reason = "apply_build_messages returns unit and this mutator still applies the same messages before replacing unit with unit")]
        apply_build_messages(artifacts, &mut packages, &mut integration_tests);

        let known: crate::HashSet<&str> = binaries.iter().map(|binary| binary.package_id.as_str()).collect();
        packages.retain(|package, _environment| known.contains(package.as_str()));

        Ok(Self {
            common: common(),
            packages,
            integration_tests,
        })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn configure(&self, command: &mut Command, binary: &TestBinary) {
        let _ = command.envs(&self.common);

        let Some(package) = self.packages.get(&binary.package_id) else {
            return;
        };

        let _ = command.envs(&package.variables);
        if self.integration_tests.contains(&binary.path) {
            let _ = command.envs(&package.binary_executables);
        }
    }
}

fn package_environments(metadata: &Value) -> HashMap<String, PackageEnvironment> {
    metadata
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|package| {
            let package_id = package.get("id")?.as_str()?;
            let version_text = package.get("version")?.as_str()?;
            let version = cargo_metadata::semver::Version::parse(version_text).ok()?;
            let manifest_path = Utf8Path::new(package.get("manifest_path")?.as_str()?);
            let manifest_dir = manifest_path.parent().unwrap_or_else(|| Utf8Path::new(""));
            let mut variables = BTreeMap::new();

            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_MANIFEST_DIR", manifest_dir);
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_MANIFEST_PATH", manifest_path);
            let package_name = package.get("name").and_then(Value::as_str).unwrap_or("");
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_PKG_NAME", package_name);
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_PKG_VERSION", version_text);
            let major = version.major.to_string();
            let minor = version.minor.to_string();
            let patch = version.patch.to_string();
            let pre = version.pre.as_str();
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_PKG_VERSION_MAJOR", major);
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_PKG_VERSION_MINOR", minor);
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_PKG_VERSION_PATCH", patch);
            // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
            insert(&mut variables, "CARGO_PKG_VERSION_PRE", pre);
            insert(
                &mut variables,
                "CARGO_PKG_AUTHORS",
                package
                    .get("authors")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(":"),
            );
            // #[gamma::skip(collection.reverse_array, reason = "every tuple writes a distinct map key, so iteration order cannot change the final environment")]
            for (variable, field) in [
                ("CARGO_PKG_DESCRIPTION", "description"),
                ("CARGO_PKG_HOMEPAGE", "homepage"),
                ("CARGO_PKG_REPOSITORY", "repository"),
                ("CARGO_PKG_LICENSE", "license"),
                ("CARGO_PKG_LICENSE_FILE", "license_file"),
                ("CARGO_PKG_README", "readme"),
                ("CARGO_PKG_RUST_VERSION", "rust_version"),
            ] {
                let value = package.get(field).and_then(Value::as_str).unwrap_or("");
                // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
                insert(&mut variables, variable, value);
            }

            Some((
                package_id.to_owned(),
                PackageEnvironment {
                    variables,
                    binary_executables: BTreeMap::new(),
                },
            ))
        })
        .collect()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn apply_build_messages(
    artifacts: &str,
    packages: &mut HashMap<String, PackageEnvironment>,
    integration_tests: &mut crate::HashSet<Utf8PathBuf>,
) {
    for line in artifacts.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(package_id) = message.get("package_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(package) = packages.get_mut(package_id) else {
            continue;
        };

        match message.get("reason").and_then(Value::as_str) {
            Some("build-script-executed") => {
                // #[gamma::skip(call_result.default, reason = "apply_build_script returns unit and this mutator still applies the same message before replacing unit with unit")]
                apply_build_script(&message, package);
            }
            Some("compiler-artifact") => {
                // #[gamma::skip(call_result.default, reason = "apply_artifact returns unit and this mutator still applies the same artifact before replacing unit with unit")]
                apply_artifact(&message, package, integration_tests);
            }
            _ => {}
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn apply_build_script(message: &Value, package: &mut PackageEnvironment) {
    if let Some(out_dir) = message.get("out_dir").and_then(Value::as_str) {
        // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
        insert(&mut package.variables, "OUT_DIR", out_dir);
    }

    if let Some(environment) = message.get("env").and_then(Value::as_array) {
        for pair in environment {
            let Some(pair) = pair.as_array() else {
                continue;
            };
            let [name, value] = pair.as_slice() else {
                continue;
            };
            if let (Some(name), Some(value)) = (name.as_str(), value.as_str()) {
                // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same variable before replacing unit with unit")]
                insert(&mut package.variables, name, value);
            }
        }
    }
}

fn apply_artifact(message: &Value, package: &mut PackageEnvironment, integration_tests: &mut crate::HashSet<Utf8PathBuf>) {
    let Some(executable) = message.get("executable").and_then(Value::as_str) else {
        return;
    };
    let kinds = message
        .get("target")
        .and_then(|target| target.get("kind"))
        .and_then(Value::as_array);

    if kinds.is_some_and(|kinds| kinds.iter().any(|kind| matches!(kind.as_str(), Some("test" | "bench")))) {
        let _ = integration_tests.insert(Utf8PathBuf::from(executable));
    }

    let is_binary = kinds.is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
    let is_test_profile = message
        .get("profile")
        .and_then(|profile| profile.get("test"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_binary
        && !is_test_profile
        && let Some(name) = message.get("target").and_then(|target| target.get("name")).and_then(Value::as_str)
    {
        let variable = format!("CARGO_BIN_EXE_{name}");
        // #[gamma::skip(call_result.default, reason = "insert returns unit and this mutator still inserts the same executable before replacing unit with unit")]
        insert(&mut package.binary_executables, variable, executable);
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn common_environment(
    root: &Utf8Path,
    cargo: &OsStr,
    windows: bool,
    mut lookup: impl FnMut(&str) -> Option<OsString>,
    run: impl FnMut(&str, &[&str], &Utf8Path) -> Option<String>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
    is_file: impl FnMut(&Path) -> bool,
) -> BTreeMap<OsString, OsString> {
    let mut variables = BTreeMap::new();
    let rustup = rustup_environment(root, &mut lookup, run);
    let cargo = lookup("CARGO")
        .or_else(|| rustup.as_ref().map(|environment| environment.cargo.clone()))
        .unwrap_or_else(|| resolve_executable(cargo, windows, &mut lookup, current_dir, is_file).unwrap_or_else(|| cargo.to_owned()));

    insert(&mut variables, "CARGO", cargo);
    if let Some(home) = lookup("CARGO_HOME").or_else(|| default_cargo_home(windows, &mut lookup)) {
        insert(&mut variables, "CARGO_HOME", home);
    }

    if let Some(rustup) = rustup {
        insert(&mut variables, "RUSTUP_TOOLCHAIN", rustup.toolchain);
        insert(&mut variables, "RUSTUP_TOOLCHAIN_SOURCE", rustup.source);
        insert(&mut variables, "RUSTUP_HOME", rustup.home);
        insert(
            &mut variables,
            "RUST_RECURSION_COUNT",
            lookup("RUST_RECURSION_COUNT").unwrap_or_else(|| "1".into()),
        );
        #[cfg(windows)]
        if lookup("LD_LIBRARY_PATH").is_none() {
            insert(&mut variables, "LD_LIBRARY_PATH", rustup.sysroot.join("lib"));
        }
    }

    variables
}

struct RustupEnvironment {
    cargo: OsString,
    toolchain: OsString,
    source: OsString,
    home: OsString,
    #[cfg(windows)]
    sysroot: PathBuf,
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn rustup_environment(
    root: &Utf8Path,
    mut lookup: impl FnMut(&str) -> Option<OsString>,
    mut run: impl FnMut(&str, &[&str], &Utf8Path) -> Option<String>,
) -> Option<RustupEnvironment> {
    let active = run("rustup", &["show", "active-toolchain"], root)?;
    let toolchain = active.split_whitespace().next()?.to_owned();
    let cargo = run("rustup", &["which", "cargo"], root)?;
    let home = lookup("RUSTUP_HOME").or_else(|| run("rustup", &["show", "home"], root).map(Into::into))?;
    #[cfg(windows)]
    let sysroot = run("rustc", &["--print", "sysroot"], root)?;
    let source = lookup("RUSTUP_TOOLCHAIN_SOURCE").unwrap_or_else(|| {
        if lookup("RUSTUP_TOOLCHAIN").is_some() {
            "environment"
        } else if active.contains("directory override") {
            "override-db"
        } else if active.contains("overridden by") {
            "toolchain-file"
        } else {
            "default"
        }
        .into()
    });

    Some(RustupEnvironment {
        cargo: cargo.into(),
        toolchain: toolchain.into(),
        source,
        home,
        #[cfg(windows)]
        sysroot: PathBuf::from(sysroot),
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn quiet(program: &str, arguments: &[&str], root: &Utf8Path) -> Option<String> {
    quiet_with(program, arguments, root, |command| {
        command.output().map(|output| (output.status.success(), output.stdout))
    })
}

fn quiet_with(
    program: &str,
    arguments: &[&str],
    root: &Utf8Path,
    run: impl FnOnce(&mut Command) -> std::io::Result<(bool, Vec<u8>)>,
) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(root.as_std_path())
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let (success, stdout) = run(&mut command).ok()?;
    success
        .then(|| String::from_utf8(stdout).ok().map(|text| text.trim().to_owned()))
        .flatten()
        .filter(|text| !text.is_empty())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn default_cargo_home(windows: bool, mut lookup: impl FnMut(&str) -> Option<OsString>) -> Option<OsString> {
    lookup(if windows { "USERPROFILE" } else { "HOME" }).map(|home| PathBuf::from(home).join(".cargo").into())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn resolve_executable(
    program: &OsStr,
    windows: bool,
    mut lookup: impl FnMut(&str) -> Option<OsString>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
    mut is_file: impl FnMut(&Path) -> bool,
) -> Option<OsString> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return if path.is_absolute() {
            Some(path.as_os_str().to_owned())
        } else {
            current_dir().map(|current| current.join(path).into())
        };
    }

    let extensions: Vec<OsString> = if windows {
        lookup("PATHEXT").map_or_else(
            || vec![".exe".into(), ".cmd".into(), ".bat".into()],
            |value| env::split_paths(&value).map(PathBuf::into_os_string).collect(),
        )
    } else {
        vec![OsString::new()]
    };

    for directory in env::split_paths(&lookup("PATH")?) {
        for extension in &extensions {
            let mut name = program.to_owned();
            name.push(extension);
            let candidate = directory.join(name);
            if is_file(&candidate) {
                return Some(candidate.into());
            }
        }
    }

    None
}

fn insert(map: &mut BTreeMap<OsString, OsString>, name: impl Into<OsString>, value: impl Into<OsString>) {
    let _ = map.insert(name.into(), value.into());
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const PACKAGE_ID: &str = "path+file:///workspace#subject@1.2.3-alpha.1";

    fn metadata() -> Value {
        serde_json::from_str(
            r#"{
                "packages":[{
                    "name":"subject",
                    "version":"1.2.3-alpha.1",
                    "id":"path+file:///workspace#subject@1.2.3-alpha.1",
                    "license":"MIT",
                    "license_file":"/workspace/LICENSE",
                    "description":"description",
                    "source":null,
                    "dependencies":[],
                    "targets":[],
                    "features":{},
                    "manifest_path":"/workspace/Cargo.toml",
                    "metadata":null,
                    "publish":null,
                    "authors":["One","Two"],
                    "categories":[],
                    "keywords":[],
                    "readme":"/workspace/README.md",
                    "repository":"https://example.invalid/repository",
                    "homepage":"https://example.invalid",
                    "documentation":null,
                    "edition":"2024",
                    "links":null,
                    "default_run":null,
                    "rust_version":"1.90"
                }],
                "workspace_members":["path+file:///workspace#subject@1.2.3-alpha.1"],
                "workspace_default_members":["path+file:///workspace#subject@1.2.3-alpha.1"],
                "resolve":null,
                "target_directory":"/target",
                "build_directory":"/target",
                "version":1,
                "workspace_root":"/workspace",
                "metadata":null
            }"#,
        )
        .expect("the metadata fixture is valid")
    }

    fn value<'a>(environment: &'a PackageEnvironment, name: &str) -> Option<&'a str> {
        environment.variables.get(OsStr::new(name)).and_then(|value| value.to_str())
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the scripted adapter matches the production optional-output signature"
    )]
    fn tool_output(program: &str, arguments: &[&str], root: &Utf8Path) -> Option<String> {
        assert_eq!(root, Utf8Path::new("workspace"));
        Some(
            match (program, arguments) {
                ("rustup", ["show", "active-toolchain"]) => "stable-target (default)",
                ("rustup", ["which", "cargo"]) => "rustup-cargo",
                ("rustup", ["show", "home"]) => "rustup-home",
                ("rustc", ["--print", "sysroot"]) => "sysroot",
                _ => panic!("unexpected tool request: {program} {arguments:?}"),
            }
            .to_owned(),
        )
    }

    #[test]
    fn rustup_source_precedence_and_home_overrides_are_reconstructed_exactly() {
        for (active, environment, expected_source) in [
            ("stable-target (default)", vec![], "default"),
            ("stable-target (directory override)", vec![], "override-db"),
            ("stable-target (overridden by file)", vec![], "toolchain-file"),
            ("stable-target (directory override; overridden by file)", vec![], "override-db"),
            (
                "stable-target (directory override)",
                vec![("RUSTUP_TOOLCHAIN", "explicit")],
                "environment",
            ),
            (
                "stable-target (directory override)",
                vec![("RUSTUP_TOOLCHAIN", "explicit"), ("RUSTUP_TOOLCHAIN_SOURCE", "custom")],
                "custom",
            ),
        ] {
            for override_home in [false, true] {
                let mut environment: BTreeMap<_, _> = environment.iter().map(|(key, value)| (*key, OsString::from(*value))).collect();
                if override_home {
                    let _ = environment.insert("RUSTUP_HOME", OsString::from("explicit-home"));
                }
                let found = rustup_environment(
                    Utf8Path::new("workspace"),
                    |name| environment.get(name).cloned(),
                    |program, arguments, root| {
                        if arguments == ["show", "active-toolchain"] {
                            assert_eq!(program, "rustup");
                            assert_eq!(root, Utf8Path::new("workspace"));
                            Some(active.to_owned())
                        } else {
                            tool_output(program, arguments, root)
                        }
                    },
                )
                .expect("all required fixture commands succeed");
                assert_eq!(found.cargo, OsStr::new("rustup-cargo"));
                assert_eq!(found.toolchain, OsStr::new("stable-target"));
                assert_eq!(found.source, OsStr::new(expected_source));
                assert_eq!(found.home, OsStr::new(if override_home { "explicit-home" } else { "rustup-home" }));
                #[cfg(windows)]
                assert_eq!(found.sysroot, PathBuf::from("sysroot"));
            }
        }
    }

    #[test]
    fn missing_rustup_results_short_circuit_without_unwrapping() {
        let stages = [
            ("rustup", vec!["show", "active-toolchain"]),
            ("rustup", vec!["which", "cargo"]),
            ("rustup", vec!["show", "home"]),
        ]
        .into_iter()
        .chain(cfg!(windows).then_some(("rustc", vec!["--print", "sysroot"])));
        for (failed_program, failed_arguments) in stages {
            assert!(
                rustup_environment(
                    Utf8Path::new("workspace"),
                    |_| None,
                    |program, arguments, root| {
                        if program == failed_program && arguments == failed_arguments {
                            None
                        } else {
                            tool_output(program, arguments, root)
                        }
                    },
                )
                .is_none(),
                "{failed_program} {failed_arguments:?}"
            );
        }
        assert!(rustup_environment(Utf8Path::new("workspace"), |_| None, |_, _, _| Some(String::new())).is_none());
    }

    #[test]
    fn default_home_uses_the_requested_platform_key_and_preserves_absence() {
        for windows in [false, true] {
            let expected_key = if windows { "USERPROFILE" } else { "HOME" };
            for present in [false, true] {
                let found = default_cargo_home(windows, |name| {
                    assert_eq!(name, expected_key);
                    present.then(|| OsString::from("home"))
                });
                assert_eq!(found, present.then(|| PathBuf::from("home").join(".cargo").into_os_string()));
            }
        }
    }

    #[test]
    fn executable_resolution_preserves_path_shortcuts_and_directory_failures() {
        let absolute = PathBuf::from(if cfg!(windows) { r"C:\root\tools\tool" } else { "/root/tools/tool" });
        let relative = PathBuf::from("tools").join("tool");
        for (program, current, expected) in [
            (absolute.clone(), None, Some(absolute.into_os_string())),
            (
                relative.clone(),
                Some(PathBuf::from("cwd")),
                Some(PathBuf::from("cwd").join(&relative).into_os_string()),
            ),
            (relative, None, None),
        ] {
            assert_eq!(
                resolve_executable(program.as_os_str(), cfg!(windows), |_| None, || current, |_| false),
                expected,
                "{program:?}"
            );
        }
        for windows in [false, true] {
            assert_eq!(resolve_executable(OsStr::new("tool"), windows, |_| None, || None, |_| false), None);
        }
    }

    #[test]
    fn executable_search_obeys_directory_extension_order_and_file_existence() {
        let path = env::join_paths(["first", "second"]).expect("the fixed relative directory names contain no separator");
        for windows in [false, true] {
            let extensions = if windows { vec![".exe", ".cmd", ".bat"] } else { vec![""] };
            for extension in extensions {
                let expected = PathBuf::from("second").join(format!("tool{extension}"));
                assert_eq!(
                    resolve_executable(
                        OsStr::new("tool"),
                        windows,
                        |name| if name == "PATH" {
                            Some(path.clone())
                        } else {
                            assert_eq!(name, "PATHEXT");
                            None
                        },
                        || panic!("bare executable names must be searched on PATH, not joined to the working directory"),
                        |candidate| candidate == expected.as_path(),
                    ),
                    Some(expected.into_os_string()),
                    "{windows} {extension}"
                );
            }
        }
        let expected = PathBuf::from("first").join("tool.exe");
        assert_eq!(
            resolve_executable(
                OsStr::new("tool"),
                true,
                |name| (name == "PATH").then(|| path.clone()),
                || None,
                |candidate| candidate == expected.as_path() || candidate == PathBuf::from("first").join("tool.cmd").as_path(),
            ),
            Some(expected.into_os_string())
        );
        let extensions = env::join_paths([".custom", ".other"]).expect("the fixed extension names contain no separator");
        let expected = PathBuf::from("first").join("tool.custom");
        assert_eq!(
            resolve_executable(
                OsStr::new("tool"),
                true,
                |name| match name {
                    "PATH" => Some(path.clone()),
                    "PATHEXT" => Some(extensions.clone()),
                    _ => panic!("unexpected environment lookup: {name}"),
                },
                || None,
                |candidate| candidate == expected.as_path() || candidate == PathBuf::from("first").join("tool.other").as_path(),
            ),
            Some(expected.into_os_string())
        );
        assert_eq!(
            resolve_executable(
                OsStr::new("tool"),
                true,
                |name| (name == "PATH").then(|| path.clone()),
                || None,
                |_| false
            ),
            None
        );
    }

    #[test]
    fn common_environment_keeps_cargo_precedence_defaults_and_rustup_values() {
        let empty = common_environment(
            Utf8Path::new("workspace"),
            OsStr::new("cargo"),
            false,
            |_| None,
            |_, _, _| None,
            || None,
            |_| false,
        );
        assert_eq!(empty, BTreeMap::from([(OsString::from("CARGO"), OsString::from("cargo"))]));
        for windows in [false, true] {
            let home_key = if windows { "USERPROFILE" } else { "HOME" };
            let found = common_environment(
                Utf8Path::new("workspace"),
                OsStr::new("cargo"),
                windows,
                |name| (name == home_key).then(|| OsString::from("home")),
                |_, _, _| None,
                || None,
                |_| false,
            );
            assert_eq!(
                found[OsStr::new("CARGO_HOME")],
                PathBuf::from("home").join(".cargo").into_os_string()
            );
            assert_eq!(found[OsStr::new("CARGO")], OsStr::new("cargo"));
        }
        for override_cargo in [false, true] {
            let found = common_environment(
                Utf8Path::new("workspace"),
                OsStr::new("fallback-cargo"),
                cfg!(windows),
                |name| match name {
                    "CARGO" if override_cargo => Some("explicit-cargo".into()),
                    "CARGO_HOME" => Some("cargo-home".into()),
                    "RUSTUP_HOME" => Some("explicit-rustup-home".into()),
                    "RUSTUP_TOOLCHAIN_SOURCE" => Some("explicit-source".into()),
                    "RUST_RECURSION_COUNT" => Some("9".into()),
                    "LD_LIBRARY_PATH" if override_cargo => Some("existing-library-path".into()),
                    _ => None,
                },
                tool_output,
                || None,
                |_| false,
            );
            assert_eq!(
                found[OsStr::new("CARGO")],
                OsStr::new(if override_cargo { "explicit-cargo" } else { "rustup-cargo" })
            );
            assert_eq!(found[OsStr::new("CARGO_HOME")], OsStr::new("cargo-home"));
            assert_eq!(found[OsStr::new("RUSTUP_HOME")], OsStr::new("explicit-rustup-home"));
            assert_eq!(found[OsStr::new("RUSTUP_TOOLCHAIN")], OsStr::new("stable-target"));
            assert_eq!(found[OsStr::new("RUSTUP_TOOLCHAIN_SOURCE")], OsStr::new("explicit-source"));
            assert_eq!(found[OsStr::new("RUST_RECURSION_COUNT")], OsStr::new("9"));
            #[cfg(windows)]
            if override_cargo {
                assert!(!found.contains_key(OsStr::new("LD_LIBRARY_PATH")));
            } else {
                assert_eq!(
                    found[OsStr::new("LD_LIBRARY_PATH")],
                    PathBuf::from("sysroot").join("lib").into_os_string()
                );
            }
        }
        let found = common_environment(
            Utf8Path::new("workspace"),
            OsStr::new("cargo"),
            cfg!(windows),
            |_| None,
            tool_output,
            || None,
            |_| false,
        );
        assert_eq!(found[OsStr::new("RUST_RECURSION_COUNT")], OsStr::new("1"));
        assert_eq!(found[OsStr::new("RUSTUP_HOME")], OsStr::new("rustup-home"));
        assert_eq!(found[OsStr::new("CARGO")], OsStr::new("rustup-cargo"));
    }

    #[test]
    fn quiet_constructs_the_command_and_filters_scripted_output() {
        for (output, expected) in [
            (
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "scripted spawn failure")),
                None,
            ),
            (Ok((false, b"failed output".to_vec())), None),
            (Ok((true, vec![0xff])), None),
            (Ok((true, Vec::new())), None),
            (Ok((true, b" \r\n\t ".to_vec())), None),
            (Ok((true, b" \r\nready\nvalue \t".to_vec())), Some("ready\nvalue".to_owned())),
        ] {
            let actual = quiet_with("scripted-tool", &["--query", "value"], Utf8Path::new("workspace"), |command| {
                assert_eq!(command.get_program(), OsStr::new("scripted-tool"));
                assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new("--query"), OsStr::new("value")]);
                assert_eq!(command.get_current_dir(), Some(Path::new("workspace")));
                output
            });
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn cargo_environment_filters_unknown_packages_and_keeps_build_messages() {
        let mut document = metadata();
        let mut unknown = document["packages"][0].clone();
        unknown["id"] = serde_json::json!("unknown");
        document["packages"].as_array_mut().expect("fixture packages array").push(unknown);
        let binary = TestBinary {
            package_id: PACKAGE_ID.to_owned(),
            ..crate::testing::test_binary("target/unit")
        };
        let messages = [
            serde_json::json!({"reason": "build-script-executed", "package_id": PACKAGE_ID, "env": [["FIXTURE_VALUE", "ready"]]}),
            serde_json::json!({"reason": "compiler-artifact", "package_id": PACKAGE_ID, "target": {"kind": ["test"]}, "executable": "target/integration"}),
        ].into_iter().map(|value| value.to_string()).collect::<Vec<_>>().join("\n");
        let common = BTreeMap::from([(OsString::from("CARGO"), OsString::from("scripted-cargo"))]);
        let found = TestEnvironment::from_cargo_with(&document.to_string(), &messages, &[binary], || common.clone())
            .expect("the scripted metadata is valid");
        assert_eq!(found.packages.len(), 1);
        assert!(found.packages.contains_key(PACKAGE_ID));
        assert!(!found.packages.contains_key("unknown"));
        assert_eq!(value(&found.packages[PACKAGE_ID], "FIXTURE_VALUE"), Some("ready"));
        assert!(found.integration_tests.contains(Utf8Path::new("target/integration")));
        assert_eq!(found.common, common);
    }

    #[test]
    fn package_metadata_becomes_the_cargo_runtime_contract() {
        let packages = package_environments(&metadata());
        let package = &packages[PACKAGE_ID];

        assert_eq!(value(package, "CARGO_MANIFEST_DIR"), Some("/workspace"));
        assert_eq!(value(package, "CARGO_MANIFEST_PATH"), Some("/workspace/Cargo.toml"));
        assert_eq!(value(package, "CARGO_PKG_NAME"), Some("subject"));
        assert_eq!(value(package, "CARGO_PKG_VERSION"), Some("1.2.3-alpha.1"));
        assert_eq!(value(package, "CARGO_PKG_VERSION_MAJOR"), Some("1"));
        assert_eq!(value(package, "CARGO_PKG_VERSION_MINOR"), Some("2"));
        assert_eq!(value(package, "CARGO_PKG_VERSION_PATCH"), Some("3"));
        assert_eq!(value(package, "CARGO_PKG_VERSION_PRE"), Some("alpha.1"));
        assert_eq!(value(package, "CARGO_PKG_AUTHORS"), Some("One:Two"));
        assert_eq!(value(package, "CARGO_PKG_DESCRIPTION"), Some("description"));
        assert_eq!(value(package, "CARGO_PKG_HOMEPAGE"), Some("https://example.invalid"));
        assert_eq!(value(package, "CARGO_PKG_REPOSITORY"), Some("https://example.invalid/repository"));
        assert_eq!(value(package, "CARGO_PKG_LICENSE"), Some("MIT"));
        assert_eq!(value(package, "CARGO_PKG_LICENSE_FILE"), Some("/workspace/LICENSE"));
        assert_eq!(value(package, "CARGO_PKG_README"), Some("/workspace/README.md"));
        assert_eq!(value(package, "CARGO_PKG_RUST_VERSION"), Some("1.90"));
    }

    #[test]
    fn absent_optional_package_strings_become_empty_cargo_variables() {
        for (field, variable) in [
            ("description", "CARGO_PKG_DESCRIPTION"),
            ("homepage", "CARGO_PKG_HOMEPAGE"),
            ("repository", "CARGO_PKG_REPOSITORY"),
            ("license", "CARGO_PKG_LICENSE"),
            ("license_file", "CARGO_PKG_LICENSE_FILE"),
            ("readme", "CARGO_PKG_README"),
            ("rust_version", "CARGO_PKG_RUST_VERSION"),
        ] {
            let mut metadata = metadata();
            let package = metadata["packages"][0].as_object_mut().expect("package object");
            let _ = package.remove(field);
            let packages = package_environments(&metadata);

            assert_eq!(value(&packages[PACKAGE_ID], variable), Some(""), "{field}");
        }
    }

    #[test]
    fn malformed_required_package_fields_are_ignored_without_panicking() {
        for (field, replacement) in [
            ("id", None),
            ("id", Some(serde_json::json!(42))),
            ("version", None),
            ("version", Some(serde_json::json!(42))),
            ("version", Some(serde_json::json!("not-semver"))),
            ("manifest_path", None),
            ("manifest_path", Some(serde_json::json!(42))),
        ] {
            let mut document = metadata();
            let package = document["packages"][0].as_object_mut().expect("the fixture has a package object");
            match replacement {
                Some(value) => {
                    let _ = package.insert(field.to_owned(), value);
                }
                None => {
                    let _ = package.remove(field);
                }
            }
            assert!(package_environments(&document).is_empty(), "{field}");
        }
    }

    #[test]
    fn bare_manifests_and_missing_names_have_empty_not_placeholder_values() {
        for manifest in ["Cargo.toml", ""] {
            let mut document = metadata();
            document["packages"][0]["manifest_path"] = serde_json::json!(manifest);
            let _ = document["packages"][0]
                .as_object_mut()
                .expect("fixture package object")
                .remove("name");
            let packages = package_environments(&document);
            assert_eq!(value(&packages[PACKAGE_ID], "CARGO_MANIFEST_DIR"), Some(""), "{manifest:?}");
            assert_eq!(value(&packages[PACKAGE_ID], "CARGO_PKG_NAME"), Some(""), "{manifest:?}");
        }
    }

    #[test]
    fn malformed_cargo_metadata_is_a_diagnostic_not_a_panic() {
        let cause = TestEnvironment::from_cargo("not-json", "", &[], Utf8Path::new("."), OsStr::new("cargo"))
            .expect_err("invalid JSON must be rejected before environment discovery");
        assert!(
            cause
                .to_string()
                .contains("could not decode Cargo metadata for the test environment")
        );
    }

    #[test]
    fn malformed_build_messages_and_environment_pairs_do_not_hide_later_valid_entries() {
        let mut packages = package_environments(&metadata());
        let mut integration = crate::HashSet::default();
        let messages = [
            "not-json".to_owned(),
            serde_json::json!({"reason": "build-script-executed"}).to_string(),
            serde_json::json!({"reason": "build-script-executed", "package_id": "unknown"}).to_string(),
            serde_json::json!({
                "reason": "build-script-executed",
                "package_id": PACKAGE_ID,
                "out_dir": "/target/out",
                "env": [42, ["short"], ["long", "value", "extra"], [42, "wrong"], ["BUILD_OK", "ready"]]
            })
            .to_string(),
        ]
        .join("\n");
        apply_build_messages(&messages, &mut packages, &mut integration);
        assert_eq!(value(&packages[PACKAGE_ID], "OUT_DIR"), Some("/target/out"));
        assert_eq!(value(&packages[PACKAGE_ID], "BUILD_OK"), Some("ready"));
        assert_eq!(value(&packages[PACKAGE_ID], "short"), None);
        assert_eq!(value(&packages[PACKAGE_ID], "long"), None);
        assert!(integration.is_empty());
    }

    #[test]
    fn artifact_kind_and_test_profile_independently_control_runtime_variables() {
        for (kinds, profile, executable, is_integration, is_binary) in [
            (
                serde_json::json!(["lib"]),
                serde_json::json!({"test": false}),
                Some("/target/a"),
                false,
                false,
            ),
            (
                serde_json::json!([]),
                serde_json::json!({"test": false}),
                Some("/target/a"),
                false,
                false,
            ),
            (
                serde_json::json!(["bin", "example"]),
                serde_json::json!({"test": false}),
                Some("/target/a"),
                false,
                true,
            ),
            (
                serde_json::json!(["bin"]),
                serde_json::json!({"test": true}),
                Some("/target/a"),
                false,
                false,
            ),
            (serde_json::json!(["bin"]), Value::Null, Some("/target/a"), false, true),
            (
                serde_json::json!(["test", "lib"]),
                serde_json::json!({"test": true}),
                Some("/target/a"),
                true,
                false,
            ),
            (
                serde_json::json!(["bench", "lib"]),
                serde_json::json!({"test": true}),
                Some("/target/a"),
                true,
                false,
            ),
            (serde_json::json!(["bin"]), serde_json::json!({"test": false}), None, false, false),
        ] {
            let mut package = PackageEnvironment::default();
            let mut integration = crate::HashSet::default();
            let message = serde_json::json!({
                "target": {"kind": kinds, "name": "tool"},
                "profile": profile,
                "executable": executable,
            });
            apply_artifact(&message, &mut package, &mut integration);
            assert_eq!(integration.contains(Utf8Path::new("/target/a")), is_integration, "{message}");
            assert_eq!(
                package
                    .binary_executables
                    .get(OsStr::new("CARGO_BIN_EXE_tool"))
                    .and_then(|path| path.to_str()),
                is_binary.then_some("/target/a"),
                "{message}"
            );
        }
    }

    #[test]
    fn build_messages_add_build_script_and_integration_test_variables() {
        let mut packages = package_environments(&metadata());
        let mut integration_tests = crate::HashSet::default();
        let artifacts = format!(
            r#"{{"reason":"build-script-executed","package_id":"{PACKAGE_ID}","out_dir":"/target/out","env":[["SUBJECT_BUILD_VALUE","ready"]]}}
{{"reason":"compiler-artifact","package_id":"{PACKAGE_ID}","target":{{"kind":["bin"],"name":"subject-cli"}},"profile":{{"test":false}},"executable":"/target/subject-cli"}}
{{"reason":"compiler-artifact","package_id":"{PACKAGE_ID}","target":{{"kind":["test"],"name":"environment"}},"profile":{{"test":true}},"executable":"/target/environment"}}"#
        );

        apply_build_messages(&artifacts, &mut packages, &mut integration_tests);

        let package = &packages[PACKAGE_ID];
        assert_eq!(value(package, "OUT_DIR"), Some("/target/out"));
        assert_eq!(value(package, "SUBJECT_BUILD_VALUE"), Some("ready"));
        assert_eq!(
            package
                .binary_executables
                .get(OsStr::new("CARGO_BIN_EXE_subject-cli"))
                .and_then(|value| value.to_str()),
            Some("/target/subject-cli")
        );
        assert!(integration_tests.contains(Utf8Path::new("/target/environment")));
    }

    #[test]
    fn only_integration_tests_receive_binary_executable_variables() {
        let package = TestBinary {
            package_id: PACKAGE_ID.to_owned(),
            ..crate::testing::test_binary("/target/unit")
        };
        let integration = TestBinary {
            path: Utf8PathBuf::from("/target/integration"),
            ..package.clone()
        };
        let mut packages = package_environments(&metadata());
        insert(
            &mut packages.get_mut(PACKAGE_ID).expect("known package").binary_executables,
            "CARGO_BIN_EXE_subject",
            "/target/subject",
        );
        let mut integration_tests = crate::HashSet::default();
        let _ = integration_tests.insert(integration.path.clone());
        let environment = TestEnvironment {
            common: BTreeMap::from([(OsString::from("CARGO"), OsString::from("/bin/cargo"))]),
            packages,
            integration_tests,
        };

        let mut unit = Command::new("unit");
        environment.configure(&mut unit, &package);
        assert!(unit.get_envs().all(|(name, _value)| name != OsStr::new("CARGO_BIN_EXE_subject")));

        let mut integration_command = Command::new("integration");
        environment.configure(&mut integration_command, &integration);
        let configured: BTreeMap<_, _> = integration_command.get_envs().collect();
        assert_eq!(configured[OsStr::new("CARGO_BIN_EXE_subject")], Some(OsStr::new("/target/subject")));

        let mut nextest = Command::new("cargo-nextest");
        environment.configure(&mut nextest, &integration);
        assert!(
            nextest
                .get_envs()
                .any(|(name, value)| { name == OsStr::new("CARGO_PKG_NAME") && value == Some(OsStr::new("subject")) })
        );
        assert!(
            nextest
                .get_envs()
                .any(|(name, value)| { name == OsStr::new("CARGO") && value == Some(OsStr::new("/bin/cargo")) })
        );
    }
}
