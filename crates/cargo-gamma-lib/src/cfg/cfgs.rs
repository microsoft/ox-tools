// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! One resolved configuration per package.

use super::CfgSet;
use crate::HashMap;

/// One [`CfgSet`] per package, since features differ between packages but the target does not.
#[derive(Clone, Debug, Default)]
pub struct Cfgs {
    per_package: HashMap<String, CfgSet>,
    fallback: CfgSet,
}

impl Cfgs {
    /// Builds a set for every package named in `features`, sharing one target answer.
    ///
    /// Every set describes the instrumented build, which is `cargo test --no-run`, so `test` is
    /// among the predicates that hold; see [`CfgSet::with_test`].
    #[must_use]
    pub fn new(target: &CfgSet, features: &HashMap<String, Vec<String>>) -> Self {
        let per_package = features
            .iter()
            .map(|(package, enabled)| (package.clone(), target.clone().with_features(enabled.iter().cloned()).with_test()))
            .collect();

        Self {
            per_package,
            fallback: CfgSet::unconditional(),
        }
    }

    /// Returns a map under which nothing is stripped, for callers with no cfg information.
    #[must_use]
    pub fn unconditional() -> Self {
        Self::default()
    }

    /// Returns the set for a package.
    ///
    /// A package that was never resolved gets the unconditional set, so an unexpected name leaves
    /// its code mutable rather than silently emptying it.
    #[must_use]
    pub fn for_package(&self, package: &str) -> &CfgSet {
        self.per_package.get(package).unwrap_or(&self.fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_gets_its_own_features() {
        let mut features: HashMap<String, Vec<String>> = HashMap::default();

        let _old = features.insert("alpha".to_owned(), vec!["std".to_owned()]);
        let _old = features.insert("beta".to_owned(), Vec::new());

        let cfgs = Cfgs::new(&CfgSet::parse("unix\n"), &features);

        assert!(cfgs.for_package("alpha").holds_str("feature = \"std\""));
        assert!(!cfgs.for_package("beta").holds_str("feature = \"std\""));
        assert!(cfgs.for_package("alpha").holds_str("unix"));
    }

    #[test]
    fn an_unknown_package_is_left_alone() {
        let cfgs = Cfgs::new(&CfgSet::parse("unix\n"), &HashMap::default());

        assert!(cfgs.for_package("nobody").holds_str("windows"));
    }

    /// Every set the collector is handed comes from here, so this is where `--cfg test` has to be
    /// attached; a set built without it silently loses the unit-test target's code.
    #[test]
    fn every_package_set_describes_a_test_build() {
        let mut features: HashMap<String, Vec<String>> = HashMap::default();

        let _old = features.insert("alpha".to_owned(), Vec::new());

        let cfgs = Cfgs::new(&CfgSet::parse("unix\n"), &features);

        assert!(cfgs.for_package("alpha").holds_str("any(feature = \"absent\", test)"));
        assert!(cfgs.for_package("alpha").holds_str("not(test)"));
    }

    #[test]
    fn the_unconditional_map_strips_nothing() {
        let cfgs = Cfgs::unconditional();

        assert!(cfgs.for_package("anything").holds_str("windows"));
    }
}
