// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};

use super::MutantDefinition;
#[cfg(test)]
use super::MutationSite;
use crate::HashMap;

/// Hands out one shared copy of each distinct string a population repeats.
///
/// Three source fields are drawn from small closed sets — the file, the mutator name and the
/// enclosing item path — while a run produces hundreds of thousands of mutations. Owned per
/// mutation, those three are hundreds of thousands of heap copies of a few thousand distinct
/// values; shared, they are one allocation each and a pointer per mutant.
///
/// Sharing happens in two places because it can only ever collapse what the holder can see. A file
/// shares its own strings as it produces its mutants, and [`Interner::share`] collapses what
/// repeats *between* files once the whole population exists.
#[derive(Debug, Default)]
pub struct Interner {
    /// The shared copy of each distinct string handed out so far.
    ///
    /// Keyed through the crate's `FxHashMap`, not the standard `SipHash` map: the keys are the
    /// tool's own mutator names and item paths — bounded and non-adversarial — so the
    /// DoS-resistant hash the standard map defaults to buys nothing on a per-mutant path.
    texts: HashMap<String, Arc<str>>,

    /// The shared copy of each distinct path handed out so far, keyed the same way and for the same
    /// reason as [`Self::texts`].
    paths: HashMap<Utf8PathBuf, Arc<Utf8Path>>,
}

impl Interner {
    /// The shared copy of a string, creating it the first time the value is seen.
    pub fn text(&mut self, value: &str) -> Arc<str> {
        if let Some(shared) = self.texts.get(value) {
            return Arc::clone(shared);
        }

        let shared: Arc<str> = Arc::from(value);
        let _stored = self.texts.insert(value.to_owned(), Arc::clone(&shared));

        shared
    }

    /// The shared copy of a path, creating it the first time the value is seen.
    pub fn path(&mut self, value: &Utf8Path) -> Arc<Utf8Path> {
        if let Some(shared) = self.paths.get(value) {
            return Arc::clone(shared);
        }

        let shared: Arc<Utf8Path> = Arc::from(value);
        let _stored = self.paths.insert(value.to_owned(), Arc::clone(&shared));

        shared
    }

    /// Collapses a population's repeated strings onto one shared copy each.
    ///
    /// Used after decoding a report, where every mutant arrived with its own copies. The values are
    /// unchanged — only how many allocations hold them.
    pub fn share(&mut self, mutations: &mut [MutantDefinition]) {
        for mutation in mutations {
            mutation.file = self.path(&mutation.file);
            mutation.mutator = self.text(&mutation.mutator);
            mutation.item_path = self.text(&mutation.item_path);
            if let Some(trait_impl) = mutation.trait_impl.as_deref() {
                mutation.trait_impl = Some(self.text(trait_impl));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MutantId;
    use crate::ops::collect::Shape;

    /// Two mutants naming the same file must end up pointing at one string, not two equal ones.
    #[test]
    fn a_repeated_value_is_handed_out_as_the_same_allocation() {
        let mut interner = Interner::default();

        let first = interner.text("arith.add_to_sub");
        let second = interner.text("arith.add_to_sub");
        let other = interner.text("arith.add_to_mul");

        assert!(Arc::ptr_eq(&first, &second), "the same value came back as a second allocation");
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(&*other, "arith.add_to_mul");

        let path = interner.path(Utf8Path::new("src/lib.rs"));
        let again = interner.path(Utf8Path::new("src/lib.rs"));

        assert!(Arc::ptr_eq(&path, &again));
        assert_eq!(path.as_str(), "src/lib.rs");
    }

    /// A decoded report arrives with a copy per mutant, which is exactly what sharing undoes.
    #[test]
    fn sharing_a_decoded_population_leaves_its_values_alone() {
        let sample = |name: &str| MutantDefinition {
            id: MutantId::new("deadbeefcafe"),
            file: Arc::from(Utf8Path::new(name)),
            site: Arc::new(MutationSite {
                span: 0..1,
                line: 1,
                end_line: 1,
                column: 1,
                original: "true".to_owned().into(),
            }),
            mutator: Arc::from("lit.true_to_false"),
            item_path: Arc::from("subject::f"),
            trait_impl: None,
            occurrence: 0,
            replacement_index: 0,
            replacement: "false".to_owned().into(),
            shape: Shape::Expr,
        };

        let mut mutations = vec![sample("a.rs"), sample("a.rs")];

        assert!(
            !Arc::ptr_eq(&mutations[0].file, &mutations[1].file),
            "the fixture must start unshared"
        );

        let before: Vec<String> = mutations.iter().map(|m| format!("{} {}", m.file, m.mutator)).collect();

        Interner::default().share(&mut mutations);

        let after: Vec<String> = mutations.iter().map(|m| format!("{} {}", m.file, m.mutator)).collect();

        assert_eq!(before, after, "sharing changed a value");
        assert!(Arc::ptr_eq(&mutations[0].file, &mutations[1].file));
        assert!(Arc::ptr_eq(&mutations[0].mutator, &mutations[1].mutator));
    }
}
