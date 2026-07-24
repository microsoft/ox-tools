// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Placeholder substitution for the command template.
//!
//! A fixed, small set of `{token}` replacements — deliberately not an
//! expression language:
//!
//! - Per-package tokens (valid in per-package mode): `{name}`, `{spec}`,
//!   `{version}`, `{manifest}`. Replaced textually inside each argument.
//! - The once token (valid only in `--once` mode): `{packages}`. Must stand
//!   alone as a whole argument; it expands to the resolved selection flags,
//!   which is several tokens.
//!
//! Using a token in the wrong mode is a usage error ([`PlaceholderMisuseError`]).

use crate::error::{EachError, PlaceholderMisuseError};

/// Per-package placeholder tokens.
const PER_PACKAGE_TOKENS: [&str; 4] = ["{name}", "{spec}", "{version}", "{manifest}"];
/// The once-mode placeholder token.
const PACKAGES_TOKEN: &str = "{packages}";

/// The substitution context for one command invocation.
#[derive(Debug, Clone)]
pub enum Placeholders {
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
    },
    /// Once mode: `{packages}` expands to these pre-computed selection flags.
    Once {
        /// The cargo selection flags for the resolved set (e.g.
        /// `["--workspace"]` or `["--package", "a@1", "--package", "b@2"]`).
        packages: Vec<String>,
    },
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
pub fn substitute(args: &[String], placeholders: &Placeholders) -> Result<Vec<String>, EachError> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match placeholders {
            Placeholders::Package {
                name,
                spec,
                version,
                manifest,
            } => {
                if arg.contains(PACKAGES_TOKEN) {
                    return Err(PlaceholderMisuseError::new(PACKAGES_TOKEN.to_owned(), "only valid in --once mode".to_owned()).into());
                }
                // The `{name}` / `{spec}` / … literals are cargo-each
                // placeholder tokens, not Rust format-string arguments.
                #[expect(
                    clippy::literal_string_with_formatting_args,
                    reason = "cargo-each placeholder tokens, not format args"
                )]
                let replaced = arg
                    .replace("{name}", name)
                    .replace("{spec}", spec)
                    .replace("{version}", version)
                    .replace("{manifest}", manifest);
                out.push(replaced);
            }
            Placeholders::Once { packages } => {
                if let Some(token) = PER_PACKAGE_TOKENS.iter().find(|t| arg.contains(**t)) {
                    return Err(PlaceholderMisuseError::new(
                        (*token).to_owned(),
                        "per-package token is not valid in --once mode".to_owned(),
                    )
                    .into());
                }
                if arg == PACKAGES_TOKEN {
                    out.extend(packages.iter().cloned());
                } else if arg.contains(PACKAGES_TOKEN) {
                    return Err(PlaceholderMisuseError::new(
                        PACKAGES_TOKEN.to_owned(),
                        "must stand alone as a whole argument (it expands to multiple tokens)".to_owned(),
                    )
                    .into());
                } else {
                    out.push(arg.clone());
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
        };
        let out = substitute(&args(&["clippy", "{packages}", "--all-targets"]), &ph).expect("substitute");
        assert_eq!(out, ["clippy", "--package", "a@1", "--package", "b@2", "--all-targets"]);
    }

    #[test]
    fn once_rejects_per_package_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
        };
        let err = substitute(&args(&["test", "--package", "{name}"]), &ph).expect_err("misuse");
        assert!(err.to_string().contains("{name}"));
    }

    #[test]
    fn once_rejects_embedded_packages_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
        };
        let err = substitute(&args(&["x={packages}"]), &ph).expect_err("misuse");
        assert!(err.to_string().contains("stand alone"));
    }
}
