// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use camino::Utf8Path;
#[cfg(test)]
use camino::Utf8PathBuf;

use super::Mutant;

/// Shares repeated source and package strings across a coordinated mutant population.
#[derive(Debug, Default)]
pub struct Interner {
    source: cargo_gamma_engine::model::Interner,
}

impl Interner {
    pub fn text(&mut self, value: &str) -> Arc<str> {
        self.source.text(value)
    }

    pub fn path(&mut self, value: &Utf8Path) -> Arc<Utf8Path> {
        self.source.path(value)
    }

    pub fn share(&mut self, mutants: &mut [Mutant]) {
        for mutant in mutants {
            mutant.file = self.path(&mutant.file);
            mutant.package = self.text(&mutant.package);
            mutant.mutator = self.text(&mutant.mutator);
            mutant.item_path = self.text(&mutant.item_path);
            if let Some(trait_impl) = mutant.trait_impl.as_deref() {
                mutant.trait_impl = Some(self.text(trait_impl));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn sharing_a_decoded_population_leaves_its_values_alone() {
        let mut first = fixtures::mutant();
        let mut second = fixtures::mutant();

        first.file = Arc::from(Utf8PathBuf::from("a.rs"));
        second.file = Arc::from(Utf8PathBuf::from("a.rs"));
        let mut mutants = vec![first, second];

        assert!(!Arc::ptr_eq(&mutants[0].file, &mutants[1].file));

        Interner::default().share(&mut mutants);

        assert!(Arc::ptr_eq(&mutants[0].file, &mutants[1].file));
        assert!(Arc::ptr_eq(&mutants[0].mutator, &mutants[1].mutator));
        assert!(Arc::ptr_eq(&mutants[0].package, &mutants[1].package));
    }
}
