// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

fn probe_output(status: ExitStatus, stdout: &[u8]) -> Output {
    Output {
        status,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}

fn discovery_error(probes: impl IntoIterator<Item = (&'static str, Output)>) -> Box<dyn std::error::Error> {
    let mut probes = probes.into_iter();
    let error = CargoInvoker::discover_with(PathBuf::from("."), BTreeMap::new(), None, |command| {
        let (program, output) = probes.next().expect("discovery ran a probe beyond the scripted failure");
        assert_eq!(command.get_program(), program);
        Ok(output)
    })
    .expect_err("the scripted probe results cannot yield a workspace and toolchain");
    assert!(probes.next().is_none(), "discovery stopped before the failing probe");
    error
}

#[test]
fn rejects_a_workspace_manifest_without_a_parent() {
    let error = discovery_error([("cargo", probe_output(ExitStatus::default(), b""))]);
    assert_eq!(error.to_string(), "workspace manifest has no parent directory");
}

#[cfg(any(unix, windows))]
#[test]
fn reports_a_failed_rustc_sysroot_probe() {
    #[cfg(unix)]
    let status = <ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(7 << 8);
    #[cfg(windows)]
    let status = <ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw(7);
    let error = discovery_error([
        ("cargo", probe_output(ExitStatus::default(), b"workspace/Cargo.toml")),
        ("rustc", probe_output(status, b"")),
    ]);
    assert_eq!(error.to_string(), format!("selecting Rust toolchain failed ({status})"));
}

#[test]
fn rejects_non_utf8_probe_output() {
    let error = discovery_error([("cargo", probe_output(ExitStatus::default(), &[0xff]))]);
    assert!(error.is::<std::string::FromUtf8Error>(), "{error}");
}

#[test]
fn discovery_leaves_cargo_home_to_cargo_when_unset_or_empty() -> Result<()> {
    for home in [None, Some(OsString::new())] {
        let temp = tempfile::tempdir()?;
        let invocation = temp.path().join("invocation");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&invocation)?;
        fs::create_dir(&workspace)?;
        let manifest = workspace.join("Cargo.toml");
        let mut child_env = BTreeMap::new();
        if let Some(home) = home {
            child_env.insert("CARGO_HOME".into(), home);
        }
        let cargo = CargoInvoker::discover_with(invocation.clone(), child_env, Some(temp.path()), |command| {
            match command.get_program().to_str() {
                Some("cargo") => Ok(probe_output(ExitStatus::default(), manifest.to_string_lossy().as_bytes())),
                Some("rustc") => Ok(probe_output(ExitStatus::default(), b"workspace-toolchain\n")),
                _ => panic!("unexpected discovery probe: {command:?}"),
            }
        })?;
        assert!(cargo.command().get_envs().all(|(name, _)| name != OsStr::new("CARGO_HOME")));
        assert_eq!(cargo.root, workspace.canonicalize()?);
        assert_eq!(cargo.command().get_current_dir(), Some(cargo.root.as_path()));
        assert_eq!(cargo.scratch.path().parent().unwrap().canonicalize()?, temp.path().canonicalize()?);
    }
    Ok(())
}

#[test]
fn resolves_relative_cargo_home_from_the_invocation_directory() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let invocation = temp.path().join("invocation");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&invocation)?;
    fs::create_dir(&workspace)?;
    let manifest = workspace.join("Cargo.toml");
    let child_env = BTreeMap::from([("CARGO_HOME".into(), "cache".into())]);
    let cargo = CargoInvoker::discover_with(invocation.clone(), child_env, Some(temp.path()), |command| {
        match command.get_program().to_str() {
            Some("cargo") => Ok(probe_output(ExitStatus::default(), manifest.to_string_lossy().as_bytes())),
            Some("rustc") => Ok(probe_output(ExitStatus::default(), b"workspace-toolchain\n")),
            _ => panic!("unexpected discovery probe: {command:?}"),
        }
    })?;
    let command = cargo.command();
    let env = command.get_envs().collect::<BTreeMap<_, _>>();
    let expected = std::path::absolute(invocation.join("cache"))?;
    assert_eq!(env.get(OsStr::new("CARGO_HOME")).copied().flatten(), Some(expected.as_os_str()));
    Ok(())
}

#[test]
fn reads_candidate_versions_and_ignores_discovery_metadata() -> Result<()> {
    let report = r#"[{"name":"peer","new_version":"1.2.0","old_version":"1.1.0","classification":"compatible"}]"#;
    assert_eq!(
        Candidates::from_report(report)?.packages,
        BTreeMap::from([("peer".into(), "1.2.0".into())])
    );
    assert!(Candidates::from_report("[]")?.is_empty());
    Ok(())
}

#[test]
fn deduplicates_candidates_with_the_same_name_and_version() -> Result<()> {
    let report = r#"[
        {"name":"peer","new_version":"1.2.0"},
        {"name":"other","new_version":"1.2.0"},
        {"name":"peer","new_version":"1.2.0"}
    ]"#;
    assert_eq!(
        Candidates::from_report(report)?.packages,
        BTreeMap::from([("other".into(), "1.2.0".into()), ("peer".into(), "1.2.0".into())])
    );
    Ok(())
}

#[test]
fn rejects_invalid_candidate_reports() {
    for report in ["", "{}"] {
        assert!(Candidates::from_report(report).is_err(), "{report}");
    }
    for (report, diagnostic) in [
        (r#"[{"new_version":"1.2.0"}]"#, "candidate is missing its name"),
        (r#"[{"name":"peer","old_version":"1.1.0"}]"#, "candidate is missing its version"),
        (
            r#"[{"name":"peer","new_version":"1.1.0"},{"name":"peer","new_version":"1.2.0"}]"#,
            "report lists two versions for `peer`: `1.1.0` and `1.2.0`; keep one",
        ),
    ] {
        assert_eq!(Candidates::from_report(report).unwrap_err().to_string(), diagnostic);
    }
}

#[test]
#[cfg_attr(miri, ignore = "uses the host filesystem")]
fn validate_packages_reports_the_missing_candidate_archive() -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let archives = tempfile::tempdir_in(scratch.path())?;
    let candidates = BTreeMap::from([("peer".into(), "1.2.0".into())]);
    let cargo = CargoInvoker {
        root: PathBuf::from("workspace"),
        child_env: BTreeMap::new(),
        scratch,
    };
    let error =
        validate_packages(&cargo, archives.path(), candidates).expect_err("peer was reported as packaged, but its archive is missing");
    let missing = archives.path().join("peer-1.2.0.crate");
    assert!(
        error.to_string().starts_with(&format!("extracting {}:", missing.display())),
        "{error}"
    );
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "uses the host filesystem")]
fn reports_a_missing_workspace_when_preparing_scratch() -> Result<()> {
    let parent = tempfile::tempdir()?;
    let error = CargoInvoker::new(&parent.path().join("missing-workspace"), BTreeMap::new(), Some(parent.path())).unwrap_err();
    assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::NotFound);
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "uses the host filesystem")]
fn rejects_a_scratch_parent_inside_the_workspace() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let scratch_parent = tempfile::tempdir_in(workspace.path())?;
    let error = CargoInvoker::new(workspace.path(), BTreeMap::new(), Some(scratch_parent.path()))
        .expect_err("scratch must be outside the workspace");
    assert_eq!(error.to_string(), "select a temporary directory outside the source workspace");
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "uses the host filesystem")]
fn rejects_a_missing_scratch_parent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace = tempfile::tempdir_in(temp.path())?;
    let scratch_parent = temp.path().join("nonexistent");
    let error =
        CargoInvoker::new(workspace.path(), BTreeMap::new(), Some(&scratch_parent)).expect_err("the scratch parent must already exist");
    assert!(error.to_string().starts_with("preparing release scratch directory in"), "{error}");
    Ok(())
}

#[test]
#[cfg_attr(miri, ignore = "uses the host filesystem")]
fn reports_when_the_candidate_patch_cannot_be_written() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let scratch = tempfile::tempdir_in(temp.path())?;
    fs::remove_dir(scratch.path())?;
    let cargo = CargoInvoker {
        root: temp.path().to_path_buf(),
        child_env: BTreeMap::new(),
        scratch,
    };
    let error = test_archives(&cargo, &BTreeMap::new()).unwrap_err();
    assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::NotFound);
    Ok(())
}

#[cfg(any(unix, windows))]
#[test]
fn reports_paths_cargo_cannot_represent_in_patch_toml() {
    #[cfg(unix)]
    let directory = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let directory = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xd800])
    };
    let sources = BTreeMap::from([("candidate".into(), directory.into())]);
    let temp = tempfile::tempdir();
    let cargo = CargoInvoker {
        root: PathBuf::from("workspace"),
        child_env: BTreeMap::new(),
        scratch: temp.unwrap(),
    };
    assert_eq!(
        test_archives(&cargo, &sources).unwrap_err().to_string(),
        "candidate archive path must be UTF-8"
    );
}

#[test]
fn reports_when_the_required_cargo_process_cannot_start() {
    let mut command = Command::new("cargo");
    for error in [
        capture(
            &mut command,
            "locating the Cargo workspace",
            |_| Err(io::ErrorKind::NotFound.into()),
        )
        .unwrap_err(),
        run(&mut command, "testing a candidate", |_| Err(io::ErrorKind::NotFound.into())).unwrap_err(),
    ] {
        assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::NotFound);
    }
}

#[test]
#[cfg_attr(all(miri, windows), ignore = "Windows Command environment uses CompareStringOrdinal")]
fn commands_use_workspace_config_and_keep_builds_in_scratch() -> Result<()> {
    let root = tempfile::tempdir()?;
    let scratch = tempfile::tempdir()?;
    let patch = scratch.path().join("patch.toml");
    let manifest = scratch.path().join("Cargo.toml");
    let cargo = CargoInvoker {
        root: root.path().to_path_buf(),
        child_env: BTreeMap::from([
            ("RUSTUP_TOOLCHAIN".into(), "workspace-toolchain".into()),
            ("CARGO_BUILD_BUILD_DIR".into(), "caller-build".into()),
        ]),
        scratch,
    };
    let command = archive_test_command(&cargo, &patch, &manifest, Some("--no-default-features"));
    assert_eq!(command.get_current_dir(), Some(root.path()));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            "--config".as_ref(),
            patch.as_os_str(),
            "test".as_ref(),
            "--no-default-features".as_ref(),
            "--manifest-path".as_ref(),
            manifest.as_os_str(),
        ]
    );
    let env: BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env.get(OsStr::new("RUSTUP_TOOLCHAIN")),
        Some(&Some(OsStr::new("workspace-toolchain")))
    );
    for key in ["CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
        let directory = env.get(OsStr::new(key)).copied().flatten().map(Path::new).unwrap();
        assert!(directory.starts_with(cargo.scratch.path()) && directory != cargo.scratch.path());
    }
    assert!(!env.contains_key(OsStr::new("CARGO_HOME")));
    Ok(())
}

#[test]
fn package_command_selects_only_the_candidates() -> Result<()> {
    let root = tempfile::tempdir()?;
    let scratch = tempfile::tempdir()?;
    let cargo = CargoInvoker {
        root: root.path().to_path_buf(),
        child_env: BTreeMap::new(),
        scratch,
    };
    let candidates = BTreeMap::from([("consumer".to_owned(), "1.2.0".to_owned()), ("peer".to_owned(), "3.4.0".to_owned())]);
    let command = cargo.package_command(&candidates);
    let artifacts = cargo.scratch.path().join("artifacts");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            "package".as_ref(),
            "--allow-dirty".as_ref(),
            "--target-dir".as_ref(),
            artifacts.as_os_str(),
            "--package".as_ref(),
            "consumer@1.2.0".as_ref(),
            "--package".as_ref(),
            "peer@3.4.0".as_ref(),
        ]
    );
    Ok(())
}

#[test]
fn archive_commands_cover_all_three_feature_modes() -> Result<()> {
    let cargo = CargoInvoker {
        root: PathBuf::from("workspace"),
        child_env: BTreeMap::new(),
        scratch: tempfile::tempdir()?,
    };
    let patch = cargo.scratch.path().join("candidates.toml");
    let manifest = cargo.scratch.path().join("candidate/Cargo.toml");
    let args = ARCHIVE_FEATURE_MODES.map(|feature| {
        archive_test_command(&cargo, &patch, &manifest, feature)
            .get_args()
            .map(OsStr::to_os_string)
            .collect::<Vec<_>>()
    });
    assert!(args[0].iter().any(|arg| arg == "--all-features"));
    assert!(args[1].iter().any(|arg| arg == "--no-default-features"));
    assert!(args[2].iter().all(|arg| arg != "--all-features" && arg != "--no-default-features"));
    Ok(())
}
