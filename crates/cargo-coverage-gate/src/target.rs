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

/// One completed `rustc` invocation, reduced to the parts target resolution reads.
///
/// Decoupling this from [`std::process::Output`] is what lets the tests supply an
/// invocation result directly: `Output` carries an `ExitStatus`, which no portable API
/// can construct, so faking one means really running something.
struct RustcRun {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

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

    /// Spawns the real `rustc`, delegating every decision to [`Self::resolve_with_runner`].
    ///
    /// Deliberately thin: this is the only part of target resolution that touches a
    /// process, so it is the only part that cannot be exercised without one.
    fn resolve_with_rustc(target: Option<&str>, rustc: &OsStr) -> Result<Self, ResolveTargetError> {
        let rustc_display = rustc.to_string_lossy().into_owned();
        Self::resolve_with_runner(target, &rustc_display, |args| {
            let output = Command::new(rustc).args(args).output()?;
            Ok(RustcRun {
                success: output.status.success(),
                status: output.status.to_string(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        })
    }

    /// Resolves a target from whatever `run` reports, without assuming a process.
    ///
    /// Taking the invocation as a closure keeps the parsing and error mapping below
    /// free of any dependency on spawning: the tests drive the awkward cases with
    /// canned [`RustcRun`] values instead of a generated shell script, which they used
    /// to write into a temp directory and execute. That script was also a liability —
    /// writing an executable and immediately running it races with `fork` on Linux and
    /// intermittently failed CI with `ETXTBSY`.
    fn resolve_with_runner(
        target: Option<&str>,
        rustc_display: &str,
        mut run: impl FnMut(&[&str]) -> Result<RustcRun, std::io::Error>,
    ) -> Result<Self, ResolveTargetError> {
        // An explicit triple skips host discovery, but every triple still
        // needs its cfg values because they cannot be derived from its name.
        let triple = if let Some(target) = target {
            target.to_owned()
        } else {
            let command = format!("{rustc_display} -vV");
            let output = run(&["-vV"]).map_err(|error| ExecuteRustcError::caused_by(command.clone(), error))?;
            if !output.success {
                return Err(RustcCommandFailedError::new(command, output.status, output.stderr).into());
            }
            output
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .map(str::to_owned)
                .ok_or_else(|| MissingRustcHostTargetError::new(command))?
        };

        let command = format!("{rustc_display} --print cfg --target {triple}");
        let output = run(&["--print", "cfg", "--target", &triple]).map_err(|error| ExecuteRustcError::caused_by(command.clone(), error))?;
        if !output.success {
            return Err(RustcCommandFailedError::new(command, output.status, output.stderr).into());
        }

        let cfg = output
            .stdout
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
    use std::io::{Error as IoError, ErrorKind};

    use super::*;

    /// A successful invocation printing `stdout`.
    fn ok(stdout: &str) -> RustcRun {
        RustcRun {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    /// An invocation that ran but exited non-zero.
    fn failed(status: &str, stderr: &str) -> RustcRun {
        RustcRun {
            success: false,
            status: status.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    /// Answers each invocation with the next canned result, in order.
    fn replaying(results: Vec<Result<RustcRun, IoError>>) -> impl FnMut(&[&str]) -> Result<RustcRun, IoError> {
        let mut remaining = results.into_iter();
        move |_| {
            remaining
                .next()
                .expect("runner called more times than the test supplied results for")
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

    /// Covers the real spawn path end to end against the toolchain running the tests,
    /// which is the one `rustc` guaranteed to exist here. Asserting the host's own cfg
    /// rather than a hard-coded triple keeps it honest on every platform.
    #[test]
    #[cfg_attr(miri, ignore = "spawns rustc; miri isolation forbids process execution")]
    fn resolves_host_and_cfg_from_real_rustc() {
        let target = TargetContext::resolve(None).expect("resolving the host target must succeed");

        assert!(!target.triple.is_empty(), "rustc must report a host triple");
        assert!(target.matches(&Platform::from_str(&target.triple).expect("reported triple must parse")));
        #[cfg(windows)]
        assert!(target.matches(&Platform::from_str("cfg(windows)").expect("windows cfg")));
        #[cfg(unix)]
        assert!(target.matches(&Platform::from_str("cfg(unix)").expect("unix cfg")));
    }

    /// An explicit triple skips host discovery but still queries cfg values, and rustc
    /// answers for any target it knows regardless of the host or what is installed.
    #[test]
    #[cfg_attr(miri, ignore = "spawns rustc; miri isolation forbids process execution")]
    fn resolves_explicit_target_from_real_rustc() {
        let target = TargetContext::resolve(Some("x86_64-unknown-linux-gnu")).expect("resolving an explicit target must succeed");

        assert_eq!(target.triple, "x86_64-unknown-linux-gnu");
        assert!(target.matches(&Platform::from_str("cfg(unix)").expect("unix cfg")));
        assert!(!target.matches(&Platform::from_str("cfg(windows)").expect("windows cfg")));
    }

    /// The spawn shim's own failure path: a program that is not there at all.
    #[test]
    #[cfg_attr(miri, ignore = "spawns a process; miri isolation forbids that")]
    fn reports_a_rustc_that_cannot_be_launched() {
        let error =
            TargetContext::resolve_with_rustc(None, "cargo-coverage-gate-no-such-rustc".as_ref()).expect_err("a missing rustc must fail");

        assert!(error.to_string().contains("failed to resolve"));
        assert!(error.source().is_some(), "resolve error must preserve its typed cause");
    }

    #[test]
    fn reports_a_failed_host_query() {
        let error = TargetContext::resolve_with_runner(None, "rustc", replaying(vec![Ok(failed("exit status: 7", "boom"))]))
            .expect_err("a failed host query must fail");

        let rendered = format!("{error:?}");
        assert!(rendered.contains("RustcCommandFailedError"), "unexpected cause: {rendered}");
        assert!(rendered.contains("boom"), "the stderr must reach the error: {rendered}");
    }

    #[test]
    fn reports_version_output_without_a_host_line() {
        let error = TargetContext::resolve_with_runner(None, "rustc", replaying(vec![Ok(ok("rustc 1.97.0"))]))
            .expect_err("version output with no host line must fail");

        let rendered = format!("{error:?}");
        assert!(rendered.contains("MissingRustcHostTargetError"), "unexpected cause: {rendered}");
    }

    #[test]
    fn reports_a_host_query_that_cannot_be_launched() {
        let error = TargetContext::resolve_with_runner(None, "rustc", replaying(vec![Err(IoError::other("launch failed"))]))
            .expect_err("a host query that cannot spawn must fail");

        let rendered = format!("{error:?}");
        assert!(rendered.contains("ExecuteRustcError"), "unexpected cause: {rendered}");
    }

    #[test]
    fn reports_a_failed_cfg_query() {
        let error = TargetContext::resolve_with_runner(
            Some("x86_64-unknown-linux-gnu"),
            "rustc",
            replaying(vec![Ok(failed("exit status: 8", "no such target"))]),
        )
        .expect_err("a failed cfg query must fail");

        let rendered = format!("{error:?}");
        assert!(rendered.contains("RustcCommandFailedError"), "unexpected cause: {rendered}");
        assert!(rendered.contains("no such target"), "the stderr must reach the error: {rendered}");
    }

    #[test]
    fn reports_a_cfg_query_that_cannot_be_launched() {
        // The host query succeeds first, so this exercises the second invocation's
        // spawn failure rather than the first.
        let error = TargetContext::resolve_with_runner(
            None,
            "rustc",
            replaying(vec![
                Ok(ok("host: x86_64-unknown-linux-gnu")),
                Err(IoError::from(ErrorKind::NotFound)),
            ]),
        )
        .expect_err("a cfg query that cannot spawn must fail");

        let rendered = format!("{error:?}");
        assert!(rendered.contains("ExecuteRustcError"), "unexpected cause: {rendered}");
    }

    #[test]
    fn reports_unparsable_cfg_output() {
        let error = TargetContext::resolve_with_runner(
            Some("x86_64-unknown-linux-gnu"),
            "rustc",
            replaying(vec![Ok(ok("unix\nnot a cfg"))]),
        )
        .expect_err("unparsable cfg output must fail");

        let rendered = format!("{error:?}");
        assert!(rendered.contains("InvalidRustcCfgError"), "unexpected cause: {rendered}");
        assert!(
            rendered.contains("not a cfg"),
            "the offending value must reach the error: {rendered}"
        );
    }

    #[test]
    fn ignores_blank_cfg_lines() {
        let target = TargetContext::resolve_with_runner(
            Some("x86_64-unknown-linux-gnu"),
            "rustc",
            replaying(vec![Ok(ok("unix\n\ntarget_os=\"linux\"\n"))]),
        )
        .expect("blank lines must be skipped rather than parsed");

        assert!(target.matches(&Platform::from_str("cfg(unix)").expect("unix cfg")));
        assert!(target.matches(&Platform::from_str("cfg(target_os = \"linux\")").expect("target_os cfg")));
    }
}
