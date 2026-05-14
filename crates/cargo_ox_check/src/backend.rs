// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CI backend identification and autodetection.
//!
//! `cargo-ox-check` emits files for one or more CI backends (`github`, `ado`).
//! The set of backends is chosen by, in order:
//!
//! 1. Explicit `--backend <name>` flag(s).
//! 2. Explicit `--no-backends` switch (yields the empty set).
//! 3. Autodetection from the `origin` git remote URL.
//!
//! See [design.md §5.2](../../docs/design/design.md) for the resolution order.

use std::path::Path;
use std::process::Command;

use ohno::{AppError, IntoAppError as _, app_err};

/// Supported CI backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Backend {
    /// GitHub Actions.
    GitHub,
    /// Azure DevOps Pipelines.
    Ado,
}

impl Backend {
    /// Canonical lowercase name as used on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Ado => "ado",
        }
    }

    /// Parse a backend name as accepted by `--backend`.
    ///
    /// # Errors
    ///
    /// Returns an error for any name other than `github` or `ado`.
    pub fn parse(name: &str) -> Result<Self, AppError> {
        match name {
            "github" => Ok(Self::GitHub),
            "ado" => Ok(Self::Ado),
            other => Err(app_err!(
                "unknown backend '{other}' (valid values: github, ado)"
            )),
        }
    }
}

/// Autodetect backends from a git remote URL.
///
/// Returns an empty `Vec` if the host is unrecognized. The caller decides
/// whether an empty result is an error.
#[must_use]
pub fn detect_from_url(url: &str) -> Vec<Backend> {
    if let Some(host) = extract_host(url) {
        if host == "github.com" || host.ends_with(".github.com") {
            return vec![Backend::GitHub];
        }
        if host == "dev.azure.com"
            || host == "ssh.dev.azure.com"
            || host.ends_with(".visualstudio.com")
        {
            return vec![Backend::Ado];
        }
    }
    Vec::new()
}

/// Extract the host portion of a git URL.
///
/// Handles the three common forms:
/// - `https://host/owner/repo[.git]`
/// - `ssh://user@host/path`
/// - `user@host:owner/repo[.git]` (the scp-style shorthand)
fn extract_host(url: &str) -> Option<&str> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // scheme://[user@]host[:port]/path
    if let Some(scheme_end) = url.find("://")
        && scheme_end > 0
    {
        let after_scheme = url.get(scheme_end + 3..).unwrap_or_default();
        let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        let host_start = authority.rfind('@').map_or(0, |i| i + 1);
        let host_part = &authority[host_start..];
        let host_end = host_part.find(':').unwrap_or(host_part.len());
        let host = &host_part[..host_end];
        return (!host.is_empty()).then_some(host);
    }

    // scp-style: user@host:path
    if let Some(at_idx) = url.find('@')
        && let Some(colon_idx) = url[at_idx + 1..].find(':')
    {
        let host = &url[at_idx + 1..at_idx + 1 + colon_idx];
        return (!host.is_empty()).then_some(host);
    }

    None
}

/// Read the `origin` remote URL via `git config`.
///
/// # Errors
///
/// Returns an error if `git` is not on PATH, the command exits non-zero, or
/// no `origin` remote is configured.
pub fn read_origin_url(repo_root: &Path) -> Result<String, AppError> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_root)
        .output()
        .into_app_err("failed to invoke `git config` — is git installed and on PATH?")?;

    if !output.status.success() {
        return Err(app_err!(
            "`git config --get remote.origin.url` exited with {} in {}",
            output.status,
            repo_root.display()
        ));
    }

    let url = String::from_utf8(output.stdout)
        .into_app_err("git config output was not valid UTF-8")?
        .trim()
        .to_owned();

    if url.is_empty() {
        return Err(app_err!(
            "no `origin` remote configured in {}",
            repo_root.display()
        ));
    }

    Ok(url)
}

/// Resolve the effective backend set from CLI flags plus autodetection.
///
/// Resolution order:
/// 1. `--no-backends` → empty set.
/// 2. Explicit `--backend <name>` flags.
/// 3. Autodetect from `origin`.
///
/// # Errors
///
/// - Returns an error if a `--backend` name is invalid.
/// - Returns an error if no backends are specified, `--no-backends` is not
///   set, and autodetection fails (unrecognized host or no remote).
pub fn resolve(
    flag_backends: &[String],
    no_backends: bool,
    repo_root: &Path,
) -> Result<Vec<Backend>, AppError> {
    if no_backends {
        return Ok(Vec::new());
    }

    if !flag_backends.is_empty() {
        let mut parsed = Vec::with_capacity(flag_backends.len());
        for name in flag_backends {
            parsed.push(Backend::parse(name)?);
        }
        parsed.sort_unstable();
        parsed.dedup();
        return Ok(parsed);
    }

    let url = read_origin_url(repo_root)?;
    let detected = detect_from_url(&url);
    if detected.is_empty() {
        return Err(app_err!(
            "could not autodetect a CI backend from origin URL '{url}'. \
             Pass --backend github|ado explicitly, or --no-backends."
        ));
    }
    Ok(detected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_names() {
        assert_eq!(Backend::parse("github").unwrap(), Backend::GitHub);
        assert_eq!(Backend::parse("ado").unwrap(), Backend::Ado);
        Backend::parse("gitlab").unwrap_err();
        Backend::parse("").unwrap_err();
    }

    #[test]
    fn backend_names_roundtrip() {
        assert_eq!(Backend::GitHub.name(), "github");
        assert_eq!(Backend::Ado.name(), "ado");
    }

    #[test]
    fn extract_host_https() {
        assert_eq!(extract_host("https://github.com/foo/bar.git"), Some("github.com"));
        assert_eq!(
            extract_host("https://dev.azure.com/org/proj/_git/repo"),
            Some("dev.azure.com")
        );
        assert_eq!(
            extract_host("https://acme.visualstudio.com/proj/_git/repo"),
            Some("acme.visualstudio.com")
        );
    }

    #[test]
    fn extract_host_ssh_url() {
        assert_eq!(
            extract_host("ssh://git@github.com:22/foo/bar.git"),
            Some("github.com")
        );
        assert_eq!(
            extract_host("ssh://git@ssh.dev.azure.com/v3/org/proj/repo"),
            Some("ssh.dev.azure.com")
        );
    }

    #[test]
    fn extract_host_scp_style() {
        assert_eq!(extract_host("git@github.com:foo/bar.git"), Some("github.com"));
        assert_eq!(
            extract_host("git@ssh.dev.azure.com:v3/org/proj/repo"),
            Some("ssh.dev.azure.com")
        );
    }

    #[test]
    fn extract_host_handles_garbage() {
        assert_eq!(extract_host(""), None);
        assert_eq!(extract_host("   "), None);
        assert_eq!(extract_host("not-a-url"), None);
        assert_eq!(extract_host("://nohost"), None);
    }

    #[test]
    fn detect_github() {
        assert_eq!(
            detect_from_url("https://github.com/foo/bar.git"),
            vec![Backend::GitHub]
        );
        assert_eq!(
            detect_from_url("git@github.com:foo/bar.git"),
            vec![Backend::GitHub]
        );
    }

    #[test]
    fn detect_ado() {
        assert_eq!(
            detect_from_url("https://dev.azure.com/org/proj/_git/repo"),
            vec![Backend::Ado]
        );
        assert_eq!(
            detect_from_url("https://acme.visualstudio.com/proj/_git/repo"),
            vec![Backend::Ado]
        );
        assert_eq!(
            detect_from_url("ssh://git@ssh.dev.azure.com/v3/org/proj/repo"),
            vec![Backend::Ado]
        );
    }

    #[test]
    fn detect_unknown_host() {
        assert!(detect_from_url("https://gitlab.com/foo/bar.git").is_empty());
        assert!(detect_from_url("").is_empty());
    }

    #[test]
    fn resolve_no_backends_wins() {
        let result = resolve(&[], true, Path::new(".")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_explicit_backends_skip_autodetect() {
        let result = resolve(
            &["github".to_owned(), "ado".to_owned()],
            false,
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(result, vec![Backend::GitHub, Backend::Ado]);
    }

    #[test]
    fn resolve_explicit_backends_deduplicate() {
        let result = resolve(
            &["github".to_owned(), "github".to_owned()],
            false,
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(result, vec![Backend::GitHub]);
    }

    #[test]
    fn resolve_invalid_backend_name() {
        let result = resolve(&["gitlab".to_owned()], false, Path::new("/nonexistent"));
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown backend 'gitlab'"));
    }
}
