// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolving a name or a selector against the catalog.

use super::catalog::{PRESETS, REGISTRY};
use super::{Mutator, Preset};
use crate::error::{Error, error};
use crate::{HashSet, Result};

/// Looks up a mutator by its exact registry name.
#[must_use]
pub fn find(name: &str) -> Option<&'static Mutator> {
    REGISTRY.iter().find(|mutator| mutator.name == name)
}

/// Looks up a mutator preset by name, without the leading `@`.
#[must_use]
pub fn find_preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

/// Returns the distinct family prefixes present in the registry, in registry order.
#[must_use]
pub fn families() -> Vec<&'static str> {
    let mut seen = HashSet::default();
    let mut out = Vec::new();

    for mutator in REGISTRY {
        let family = mutator.name.split('.').next().unwrap_or(mutator.name);

        if seen.insert(family) {
            out.push(family);
        }
    }

    out
}

/// Expands one selector into the mutator names it matches.
///
/// A selector is a full name, a family prefix, an `@preset`, an academic alias, or `all`.
pub fn resolve(selector: &str) -> Result<Vec<&'static str>> {
    if selector == "all" {
        return Ok(REGISTRY.iter().map(|m| m.name).collect());
    }

    if let Some(preset_name) = selector.strip_prefix('@') {
        let preset = find_preset(preset_name).ok_or_else(|| unknown(selector))?;
        let mut names = Vec::new();

        for member in preset.members {
            match *member {
                "*" => names.extend(REGISTRY.iter().map(|m| m.name)),
                "@default" => names.extend(REGISTRY.iter().filter(|m| m.default_on).map(|m| m.name)),
                other => names.extend(resolve(other)?),
            }
        }

        return Ok(names);
    }

    if let Some(mutator) = find(selector) {
        return Ok(vec![mutator.name]);
    }

    // Family or sub-family prefix: `arith`, or `combinator.iter`.
    let prefix = format!("{selector}.");
    let matched: Vec<&'static str> = REGISTRY
        .iter()
        .filter(|m| m.name.starts_with(prefix.as_str()))
        .map(|m| m.name)
        .collect();

    if !matched.is_empty() {
        return Ok(matched);
    }

    // Academic or industry alias, matched case-insensitively.
    let matched: Vec<&'static str> = REGISTRY
        .iter()
        .filter(|m| m.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(selector)))
        .map(|m| m.name)
        .collect();

    if matched.is_empty() { Err(unknown(selector)) } else { Ok(matched) }
}

/// Builds the error for an unmatched selector, with a spelling suggestion.
fn unknown(selector: &str) -> Error {
    let mut best: Option<(f64, String)> = None;

    let candidates = REGISTRY
        .iter()
        .map(|m| m.name.to_owned())
        .chain(families().into_iter().map(ToOwned::to_owned))
        .chain(REGISTRY.iter().flat_map(|m| m.aliases.iter().map(|a| (*a).to_owned())))
        .chain(PRESETS.iter().map(|preset| format!("@{}", preset.name)));

    for candidate in candidates {
        let score = strsim::jaro_winkler(selector, &candidate);

        if score > best.as_ref().map_or(0.85, |(previous, _)| *previous) {
            best = Some((score, candidate));
        }
    }

    best.map_or_else(
        || error!("unknown mutator selector `{selector}`; run `cargo gamma list mutators` to see the registry"),
        |(_, suggestion)| error!("unknown mutator selector `{selector}`; did you mean `{suggestion}`?"),
    )
    .usage()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_distant_unknown_selector_falls_back_to_the_registry_hint() {
        let error = resolve("zzzzzzzzzz").expect_err("the selector is not in the registry");
        let message = error.to_string();

        assert!(message.contains("unknown mutator selector `zzzzzzzzzz`"), "{message}");
        assert!(message.contains("run `cargo gamma list mutators`"), "{message}");
        assert!(!message.contains("did you mean"), "{message}");
    }

    #[test]
    fn a_close_unknown_selector_offers_a_spelling_suggestion() {
        let error = resolve("reltional").expect_err("the selector misspells a known family");
        let message = error.to_string();

        assert!(message.contains("unknown mutator selector `reltional`"), "{message}");
        assert!(message.contains("did you mean `relational`?"), "{message}");
    }
}
