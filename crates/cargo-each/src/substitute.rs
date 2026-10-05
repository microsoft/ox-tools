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
//! - The workspace token `{workspace-rust-version}`, valid in Cargo-backed
//!   per-package, per-target, and once modes. An absent root declaration
//!   expands to an empty string.
//! - JSON-record fields through `{json:key}`, valid only in JSON-record mode.
//!
//! Using a token in the wrong mode is a usage error ([`PlaceholderMisuseError`]).
//!
//! Every `{json:…}` sequence is validated as a JSON placeholder. Other
//! unrecognized `{…}` sequences — a typo like `{manfiest}`, a wrong-case
//! `{Name}`, or a literal brace an argument genuinely needs — are passed through
//! **verbatim** to the spawned command. There is no brace-escape mechanism, so
//! this passthrough is a deliberate part of the contract, not an oversight.

use crate::error::{EachError, JsonRecordFieldError, PlaceholderMisuseError};
use crate::plan::Mode;

/// Per-package placeholder tokens.
const PER_PACKAGE_TOKENS: [&str; 4] = ["{name}", "{spec}", "{version}", "{manifest}"];
/// The per-target placeholder token.
const TARGET_TOKEN: &str = "{target}";
/// The once-mode placeholder token.
const PACKAGES_TOKEN: &str = "{packages}";
/// The workspace-wide Rust compatibility floor token.
const WORKSPACE_RUST_VERSION_TOKEN: &str = "{workspace-rust-version}";
/// Prefix of a dynamic JSON-record field placeholder.
const JSON_TOKEN_PREFIX: &str = "{json:";

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

fn visit_json_tokens<'a>(arg: &'a str, mut visit: impl FnMut(usize, usize, &'a str) -> Result<(), EachError>) -> Result<(), EachError> {
    let mut cursor = 0;
    while let Some(relative_start) = arg[cursor..].find(JSON_TOKEN_PREFIX) {
        let start = cursor + relative_start;
        let key_start = start + JSON_TOKEN_PREFIX.len();
        let Some(relative_end) = arg[key_start..].find('}') else {
            return Err(
                PlaceholderMisuseError::new(arg[start..].to_owned(), "JSON placeholder is missing its closing `}`".to_owned()).into(),
            );
        };
        let end = key_start + relative_end + 1;
        let key = &arg[key_start..end - 1];
        if key.is_empty() || key.contains('{') {
            return Err(
                PlaceholderMisuseError::new(arg[start..end].to_owned(), "expected a nonempty top-level field name".to_owned()).into(),
            );
        }
        visit(start, end, key)?;
        cursor = end;
    }
    Ok(())
}

fn replace_json_arg(
    arg: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
    source: &str,
    line: usize,
) -> Result<String, EachError> {
    let mut replaced = String::with_capacity(arg.len());
    let mut cursor = 0;
    visit_json_tokens(arg, |start, end, key| {
        replaced.push_str(&arg[cursor..start]);
        let Some(value) = fields.get(key) else {
            return Err(JsonRecordFieldError::new(source.to_owned(), line, key.to_owned(), "field is missing".to_owned()).into());
        };
        let Some(value) = value.as_str() else {
            return Err(JsonRecordFieldError::new(source.to_owned(), line, key.to_owned(), "field is not a string".to_owned()).into());
        };
        if value.contains('\0') {
            return Err(JsonRecordFieldError::new(
                source.to_owned(),
                line,
                key.to_owned(),
                "field contains a NUL byte, which cannot be passed in a process argument".to_owned(),
            )
            .into());
        }
        replaced.push_str(value);
        cursor = end;
        Ok(())
    })?;
    replaced.push_str(&arg[cursor..]);
    Ok(replaced)
}

fn replace_arg<'a>(arg: &str, placeholders: &'a Placeholders, mut replacements: Vec<(&'static str, &'a str)>) -> String {
    if arg.contains(WORKSPACE_RUST_VERSION_TOKEN) {
        let version = placeholders.workspace_rust_version().unwrap_or_default();
        replacements.push((WORKSPACE_RUST_VERSION_TOKEN, version));
    }

    let mut rest = arg;
    let mut replaced = String::with_capacity(arg.len());
    while let Some((offset, token, value)) = replacements
        .iter()
        .filter_map(|&(token, value)| rest.find(token).map(|offset| (offset, token, value)))
        .min_by_key(|&(offset, _, _)| offset)
    {
        let (literal, token_and_rest) = rest.split_at(offset);
        let (_token, remaining) = token_and_rest.split_at(token.len());
        replaced.push_str(literal);
        replaced.push_str(value);
        // #[gamma::skip(stmt.delete_assign, tag = "outofmemory", reason = "the substitution loop must consume each matched token")]
        rest = remaining;
    }
    replaced.push_str(rest);

    replaced
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
        if arg.contains(JSON_TOKEN_PREFIX) {
            return Err(PlaceholderMisuseError::new("{json:key}".to_owned(), "only valid in JSON-record mode".to_owned()).into());
        }
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

/// Validate placeholders accepted by JSON-record mode.
pub(crate) fn validate_json_placeholders(args: &[String]) -> Result<(), EachError> {
    for arg in args {
        for token in PER_PACKAGE_TOKENS
            .into_iter()
            .chain([TARGET_TOKEN, PACKAGES_TOKEN, WORKSPACE_RUST_VERSION_TOKEN])
        {
            if arg.contains(token) {
                return Err(PlaceholderMisuseError::new(token.to_owned(), "not valid in JSON-record mode".to_owned()).into());
            }
        }
        visit_json_tokens(arg, |_start, _end, _key| Ok(()))?;
    }
    Ok(())
}

/// Expand JSON placeholders after [`validate_json_placeholders`] succeeds.
pub(crate) fn substitute_json_prevalidated(
    args: &[String],
    fields: &serde_json::Map<String, serde_json::Value>,
    source: &str,
    line: usize,
) -> Result<Vec<String>, EachError> {
    args.iter().map(|arg| replace_json_arg(arg, fields, source, line)).collect()
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
    match placeholders {
        Placeholders::Package { .. } => validate_placeholders(args, Mode::PerPackage)?,
        Placeholders::Target { .. } => validate_placeholders(args, Mode::PerTarget)?,
        Placeholders::Once { .. } => validate_placeholders(args, Mode::Once)?,
    }
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
                let replacements = vec![
                    // #[gamma::skip(literal.str_to_empty, tag = "outofmemory", reason = "the stopped campaign exhausted its memory budget when the package name token was emptied")]
                    ("{name}", name.as_str()),
                    // #[gamma::skip(literal.str_to_empty, tag = "outofmemory", reason = "the stopped campaign exhausted its memory budget when the package spec token was emptied")]
                    ("{spec}", spec.as_str()),
                    // #[gamma::skip(literal.str_to_empty, tag = "outofmemory", reason = "the stopped campaign exhausted its memory budget when the package version token was emptied")]
                    ("{version}", version.as_str()),
                    // #[gamma::skip(literal.str_to_empty, tag = "outofmemory", reason = "the stopped campaign exhausted its memory budget when the package manifest token was emptied")]
                    ("{manifest}", manifest.as_str()),
                ];
                let replaced = replace_arg(arg, placeholders, replacements);
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
                let replacements = vec![
                    // #[gamma::skip(literal.str_to_empty, tag = "timeout", reason = "the stopped campaign timed out when the target-mode name token was emptied")]
                    ("{name}", name.as_str()),
                    // #[gamma::skip(literal.str_to_empty, tag = "outofmemory", reason = "the stopped campaign exhausted its memory budget when the target-mode spec token was emptied")]
                    ("{spec}", spec.as_str()),
                    // #[gamma::skip(literal.str_to_empty, tag = "timeout", reason = "the stopped campaign timed out when the target-mode version token was emptied")]
                    ("{version}", version.as_str()),
                    // #[gamma::skip(literal.str_to_empty, tag = "outofmemory", reason = "the stopped campaign exhausted its memory budget when the target-mode manifest token was emptied")]
                    ("{manifest}", manifest.as_str()),
                    (TARGET_TOKEN, target.as_str()),
                ];
                let replaced = replace_arg(arg, placeholders, replacements);
                out.push(replaced);
            }
            Placeholders::Once { packages, .. } => {
                // Validation above guarantees each arg is either exactly
                // `{packages}` or contains no placeholder token at all.
                if arg == PACKAGES_TOKEN {
                    out.extend(packages.iter().cloned());
                } else {
                    out.push(replace_arg(arg, placeholders, Vec::new()));
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
        let out = substitute(
            &args(&["{name}", "{spec}", "{version}", "{manifest}", "{name}:{version}:{name}"]),
            &pkg(),
        )
        .expect("substitute");
        assert_eq!(
            out,
            [
                "cargo-anvil",
                "cargo-anvil@0.4.0",
                "0.4.0",
                "/ws/cargo-anvil/Cargo.toml",
                "cargo-anvil:0.4.0:cargo-anvil",
            ]
        );
    }

    #[test]
    fn spec_and_name_distinct() {
        let out = substitute(&args(&["--package", "{name}", "note={spec}"]), &pkg()).expect("substitute");
        assert_eq!(out, ["--package", "cargo-anvil", "note=cargo-anvil@0.4.0"]);
    }

    #[test]
    fn packages_token_rejected_in_per_package_mode() {
        let err = substitute(&args(&["clippy", "{packages}"]), &pkg()).expect_err("misuse");
        assert_eq!(
            err.to_string(),
            "placeholder `{packages}` cannot be used here: only valid in --once mode"
        );
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
        assert_eq!(
            err.to_string(),
            "placeholder `{name}` cannot be used here: per-package token is not valid in --once mode"
        );
    }

    #[test]
    fn once_rejects_target_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: None,
        };
        let err = substitute(&args(&["test", "--test", "{target}"]), &ph).expect_err("misuse");
        assert_eq!(
            err.to_string(),
            "placeholder `{target}` cannot be used here: per-target token is not valid in --once mode"
        );
    }

    #[test]
    fn once_rejects_embedded_packages_token() {
        let ph = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: None,
        };
        let err = substitute(&args(&["x={packages}"]), &ph).expect_err("misuse");
        assert_eq!(
            err.to_string(),
            "placeholder `{packages}` cannot be used here: must stand alone as a whole argument (it expands to multiple tokens)"
        );
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
        let out = substitute(
            &args(&["{name}", "{spec}", "{version}", "{manifest}", "{target}", "{target}:{name}"]),
            &ph,
        )
        .expect("substitute");
        assert_eq!(
            out,
            [
                "cargo-anvil",
                "cargo-anvil@0.4.0",
                "0.4.0",
                "/ws/cargo-anvil/Cargo.toml",
                "loom",
                "loom:cargo-anvil",
            ]
        );
    }

    #[test]
    fn target_token_is_rejected_in_per_package_mode() {
        let err = substitute(&args(&["echo", "{target}"]), &pkg()).expect_err("misuse");
        assert_eq!(
            err.to_string(),
            "placeholder `{target}` cannot be used here: only valid in per-target mode"
        );
    }

    #[test]
    fn unknown_and_brace_like_tokens_pass_through_verbatim() {
        let input = args(&["{manfiest}", "x={Name}", "{", "}"]);
        assert_eq!(substitute(&input, &pkg()).expect("substitute"), input);
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
    fn absent_workspace_rust_version_expands_to_empty_in_every_mode() {
        let command = args(&["echo", "{workspace-rust-version}"]);
        assert_eq!(substitute(&command, &pkg()).expect("optional package value"), ["echo", ""]);
        assert_eq!(
            substitute(&args(&["+{workspace-rust-version}"]), &pkg()).expect("textual empty substitution"),
            ["+"]
        );

        let target = Placeholders::Target {
            name: "crate".to_owned(),
            spec: "crate@1.0.0".to_owned(),
            version: "1.0.0".to_owned(),
            manifest: "/ws/crate/Cargo.toml".to_owned(),
            target: "example".to_owned(),
            workspace_rust_version: None,
        };
        assert_eq!(substitute(&command, &target).expect("optional target value"), ["echo", ""]);

        let once = Placeholders::Once {
            packages: args(&["--workspace"]),
            workspace_rust_version: None,
        };
        assert_eq!(substitute(&command, &once).expect("optional once value"), ["echo", ""]);
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
    fn manifest_values_are_not_rescanned_for_target_tokens() {
        let placeholders = Placeholders::Target {
            name: "crate".to_owned(),
            spec: "crate@1.0.0".to_owned(),
            version: "1.0.0".to_owned(),
            manifest: "/ws/{target}/crate/Cargo.toml".to_owned(),
            target: "example".to_owned(),
            workspace_rust_version: None,
        };
        assert_eq!(
            substitute(&args(&["{manifest}:{target}"]), &placeholders).expect("substitute target placeholders"),
            ["/ws/{target}/crate/Cargo.toml:example"]
        );
    }

    #[test]
    fn detects_workspace_rust_version_usage() {
        assert!(uses_workspace_rust_version(&args(&["tool", "v={workspace-rust-version}"])));
        assert!(!uses_workspace_rust_version(&args(&["tool", "{name}"])));
    }

    #[test]
    fn json_fields_expand_without_rescanning_inserted_values() {
        let fields = serde_json::json!({
            "package": "alpha",
            "test": "{json:package}"
        })
        .as_object()
        .expect("object")
        .clone();
        let command = args(&["{json:package}:{json:test}:{json:package}"]);
        validate_json_placeholders(&command).expect("valid JSON placeholders");
        assert_eq!(
            substitute_json_prevalidated(&command, &fields, "records.jsonl", 4).expect("JSON substitution"),
            ["alpha:{json:package}:alpha"]
        );
    }

    #[test]
    fn json_mode_rejects_missing_nonstring_and_foreign_placeholders() {
        let fields = serde_json::json!({"number": 1, "nul": "a\u{0}b"})
            .as_object()
            .expect("object")
            .clone();
        for (command, expected) in [
            ("{json:missing}", "field is missing"),
            ("{json:number}", "field is not a string"),
            ("{json:nul}", "field contains a NUL byte"),
            ("{name}", "not valid in JSON-record mode"),
            ("{json:", "missing its closing"),
            ("{json:}", "nonempty top-level field name"),
            ("{json:{nested}}", "nonempty top-level field name"),
        ] {
            let command = args(&[command]);
            let error = validate_json_placeholders(&command)
                .and_then(|()| substitute_json_prevalidated(&command, &fields, "records.jsonl", 2))
                .expect_err("invalid JSON placeholder must fail");
            assert!(error.to_string().contains(expected), "{command:?}: {error}");
        }
    }

    #[test]
    fn ordinary_modes_reject_json_placeholders() {
        let error = substitute(&args(&["echo", "{json:value}"]), &pkg()).expect_err("JSON placeholder outside JSON mode must fail");
        assert!(error.to_string().contains("only valid in JSON-record mode"));
    }
}
