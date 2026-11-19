// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded, run-local reuse of definitive proofs under a complete compilation identity.

use super::{BuildScope, HashMap, HashSet, Plan, Proofs, Workspace};

#[cfg(test)]
#[derive(Clone, Copy)]
enum Identity {
    Live,
    Scripted(Option<[u8; 32]>),
}

#[cfg(test)]
thread_local! {
    static IDENTITY: std::cell::Cell<Identity> = const { std::cell::Cell::new(Identity::Live) };
}

/// Returns the complete compilation-context identity, or `None` when it cannot be sealed.
pub(super) fn identity(work: &Workspace, plan: &Plan) -> Option<[u8; 32]> {
    #[cfg(test)]
    if let Identity::Scripted(identity) = IDENTITY.with(std::cell::Cell::get) {
        return identity;
    }
    work.proof_context_identity(plan)
}

#[cfg(test)]
pub(super) fn with_identity<T>(identity: Option<[u8; 32]>, operation: impl FnOnce() -> T) -> T {
    struct Restore(Identity);
    impl Drop for Restore {
        fn drop(&mut self) {
            IDENTITY.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(IDENTITY.with(|slot| slot.replace(Identity::Scripted(identity))));
    operation()
}

/// Definitive isolation results retained across failed convergence rounds.
///
/// `true` means the selected schema failed compilation; `false` means it compiled.
/// Inconclusive results are never retained.
#[derive(Debug, Default)]
pub struct ProofCache {
    results: HashMap<[u8; 32], bool>,
}

impl ProofCache {
    /// Returns a retained verdict, or `None` for an unavailable key or cache miss.
    pub(super) fn get(&self, key: Option<[u8; 32]>) -> Option<bool> {
        self.results.get(&key?).copied()
    }

    /// Retains a definitive verdict; an unavailable key or inconclusive verdict is a no-op.
    pub(super) fn insert(&mut self, key: Option<[u8; 32]>, verdict: Option<bool>) {
        if let (Some(key), Some(verdict)) = (key, verdict) {
            // Reuse is optional; the existing invocation launch bound also caps retained entries.
            if self.results.len() >= super::MAX_INVOCATION_ISOLATION_PROOFS {
                self.results.clear();
            }
            let _previous = self.results.insert(key, verdict);
        }
    }
}

fn field(hasher: &mut blake3::Hasher, value: &impl serde::Serialize) -> Option<()> {
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "All callers serialize derived DTOs containing UTF-8 paths, strings, integers, byte arrays, and string-keyed maps; none has a fallible custom serializer.")]
    let bytes = serde_json::to_vec(value).ok()?;
    let _hasher = hasher.update(&(bytes.len() as u64).to_le_bytes()).update(&bytes);
    Some(())
}

/// Fingerprints the frozen source/mutation generation, or returns `None` for missing
/// source/digests, mismatched source bytes, or mutations outside the validated files.
pub(super) fn generation(plan: &Plan) -> Option<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Serializing a UTF-8 path to a JSON byte vector cannot fail.")]
    field(&mut hasher, &plan.root)?;
    let mut files = plan.files.iter().collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let mut validated = HashSet::default();
    for file in files {
        let source = file.source.as_deref()?;
        let digest = plan.digests.get(&file.path)?;
        let normalized = source.strip_prefix('\u{feff}').unwrap_or(source);
        if blake3::hash(normalized.as_bytes()).to_hex().as_str() != digest {
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "This tuple contains only a UTF-8 path, strings, and a boolean, whose JSON serialization cannot fail.")]
        field(&mut hasher, &(&file.path, &file.package, digest, source.starts_with('\u{feff}')))?;
        let _inserted = validated.insert(file.path.as_path());
    }
    let mut mutants = plan.mutants.iter().collect::<Vec<_>>();
    mutants.sort_unstable_by_key(|mutant| mutant.ordinal);
    for mutant in mutants {
        if !validated.contains(mutant.file.as_ref()) {
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "These derived mutant fields contain only integers, UTF-8 strings and paths, a range, and an enum; JSON serialization cannot fail.")]
        field(
            &mut hasher,
            &(
                mutant.ordinal,
                &mutant.id,
                &mutant.file,
                &mutant.package,
                &mutant.span,
                &mutant.replacement,
                &mutant.shape,
            ),
        )?;
    }
    let mut specs = plan.specs.iter().collect::<Vec<_>>();
    specs.sort_unstable_by_key(|(package, _spec)| *package);
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Package specs are derived DTOs containing strings and vectors, without a fallible custom serializer.")]
    field(&mut hasher, &specs)?;
    Some(*hasher.finalize().as_bytes())
}

/// Identifies a proof under its frozen generation, compilation context, Cargo scope and verb.
///
/// `background` contains guards left instrumented outside the candidate partition;
/// `active` is the sorted candidate-ordinal list added to that background for this proof.
/// `None` means the generation or compilation identity is unavailable, so reuse is disabled.
pub(super) fn key(
    proofs: &Proofs,
    compilation: Option<[u8; 32]>,
    scope: BuildScope<'_>,
    verb: &[&str],
    background: &HashSet<u32>,
    active: &[u32],
) -> Option<[u8; 32]> {
    let generation = proofs.reuse_generation?;
    let compilation = compilation?;
    let mut hasher = blake3::Hasher::new();
    let mut background = background.iter().copied().collect::<Vec<_>>();
    background.sort_unstable();
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "This tuple contains only byte arrays, integer sets and slices, and strings; JSON serialization cannot fail.")]
    field(
        &mut hasher,
        &(generation, compilation, scope.roots, scope.mutants, verb, background, active),
    )?;
    Some(*hasher.finalize().as_bytes())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn proof_key(generation: u8, compilation: u8, background: &[u32], active: &[u32]) -> Option<[u8; 32]> {
        key(
            &Proofs {
                reuse_generation: Some([generation; 32]),
                ..Proofs::default()
            },
            Some([compilation; 32]),
            BuildScope {
                roots: None,
                mutants: None,
                publish_progress: false,
            },
            &["check", "--tests", "--keep-going"],
            &background.iter().copied().collect(),
            active,
        )
    }

    #[test]
    fn reuse_requires_identical_source_compilation_and_complete_schema() {
        let original = proof_key(1, 2, &[3], &[1, 2]);
        let mut cache = ProofCache::default();
        cache.insert(original, Some(true));
        assert_eq!(cache.get(original), Some(true));
        for key in [
            proof_key(2, 2, &[3], &[1, 2]),
            proof_key(1, 3, &[3], &[1, 2]),
            proof_key(1, 2, &[4], &[1, 2]),
            proof_key(1, 2, &[3], &[1]),
            None,
        ] {
            assert_eq!(cache.get(key), None);
        }
        cache.insert(original, None);
        assert_eq!(cache.get(original), Some(true));
    }

    #[test]
    fn changing_roots_target_verb_or_attribution_scope_cannot_reuse_a_proof() {
        let proofs = Proofs {
            reuse_generation: Some([1; 32]),
            ..Proofs::default()
        };
        let background = HashSet::default();
        let roots = vec!["one".to_owned()];
        let mutants = vec!["two".to_owned()];
        let original = proof_key(1, 2, &[], &[1]);
        for (roots, mutants, verb) in [
            (Some(roots.as_slice()), None, vec!["check", "--tests", "--keep-going"]),
            (None, Some(mutants.as_slice()), vec!["check", "--tests", "--keep-going"]),
            (None, None, vec!["check", "--lib"]),
        ] {
            assert_ne!(
                key(
                    &proofs,
                    Some([2; 32]),
                    BuildScope {
                        roots,
                        mutants,
                        publish_progress: false
                    },
                    &verb,
                    &background,
                    &[1]
                ),
                original
            );
        }
    }

    #[test]
    fn incomplete_results_are_not_promoted_and_reuse_storage_is_bounded() {
        let mut cache = ProofCache::default();
        let key = proof_key(1, 2, &[], &[1]);
        cache.insert(key, None);
        cache.insert(None, Some(false));
        assert_eq!(cache.get(key), None);
        for index in 0..=super::super::MAX_INVOCATION_ISOLATION_PROOFS {
            let key = blake3::hash(&index.to_le_bytes());
            cache.insert(Some(*key.as_bytes()), Some(false));
        }
        assert_eq!(cache.results.len(), 1);
    }

    #[test]
    fn unrelated_proofs_remain_reusable_until_capacity_eviction() {
        let mut cache = ProofCache::default();
        let first = proof_key(1, 2, &[], &[1]);
        let second = proof_key(1, 2, &[], &[2]);
        cache.insert(first, Some(true));
        cache.insert(second, Some(false));
        assert_eq!(cache.get(first), Some(true));
        assert_eq!(cache.get(second), Some(false));
        cache.insert(first, None);
        assert_eq!(cache.get(first), Some(true));
    }

    #[test]
    fn source_identity_normalizes_only_the_leading_bom_and_is_order_independent() {
        use camino::{Utf8Path, Utf8PathBuf};

        use crate::discover::TargetFile;

        let mut plan = super::super::tests::plan(Utf8Path::new("source-identity"), 2);
        let source = "fn value() -> u32 { 1 }";
        for (index, path) in ["src/lib.rs", "src/other.rs"].into_iter().enumerate() {
            let path = Utf8PathBuf::from(path);
            plan.mutants[index].file = path.clone().into();
            let _previous = plan.digests.insert(path.clone(), crate::discover::digest(source.as_bytes()));
            plan.files.push(TargetFile {
                absolute: plan.root.join(&path),
                path,
                package: "subject".into(),
                source: Some(source.into()),
            });
        }
        for index in 0..12 {
            let name = format!("package-{index}");
            let _previous = plan.specs.insert(name.clone(), (plan.root.join(&name), format!("{name}@1.0.0")));
        }
        let original = generation(&plan).unwrap();
        let mut reordered = plan.clone();
        reordered.files.reverse();
        reordered.mutants.reverse();
        let mut specs = reordered.specs.into_iter().collect::<Vec<_>>();
        specs.reverse();
        reordered.specs = specs.into_iter().collect();
        assert_eq!(generation(&reordered), Some(original));

        let mut changed = plan.clone();
        changed.files[0].source = Some(format!("\u{feff}{source}"));
        let with_bom = generation(&changed).expect("the leading BOM does not change the normalized source digest");
        assert_ne!(with_bom, original, "BOM presence remains part of the source generation");
        changed.files[0].source = Some(format!("{source}\u{feff}"));
        assert!(
            generation(&changed).is_none(),
            "a trailing BOM cannot be stripped as an encoding marker"
        );
        changed.files[0].source = Some(format!("x{source}"));
        assert!(generation(&changed).is_none(), "an ordinary leading character cannot be stripped");
        let _previous = plan
            .specs
            .insert("package-0".into(), (plan.root.join("different"), "package-0@2.0.0".into()));
        assert_ne!(generation(&plan), Some(original));
    }

    #[test]
    fn proof_identity_canonicalizes_background_populations() {
        let forward = (1..=32).collect::<Vec<_>>();
        let reverse = forward.iter().rev().copied().collect::<Vec<_>>();
        assert_eq!(proof_key(1, 2, &forward, &[40]), proof_key(1, 2, &reverse, &[40]));
        assert_ne!(proof_key(1, 2, &forward, &[40]), proof_key(1, 2, &forward[..31], &[40]));
    }

    #[test]
    fn source_generation_covers_ordinal_mapping_edits_and_missing_source_evidence() {
        use camino::{Utf8Path, Utf8PathBuf};

        let root = Utf8Path::new("memory-proof-cache");
        let mut plan = super::super::tests::plan(root, 2);
        let path = Utf8PathBuf::from("src/lib.rs");
        let source = "fn value() -> u32 { 1 }";
        plan.files.push(crate::discover::TargetFile {
            path: path.clone(),
            absolute: root.join(&path),
            package: "subject".into(),
            source: Some(source.into()),
        });
        let _old = plan.digests.insert(path.clone(), crate::discover::digest(source.as_bytes()));
        let original = generation(&plan).unwrap();
        let mut changed = plan.clone();
        changed.mutants[0].replacement = "42u32".into();
        assert_ne!(generation(&changed), Some(original));
        changed = plan.clone();
        changed.mutants.swap(0, 1);
        assert_eq!(generation(&changed), Some(original));
        changed.mutants[0].ordinal = 3;
        assert_ne!(generation(&changed), Some(original));
        changed = plan.clone();
        changed.files[0].source = None;
        assert_eq!(generation(&changed), None);
        changed = plan.clone();
        changed.files[0].source = Some("fn value() -> u32 { 2 }".into());
        assert_eq!(generation(&changed), None);
        let _old = changed.digests.insert(path, crate::discover::digest(b"fn value() -> u32 { 2 }"));
        assert_ne!(generation(&changed), Some(original));
        changed = plan.clone();
        changed.files.clear();
        assert_eq!(generation(&changed), None);
        plan.digests.clear();
        assert_eq!(generation(&plan), None);
    }
}
