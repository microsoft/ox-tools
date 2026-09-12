// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Placeholder substitution for the command template.
//!
//! A fixed, small set of `{token}` replacements — deliberately not an
//! expression language:
//!
//! - Package tokens (valid in per-package and per-target modes): `{name}`,
//!   `{spec}`, `{version}`, `{manifest}`. Replaced textually inside each
//!   argument.
//! - The per-target token `{target}`.
//! - The once token (valid only in `--once` mode): `{packages}`. Must stand
//!   alone as a whole argument; it expands to the resolved selection flags,
//!   which is several tokens.
//! - The workspace token `{workspace-rust-version}`, valid in every mode.
//!
//! Using a token in the wrong mode is a usage error ([`PlaceholderMisuseError`]).
//!
//! Only the tokens above are interpreted. Any other `{…}` sequence — a typo
//! like `{manfiest}`, a wrong-case `{Name}`, or a literal brace an argument
//! genuinely needs — is passed through **verbatim** to the spawned command.
//! There is no brace-escape mechanism, so this passthrough is a deliberate part
//! of the contract (`cargo-each` never interprets the command beyond these
//! fixed substitutions), not an oversight.

use crate::error::{EachError, PlaceholderMisuseError, WorkspaceRustVersionError};
use crate::plan::Mode;

/// Per-package placeholder tokens.
const PER_PACKAGE_TOKENS: [&str; 4] = ["{name}", "{spec}", "{version}", "{manifest}"];
/// The per-target placeholder token.
const TARGET_TOKEN: &str = "{target}";
/// The once-mode placeholder token.
const PACKAGES_TOKEN: &str = "{packages}";
/// The workspace-wide Rust compatibility floor token.
const WORKSPACE_RUST_VERSION_TOKEN: &str = "{workspace-rust-version}";

/// The substitution context for one command invocation.
#[derive(Debug, Clone)]
pub(crate) enum Placeholders {
    /// Per-package mode: substitute the member's facts into each argument.
    Package {
        /// `{name}` — bare package name.
        name: String,
        /// `{spec}` — `name@version`.
        spec: String,
        /// `{version}` — package version.
        version: String,
        /// `{manifest}` — absolute path to the member's `Cargo.toml`.
        manifest: String,
        /// The root workspace Rust-version declaration, when requested.
        workspace_rust_version: Option<String>,
    },
    /// Per-target mode: package facts plus the selected target name.
    Target {
        name: String,
        spec: String,
        version: String,
        manifest: String,
        target: String,
        workspace_rust_version: Option<String>,
    },
    /// Once mode: `{packages}` expands to these pre-computed selection flags.
    Once {
        /// The cargo selection flags for the resolved set (e.g.
        /// `["--workspace"]` or `["--package", "a@1", "--package", "b@2"]`).
        packages: Vec<String>,
        /// The root workspace Rust-version declaration, when requested.
        workspace_rust_version: Option<String>,
    },
}

impl Placeholders {
    fn workspace_rust_version(&self) -> Option<&str> {
        match self {
            Self::Package {
                workspace_rust_version, ..
            }
            | Self::Target {
                workspace_rust_version, ..
            }
            | Self::Once {
                workspace_rust_version, ..
            } => workspace_rust_version.as_deref(),
        }
    }
}

fn replace_workspace_rust_version(arg: String, placeholders: &Placeholders) -> Result<String, EachError> {
    if !arg.contains(WORKSPACE_RUST_VERSION_TOKEN) {
        return Ok(arg);
    }
    let version = placeholders
        .workspace_rust_version()
        .ok_or_else(|| WorkspaceRustVersionError::new("the command uses the placeholder but its root value was not resolved".to_owned()))?;
    Ok(arg.replace(WORKSPACE_RUST_VERSION_TOKEN, version))
}

/// Whether a command template uses the lazy workspace Rust-version token.
#[must_use]
pub(crate) fn uses_workspace_rust_version(args: &[String]) -> bool {
    args.iter().any(|arg| arg.contains(WORKSPACE_RUST_VERSION_TOKEN))
}

/// Validate that `args` only reference placeholders valid for the mode.
///
/// Checks mode-consistency without expanding the tokens — the check factored
/// out of [`substitute`] so the contract can be enforced even when the
/// selection resolves to no members (where `substitute` is never called) — a
/// misused placeholder is then a usage error rather than a silent no-op.
///
/// # Errors
///
/// Returns [`EachError`] if a per-package token appears under [`Mode::Once`],
/// if `{packages}` appears outside [`Mode::Once`], or if `{packages}` is
/// embedded in a larger argument rather than standing alone.
pub(crate) fn validate_placeholders(args: &[String], mode: Mode) -> Result<(), EachError> {
    for arg in args {
        if mode == Mode::Once {
            if let Some(token) = PER_PACKAGE_TOKENS.iter().find(|t| arg.contains(**t)) {
                return Err(
                    PlaceholderMisuseError::new((*token).to_owned(), "per-package token is not valid in --once mode".to_owned()).into(),
                );
            }
            if arg.contains(TARGET_TOKEN) {
                return Err(PlaceholderMisuseError::new(
                    TARGET_TOKEN.to_owned(),
                    "per-target token is not valid in --once mode".to_owned(),
                )
                .into());
            }
            if arg != PACKAGES_TOKEN && arg.contains(PACKAGES_TOKEN) {
                return Err(PlaceholderMisuseError::new(
                    PACKAGES_TOKEN.to_owned(),
                    "must stand alone as a whole argument (it expands to multiple tokens)".to_owned(),
                )
                .into());
            }
        } else {
            if arg.contains(PACKAGES_TOKEN) {
                return Err(PlaceholderMisuseError::new(PACKAGES_TOKEN.to_owned(), "only valid in --once mode".to_owned()).into());
            }
            if mode == Mode::PerPackage && arg.contains(TARGET_TOKEN) {
                return Err(PlaceholderMisuseError::new(TARGET_TOKEN.to_owned(), "only valid in per-target mode".to_owned()).into());
            }
        }
    }
    Ok(())
}

/// Substitute placeholders in `args` for one invocation.
///
/// Returns the fully-expanded argument vector.
///
/// # Errors
///
/// Returns [`EachError`] if a token is used in the wrong mode (a per-package
/// token under `--once`, or `{packages}` outside `--once`), or if `{packages}`
/// is embedded in a larger argument rather than standing alone.
pub(crate) fn substitute(args: &[String], placeholders: &Placeholders) -> Result<Vec<String>, EachError> {
    let mode = match placeholders {
        Placeholders::Package { .. } => Mode::PerPackage,
        Placeholders::Target { .. } => Mode::PerTarget,
        Placeholders::Once { .. } => Mode::Once,
    };
    validate_placeholders(args, mode)?;
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match placeholders {
            Placeholders::Package {
                name,
                spec,
                version,
                manifest,
                ..
            } => {
                let original = replace_workspace_rust_version(arg.clone(), placeholders)?;
                // The `{name}` / `{spec}` / … literals are cargo-each
                // placeholder tokens, not Rust format-string arguments.
                #[expect(
                    clippy::literal_string_with_formatting_args,
                    reason = "cargo-each placeholder tokens, not format args"
                )]
                let replaced = original
                    .replace("{name}", name)
                    .replace("{spec}", spec)
                    .replace("{version}", version)
                    .replace("{manifest}", manifest);
                out.push(replaced);
            }
            Placeholders::Target {
                name,
                spec,
                version,
                manifest,
                target,
                ..
            } => {
                let original = replace_workspace_rust_version(arg.clone(), placeholders)?;
                #[expect(
                    clippy::literal_string_with_formatting_args,
                    reason = "cargo-each placeholder tokens, not format args"
                )]
                let replaced = original
                    .replace("{name}", name)
                    .replace("{spec}", spec)
                    .replace("{version}", version)
                    .replace("{manifest}", manifest)
                    .replace(TARGET_TOKEN, target);
                out.push(replaced);
            }
            Placeholders::Once { packages, .. } => {
                // Validation above guarantees each arg is either exactly
                // `{packages}` or contains no placeholder token at all.
                if arg == PACKAGES_TOKEN {
                    out.extend(packages.iter().cloned());
                } else {
                    out.push(replace_workspace_rust_version(arg.clone(), placeholders)?);
                }
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn pkg() -> Placeholders {
        Placeholders::Package {
            name: "cargo-anvil".to_owned(),
            spec: "cargo-anvil@0.4.0".to_owned(),
            version: "0.4.0".to_owned(),
            manifest: "/ws/cargo-anvil/Cargo.toml".to_owned(),
            workspace_rust_version: None,
        }
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn per_package_tokens_expand() {
        let out = substitute(&args(&["check-external-types", "--manifest-path", "{manifest}"]), &pkg()).expect("substitute");
        assert_eq!(out, ["check-external-types", "--manifest-path", "/ws/cargo-anvil/Cargo.toml"]);
    }

    #[test]
    fn spec_and_name_distinct() {
        let out = substitute(&args(&["--package", "{name}", "note={spec}"]), &pkg()).expect("substitute");
        assert_eq!(out, ["--package", "cargo-anvil", "note=cargo-anvil@0.4.0"]);
    }

    #[test]
    fn packages_token_rejected_in_per_package_mode() {
        let err = substitute(&args(&["clippy", "{packages}"]), &pkg()).expect_err("misuse");
        assert!(err.to_string().contains("{packages}"));
    }

    #[test]
    fn once_expands_packages_token() {
        let ph = Placeholders::Once {
            packages: args(&["--package", "a@1", "--package", "b@2"]),
            workspace_rust_version: None,
        };
        let out = substitute(&args(&["clippy", "{packages}", "--all-targets"]), &ph).expect("substitute");
        assert_eq!(out, ["clippy", "--package", "a@1", "--package", "b@2", "--all-targets"]);
    }

    #[test]
    fn once_rejects_per_package_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: None,
        };
        let err = substitute(&args(&["test", "--package", "{name}"]), &ph).expect_err("misuse");
        assert!(err.to_string().contains("{name}"));
    }

    #[test]
    fn once_rejects_target_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: None,
        };
        let err = substitute(&args(&["test", "--test", "{target}"]), &ph).expect_err("misuse");
        assert!(err.to_string().contains("{target}"));
    }

    #[test]
    fn once_rejects_embedded_packages_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: None,
        };
        let err = substitute(&args(&["x={packages}"]), &ph).expect_err("misuse");
        assert!(err.to_string().contains("stand alone"));
    }

    #[test]
    fn target_mode_expands_package_and_target_tokens() {
        let ph = Placeholders::Target {
            name: "cargo-anvil".to_owned(),
            spec: "cargo-anvil@0.4.0".to_owned(),
            version: "0.4.0".to_owned(),
            manifest: "/ws/cargo-anvil/Cargo.toml".to_owned(),
            target: "loom".to_owned(),
            workspace_rust_version: None,
        };
        let out = substitute(&args(&["test", "-p", "{name}", "--test", "{target}"]), &ph).expect("substitute");
        assert_eq!(out, ["test", "-p", "cargo-anvil", "--test", "loom"]);
    }

    #[test]
    fn target_token_is_rejected_in_per_package_mode() {
        let err = substitute(&args(&["echo", "{target}"]), &pkg()).expect_err("misuse");
        assert!(err.to_string().contains("per-target"));
    }

    #[test]
    fn workspace_rust_version_expands_in_every_mode() {
        let command = args(&["rustup", "toolchain", "install", "{workspace-rust-version}"]);
        let mut package = pkg();
        let Placeholders::Package {
            workspace_rust_version, ..
        } = &mut package
        else {
            unreachable!("pkg returns package placeholders");
        };
        *workspace_rust_version = Some("1.80".to_owned());
        assert_eq!(
            substitute(&command, &package).expect("package substitution"),
            ["rustup", "toolchain", "install", "1.80"]
        );

        let once = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: Some("1.80".to_owned()),
        };
        assert_eq!(
            substitute(&command, &once).expect("once substitution"),
            ["rustup", "toolchain", "install", "1.80"]
        );
    }

    #[test]
    fn package_values_are_not_rescanned_for_workspace_tokens() {
        let placeholders = Placeholders::Package {
            name: "crate".to_owned(),
            spec: "crate@1.0.0".to_owned(),
            version: "1.0.0".to_owned(),
            manifest: "/ws/{workspace-rust-version}/crate/Cargo.toml".to_owned(),
            workspace_rust_version: Some("1.80".to_owned()),
        };
        assert_eq!(
            substitute(&args(&["{workspace-rust-version}", "{manifest}"]), &placeholders).expect("substitute package placeholders"),
            ["1.80", "/ws/{workspace-rust-version}/crate/Cargo.toml"]
        );
    }

    #[test]
    fn target_values_are_not_rescanned_for_workspace_tokens() {
        let placeholders = Placeholders::Target {
            name: "crate".to_owned(),
            spec: "crate@1.0.0".to_owned(),
            version: "1.0.0".to_owned(),
            manifest: "/ws/{workspace-rust-version}/crate/Cargo.toml".to_owned(),
            target: "example".to_owned(),
            workspace_rust_version: Some("1.80".to_owned()),
        };
        assert_eq!(
            substitute(&args(&["{workspace-rust-version}", "{manifest}:{target}"]), &placeholders,)
                .expect("substitute target placeholders"),
            ["1.80", "/ws/{workspace-rust-version}/crate/Cargo.toml:example"]
        );
    }

    #[test]
    fn detects_workspace_rust_version_usage() {
        assert!(uses_workspace_rust_version(&args(&["tool", "v={workspace-rust-version}"])));
        assert!(!uses_workspace_rust_version(&args(&["tool", "{name}"])));
    }
}
