// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rust target discovery and Cargo-style selector matching.

use std::env;
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::str::FromStr;

use cargo_platform::{Cfg, Platform};

use crate::CoverageGateError;
use crate::error::{ExecuteRustcError, InvalidRustcCfgError, MissingRustcHostTargetError, ResolveTargetError, RustcCommandFailedError};

/// The Rust target triple and cfg values used to resolve target policy.
#[derive(Debug, Clone)]
pub(crate) struct TargetContext {
    pub(crate) triple: String,
    cfg: Vec<Cfg>,
}

impl TargetContext {
    /// Resolve an explicit Rust target, or the rustc host target when omitted.
    pub(crate) fn resolve(target: Option<&str>) -> Result<Self, CoverageGateError> {
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        Self::resolve_with_rustc(target, &rustc).map_err(Into::into)
    }

    fn resolve_with_rustc(target: Option<&str>, rustc: &OsStr) -> Result<Self, ResolveTargetError> {
        let rustc_display = rustc.to_string_lossy();
        let triple = if let Some(target) = target {
            target.to_owned()
        } else {
            let command = format!("{rustc_display} -vV");
            let output = Command::new(rustc)
                .arg("-vV")
                .output()
                .map_err(|error| ExecuteRustcError::caused_by(command.clone(), error))?;
            if !output.status.success() {
                return Err(RustcCommandFailedError::new(
                    command,
                    output.status.to_string(),
                    String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                )
                .into());
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .map(str::to_owned)
                .ok_or_else(|| MissingRustcHostTargetError::new(command))?
        };

        let command = format!("{rustc_display} --print cfg --target {triple}");
        let output = Command::new(rustc)
            .args(["--print", "cfg", "--target", &triple])
            .output()
            .map_err(|error| ExecuteRustcError::caused_by(command.clone(), error))?;
        if !output.status.success() {
            return Err(RustcCommandFailedError::new(
                command,
                output.status.to_string(),
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            )
            .into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let cfg = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| Cfg::from_str(line).map_err(|error| InvalidRustcCfgError::caused_by(line.to_owned(), triple.clone(), error).into()))
            .collect::<Result<Vec<_>, ResolveTargetError>>()?;

        Ok(Self { triple, cfg })
    }

    pub(crate) fn matches(&self, platform: &Platform) -> bool {
        platform.matches(&self.triple, &self.cfg)
    }

    #[cfg(test)]
    pub(crate) fn from_parts(triple: &str, cfg: &[&str]) -> Self {
        Self {
            triple: triple.to_owned(),
            cfg: cfg
                .iter()
                .map(|value| Cfg::from_str(value).expect("test cfg must use rustc --print cfg syntax"))
                .collect(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::error::Error as _;
    use std::fs::write;
    #[cfg(unix)]
    use std::fs::{metadata, set_permissions};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use tempfile::{TempDir, tempdir};

    use super::*;

    fn fake_rustc(vv_stdout: &str, vv_exit: i32, cfg_stdout: &str, cfg_exit: i32) -> TempDir {
        let temp = tempdir().expect("tempdir");
        let path = fake_rustc_path(&temp);
        let vv_output = vv_stdout.lines().map(echo_line).collect::<Vec<_>>().join("\n");
        let cfg_output = cfg_stdout.lines().map(echo_line).collect::<Vec<_>>().join("\n");

        #[cfg(windows)]
        let script = format!("@echo off\nif \"%1\"==\"-vV\" (\n{vv_output}\nexit /b {vv_exit}\n)\n{cfg_output}\nexit /b {cfg_exit}\n");
        #[cfg(not(windows))]
        let script = format!("#!/bin/sh\nif [ \"$1\" = \"-vV\" ]; then\n{vv_output}\nexit {vv_exit}\nfi\n{cfg_output}\nexit {cfg_exit}\n");
        write(&path, script).expect("write fake rustc");

        #[cfg(unix)]
        {
            let mut permissions = metadata(&path).expect("fake rustc metadata").permissions();
            permissions.set_mode(0o755);
            set_permissions(&path, permissions).expect("make fake rustc executable");
        }

        temp
    }

    #[cfg(windows)]
    fn echo_line(line: &str) -> String {
        format!("echo {line}")
    }

    #[cfg(not(windows))]
    fn echo_line(line: &str) -> String {
        format!("printf '%s\\n' '{line}'")
    }

    fn fake_rustc_path(temp: &TempDir) -> PathBuf {
        #[cfg(windows)]
        {
            temp.path().join("rustc.cmd")
        }
        #[cfg(not(windows))]
        {
            temp.path().join("rustc")
        }
    }

    #[test]
    fn matches_exact_and_cfg_selectors() {
        let target = TargetContext::from_parts(
            "x86_64-pc-windows-msvc",
            &["windows", "target_arch=\"x86_64\"", "target_os=\"windows\""],
        );
        assert!(target.matches(&Platform::from_str("x86_64-pc-windows-msvc").expect("exact target")));
        assert!(target.matches(&Platform::from_str("cfg(windows)").expect("windows cfg")));
        assert!(target.matches(&Platform::from_str("cfg(target_os = \"windows\")").expect("target_os cfg")));
        assert!(!target.matches(&Platform::from_str("cfg(unix)").expect("unix cfg")));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and spawns a fake rustc process; miri isolation forbids both")]
    fn resolves_host_and_cfg_from_rustc() {
        let temp = fake_rustc(
            "rustc 1.97.0\nhost: x86_64-pc-windows-msvc",
            0,
            "windows\ntarget_arch=\"x86_64\"\ntarget_os=\"windows\"",
            0,
        );
        let target = TargetContext::resolve_with_rustc(None, fake_rustc_path(&temp).as_os_str()).expect("resolve fake host");

        assert_eq!(target.triple, "x86_64-pc-windows-msvc");
        assert!(target.matches(&Platform::from_str("cfg(windows)").expect("windows cfg")));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and spawns fake rustc processes; miri isolation forbids both")]
    fn rejects_failed_or_malformed_rustc_output() {
        let missing = fake_rustc_path(&tempdir().expect("tempdir"));
        let error = TargetContext::resolve_with_rustc(None, missing.as_os_str()).expect_err("missing rustc must fail");
        let rendered = error.to_string();
        assert!(rendered.contains("failed to resolve"));
        assert!(error.source().is_some(), "resolve error must preserve its typed cause");

        let failed_host = fake_rustc("", 7, "", 0);
        let error =
            TargetContext::resolve_with_rustc(None, fake_rustc_path(&failed_host).as_os_str()).expect_err("failed host query must fail");
        assert!(error.to_string().contains("failed to resolve"));

        let missing_host = fake_rustc("rustc 1.97.0", 0, "", 0);
        let error =
            TargetContext::resolve_with_rustc(None, fake_rustc_path(&missing_host).as_os_str()).expect_err("missing host must fail");
        assert!(error.to_string().contains("failed to resolve"));

        let failed_cfg = fake_rustc("", 0, "", 8);
        let error = TargetContext::resolve_with_rustc(Some("x86_64-unknown-linux-gnu"), fake_rustc_path(&failed_cfg).as_os_str())
            .expect_err("failed cfg query must fail");
        assert!(error.to_string().contains("failed to resolve"));

        let invalid_cfg = fake_rustc("", 0, "not a cfg", 0);
        let error = TargetContext::resolve_with_rustc(Some("x86_64-unknown-linux-gnu"), fake_rustc_path(&invalid_cfg).as_os_str())
            .expect_err("invalid cfg must fail");
        assert!(error.to_string().contains("failed to resolve"));
    }
}
