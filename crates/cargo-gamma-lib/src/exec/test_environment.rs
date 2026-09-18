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
#[derive(Debug)]
pub(super) struct TestEnvironment {
    common: BTreeMap<OsString, OsString>,
    packages: HashMap<String, PackageEnvironment>,
    integration_tests: crate::HashSet<Utf8PathBuf>,
}

#[derive(Debug, Default)]
struct PackageEnvironment {
    variables: BTreeMap<OsString, OsString>,
    binary_executables: BTreeMap<OsString, OsString>,
}

impl TestEnvironment {
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
        let metadata: Value = serde_json::from_str(metadata)
            .map_err(|cause| error!("could not decode Cargo metadata for the test environment").caused_by(cause))?;
        let mut packages = package_environments(&metadata);
        let mut integration_tests = crate::HashSet::default();

        apply_build_messages(artifacts, &mut packages, &mut integration_tests);

        let known: crate::HashSet<&str> = binaries.iter().map(|binary| binary.package_id.as_str()).collect();
        packages.retain(|package, _environment| known.contains(package.as_str()));

        Ok(Self {
            common: common_environment(root, cargo),
            packages,
            integration_tests,
        })
    }

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

            insert(&mut variables, "CARGO_MANIFEST_DIR", manifest_dir);
            insert(&mut variables, "CARGO_MANIFEST_PATH", manifest_path);
            insert(
                &mut variables,
                "CARGO_PKG_NAME",
                package.get("name").and_then(Value::as_str).unwrap_or(""),
            );
            insert(&mut variables, "CARGO_PKG_VERSION", version_text);
            insert(&mut variables, "CARGO_PKG_VERSION_MAJOR", version.major.to_string());
            insert(&mut variables, "CARGO_PKG_VERSION_MINOR", version.minor.to_string());
            insert(&mut variables, "CARGO_PKG_VERSION_PATCH", version.patch.to_string());
            insert(&mut variables, "CARGO_PKG_VERSION_PRE", version.pre.as_str());
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
            for (variable, field) in [
                ("CARGO_PKG_DESCRIPTION", "description"),
                ("CARGO_PKG_HOMEPAGE", "homepage"),
                ("CARGO_PKG_REPOSITORY", "repository"),
                ("CARGO_PKG_LICENSE", "license"),
                ("CARGO_PKG_LICENSE_FILE", "license_file"),
                ("CARGO_PKG_README", "readme"),
                ("CARGO_PKG_RUST_VERSION", "rust_version"),
            ] {
                insert(&mut variables, variable, package.get(field).and_then(Value::as_str).unwrap_or(""));
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
            Some("build-script-executed") => apply_build_script(&message, package),
            Some("compiler-artifact") => apply_artifact(&message, package, integration_tests),
            _ => {}
        }
    }
}

fn apply_build_script(message: &Value, package: &mut PackageEnvironment) {
    if let Some(out_dir) = message.get("out_dir").and_then(Value::as_str) {
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
        insert(&mut package.binary_executables, format!("CARGO_BIN_EXE_{name}"), executable);
    }
}

fn common_environment(root: &Utf8Path, cargo: &OsStr) -> BTreeMap<OsString, OsString> {
    let mut variables = BTreeMap::new();
    let rustup = rustup_environment(root);
    let cargo = env::var_os("CARGO")
        .or_else(|| rustup.as_ref().map(|environment| environment.cargo.clone()))
        .unwrap_or_else(|| resolve_executable(cargo).unwrap_or_else(|| cargo.to_owned()));

    insert(&mut variables, "CARGO", cargo);
    if let Some(home) = env::var_os("CARGO_HOME").or_else(default_cargo_home) {
        insert(&mut variables, "CARGO_HOME", home);
    }

    if let Some(rustup) = rustup {
        insert(&mut variables, "RUSTUP_TOOLCHAIN", rustup.toolchain);
        insert(&mut variables, "RUSTUP_TOOLCHAIN_SOURCE", rustup.source);
        insert(&mut variables, "RUSTUP_HOME", rustup.home);
        insert(
            &mut variables,
            "RUST_RECURSION_COUNT",
            env::var_os("RUST_RECURSION_COUNT").unwrap_or_else(|| "1".into()),
        );
        #[cfg(windows)]
        if env::var_os("LD_LIBRARY_PATH").is_none() {
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

fn rustup_environment(root: &Utf8Path) -> Option<RustupEnvironment> {
    let active = quiet("rustup", &["show", "active-toolchain"], root)?;
    let toolchain = active.split_whitespace().next()?.to_owned();
    let cargo = quiet("rustup", &["which", "cargo"], root)?;
    let home = env::var_os("RUSTUP_HOME").or_else(|| quiet("rustup", &["show", "home"], root).map(Into::into))?;
    #[cfg(windows)]
    let sysroot = quiet("rustc", &["--print", "sysroot"], root)?;
    let source = env::var_os("RUSTUP_TOOLCHAIN_SOURCE").unwrap_or_else(|| {
        if env::var_os("RUSTUP_TOOLCHAIN").is_some() {
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

fn quiet(program: &str, arguments: &[&str], root: &Utf8Path) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(root.as_std_path())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok().map(|text| text.trim().to_owned()))
        .flatten()
        .filter(|text| !text.is_empty())
}

fn default_cargo_home() -> Option<OsString> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|home| PathBuf::from(home).join(".cargo").into())
}

fn resolve_executable(program: &OsStr) -> Option<OsString> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return if path.is_absolute() {
            Some(path.as_os_str().to_owned())
        } else {
            env::current_dir().ok().map(|current| current.join(path).into())
        };
    }

    let extensions: Vec<OsString> = if cfg!(windows) {
        env::var_os("PATHEXT").map_or_else(
            || vec![".exe".into(), ".cmd".into(), ".bat".into()],
            |value| env::split_paths(&value).map(PathBuf::into_os_string).collect(),
        )
    } else {
        vec![OsString::new()]
    };

    for directory in env::split_paths(&env::var_os("PATH")?) {
        for extension in &extensions {
            let mut name = program.to_owned();
            name.push(extension);
            let candidate = directory.join(name);
            if candidate.is_file() {
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
