// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[test]
fn survey_metadata_loads_once_with_workspace_targets_features_and_the_required_dependency_graph() {
    let features = FeatureArgs {
        features: vec!["alpha/fast".into()],
        all_features: true,
        no_default_features: true,
    };
    for cache_inputs in [false, true] {
        let mut reads = 0;
        let metadata = load_survey_metadata_using(Utf8Path::new("workspace"), &features, cache_inputs, |metadata| {
            reads += 1;
            let command = metadata.cargo_command();
            assert_eq!(command.get_current_dir(), Some(std::path::Path::new("workspace")));
            let arguments = command.get_args().map(|argument| argument.to_str().unwrap()).collect::<Vec<_>>();
            for expected in ["--all-features", "--no-default-features", "alpha/fast"] {
                assert!(arguments.contains(&expected), "{arguments:?}");
            }
            assert_eq!(arguments.contains(&"--offline"), !cache_inputs, "{arguments:?}");
            assert_eq!(arguments.contains(&"--no-deps"), cache_inputs, "{arguments:?}");
            let mut metadata: Metadata =
                serde_json::from_str(include_str!("../../../tests/fixtures/correctness/resource-dependencies.json")).unwrap();
            if cache_inputs {
                metadata.resolve = None;
                metadata.packages.retain(|package| metadata.workspace_members.contains(&package.id));
            }
            Ok(metadata)
        })
        .unwrap();
        assert_eq!(reads, 1);
        assert_eq!(metadata.workspace_root.as_str(), "C:/fixture");
        assert_eq!(metadata.target_directory.as_str(), "C:/fixture/target");
        let members = metadata.workspace_packages();
        assert_eq!(
            members.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(
            members[0].targets.iter().map(|target| target.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "cli"]
        );
        if !cache_inputs {
            assert_eq!(super::super::resource_dependencies::of(&metadata).len(), 7);
        }
    }
}

#[test]
fn resource_metadata_failure_reports_the_workspace_original_cause_and_recovery() {
    let error = load_survey_metadata_using(Utf8Path::new("workspace"), &FeatureArgs::default(), false, |_metadata| {
        Err(cargo_metadata::Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "resource metadata fixture failure",
        )))
    })
    .unwrap_err();
    let message = error.to_string();
    for expected in [
        "could not resolve resource dependencies for `workspace`",
        "offline Cargo metadata",
        "cargo metadata",
        "cargo fetch",
        "resource metadata fixture failure",
    ] {
        assert!(message.contains(expected), "{message}");
    }
}
