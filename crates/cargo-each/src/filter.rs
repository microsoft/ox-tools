// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `--filter` / `--exclude-filter` Boolean expression language.
//!
//! Expressions combine a small, fixed set of predicates over cargo metadata
//! with `not`, `and`, `or`, and parentheses. This is enough to express every
//! ad-hoc `cargo metadata` filter the recipes currently hand-roll:
//!
//! - `lib` / `bin` / `target-kind:<kind>` — the member has a target of that
//!   kind.
//! - `publishable` — Cargo permits publishing the package.
//! - `feature:<name>` — the package declares the feature.
//! - `dep:<name>` — the member declares `<name>` as a dependency.
//! - `metadata:<dotted.key>` — the `package.metadata.<dotted.key>` key exists.
//! - `metadata:<dotted.key>=<value>` — that key equals `<value>` (numeric
//!   compare when both sides parse as a number, else string compare).

use cargo_metadata::TargetKind;
use serde_json::Value;

use crate::error::{EachError, InvalidFilterExpressionError};
use crate::workspace::{Member, parse_target_kind};

/// A parsed filter predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Predicate {
    /// `left and right`: both operands match.
    And(Vec<Self>),
    /// `left or right`: at least one operand matches.
    Or(Vec<Self>),
    /// `not operand`: the operand does not match.
    Not(Box<Self>),
    /// `lib`: member has a `lib` target.
    HasLib,
    /// `bin`: member has a `bin` target.
    HasBin,
    /// `target-kind:<kind>`: member has a target of that Cargo kind.
    HasTargetKind(TargetKind),
    /// `publishable`: Cargo permits publishing the package.
    Publishable,
    /// `feature:<name>`: member declares the named feature.
    HasFeature(String),
    /// `dep:<name>`: member declares `<name>` as a dependency.
    DependsOn(String),
    /// `metadata:<dotted.key>`: the metadata key is present.
    MetadataPresent(String),
    /// `metadata:<dotted.key>=<value>`: the metadata key equals `<value>`.
    MetadataEquals {
        /// The dotted metadata key path.
        key: String,
        /// The expected value.
        value: String,
    },
}

impl Predicate {
    /// Parse a Boolean filter expression.
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] if the Boolean syntax is malformed, an atom is not
    /// a recognized predicate, or a predicate argument is invalid.
    pub(crate) fn parse(spec: &str) -> Result<Self, EachError> {
        ExpressionParser::new(spec)?.parse()
    }

    /// Parse one atomic predicate from its command-line spelling.
    fn parse_atom(spec: &str) -> Result<Self, EachError> {
        match spec {
            "lib" => Ok(Self::HasLib),
            "bin" => Ok(Self::HasBin),
            "publishable" => Ok(Self::Publishable),
            _ => {
                if let Some(kind) = spec.strip_prefix("target-kind:") {
                    let Some(kind) = parse_target_kind(kind) else {
                        return Err(invalid(
                            spec,
                            "unknown target kind; expected one of: lib, rlib, dylib, cdylib, staticlib, proc-macro, bin, example, test, bench, custom-build",
                        ));
                    };
                    Ok(Self::HasTargetKind(kind))
                } else if let Some(name) = spec.strip_prefix("feature:") {
                    if name.is_empty() {
                        return Err(invalid(spec, "empty feature name"));
                    }
                    Ok(Self::HasFeature(name.to_owned()))
                } else if let Some(name) = spec.strip_prefix("dep:") {
                    if name.is_empty() {
                        return Err(invalid(spec, "empty dependency name"));
                    }
                    Ok(Self::DependsOn(name.to_owned()))
                } else if let Some(rest) = spec.strip_prefix("metadata:") {
                    parse_metadata(spec, rest)
                } else {
                    Err(invalid(
                        spec,
                        "expected one of: lib, bin, target-kind:<kind>, publishable, feature:<name>, dep:<name>, metadata:<key>[=<value>]",
                    ))
                }
            }
        }
    }

    /// Evaluate this predicate against a workspace member.
    #[must_use]
    pub(crate) fn matches(&self, member: &Member) -> bool {
        match self {
            Self::And(predicates) => predicates.iter().all(|predicate| predicate.matches(member)),
            Self::Or(predicates) => predicates.iter().any(|predicate| predicate.matches(member)),
            Self::Not(predicate) => !predicate.matches(member),
            Self::HasLib => member.has_lib,
            Self::HasBin => member.has_bin,
            Self::HasTargetKind(kind) => member.targets.iter().any(|target| target.kinds.contains(kind)),
            Self::Publishable => member.publishable,
            Self::HasFeature(name) => member.features.contains(name),
            Self::DependsOn(name) => member.dependencies.contains(name),
            Self::MetadataPresent(key) => lookup(&member.metadata, key).is_some(),
            Self::MetadataEquals { key, value } => lookup(&member.metadata, key).is_some_and(|v| value_equals(v, value)),
        }
    }
}

/// Parse the portion of a `metadata:` predicate after the prefix.
fn parse_metadata(spec: &str, rest: &str) -> Result<Predicate, EachError> {
    if let Some((key, value)) = rest.split_once('=') {
        validate_key(spec, key)?;
        Ok(Predicate::MetadataEquals {
            key: key.to_owned(),
            value: parse_metadata_value(spec, value)?,
        })
    } else {
        validate_key(spec, rest)?;
        Ok(Predicate::MetadataPresent(rest.to_owned()))
    }
}

fn parse_metadata_value(spec: &str, value: &str) -> Result<String, EachError> {
    if !value.starts_with('"') {
        if value.contains('"') {
            return Err(invalid(spec, "double quotes must surround the complete metadata value"));
        }
        return Ok(value.to_owned());
    }
    let Some(inner) = value.strip_prefix('"').and_then(|value| value.strip_suffix('"')) else {
        return Err(invalid(spec, "unclosed double-quoted metadata value"));
    };
    let mut parsed = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            parsed.push(character);
            continue;
        }
        match characters.next() {
            Some(escaped @ ('"' | '\\')) => parsed.push(escaped),
            Some(other) => {
                return Err(invalid(
                    spec,
                    &format!("unsupported escape `\\{other}` in metadata value; expected `\\\"` or `\\\\`"),
                ));
            }
            None => return Err(invalid(spec, "trailing `\\` in metadata value")),
        }
    }
    Ok(parsed)
}

/// Reject metadata keys that are not a valid dotted path — empty, or with any
/// empty segment (`a..b`, `.role`, `role.`). Such keys parse but can never
/// match (`Value::get("")` is always `None`), so a typo would otherwise
/// silently yield an empty result set instead of a loud usage error.
fn validate_key(spec: &str, key: &str) -> Result<(), EachError> {
    if key.is_empty() {
        return Err(invalid(spec, "empty metadata key"));
    }
    if key.split('.').any(str::is_empty) {
        return Err(invalid(spec, "metadata key must be a dotted path with non-empty segments"));
    }
    Ok(())
}

fn invalid(spec: &str, reason: &str) -> EachError {
    InvalidFilterExpressionError::new(spec.to_owned(), reason.to_owned()).into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Atom(&'a str),
    And,
    Or,
    Not,
    LeftParen,
    RightParen,
}

impl Token<'_> {
    fn display(self) -> &'static str {
        match self {
            Self::Atom(_) => "predicate",
            Self::And => "`and`",
            Self::Or => "`or`",
            Self::Not => "`not`",
            Self::LeftParen => "`(`",
            Self::RightParen => "`)`",
        }
    }
}

struct ExpressionParser<'a> {
    spec: &'a str,
    tokens: Vec<Token<'a>>,
    position: usize,
}

impl<'a> ExpressionParser<'a> {
    const MAX_NESTING: usize = 64;

    fn new(spec: &'a str) -> Result<Self, EachError> {
        Ok(Self {
            spec,
            tokens: tokenize(spec)?,
            position: 0,
        })
    }

    fn parse(mut self) -> Result<Predicate, EachError> {
        if self.tokens.is_empty() {
            return Err(invalid(self.spec, "empty expression"));
        }
        if self.tokens.iter().all(|token| matches!(token, Token::Atom(_))) {
            return Predicate::parse_atom(self.spec.trim());
        }
        let expression = self.parse_or(0)?;
        if let Some(token) = self.peek() {
            return Err(invalid(
                self.spec,
                &format!("unexpected {}; expected `and`, `or`, or end of expression", token.display()),
            ));
        }
        Ok(expression)
    }

    fn parse_or(&mut self, depth: usize) -> Result<Predicate, EachError> {
        let mut operands = vec![self.parse_and(depth)?];
        while self.peek() == Some(Token::Or) {
            self.position += 1;
            operands.push(self.parse_and(depth)?);
        }
        Ok(if operands.len() == 1 {
            operands.pop().expect("length checked above to contain exactly one operand")
        } else {
            Predicate::Or(operands)
        })
    }

    fn parse_and(&mut self, depth: usize) -> Result<Predicate, EachError> {
        let mut operands = vec![self.parse_unary(depth)?];
        while self.peek() == Some(Token::And) {
            self.position += 1;
            operands.push(self.parse_unary(depth)?);
        }
        Ok(if operands.len() == 1 {
            operands.pop().expect("length checked above to contain exactly one operand")
        } else {
            Predicate::And(operands)
        })
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Predicate, EachError> {
        if depth > Self::MAX_NESTING {
            return Err(invalid(self.spec, "expression nesting exceeds 64 levels"));
        }
        match self.peek() {
            Some(Token::Atom(atom)) => {
                self.position += 1;
                Predicate::parse_atom(atom)
            }
            Some(Token::Not) => {
                self.position += 1;
                Ok(Predicate::Not(Box::new(self.parse_unary(depth + 1)?)))
            }
            Some(Token::LeftParen) => {
                self.position += 1;
                let expression = self.parse_or(depth + 1)?;
                if self.peek() != Some(Token::RightParen) {
                    return Err(invalid(self.spec, "unclosed `(`"));
                }
                self.position += 1;
                Ok(expression)
            }
            Some(token) => Err(invalid(
                self.spec,
                &format!("unexpected {}; expected a predicate, `not`, or `(`", token.display()),
            )),
            None => Err(invalid(
                self.spec,
                "unexpected end of expression; expected a predicate, `not`, or `(`",
            )),
        }
    }

    fn peek(&self) -> Option<Token<'a>> {
        self.tokens.get(self.position).copied()
    }
}

fn tokenize(spec: &str) -> Result<Vec<Token<'_>>, EachError> {
    let mut tokens = Vec::new();
    let mut atom_start = None;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in spec.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        if character == '"' {
            quoted = true;
            if atom_start.is_none() {
                atom_start = Some(index);
            }
            continue;
        }
        if character.is_whitespace() || matches!(character, '(' | ')') {
            if let Some(start) = atom_start.take() {
                tokens.push(classify_token(&spec[start..index]));
            }
            match character {
                '(' => tokens.push(Token::LeftParen),
                ')' => tokens.push(Token::RightParen),
                _ => {}
            }
        } else if atom_start.is_none() {
            atom_start = Some(index);
        }
    }
    if let Some(start) = atom_start {
        tokens.push(classify_token(&spec[start..]));
    }
    if quoted {
        return Err(invalid(spec, "unclosed double quote"));
    }
    Ok(tokens)
}

fn classify_token(token: &str) -> Token<'_> {
    match token {
        "and" => Token::And,
        "or" => Token::Or,
        "not" => Token::Not,
        atom => Token::Atom(atom),
    }
}

/// Walk a dotted key path (`coverage-gate.min-lines-percent`) into a JSON
/// metadata value.
fn lookup<'v>(metadata: &'v Value, dotted_key: &str) -> Option<&'v Value> {
    let mut node = metadata;
    for segment in dotted_key.split('.') {
        node = node.get(segment)?;
    }
    Some(node)
}

/// Compare a metadata node to an expected string. When both the node's
/// scalar rendering and the expected value parse as `f64`, compare
/// numerically (so `0` matches `0.0`); otherwise compare as strings.
fn value_equals(node: &Value, expected: &str) -> bool {
    let rendered = match node {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return false,
    };
    match (rendered.parse::<f64>(), expected.parse::<f64>()) {
        // Both sides are numbers: compare numerically so `0` matches `0.0`. The
        // string-equality fallback also covers a literal `NaN`, which never
        // compares equal to itself numerically (so `metadata:key=NaN` still
        // matches a `"NaN"` value). Exact equality is intended — both sides are
        // short decimal literals, so no epsilon fuzz is meaningful here.
        #[expect(clippy::float_cmp, reason = "comparing exact parsed values of short decimal literals")]
        (Ok(a), Ok(b)) => a == b || rendered == expected,
        _ => rendered == expected,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::workspace::MemberTarget;

    fn member_with(deps: &[&str], metadata: Value, has_lib: bool, has_bin: bool) -> Member {
        Member {
            name: "m".to_owned(),
            version: "0.1.0".to_owned(),
            manifest_path: PathBuf::from("/ws/m/Cargo.toml"),
            publishable: true,
            features: BTreeSet::new(),
            targets: Vec::new(),
            has_lib,
            has_bin,
            dependencies: deps.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>(),
            metadata,
        }
    }

    #[test]
    fn parses_kind_predicates() {
        assert_eq!(Predicate::parse("lib").expect("lib"), Predicate::HasLib);
        assert_eq!(Predicate::parse("bin").expect("bin"), Predicate::HasBin);
        assert_eq!(
            Predicate::parse("target-kind:proc-macro").expect("target kind"),
            Predicate::HasTargetKind(TargetKind::ProcMacro)
        );
        assert_eq!(Predicate::parse("publishable").expect("publishable"), Predicate::Publishable);
        assert_eq!(
            Predicate::parse("feature:loom").expect("feature"),
            Predicate::HasFeature("loom".to_owned())
        );
    }

    #[test]
    fn parses_dep_predicate() {
        assert_eq!(Predicate::parse("dep:loom").expect("dep"), Predicate::DependsOn("loom".to_owned()));
    }

    #[test]
    fn parses_metadata_predicates() {
        assert_eq!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent").expect("present"),
            Predicate::MetadataPresent("coverage-gate.min-lines-percent".to_owned())
        );
        assert_eq!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent=0").expect("equals"),
            Predicate::MetadataEquals {
                key: "coverage-gate.min-lines-percent".to_owned(),
                value: "0".to_owned()
            }
        );
    }

    #[test]
    fn parses_boolean_operators_with_conventional_precedence() {
        assert_eq!(
            Predicate::parse("lib or bin and not publishable").expect("expression"),
            Predicate::Or(vec![
                Predicate::HasLib,
                Predicate::And(vec![Predicate::HasBin, Predicate::Not(Box::new(Predicate::Publishable)),]),
            ])
        );
    }

    #[test]
    fn parentheses_override_boolean_precedence() {
        assert_eq!(
            Predicate::parse("(lib or bin) and publishable").expect("expression"),
            Predicate::And(vec![
                Predicate::Or(vec![Predicate::HasLib, Predicate::HasBin]),
                Predicate::Publishable,
            ])
        );
    }

    #[test]
    fn atomic_metadata_value_may_contain_spaces() {
        assert_eq!(
            Predicate::parse("metadata:role=script only").expect("atomic predicate"),
            Predicate::MetadataEquals {
                key: "role".to_owned(),
                value: "script only".to_owned(),
            }
        );
    }

    #[test]
    fn quoted_metadata_value_may_contain_boolean_syntax() {
        assert_eq!(
            Predicate::parse(r#"metadata:role="research and (development)" and lib"#).expect("expression"),
            Predicate::And(vec![
                Predicate::MetadataEquals {
                    key: "role".to_owned(),
                    value: "research and (development)".to_owned(),
                },
                Predicate::HasLib,
            ])
        );
        assert_eq!(
            Predicate::parse(r#"metadata:role="quoted \"value\" and \\ path""#).expect("quoted atom"),
            Predicate::MetadataEquals {
                key: "role".to_owned(),
                value: "quoted \"value\" and \\ path".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_malformed_boolean_expressions() {
        for expression in [
            "",
            "lib and",
            "or lib",
            "lib bin",
            "(lib or bin",
            "lib or )",
            "()",
            "lib AND bin",
            r#"metadata:role="unclosed"#,
            r#"metadata:role="unsupported\q""#,
        ] {
            Predicate::parse(expression).expect_err(expression);
        }
        let too_deep = format!("{}lib{}", "(".repeat(66), ")".repeat(66));
        Predicate::parse(&too_deep).expect_err("excessive nesting");
    }

    #[test]
    fn rejects_unknown_and_empty() {
        Predicate::parse("nonsense").expect_err("unknown predicate must error");
        Predicate::parse("dep:").expect_err("empty dependency name must error");
        Predicate::parse("feature:").expect_err("empty feature name must error");
        Predicate::parse("target-kind:no-such-kind").expect_err("unknown target kind must error");
        Predicate::parse("metadata:").expect_err("empty metadata key must error");
    }

    #[test]
    fn rejects_metadata_keys_with_empty_segments() {
        // Keys with empty path segments parse but can never match, so they are
        // a loud usage error rather than a silent empty result.
        Predicate::parse("metadata:a..b").expect_err("double dot must error");
        Predicate::parse("metadata:.role").expect_err("leading dot must error");
        Predicate::parse("metadata:role.").expect_err("trailing dot must error");
        Predicate::parse("metadata:a..b=1").expect_err("double dot with value must error");
        Predicate::parse("metadata:.role=1").expect_err("leading dot with value must error");
    }

    #[test]
    fn kind_predicates_match() {
        let mut m = member_with(&[], Value::Null, true, false);
        m.targets.push(MemberTarget {
            name: "macros".to_owned(),
            kinds: std::iter::once(TargetKind::ProcMacro).collect(),
            required_features: BTreeSet::new(),
        });
        assert!(Predicate::HasLib.matches(&m));
        assert!(!Predicate::HasBin.matches(&m));
        assert!(Predicate::HasTargetKind(TargetKind::ProcMacro).matches(&m));
        assert!(!Predicate::HasTargetKind(TargetKind::Test).matches(&m));
    }

    #[test]
    fn package_fact_predicates_match() {
        let mut m = member_with(&[], Value::Null, false, false);
        m.publishable = false;
        m.features.insert("loom".to_owned());
        assert!(!Predicate::Publishable.matches(&m));
        assert!(Predicate::HasFeature("loom".to_owned()).matches(&m));
        assert!(!Predicate::HasFeature("serde".to_owned()).matches(&m));
    }

    #[test]
    fn boolean_expression_matches_member() {
        let mut m = member_with(&[], Value::Null, true, false);
        m.features.insert("loom".to_owned());
        assert!(
            Predicate::parse("publishable and (bin or feature:loom)")
                .expect("expression")
                .matches(&m)
        );
        assert!(
            !Predicate::parse("not lib or metadata:role=script-only")
                .expect("expression")
                .matches(&m)
        );
    }

    #[test]
    fn dep_predicate_matches() {
        let m = member_with(&["loom", "serde"], Value::Null, true, false);
        assert!(Predicate::DependsOn("loom".to_owned()).matches(&m));
        assert!(!Predicate::DependsOn("tokio".to_owned()).matches(&m));
    }

    #[test]
    fn metadata_present_and_equals() {
        let m = member_with(&[], json!({ "coverage-gate": { "min-lines-percent": 0 } }), true, false);
        assert!(Predicate::parse("metadata:coverage-gate.min-lines-percent").expect("p").matches(&m));
        // numeric-aware: `0` matches the JSON number `0`.
        assert!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent=0")
                .expect("p")
                .matches(&m)
        );
        assert!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent=0.0")
                .expect("p")
                .matches(&m)
        );
        assert!(
            !Predicate::parse("metadata:coverage-gate.min-lines-percent=50")
                .expect("p")
                .matches(&m)
        );
        assert!(!Predicate::parse("metadata:missing.key").expect("p").matches(&m));
    }

    #[test]
    fn metadata_string_equals() {
        let m = member_with(&[], json!({ "role": "script-only" }), true, false);
        assert!(Predicate::parse("metadata:role=script-only").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:role=library").expect("p").matches(&m));
    }

    #[test]
    fn metadata_nan_uses_string_equality() {
        // Both sides parse as `f64::NAN`, and `NaN == NaN` is false; the string
        // fallback must still match a literal `"NaN"` metadata value.
        let m = member_with(&[], json!({ "marker": "NaN" }), true, false);
        assert!(Predicate::parse("metadata:marker=NaN").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:marker=nan").expect("p").matches(&m));
    }

    #[test]
    fn metadata_bool_equals() {
        let m = member_with(&[], json!({ "flag": true }), true, false);
        assert!(Predicate::parse("metadata:flag=true").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:flag=false").expect("p").matches(&m));
    }

    #[test]
    fn metadata_non_scalar_never_equals() {
        // Object / array metadata values can be *present* but never compare
        // equal to a scalar expected string.
        let m = member_with(&[], json!({ "obj": { "a": 1 }, "arr": [1, 2] }), true, false);
        assert!(Predicate::parse("metadata:obj").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:obj=x").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:arr=x").expect("p").matches(&m));
    }

    #[test]
    fn rejects_empty_metadata_key_before_equals() {
        Predicate::parse("metadata:=value").expect_err("empty key with value must error");
    }
}
