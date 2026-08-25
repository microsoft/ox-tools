// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Active compilation-target discovery and Cargo-style selector matching.

use std::env;
use std::ffi::OsString;
use std::process::Command;
use std::str::FromStr;

use cargo_platform::{Cfg, Platform};

use crate::error::{CoverageGateError, ResolveTargetError};

/// The target triple and cfg values used to resolve target policy.
#[derive(Debug, Clone)]
pub(crate) struct TargetContext {
    pub(crate) triple: String,
    cfg: Vec<Cfg>,
}

impl TargetContext {
    /// Resolve an explicit target, or the active rustc host when omitted.
    pub(crate) fn resolve(target: Option<&str>) -> Result<Self, CoverageGateError> {
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let triple = if let Some(target) = target {
            target.to_owned()
        } else {
            let output = Command::new(&rustc)
                .arg("-vV")
                .output()
                .map_err(|error| ResolveTargetError::new(format!("could not execute `{}`: {error}", rustc.to_string_lossy())))?;
            if !output.status.success() {
                return Err(ResolveTargetError::new(format!(
                    "`{} -vV` exited with {}: {}",
                    rustc.to_string_lossy(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
                .into());
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .map(str::to_owned)
                .ok_or_else(|| ResolveTargetError::new(format!("`{} -vV` did not report a host triple", rustc.to_string_lossy())))?
        };

        let output = Command::new(&rustc)
            .args(["--print", "cfg", "--target", &triple])
            .output()
            .map_err(|error| ResolveTargetError::new(format!("could not execute `{}`: {error}", rustc.to_string_lossy())))?;
        if !output.status.success() {
            return Err(ResolveTargetError::new(format!(
                "`{} --print cfg --target {triple}` exited with {}: {}",
                rustc.to_string_lossy(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let cfg = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                Cfg::from_str(line)
                    .map_err(|error| ResolveTargetError::new(format!("rustc reported invalid cfg `{line}` for `{triple}`: {error}")).into())
            })
            .collect::<Result<Vec<_>, CoverageGateError>>()?;

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
    use super::*;

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
}
