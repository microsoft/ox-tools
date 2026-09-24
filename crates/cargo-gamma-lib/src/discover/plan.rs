// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Everything a run needs, worked out before any building happens.

use camino::{Utf8Path, Utf8PathBuf};

use super::target_file::TargetFile;
use crate::model::Mutant;
use crate::suppress::Idle;
use crate::{HashMap, HashSet};

/// Everything a run needs, worked out before any building happens.
#[derive(Debug)]
pub struct Plan {
    /// Absolute path of the workspace root.
    pub root: Utf8PathBuf,

    /// The files that were analyzed.
    pub files: Vec<TargetFile>,

    /// Every mutant to be tested, with ordinals assigned.
    pub mutants: Vec<Mutant>,

    /// Mutants suppressed by an explicit policy. They stay in `mutants`, marked as ignored.
    pub suppressed: usize,

    /// Skip directives this run offered a mutant to and which suppressed none of it.
    ///
    /// See [`crate::suppress::idle`] for what "offered" excludes, and why the distinction matters.
    pub idle: Vec<Idle>,

    /// Mutants excluded by sharding, counted rather than kept.
    pub sharded_out: usize,

    /// Mutants an earlier report already settled, carried at their recorded verdict rather than
    /// run again.
    pub settled_out: usize,

    /// A digest of the normalized source each analyzed file held when its mutants were derived.
    ///
    /// A leading UTF-8 BOM is omitted just as it is during parsing, so source-edit generation
    /// checks compare the representation that supplied their line numbers.
    ///
    /// The commands that edit source apply line numbers this plan decided, and `suppress` applies
    /// them after a whole measured run — hours, on a real workspace. A line number means nothing
    /// against text it was not computed from, so the edit compares the file it is about to write
    /// against this and refuses rather than deleting or annotating the wrong line.
    pub digests: HashMap<Utf8PathBuf, String>,

    /// Files found under a mutable target that could not be analyzed, each a complete diagnostic.
    ///
    /// Carried on the plan rather than reported where it was noticed, because a scan happens once
    /// per package and the run assembles the plan from all of them; a message printed per package
    /// would arrive interleaved with build output and be read as a build problem.
    pub skipped: Vec<String>,

    /// For each workspace package, the workspace packages its test binaries can reach.
    ///
    /// A test binary can only exercise code it links, so a mutant in a package outside this set is
    /// unreachable from that binary no matter what the tests do. Running it anyway is pure cost.
    pub reach: HashMap<String, HashSet<String>>,

    /// For each workspace package, its manifest directory relative to the root, and its version.
    ///
    /// See [`Plan::spec`] for why a name on its own will not do.
    pub specs: HashMap<String, (Utf8PathBuf, String)>,
}

impl Plan {
    /// Names one package unambiguously, for a `--package` argument in a tree rooted at `root`.
    ///
    /// `--package serde` is ambiguous the moment a workspace member shares its name with a crate
    /// anywhere in the dependency graph, and a repository that dev-depends on a published version
    /// of itself does exactly that. Cargo rejects the whole invocation, and it rejects it before
    /// emitting any JSON, so a build that fails this way arrives with nothing to attribute and is
    /// indistinguishable from a tree that does not compile. Spelling the package as a path-rooted
    /// package ID makes the question unambiguous by construction, so it is asked that way always
    /// rather than only once cargo has complained.
    ///
    /// Falls back to the bare name for a package whose manifest was never located, which is no
    /// worse than what came before.
    #[must_use]
    pub fn spec(&self, root: &Utf8Path, package: &str) -> String {
        let Some((directory, version)) = self.specs.get(package) else {
            return package.to_owned();
        };

        let absolute = if directory.as_str().is_empty() {
            root.to_owned()
        } else {
            root.join(directory)
        };

        let normalized = absolute.as_str().replace('\\', "/");
        let root_slash = if normalized.starts_with('/') { "" } else { "/" };
        let source = format!("path+file://{root_slash}{normalized}");

        format!("{source}#{package}@{version}")
    }

    /// A package's directory, relative to the workspace root.
    #[must_use]
    pub fn directory_of(&self, package: &str) -> Option<&Utf8Path> {
        self.specs.get(package).map(|(directory, _version)| directory.as_path())
    }

    /// Folds one package's scan into the plan.
    ///
    /// A run scans a package, instruments it and builds it before moving to the next, so the plan
    /// is assembled a package at a time rather than handed over complete.
    pub fn absorb(&mut self, scanned: super::Scanned) {
        let files = self.source_indices();
        self.absorb_indexed(scanned, &files);
    }

    /// Indexes mutable source slots once for a package-by-package discovery sequence.
    pub(crate) fn source_indices(&self) -> HashMap<Utf8PathBuf, usize> {
        self.files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.path.clone(), index))
            .collect()
    }

    /// Folds one package's scan into the plan using a shared source-file index.
    pub(crate) fn absorb_indexed(&mut self, scanned: super::Scanned, files: &HashMap<Utf8PathBuf, usize>) {
        let super::Scanned {
            mutants,
            suppressed,
            idle,
            sharded_out,
            settled_out,
            skipped,
            digests,
            sources,
        } = scanned;

        self.mutants.extend(mutants);
        self.suppressed = self.suppressed.saturating_add(suppressed);
        self.idle.extend(idle);
        self.sharded_out = self.sharded_out.saturating_add(sharded_out);
        self.settled_out = self.settled_out.saturating_add(settled_out);
        self.skipped.extend(skipped);
        self.digests.extend(digests);
        for (path, source) in sources {
            if let Some(index) = files.get(&path)
                && let Some(file) = self.files.get_mut(*index)
            {
                file.source = Some(source);
            }
        }
    }

    /// Puts the mutants in report order, once every package has been absorbed.
    pub fn sort(&mut self) {
        self.mutants.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.span.start.cmp(&right.span.start))
                .then_with(|| left.mutator.cmp(&right.mutator))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn plan(specs: &[(&str, &str, &str)]) -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: specs
                .iter()
                .map(|(name, dir, version)| ((*name).to_owned(), (Utf8PathBuf::from(*dir), (*version).to_owned())))
                .collect(),
        }
    }

    #[test]
    fn a_package_is_named_by_where_its_manifest_is() {
        // Cargo rejects an ambiguous `--package` before it emits any JSON, so the build fails with
        // nothing to attribute and reads as though the tree does not compile. A path-rooted id
        // cannot be ambiguous.
        let plan = plan(&[("serde", "serde", "1.0.0")]);

        assert_eq!(plan.spec(Utf8Path::new("/tree"), "serde"), "path+file:///tree/serde#serde@1.0.0");
    }

    #[test]
    fn a_single_crate_repository_names_its_root() {
        // The manifest sits at the workspace root, so the relative directory is empty and joining
        // it blindly would produce the trailing separator that cargo does not match.
        let plan = plan(&[("itoa", "", "1.0.18")]);

        assert_eq!(plan.spec(Utf8Path::new("/tree"), "itoa"), "path+file:///tree#itoa@1.0.18");
    }

    #[test]
    fn the_spec_follows_the_tree_it_is_asked_about() {
        // A run builds in a scratch copy, not in the user's checkout, and a spec naming the wrong
        // root would select the wrong package or none at all.
        let plan = plan(&[("core", "crates/core", "0.2.0")]);

        assert_eq!(
            plan.spec(Utf8Path::new("/scratch/tree"), "core"),
            "path+file:///scratch/tree/crates/core#core@0.2.0"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_package_spec_is_a_file_url() {
        // Cargo package IDs are URLs even on Windows. Passing a native `D:\...` path makes Cargo
        // parse the drive letter as URL syntax and silently changes the package being named.
        let plan = plan(&[("core", "crates/core", "0.2.0")]);

        assert_eq!(
            plan.spec(Utf8Path::new(r"D:\scratch\tree"), "core"),
            "path+file:///D:/scratch/tree/crates/core#core@0.2.0"
        );
    }

    #[test]
    fn a_package_with_no_manifest_recorded_keeps_its_bare_name() {
        // Erring toward the old behaviour: a bare name builds the right thing whenever it is not
        // ambiguous, whereas a malformed id fails every time.
        let plan = plan(&[]);

        assert_eq!(plan.spec(Utf8Path::new("/tree"), "lonely"), "lonely");
    }

    #[test]
    fn absorbing_a_scan_conserves_every_count_and_collection() {
        let mut plan = plan(&[]);
        let mut mutant = fixtures::mutant();
        mutant.file = Utf8PathBuf::from("z.rs").into();
        let scanned = super::super::Scanned {
            mutants: vec![mutant],
            suppressed: 2,
            idle: Vec::new(),
            sharded_out: 3,
            settled_out: 4,
            skipped: vec!["src/broken.rs: could not parse".to_owned()],
            digests: std::iter::once((Utf8PathBuf::from("z.rs"), "digest".to_owned())).collect(),
            sources: HashMap::default(),
        };

        plan.absorb(scanned);

        assert_eq!(plan.mutants.len(), 1);
        assert_eq!(plan.suppressed, 2);
        assert_eq!(plan.sharded_out, 3);
        assert_eq!(plan.settled_out, 4);
        assert_eq!(plan.skipped, ["src/broken.rs: could not parse"]);
        assert_eq!(plan.digests[Utf8Path::new("z.rs")], "digest");
    }

    #[test]
    fn indexed_absorption_places_each_retained_source_directly() {
        let mut plan = plan(&[]);
        plan.files = ["a.rs", "b.rs"]
            .into_iter()
            .map(|path| TargetFile {
                path: Utf8PathBuf::from(path),
                absolute: Utf8PathBuf::from("/workspace").join(path),
                package: "subject".to_owned(),
                source: None,
            })
            .collect();
        let files = plan.source_indices();
        let scanned = super::super::Scanned {
            sources: [
                (Utf8PathBuf::from("b.rs"), "pub fn b() {}\n".to_owned()),
                (Utf8PathBuf::from("absent.rs"), "ignored".to_owned()),
            ]
            .into_iter()
            .collect(),
            ..super::super::Scanned::default()
        };

        plan.absorb_indexed(scanned, &files);

        assert_eq!(plan.files[0].source, None);
        assert_eq!(plan.files[1].source.as_deref(), Some("pub fn b() {}\n"));
    }

    #[test]
    fn report_order_uses_file_span_then_mutator() {
        let mut plan = plan(&[]);
        let make = |file: &str, start: usize, mutator: &str| {
            let mut mutant = fixtures::mutant();
            mutant.file = Utf8PathBuf::from(file).into();
            mutant.span = start..start + 1;
            mutant.mutator = mutator.to_owned().into();
            mutant
        };
        plan.mutants = vec![
            make("b.rs", 1, "z"),
            make("a.rs", 3, "z"),
            make("a.rs", 1, "z"),
            make("a.rs", 1, "a"),
        ];

        plan.sort();

        let order: Vec<(&str, usize, &str)> = plan
            .mutants
            .iter()
            .map(|mutant| (mutant.file.as_str(), mutant.span.start, mutant.mutator.as_ref()))
            .collect();
        assert_eq!(order, [("a.rs", 1, "a"), ("a.rs", 1, "z"), ("a.rs", 3, "z"), ("b.rs", 1, "z")]);
    }
}
