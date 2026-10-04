// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checked Cargo invocations, registry probes, and resolved-source verification.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use crate::cli::{FeatureMode, Options, TestRunner};
use crate::config::Config;
use crate::model::{Candidates, CommandRecord, Provenance, RegistryProbe, Report, Workspace};
use crate::{Result, fail};

pub(crate) fn redact(text: &str) -> String {
    let mut output = text.to_owned();
    for (key, value) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        let value = value.to_string_lossy();
        if value.len() >= 4
            && ["TOKEN", "PASSWORD", "SECRET", "CREDENTIAL"]
                .iter()
                .any(|part| upper.contains(part))
        {
            output = output.replace(value.as_ref(), "[redacted]");
        }
    }
    output
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if ["authorization:", "password", "token =", "token=", "secret", "credential"]
                .iter()
                .any(|term| lower.contains(term))
                || line.split_whitespace().any(|word| word.contains("://") && word.contains('@'))
            {
                "[credential-bearing diagnostic redacted]".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn cargo(
    directory: &Path,
    args: &[String],
    phase: &str,
    config: &Config,
    options: &Options,
    report: &mut Report,
) -> Result<Output> {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    let mut recorded = args.to_vec();
    command
        .current_dir(directory)
        .args(args)
        // Relative to current_dir: MSVC cannot consume Windows verbatim-path prefixes.
        .env("CARGO_TARGET_DIR", "target")
        .env("CARGO_TERM_COLOR", "never");
    for path in &config.paths {
        if !config.automatic(path, directory) {
            command.arg("--config").arg(path);
            recorded.extend(["--config".into(), path.to_string_lossy().into_owned()]);
        }
    }
    if options.offline {
        command.arg("--offline");
        recorded.push("--offline".into());
    }
    let output = command.output()?;
    report.commands.push(CommandRecord {
        phase: phase.into(),
        arguments: recorded.iter().map(|argument| redact(argument)).collect(),
        exit_code: output.status.code(),
        diagnostic: redact(&String::from_utf8_lossy(&output.stderr)),
        output: if phase == "provenance" || phase == "registry_probe" {
            String::new()
        } else {
            redact(&String::from_utf8_lossy(&output.stdout))
        },
        working_directory: directory.to_owned(),
        target_directory: directory.join("target"),
    });
    Ok(output)
}

pub(crate) fn checked(
    directory: &Path,
    args: &[String],
    phase: &str,
    config: &Config,
    options: &Options,
    report: &mut Report,
) -> Result<Output> {
    let output = cargo(directory, args, phase, config, options, report)?;
    if !output.status.success() {
        return fail(format!(
            "{phase} failed:\n{}\n{}",
            redact(&String::from_utf8_lossy(&output.stderr)),
            redact(&String::from_utf8_lossy(&output.stdout))
        ));
    }
    Ok(output)
}

pub(crate) fn probe_candidates(
    candidates: &Candidates,
    output: &Path,
    config: &Config,
    options: &Options,
    report: &mut Report,
) -> Result<()> {
    for candidate in &candidates.candidates {
        if (options.offline || config.offline) && !config.authoritative_local(&candidate.registry) {
            report.registry_probes.push(RegistryProbe {
                name: candidate.name.clone(),
                version: candidate.version.clone(),
                registry: candidate.registry.clone(),
                state: "offline_unverified".into(),
            });
            return fail(format!(
                "offline registry cache cannot establish absence for {}; configure an authoritative directory/local-registry replacement or retry online",
                candidate.name
            ));
        }
        let directory = output.join("registry-probes").join(&candidate.name);
        fs::create_dir_all(directory.join("src"))?;
        let mut manifest = toml_edit::DocumentMut::new();
        manifest["workspace"] = toml_edit::table();
        manifest["package"]["name"] = toml_edit::value("release-guard-registry-probe");
        manifest["package"]["version"] = toml_edit::value("0.0.0");
        manifest["package"]["edition"] = toml_edit::value("2021");
        manifest["dependencies"]["probe"]["package"] = toml_edit::value(&candidate.name);
        manifest["dependencies"]["probe"]["version"] = toml_edit::value(format!("={}", candidate.version));
        if candidate.registry != "crates-io" {
            manifest["dependencies"]["probe"]["registry"] = toml_edit::value(&candidate.registry);
        }
        fs::write(directory.join("Cargo.toml"), manifest.to_string())?;
        fs::write(directory.join("src").join("lib.rs"), "")?;
        let result = cargo(
            &directory,
            &["metadata".into(), "--format-version=1".into()],
            "registry_probe",
            config,
            options,
            report,
        )?;
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        let state = if result.status.success() {
            "published"
        } else if direct_absence(&diagnostic, &candidate.name, &candidate.version, &directory) {
            "absent"
        } else {
            registry_failure(&diagnostic)
        };
        report.registry_probes.push(RegistryProbe {
            name: candidate.name.clone(),
            version: candidate.version.clone(),
            registry: candidate.registry.clone(),
            state: state.into(),
        });
        match state {
            "absent" => {}
            "published" => {
                return fail(format!(
                    "{} {} is already published; current source cannot replace a published registry identity",
                    candidate.name, candidate.version
                ));
            }
            _ => {
                return fail(format!(
                    "registry identity could not be established for {} {}; this is not evidence of absence:\n{}",
                    candidate.name,
                    candidate.version,
                    redact(&diagnostic)
                ));
            }
        }
    }
    Ok(())
}

fn registry_failure(diagnostic: &str) -> &'static str {
    let lower = diagnostic.to_ascii_lowercase();
    if [
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "authentication",
        "credential",
        "no token found",
    ]
    .iter()
    .any(|term| lower.contains(term))
    {
        "authentication_error"
    } else if [
        "timed out",
        "timeout",
        "could not resolve",
        "failed to connect",
        "connection refused",
        "network",
        "failed to download",
        "failed to fetch",
    ]
    .iter()
    .any(|term| lower.contains(term))
    {
        "network_error"
    } else {
        "registry_error"
    }
}

fn direct_absence(diagnostic: &str, name: &str, version: &str, probe_directory: &Path) -> bool {
    let first = diagnostic.lines().find(|line| line.starts_with("error: ")).unwrap_or_default();
    let missing_name = format!("error: no matching package named `{name}` found");
    let missing_version = format!("error: failed to select a version for the requirement `{name} = \"={version}\"`");
    let requiring = diagnostic
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("required by package `"));
    let requiring_path = requiring.and_then(|requiring| {
        requiring
            .strip_prefix("release-guard-registry-probe v0.0.0 (")
            .and_then(|path| path.strip_suffix(")`"))
    });
    // This exact isolated root has one dependency: the candidate's exact version in
    // its intended registry. A namesake elsewhere in a dependency chain proves nothing.
    let directly_required = requiring_path
        .and_then(|path| Path::new(path).canonicalize().ok())
        .zip(probe_directory.canonicalize().ok())
        .is_some_and(|(actual, expected)| actual == expected);
    (first == missing_name || first == missing_version)
        && directly_required
        && diagnostic.lines().filter(|line| line.starts_with("error: ")).count() == 1
        && diagnostic.lines().any(|line| line.starts_with("location searched: "))
        && !diagnostic.contains("which satisfies")
        && ![
            "failed to update",
            "failed to fetch",
            "failed to download",
            "authentication",
            "401",
            "403",
            "timed out",
            "could not resolve",
            "failed to load",
            "yanked",
        ]
        .iter()
        .any(|term| diagnostic.to_ascii_lowercase().contains(term))
}

pub(crate) fn feature_args(options: &Options, mode: FeatureMode) -> Vec<String> {
    let mut args = Vec::new();
    match mode {
        FeatureMode::Default => {}
        FeatureMode::NoDefault => args.push("--no-default-features".into()),
        FeatureMode::All => args.push("--all-features".into()),
    }
    if let Some(features) = &options.features {
        args.extend(["--features".into(), features.clone()]);
    }
    args
}

pub(crate) fn provenance(
    directory: &Path,
    workspace: &Workspace,
    candidates: &Candidates,
    config: &Config,
    options: &Options,
    mode: FeatureMode,
    report: &mut Report,
) -> Result<cargo_metadata::Metadata> {
    let mut args = vec!["metadata".into(), "--format-version=1".into()];
    args.extend(feature_args(options, mode));
    if let Some(target) = &options.target {
        args.extend(["--filter-platform".into(), target.clone()]);
    }
    let result = checked(directory, &args, "provenance", config, options, report)?;
    let metadata: cargo_metadata::Metadata = serde_json::from_slice(&result.stdout)?;
    inspect_metadata(directory, workspace, candidates, config, mode, report, &metadata)?;
    Ok(metadata)
}

fn inspect_metadata(
    directory: &Path,
    workspace: &Workspace,
    candidates: &Candidates,
    config: &Config,
    mode: FeatureMode,
    report: &mut Report,
    metadata: &cargo_metadata::Metadata,
) -> Result<()> {
    let selected: BTreeMap<_, _> = candidates
        .candidates
        .iter()
        .map(|candidate| (candidate.name.as_str(), candidate))
        .collect();
    verify_candidate_edges(metadata, candidates, config)?;
    for package in &metadata.packages {
        let name = package.name.as_str();
        crate::source::reject_links(package.manifest_path.as_std_path())?;
        let canonical_manifest = package.manifest_path.as_std_path().canonicalize()?;
        if package.source.is_some() && workspace.packages.values().any(|original| original.manifest == canonical_manifest) {
            return fail(format!("registry source points back to development package {name}"));
        }
        if package.source.is_none() {
            let Some(expected) = workspace.packages.get(name) else {
                return fail(format!("unexpected local package in W_b: {name}"));
            };
            if expected.publishable() && !selected.contains_key(name) {
                return fail(format!("omitted publishable package leaked into W_b: {name}"));
            }
            let expected_path = directory.join(expected.manifest.strip_prefix(&workspace.root)?).canonicalize()?;
            if canonical_manifest != expected_path || package.version != expected.version {
                return fail(format!("local source provenance mismatch for {name}"));
            }
        } else if workspace.packages.get(name).is_some_and(crate::model::Package::publishable)
            && !package
                .source
                .as_ref()
                .is_some_and(|source| source.repr.starts_with("registry+") || source.repr.starts_with("sparse+"))
        {
            return fail(format!("workspace publishable package {name} resolved from a non-registry source"));
        }
        report.provenance.push(Provenance {
            configuration: format!("{mode:?}"),
            name: name.into(),
            version: package.version.to_string(),
            package_id: redact(&package.id.repr),
            source: package.source.as_ref().map(|source| redact(&source.repr)),
            manifest_path: package.manifest_path.as_std_path().to_owned(),
        });
    }
    Ok(())
}

fn verify_candidate_edges(metadata: &cargo_metadata::Metadata, candidates: &Candidates, config: &Config) -> Result<()> {
    let Some(resolve) = &metadata.resolve else {
        return fail("Cargo metadata omitted the resolved dependency graph");
    };
    let packages: BTreeMap<_, _> = metadata.packages.iter().map(|package| (&package.id, package)).collect();
    for node in &resolve.nodes {
        let package = packages[&node.id];
        for dependency in &package.dependencies {
            let Some(candidate) = candidates.candidates.iter().find(|candidate| candidate.name == dependency.name) else {
                continue;
            };
            let expected_registry = config.patch_key(&candidate.registry)?;
            let matching_registry = dependency
                .registry
                .as_deref()
                .map_or(candidate.registry == "crates-io", |index| index == expected_registry);
            if !matching_registry || !dependency.req.matches(&semver::Version::parse(&candidate.version)?) {
                continue;
            }
            let alias = dependency.rename.as_deref().unwrap_or(&dependency.name).replace('-', "_");
            for edge in node.deps.iter().filter(|edge| {
                edge.name == alias
                    && edge
                        .dep_kinds
                        .iter()
                        .any(|kind| kind.kind == dependency.kind && kind.target == dependency.target)
            }) {
                let resolved = packages[&edge.pkg];
                if resolved.source.is_some() || resolved.version.to_string() != candidate.version {
                    return fail(format!(
                        "compatible candidate patch did not unify {} -> {} (resolved {}); review competing requirements rather than widening candidates",
                        package.name, dependency.name, resolved.id,
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check(
    directory: &Path,
    workspace: &Workspace,
    candidates: &Candidates,
    config: &Config,
    options: &Options,
    report: &mut Report,
) -> Result<()> {
    for mode in &options.feature_mode {
        let metadata = provenance(directory, workspace, candidates, config, options, *mode, report)?;
        for candidate in &candidates.candidates {
            let package = metadata
                .workspace_packages()
                .into_iter()
                .find(|package| package.name.as_str() == candidate.name)
                .ok_or_else(|| std::io::Error::other(format!("candidate {} is missing from the publication workspace", candidate.name)))?;
            let mut args = vec!["build".into(), "--package".into(), package.id.repr.clone()];
            args.extend(feature_args(options, *mode));
            if let Some(target) = &options.target {
                args.extend(["--target".into(), target.clone()]);
            }
            checked(directory, &args, &format!("production:{}", candidate.name), config, options, report)?;
        }
        member_tests(directory, config, options, *mode, report)?;
        let mut args = vec!["build".into(), "--workspace".into(), "--examples".into()];
        args.extend(feature_args(options, *mode));
        if let Some(target) = &options.target {
            args.extend(["--target".into(), target.clone()]);
        }
        checked(directory, &args, "example_build", config, options, report)?;
        doc_tests(directory, &metadata, config, options, *mode, report)?;
    }
    Ok(())
}

fn member_tests(directory: &Path, config: &Config, options: &Options, mode: FeatureMode, report: &mut Report) -> Result<()> {
    let mut args: Vec<String> = match options.test_runner {
        TestRunner::Cargo => vec!["test".into()],
        TestRunner::Nextest => vec!["nextest".into(), "run".into(), "--no-tests=pass".into()],
    };
    args.extend(["--workspace".into(), "--tests".into()]);
    args.extend(feature_args(options, mode));
    if let Some(target) = &options.target {
        args.extend(["--target".into(), target.clone()]);
    }
    checked(directory, &args, "member_tests", config, options, report)?;
    Ok(())
}

fn doc_tests(
    directory: &Path,
    metadata: &cargo_metadata::Metadata,
    config: &Config,
    options: &Options,
    mode: FeatureMode,
    report: &mut Report,
) -> Result<()> {
    if !metadata
        .workspace_packages()
        .iter()
        .any(|package| package.targets.iter().any(|target| target.doctest))
    {
        return Ok(());
    }
    let mut args = vec!["test".into(), "--workspace".into(), "--doc".into()];
    args.extend(feature_args(options, mode));
    if let Some(target) = &options.target {
        args.extend(["--target".into(), target.clone()]);
    }
    checked(directory, &args, "doc_tests", config, options, report)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::{Args, FromArgMatches};

    use super::{direct_absence, inspect_metadata, probe_candidates, redact, registry_failure, verify_candidate_edges};
    use crate::cli::{FeatureMode, Options};
    use crate::config::Config;
    use crate::model::{Candidate, Candidates, Report};
    use crate::test_support;

    #[test]
    fn registry_probe_spawn_failure_is_not_absence() {
        let directory = test_support::directory();
        if std::env::var_os("RELEASE_GUARD_TEST_MISSING_CARGO").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "execute::tests::registry_probe_spawn_failure_is_not_absence"])
                .env("RELEASE_GUARD_TEST_MISSING_CARGO", "1")
                .env("CARGO", directory.path().join("missing-cargo"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let matches = Options::augment_args(clap::Command::new("probe-test"))
            .try_get_matches_from(["probe-test", "--output-dir", "."])
            .unwrap();
        let options = Options::from_arg_matches(&matches).unwrap();
        let mut config = Config::load(directory.path(), directory.path()).unwrap();
        config.offline = false;
        let candidates = Candidates {
            schema_version: 1,
            base_commit: String::new(),
            head_commit: String::new(),
            source_root: directory.path().to_owned(),
            source_digest: String::new(),
            configuration_digest: String::new(),
            artifact_roots: Vec::new(),
            candidates: vec![Candidate {
                name: "consumer".into(),
                version: "1.0.0".into(),
                registry: "crates-io".into(),
                reason: "test".into(),
            }],
        };
        let mut report = Report::new("test");
        assert!(probe_candidates(&candidates, directory.path(), &config, &options, &mut report).is_err());
        assert!(
            directory
                .path()
                .join("registry-probes")
                .join("consumer")
                .join("Cargo.toml")
                .is_file()
        );
        assert!(report.registry_probes.is_empty());
        assert!(report.commands.is_empty());
    }

    #[test]
    fn only_direct_missing_identity_is_absence() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let suffix = format!(
            "location searched: registry `crates-io`\nrequired by package `release-guard-registry-probe v0.0.0 ({})`",
            directory.display()
        );
        assert!(direct_absence(
            &format!("error: no matching package named `example` found\n{suffix}"),
            "example",
            "1.0.0",
            directory
        ));
        assert!(direct_absence(
            &format!("error: failed to select a version for the requirement `example = \"=1.0.0\"`\n{suffix}"),
            "example",
            "1.0.0",
            directory
        ));
        for failure in [
            "error: no matching package named `transitive` found\nrequired by package `release-guard-registry-probe v0.0.0`",
            "error: failed to download config.json: 401 Unauthorized",
            "error: failed to select a version for the requirement `example = \"^1\"`",
            "error: no matching package named `example` found\nfailed to fetch index\nrelease-guard-registry-probe",
        ] {
            assert!(!direct_absence(failure, "example", "1.0.0", directory), "{failure}");
        }
        let transitive = format!(
            "error: no matching package named `example` found\nlocation searched: registry `other`\nrequired by package `example v1.0.0 (registry `original`)`\n... which satisfies dependency `probe` of package `release-guard-registry-probe v0.0.0 ({})`",
            directory.display()
        );
        assert!(!direct_absence(&transitive, "example", "1.0.0", directory));
        assert!(!direct_absence(
            &format!("error: no matching package named `example` found\n{suffix}"),
            "example",
            "1.0.0",
            &directory.join("different-probe")
        ));
    }

    #[test]
    fn credentials_are_not_retained_in_diagnostics() {
        assert!(!redact("fetch https://user:password@example.invalid/index").contains("user:password"));
        assert!(!redact("Authorization: Bearer private-value").contains("private-value"));
        assert_eq!(redact("missing method: released_api"), "missing method: released_api");
    }

    #[test]
    fn registry_failures_have_distinct_machine_readable_categories() {
        assert_eq!(registry_failure("HTTP 401 Unauthorized"), "authentication_error");
        assert_eq!(registry_failure("no token found for registry"), "authentication_error");
        assert_eq!(
            registry_failure("failed to download config.json: operation timed out"),
            "network_error"
        );
        assert_eq!(registry_failure("failed to load directory source"), "registry_error");
    }

    #[test]
    fn provenance_rejects_foreign_local_sources_and_registry_shortcuts() {
        enum Mutation {
            Unknown,
            LocalVersion,
            RegistryShortcut,
            Git,
        }

        let directory = test_support::directory();
        let workspace = test_support::workspace(directory.path(), &[("consumer", ""), ("helper", "")]);
        let config = Config::load(&workspace.root, directory.path()).unwrap();
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(workspace.root.join("Cargo.toml"))
            .exec()
            .unwrap();
        let candidates = Candidates {
            schema_version: 1,
            base_commit: String::new(),
            head_commit: String::new(),
            source_root: workspace.root.clone(),
            source_digest: String::new(),
            configuration_digest: String::new(),
            artifact_roots: Vec::new(),
            candidates: vec![Candidate {
                name: "consumer".into(),
                version: "1.0.0".into(),
                registry: "crates-io".into(),
                reason: "test".into(),
            }],
        };
        let mut report = Report::new("test");
        assert!(
            inspect_metadata(
                &workspace.root,
                &workspace,
                &candidates,
                &config,
                FeatureMode::Default,
                &mut report,
                &metadata
            )
            .unwrap_err()
            .to_string()
            .contains("omitted publishable")
        );
        let original = serde_json::to_value(&metadata).unwrap();
        for (case, message) in [
            (Mutation::Unknown, "unexpected local package"),
            (Mutation::LocalVersion, "local source provenance mismatch"),
            (Mutation::RegistryShortcut, "registry source points back"),
            (Mutation::Git, "non-registry source"),
        ] {
            let mut changed = original.clone();
            let package = changed["packages"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|package| package["name"] == "consumer")
                .unwrap();
            match case {
                Mutation::Unknown => package["name"] = serde_json::json!("foreign"),
                Mutation::LocalVersion => package["version"] = serde_json::json!("9.0.0"),
                Mutation::RegistryShortcut => package["source"] = serde_json::json!("registry+https://example.invalid/index"),
                Mutation::Git => {
                    package["source"] = serde_json::json!("git+https://example.invalid/repo#0123456789012345678901234567890123456789");
                    package["manifest_path"] = serde_json::json!(workspace.root.join("Cargo.toml"));
                }
            }
            let changed = serde_json::from_value(changed).unwrap();
            assert!(
                inspect_metadata(
                    &workspace.root,
                    &workspace,
                    &candidates,
                    &config,
                    FeatureMode::Default,
                    &mut report,
                    &changed
                )
                .unwrap_err()
                .to_string()
                .contains(message),
                "{message}"
            );
        }
        let mut without_graph = original;
        without_graph["resolve"] = serde_json::Value::Null;
        assert!(
            verify_candidate_edges(&serde_json::from_value(without_graph).unwrap(), &candidates, &config)
                .unwrap_err()
                .to_string()
                .contains("omitted the resolved")
        );
    }

    #[test]
    fn compatible_dependency_must_resolve_to_the_candidate_patch() {
        let directory = test_support::directory();
        let workspace = test_support::workspace(
            directory.path(),
            &[
                ("consumer", "[dependencies]\nhelper={path='../helper',version='1'}"),
                ("helper", ""),
            ],
        );
        let config = Config::load(&workspace.root, directory.path()).unwrap();
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(workspace.root.join("Cargo.toml"))
            .exec()
            .unwrap();
        let mut metadata = serde_json::to_value(metadata).unwrap();
        let helper = metadata["packages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|package| package["name"] == "helper")
            .unwrap();
        helper["source"] = serde_json::json!("registry+https://github.com/rust-lang/crates.io-index");
        let candidates = Candidates {
            schema_version: 1,
            base_commit: String::new(),
            head_commit: String::new(),
            source_root: workspace.root,
            source_digest: String::new(),
            configuration_digest: String::new(),
            artifact_roots: Vec::new(),
            candidates: vec![Candidate {
                name: "helper".into(),
                version: "1.0.0".into(),
                registry: "crates-io".into(),
                reason: "test".into(),
            }],
        };
        assert!(
            verify_candidate_edges(&serde_json::from_value(metadata).unwrap(), &candidates, &config)
                .unwrap_err()
                .to_string()
                .contains("did not unify")
        );
    }
}
