// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Walking the workspace to decide which files to parse and which mutants to plan.

use core::iter::once;
use core::mem;
use core::num::NonZero;
use core::panic::AssertUnwindSafe;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::ffi::OsStr;
use std::panic::{catch_unwind, resume_unwind};
use std::sync::{Barrier, Mutex, OnceLock, PoisonError};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{CargoOpt, Metadata, MetadataCommand, Target, TargetKind};
use syn::visit::{self, Visit};
use walkdir::WalkDir;

use super::compile_fail::{CompileFailTarget, compile_fail_targets};
use super::glob::{Glob, normalize_separators};
use super::killers::Killers;
use super::shard::shard_of;
use super::{Diff, Plan, TargetFile, modules};
use crate::cfg::{CfgSet, Cfgs, features};
use crate::commands::{FeatureArgs, SelectArgs};
use crate::error::{Error, error};
use crate::exec::CargoOptions;
use crate::model::{Channel, Interner, Mutant, MutantId, Outcome, Suppression};
use crate::ops::collect;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;
use crate::{HashMap, HashSet, Result, suppress};

/// Builds the plan for a run.
///
/// `notify` is called with a short human-readable message, so the caller can drive a progress
/// display without this module knowing anything about terminals. It is called once with the size
/// of the job before the files are parsed, and once per package afterwards carrying what that
/// package actually yielded — the counts do not exist until the parse is done.
#[cfg(test)]
pub fn plan(args: &SelectArgs, selection: &Selection, shard: Option<(u32, u32)>, notify: &mut impl FnMut(&str)) -> Result<Plan> {
    let survey = Survey::new(args, shard)?;

    plan_survey(survey, selection, notify)
}

/// Builds a plan for the Cargo options the caller has already resolved.
///
/// Commands that have merged `gamma.toml` into their run arguments pass the same options
/// here for every discovery pass. Re-reading the file after an edit has begun could mix the first
/// pass's plan with a later configuration generation.
pub(crate) fn plan_for_build(
    args: &SelectArgs,
    selection: &Selection,
    shard: Option<(u32, u32)>,
    cargo: &CargoOptions,
    notify: &mut impl FnMut(&str),
) -> Result<Plan> {
    let survey = Survey::for_build(args, shard, cargo)?;

    plan_survey(survey, selection, notify)
}

/// Scans one resolved survey into a plan.
fn plan_survey(survey: Survey, selection: &Selection, notify: &mut impl FnMut(&str)) -> Result<Plan> {
    // Parsing is the expensive half of discovery and says nothing while it runs, so its size is
    // announced before it starts rather than leaving the display silent.
    notify(&format!("{} for mutants", crate::report::quantity(survey.files.len(), "file")));

    let mut ordinals = 0;
    let scanned = survey.scan(None, selection, &mut ordinals)?;

    report_by_package(&survey.files, &scanned.mutants, notify);

    Ok(survey.into_plan(scanned))
}

/// The workspace, its files and its shape, worked out without parsing a line of source.
///
/// Discovery divides in two. Working out which files are worth mutating costs a cargo metadata
/// call and a directory walk; parsing them costs far more. Splitting the two lets a run copy the
/// workspace and then scan, instrument and build one package at a time, rather than parsing
/// everything before anything else can start.
#[derive(Debug)]
pub struct Survey {
    /// Absolute path of the workspace root.
    pub root: Utf8PathBuf,

    /// Every file worth mutating, sorted by path.
    pub files: Vec<TargetFile>,

    /// Every mutable source file walked for each selected package, including files excluded from
    /// mutation by `--file` or `--in-diff`.
    ///
    /// Module declarations in an excluded parent can still prove that an included child exists
    /// only under `#[cfg(test)]`, so the declaration graph must be wider than the population.
    declaration_files: HashMap<String, Vec<Utf8PathBuf>>,

    /// For each workspace package, the workspace packages its test binaries can reach.
    pub reach: HashMap<String, HashSet<String>>,

    /// The packages cargo itself would act on here, sorted.
    ///
    /// This is the whole of `--package`, `--workspace` and the invocation directory resolved to a
    /// package list, and it is deliberately wider than [`Self::packages`]: a `--file` or
    /// `--in-diff` filter narrows what gets mutated without narrowing what cargo would build and
    /// test. It is the ceiling on the oracle, so it has to be the set cargo would select rather
    /// than the set that turned out to hold mutants, or
    /// mutating one file would quietly withdraw the rest of the package's own tests.
    pub selected: Vec<String>,

    /// Every test target the workspace declares, by name, sorted and deduplicated.
    ///
    /// This is the population `--include-test` and `--exclude-test` are checked against, and it is
    /// deliberately wider than the set of binaries a given run builds. A target gated behind
    /// `required-features` is declared here and compiled only when those features are on, so
    /// checking against what was built would reject a pattern in `gamma.toml` on every run that
    /// did not happen to enable them — which is precisely the run the pattern exists to survive.
    /// Collected across every workspace member, since `--package` chooses what to mutate while
    /// these patterns choose what judges it.
    pub tests: Vec<String>,

    /// Every test target that appears to run the compiler rather than the code under test.
    ///
    /// Kept beside `tests` because it is a property of what the workspace declares rather than of
    /// what this run happens to build, and because the run that most needs to hear about one is the
    /// run that has not yet paid for it.
    pub compile_fail: Vec<CompileFailTarget>,

    /// For each package, the positions in `files` that belong to it, in `files` order.
    ///
    /// A real run never scans the whole workspace at once — it scans a package at a time, across
    /// every dependency stage — so selecting a package's files by filtering the flat list would
    /// walk every file in the workspace once per package. That term is packages times files, which
    /// is invisible on a small tree and quadratic on a monorepo.
    ///
    /// Positions rather than a second copy of the files, and positions in `files` order rather than
    /// any order of their own, because the deterministic path order is what makes two scans of the
    /// same workspace produce the same population.
    by_package: HashMap<String, Vec<usize>>,

    /// For each package, its crate roots — the lib and bin entry points the module tree hangs off.
    roots: HashMap<String, Vec<Utf8PathBuf>>,

    /// For each package, where its manifest sits relative to the workspace root, and its version.
    ///
    /// A bare `--package name` is ambiguous whenever a workspace member shares its name with a
    /// crate in the dependency graph, which happens routinely: a crate that dev-depends on a
    /// published version of itself, or two members of a graph that both vendor a common name.
    /// Cargo then refuses the build, and refuses it *before* producing any JSON, so the failure
    /// arrives with no diagnostics to attribute and looks like the tree simply not compiling.
    /// Keeping the manifest location lets every build name its packages exactly.
    specs: HashMap<String, (Utf8PathBuf, String)>,

    /// For each package, the configuration predicates that hold when it is built.
    ///
    /// Code the compiler will strip produces no mutants, because a guard there is never compiled
    /// and no test could activate it. Resolved once here rather than per file, since a `rustc`
    /// call and a feature closure per source file would dominate discovery.
    cfgs: Cfgs,

    /// Every directory a workspace target has sources in, deduplicated and sorted.
    ///
    /// Wider than [`Self::files`] on purpose: it includes the targets this run will never mutate,
    /// integration tests above all, because that is where a great many of the tests that convict
    /// mutants actually live. Only [`Self::killers`] reads it, and only under `--incremental`.
    pub(super) source_dirs: Vec<Utf8PathBuf>,

    /// Local path dependencies Cargo can read outside the workspace root.
    ///
    /// They are not mutation candidates, but their bytes can change what a workspace package
    /// compiles or what its tests observe. The record snapshot captures them separately.
    external_inputs: Vec<Utf8PathBuf>,

    /// Whether a build script may have read an external path Cargo does not report.
    untracked_build_script_inputs: bool,

    diff: Option<Diff>,
    shard: Option<(u32, u32)>,
    settled: HashMap<MutantId, Outcome>,
    exclude_trait_impls: Vec<String>,
}

/// What scanning some part of the workspace yielded.
#[derive(Debug, Default)]
pub struct Scanned {
    /// The mutants found, with ordinals already assigned to the live ones.
    pub mutants: Vec<Mutant>,

    /// How many were suppressed by an explicit policy. They stay in `mutants`, marked as ignored.
    pub suppressed: usize,

    /// The skip directives that suppressed nothing, though this run offered them the chance.
    pub idle: Vec<suppress::Idle>,

    /// How many live mutants sharding excluded. These are counted rather than kept.
    pub sharded_out: usize,

    /// How many mutants an earlier report had already settled.
    ///
    /// They are kept, carrying the verdict that report gave them, so the score is over the whole
    /// population rather than over the part of it this run happened to retry.
    pub settled_out: usize,

    /// A digest of the normalized source each analyzed file held when its mutants were derived.
    ///
    /// A leading UTF-8 BOM is omitted just as it is during parsing, so source-edit generation
    /// checks compare the representation that supplied their line numbers.
    ///
    /// A command that edits source works from line numbers this scan decided, and applies them
    /// later — for `suppress`, after a whole measured run, which can be hours. Whoever owns the
    /// tree may have written to it in between, and a line number means nothing against text it was
    /// not computed from. This is what lets the edit refuse rather than delete the wrong line.
    pub digests: HashMap<Utf8PathBuf, String>,

    /// Files that were found but could not be analyzed, each already a complete diagnostic.
    ///
    /// These contributed no mutants, so they are missing from both halves of the score's fraction
    /// and the score is silently a claim about less code than the caller asked about. Reporting
    /// them is not optional: a skipped file that nobody mentions is indistinguishable from a file
    /// with nothing worth mutating in it.
    pub skipped: Vec<String>,
}

impl Survey {
    /// Finds the workspace and the files worth mutating, without parsing any of them.
    ///
    /// The build this describes is the one the configuration file and the environment ask for.
    /// A caller that has already settled the run's cargo options — the run itself does, from
    /// flags the selection arguments do not carry — should use [`Survey::for_build`] instead, so
    /// that the predicates discovery evaluates are the ones the compiler will.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration or cargo metadata cannot be read, a named package does
    /// not exist, or the diff cannot be parsed.
    #[cfg(test)]
    pub fn new(args: &SelectArgs, shard: Option<(u32, u32)>) -> Result<Self> {
        let config = crate::config::Config::resolve(args)?;
        let cargo = config.cargo_options();

        Self::for_build(args, shard, &cargo)
    }

    /// Finds the files worth mutating for a build with these cargo options.
    ///
    /// Discovery and the build have to agree about what is compiled: a file surveyed under one
    /// target, profile or set of `--cfg` flags and compiled under another produces guards in code
    /// the compiler never sees, and drops mutants from code it does. The options the build will
    /// use are therefore what the configuration predicates are derived from; see
    /// [`CargoOptions::cfg_build`].
    ///
    /// The feature selection is read from both places it can be written. A selector passed through
    /// `-C`/`--cargo-arg`, or configured as a `gamma.toml` `cargo_args` entry, reaches the cargo
    /// this run invokes just as gamma's own `--features` does, so the closure and the metadata are
    /// resolved under the union of the two; see [`features::from_extra`].
    ///
    /// # Errors
    ///
    /// Returns an error if cargo metadata cannot be read, a named package does not exist, or the
    /// diff cannot be parsed.
    pub fn for_build(args: &SelectArgs, shard: Option<(u32, u32)>, cargo: &CargoOptions) -> Result<Self> {
        Self::for_build_with_cache_inputs(args, shard, cargo, false)
    }

    #[expect(clippy::too_many_lines, reason = "workspace selection is one ordered metadata pass")]
    pub(crate) fn for_build_with_cache_inputs(
        args: &SelectArgs,
        shard: Option<(u32, u32)>,
        cargo: &CargoOptions,
        cache_inputs: bool,
    ) -> Result<Self> {
        cargo.validate()?;

        let features = features::from_extra(&args.features, &cargo.extra);
        let metadata = load_metadata(&args.dir, &features)?;
        let root = Utf8PathBuf::from(metadata.workspace_root.as_str());
        let external_inputs = if cache_inputs {
            external_path_inputs(&args.dir, &features, &root)?
        } else {
            ExternalPathInputs::default()
        };
        let enabled = features::enabled(&metadata, &features);
        let mut files: Vec<TargetFile> = Vec::new();
        let mut seen: HashSet<Utf8PathBuf> = HashSet::default();
        let mut exclusion_files: Vec<TargetFile> = Vec::new();
        let mut exclusion_seen: HashSet<(Utf8PathBuf, String)> = HashSet::default();
        let mut declaration_files: HashMap<String, Vec<Utf8PathBuf>> = HashMap::default();
        let mut declaration_seen: HashSet<Utf8PathBuf> = HashSet::default();
        let mut roots: HashMap<String, Vec<Utf8PathBuf>> = HashMap::default();
        let mut specs: HashMap<String, (Utf8PathBuf, String)> = HashMap::default();
        let mut source_dirs: HashSet<Utf8PathBuf> = HashSet::default();

        let mut diff = args.in_diff.as_ref().map(|path| Diff::read(path)).transpose()?;
        let patterns = FilePatterns::new(args);

        // Only collected when there is something to check, since it is one string per Rust file in
        // the workspace and the overwhelmingly common case has no patterns at all. A diff needs the
        // same list, to say which workspace file each path it names refers to.
        let checking_patterns = !args.files.is_empty() || !args.exclude_files.is_empty();
        let checking_exclusions = !args.exclude_trait_impls.is_empty();
        let collecting_walked = checking_patterns || diff.is_some();
        let mut walked: Vec<Utf8PathBuf> = Vec::new();

        if let Some(named) = unknown_packages(&metadata, args) {
            return Err(error!("no package named `{named}` in this workspace").usage());
        }

        let selected = selected_packages(&metadata, args);

        for package in metadata.workspace_packages() {
            let mutating = selected.contains(package.name.as_str());

            if let Some(directory) = Utf8Path::new(package.manifest_path.as_str()).parent() {
                let relative = directory
                    .strip_prefix(&root)
                    .unwrap_or_else(|_outside| Utf8Path::new(""))
                    .to_owned();
                let _replaced = specs.insert(package.name.to_string(), (relative, package.version.to_string()));
            }

            // A package this run does not mutate is still walked when patterns need checking. The
            // patterns usually live in `gamma.toml` and are written once for the whole workspace,
            // whereas `--package` narrows a single run; validating them against the narrowed set
            // would reject a correct config on every run that happened to select another package.
            if !mutating && !checking_patterns && !checking_exclusions {
                continue;
            }

            for target in &package.targets {
                // Before the mutability filter, so that the tests which judge this workspace are
                // indexed whether or not their own target is one this run would mutate.
                if let Some(directory) = Utf8Path::new(target.src_path.as_str()).parent() {
                    let _added = source_dirs.insert(directory.to_owned());
                }

                if !is_mutable_target(target, enabled.get(package.name.as_str())) {
                    continue;
                }

                let source_root = Utf8Path::new(target.src_path.as_str());

                // The module tree is walked from here to find the files that exist only for tests,
                // so a root is recorded whether or not it survives the filters below: a crate root
                // excluded from mutation still says what the rest of the crate is.
                if mutating {
                    roots.entry(package.name.to_string()).or_default().push(source_root.to_owned());
                }

                let Some(directory) = source_root.parent() else {
                    continue;
                };

                for absolute in walk_rust_files(directory)? {
                    let relative = placed_under(&root, &absolute, &package.name)?;

                    if checking_exclusions && exclusion_seen.insert((absolute.clone(), package.name.to_string())) {
                        exclusion_files.push(TargetFile {
                            path: relative.clone(),
                            absolute: absolute.clone(),
                            package: package.name.to_string(),
                        });
                    }

                    if collecting_walked {
                        walked.push(relative.clone());
                    }

                    if !mutating {
                        continue;
                    }

                    if declaration_seen.insert(absolute.clone()) {
                        declaration_files
                            .entry(package.name.to_string())
                            .or_default()
                            .push(absolute.clone());
                    }

                    if !patterns.includes(&relative) {
                        continue;
                    }

                    // A lib and a bin target in one package usually share a source directory, so
                    // the same file is walked more than once. A set rather than a scan of what is
                    // already held, because a large workspace makes that scan quadratic.
                    if !seen.insert(absolute.clone()) {
                        continue;
                    }

                    files.push(TargetFile {
                        path: relative,
                        absolute,
                        package: package.name.to_string(),
                    });
                }
            }
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));
        for paths in declaration_files.values_mut() {
            paths.sort();
        }
        exclusion_files.sort_by(|left, right| left.path.cmp(&right.path));

        let cfgs = configuration(&root, cargo, &enabled);
        validate_trait_exclusions(&args.exclude_trait_impls, &exclusion_files, &cfgs)?;

        // The diff names files; only the workspace can say which files those are. Resolving it
        // needs the walk to have happened, and it fails rather than selects nothing when not one
        // of the paths it names turns out to be a file here.
        if let Some(diff) = diff.as_mut() {
            diff.resolve(&root, &walked)?;

            // A file the diff does not mention cannot contain a changed line, so it is dropped
            // before it is parsed rather than after, which is most of what makes `--in-diff` fast
            // enough to run on every pull request.
            files.retain(|file| diff.touches_file(&file.path));
        }

        if let Some(pattern) = patterns.unmatched(&walked) {
            return Err(error!(
                "no source file matches `{pattern}`; patterns are relative to the workspace root and use `/` on every platform"
            )
            .usage());
        }

        let mut by_package: HashMap<String, Vec<usize>> = HashMap::default();

        for (position, file) in files.iter().enumerate() {
            by_package.entry(file.package.clone()).or_default().push(position);
        }

        Ok(Self {
            root,
            files,
            declaration_files,
            by_package,
            reach: reachable(&metadata),
            selected: sorted(selected),
            tests: test_targets(&metadata),
            compile_fail: compile_fail_targets(&metadata),
            roots,
            specs,
            cfgs,
            diff,
            shard,
            settled: HashMap::default(),
            exclude_trait_impls: args.exclude_trait_impls.clone(),
            source_dirs: sorted(source_dirs),
            external_inputs: external_inputs.roots,
            untracked_build_script_inputs: external_inputs.has_build_scripts,
        })
    }

    /// Indexes the test functions this workspace declares right now.
    ///
    /// Only worth building for a run that is about to carry an earlier report's kills forward; see
    /// [`Killers`] for why the names are read out of the sources rather than out of a harness.
    ///
    /// A directory that cannot be walked contributes nothing here rather than failing the run,
    /// which is the opposite of what the same walk does when it builds the population — and for
    /// the opposite reason. A file missing from *this* index can only make a recorded kill
    /// unconfirmable, and an unconfirmed kill is re-run rather than believed, so the error costs
    /// time. A file missing from the population is a mutant nobody measures and a score nobody can
    /// see is wrong.
    #[must_use]
    pub fn killers(&self) -> Killers {
        let mut files = Vec::new();
        let mut complete = true;

        for directory in &self.source_dirs {
            match walk_rust_files(directory) {
                Ok(found) => files.extend(found),
                Err(_failure) => complete = false,
            }
        }

        Killers::scan_complete(&files, complete)
    }

    /// Local path dependency roots a cache snapshot must include.
    #[must_use]
    pub(crate) fn external_inputs(&self) -> &[Utf8PathBuf] {
        &self.external_inputs
    }

    /// Whether cache reuse must be disabled because a build script can read untracked paths.
    #[must_use]
    pub(crate) const fn has_untracked_build_script_inputs(&self) -> bool {
        self.untracked_build_script_inputs
    }

    /// Adopts the verdicts an earlier report already settled, before ordinals are handed out.
    ///
    /// A settled mutant is not run again — it takes no ordinal, is never instrumented and is never
    /// announced as work — but it keeps its place in the population wearing the verdict it earned.
    /// Dropping it instead would make the run report on the subset it retried: the score would be
    /// computed over a handful of survivors, `--min-score` would be judged against that, and the
    /// report written out could not be fed to the next iteration because it no longer describes the
    /// whole population.
    pub fn settle(&mut self, settled: HashMap<MutantId, Outcome>) {
        self.settled = settled;
    }

    /// The workspace packages that have files worth mutating, in a stable order.
    #[must_use]
    pub fn packages(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::default();

        for file in &self.files {
            if seen.insert(file.package.as_str()) {
                order.push(file.package.clone());
            }
        }

        order
    }

    /// An empty plan for this workspace, to be filled in a package at a time.
    #[must_use]
    pub fn skeleton(&self) -> Plan {
        Plan {
            root: self.root.clone(),
            files: self.files.clone(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            skipped: Vec::new(),
            digests: HashMap::default(),
            reach: self.reach.clone(),
            specs: self.specs.clone(),
        }
    }

    /// Parses and mutates one package's files, or every file when `package` is `None`.
    ///
    /// `ordinals` carries the last ordinal handed out, so that packages scanned one after another
    /// number their mutants continuously. Ordinals name the live mutants to the guard runtime, so
    /// they have to be unique across the whole run, not within a package.
    ///
    /// # Errors
    ///
    /// Returns an error if a file cannot be read or parsed.
    pub fn scan(&self, package: Option<&str>, selection: &Selection, ordinals: &mut u32) -> Result<Scanned> {
        let files: Vec<&TargetFile> = package.map_or_else(
            || self.files.iter().collect(),
            |wanted| {
                self.by_package
                    .get(wanted)
                    .map(|positions| positions.iter().filter_map(|&position| self.files.get(position)).collect())
                    .unwrap_or_default()
            },
        );

        let roots: Vec<Utf8PathBuf> = package.map_or_else(
            || self.roots.values().flatten().cloned().collect(),
            |wanted| self.roots.get(wanted).cloned().unwrap_or_default(),
        );
        let declaration_files: Vec<(&Utf8Path, &CfgSet)> = package.map_or_else(
            || {
                self.declaration_files
                    .iter()
                    .flat_map(|(package, paths)| {
                        let cfg = self.cfgs.for_package(package);

                        paths.iter().map(move |path| (path.as_path(), cfg))
                    })
                    .collect()
            },
            |wanted| {
                let cfg = self.cfgs.for_package(wanted);

                self.declaration_files
                    .get(wanted)
                    .map(|paths| paths.iter().map(|path| (path.as_path(), cfg)).collect())
                    .unwrap_or_default()
            },
        );
        let Scan {
            mut mutants,
            suppressed,
            idle,
            skipped,
            digests,
        } = scan(&files, &declaration_files, &roots, selection, &self.cfgs, &self.exclude_trait_impls)?;

        // Within a file the diff still has the last word: a changed line usually sits among many
        // that were not touched, and mutating those would report on code the change never went
        // near. A mutant is selected by its whole extent, from the line its site starts on to the
        // one it ends on, so editing an interior line of a multi-line site still selects it.
        if let Some(diff) = self.diff.as_ref() {
            mutants.retain(|mutant| {
                let start = u32::try_from(mutant.line).unwrap_or(u32::MAX);
                let end = u32::try_from(mutant.end_line).unwrap_or(u32::MAX);

                diff.touches(&mutant.file, start, end)
            });
        }

        // A mutant an earlier run already settled takes the verdict that run gave it and stops
        // being work: no ordinal, no shard slot, nothing built for it. It stays in the population,
        // because the score is a claim about the population and not about whichever part of it this
        // run had reason to retry.
        let mut settled_out = 0_usize;

        if !self.settled.is_empty() {
            for mutant in &mut mutants {
                if let Some(outcome) = self.settled.get(&mutant.id).copied() {
                    mutant.outcome = outcome;
                    settled_out = settled_out.saturating_add(1);
                }
            }
        }

        // Suppressed and already-settled mutants are kept but never run, so they take no part in
        // sharding: letting them occupy shard slots would make one night's shard cheaper than
        // another for no reason, and would hide how much of the population is actually being
        // exercised.
        let is_live = |mutant: &Mutant| mutant.outcome == Outcome::Pending;
        let before = mutants.iter().filter(|mutant| is_live(mutant)).count();

        if let Some((count, index)) = self.shard {
            mutants.retain(|mutant| !is_live(mutant) || shard_of(&mutant.id, count) == index);
        }

        // Each file shared its own strings as it produced its mutants, but a mutator name and a
        // package name repeat across every file in the workspace. This is the first point that has
        // the whole population, and so the first that can collapse those onto one copy each.
        Interner::default().share(&mut mutants);

        let mut live = 0_usize;

        for mutant in &mut mutants {
            if is_live(mutant) {
                live = live.saturating_add(1);
                *ordinals = ordinals.saturating_add(1);
                mutant.ordinal = *ordinals;
            }
        }

        Ok(Scanned {
            mutants,
            suppressed,
            idle,
            sharded_out: before - live,
            settled_out,
            skipped,
            digests,
        })
    }

    /// Turns a scan of the whole workspace into the plan a run works from.
    #[must_use]
    pub fn into_plan(self, scanned: Scanned) -> Plan {
        let mut plan = Plan {
            root: self.root,
            files: self.files,
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            skipped: Vec::new(),
            digests: HashMap::default(),
            reach: self.reach,
            specs: self.specs,
        };

        plan.absorb(scanned);
        plan.sort();

        plan
    }
}

fn unmatched_exclusion(index: usize, exclusion: &str) -> Error {
    error!(
        "exclude-trait-impls entry {} (`{}`) matched no trait implementations; check the unqualified Rust identifier forming the final written trait-path segment",
        index + 1,
        exclusion
    )
    .usage()
}

/// Reports what each package yielded, once the counts exist.
///
/// Package order follows the files, which are already sorted by path, so the same workspace always
/// reports in the same order. A package that produced no mutants is still named: a crate that
/// silently contributes nothing to a run is worth noticing, and its absence from the list would
/// look like it had simply not been looked at.
fn report_by_package(files: &[TargetFile], mutants: &[Mutant], notify: &mut impl FnMut(&str)) {
    let mut order: Vec<&str> = Vec::new();
    let mut counts: HashMap<&str, (usize, usize)> = HashMap::default();

    for file in files {
        let entry = counts.entry(file.package.as_str()).or_insert_with(|| {
            order.push(file.package.as_str());

            (0, 0)
        });

        entry.0 += 1;
    }

    for mutant in mutants {
        if let Some(entry) = counts.get_mut(&*mutant.package) {
            entry.1 += 1;
        }
    }

    for package in order {
        let (files, mutants) = counts.get(package).copied().unwrap_or((0, 0));

        notify(&format!(
            "{package}, {} in {}",
            crate::report::quantity(mutants, "mutant"),
            crate::report::quantity(files, "file")
        ));
    }
}

/// Turns one parsed file into its mutants, with the suppressions it declares already applied.
fn mutate(
    file: &TargetFile,
    source: &SourceFile,
    selection: &Selection,
    cfgs: &Cfgs,
    defaults: &collect::Defaults,
    exclude_trait_impls: &[String],
) -> Result<Parsed> {
    // Taken from the tree that was parsed for mutants anyway, so knowing which files exist only for
    // tests costs a walk over the top-level items rather than a second parse of everything.
    let cfg = cfgs.for_package(&file.package);
    let declared = modules::declarations(&file.absolute, source.ast(), cfg);

    // Before anything is collected, because a stated value that cannot be honoured is a hint the
    // author believes is working. Reporting it is worth more than the mutants of the file it sits
    // in, and reporting it first means the message is about the attribute rather than about
    // whatever the file happened to yield without it.
    //
    // Checked and collected in one call, which runs the stated-value audit and the numeric/import
    // indexes in a single walk of the syntax tree rather than the two separate ones a standalone
    // `check_stated` followed by `collect_with` would need.
    let candidates = collect::check_stated_and_collect_with(source, selection, cfg, defaults)?;
    let (mut found, trait_impls): (Vec<_>, Vec<_>) = collect::into_mutants_with_traits(source, &file.package, candidates)
        .into_iter()
        .unzip();
    let directives = suppress::directives_for(source, cfg)?;
    let mut suppressed = suppress::suppress(&mut found, &directives);

    for (mutant, trait_impl) in found.iter_mut().zip(trait_impls) {
        let Some(trait_name) = trait_impl
            .as_ref()
            .filter(|name| exclude_trait_impls.iter().any(|excluded| excluded == name.as_ref()))
        else {
            continue;
        };

        // Source suppression already made an overlapping mutant visible and accounted for it.
        // Configuration changes only otherwise-live matches so the population keeps one ignored
        // entry and one suppression count per mutant.
        if mutant.outcome == Outcome::Pending {
            mutant.outcome = Outcome::Ignored;
            mutant.suppression = Some(Suppression {
                channel: Channel::Config,
                reason: Some(format!("trait implementation `{trait_name}` matched `exclude-trait-impls`")),
                tag: None,
                line: None,
            });
            suppressed = suppressed.saturating_add(1);
        }
    }

    // Asked here, before the diff and the shard have had their say, because those narrow the
    // population within a file that was scanned in full. A directive whose mutants all fall
    // outside `--in-diff` has not stopped earning its place, and saying so would make every
    // incremental run condemn most of the tree.
    let idle = suppress::idle(&file.path, &found, &directives, selection);

    Ok(Parsed {
        mutants: found,
        suppressed,
        idle,
        declared,
        digest: crate::discover::digest(source.text().as_bytes()),
    })
}

struct TraitImplementations<'cfg> {
    cfg: &'cfg CfgSet,
    names: HashSet<String>,
}

impl TraitImplementations<'_> {
    fn collect(file: &syn::File, cfg: &CfgSet) -> HashSet<String> {
        let mut collector = TraitImplementations {
            cfg,
            names: HashSet::default(),
        };

        collector.visit_file(file);

        collector.names
    }
}

/// Validates workspace-scoped trait exclusions before run-scoped filters narrow the population.
fn validate_trait_exclusions(exclusions: &[String], files: &[TargetFile], cfgs: &Cfgs) -> Result<()> {
    if exclusions.is_empty() {
        return Ok(());
    }

    let mut implementations: HashSet<String> = HashSet::default();

    for file in files {
        match SourceFile::read(&file.absolute) {
            Ok(source) => implementations.extend(TraitImplementations::collect(source.ast(), cfgs.for_package(&file.package))),
            Err(error) if error.is_skippable() => {}
            Err(error) => return Err(error.into()),
        }

        if exclusions.iter().all(|exclusion| implementations.contains(exclusion.as_str())) {
            return Ok(());
        }
    }

    if let Some((index, exclusion)) = exclusions
        .iter()
        .enumerate()
        .find(|(_index, exclusion)| !implementations.contains(exclusion.as_str()))
    {
        return Err(unmatched_exclusion(index, exclusion));
    }

    Ok(())
}

impl<'ast> Visit<'ast> for TraitImplementations<'_> {
    #[expect(
        clippy::renamed_function_params,
        reason = "`item` identifies the syntax node more clearly than the trait declaration's `i`"
    )]
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !self.cfg.holds_for(item_attributes(item)) {
            return;
        }

        if let syn::Item::Impl(item) = item
            && let Some((path, _for_token)) = &item.trait_
            && let Some(segment) = path.segments.last()
        {
            let _new = self.names.insert(segment.ident.to_string());
        }

        visit::visit_item(self, item);
    }
}

fn item_attributes(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(node) => &node.attrs,
        syn::Item::Enum(node) => &node.attrs,
        syn::Item::ExternCrate(node) => &node.attrs,
        syn::Item::Fn(node) => &node.attrs,
        syn::Item::ForeignMod(node) => &node.attrs,
        syn::Item::Impl(node) => &node.attrs,
        syn::Item::Macro(node) => &node.attrs,
        syn::Item::Mod(node) => &node.attrs,
        syn::Item::Static(node) => &node.attrs,
        syn::Item::Struct(node) => &node.attrs,
        syn::Item::Trait(node) => &node.attrs,
        syn::Item::TraitAlias(node) => &node.attrs,
        syn::Item::Type(node) => &node.attrs,
        syn::Item::Union(node) => &node.attrs,
        syn::Item::Use(node) => &node.attrs,
        _ => &[],
    }
}

/// Reads, parses and mutates every file, returning the population and how much of it was suppressed.
///
/// Parsing is what discovery actually spends its time on, so the files are divided across the
/// available cores. Work is claimed one file at a time rather than in fixed blocks, since files
/// vary enormously in size and a static split leaves the machine waiting on whichever worker drew
/// the largest ones. Results are put back in file order afterwards, so the population does not
/// depend on how the work happened to land.
///
/// It happens in two phases, because one file's mutants depend on what the others declare:
/// `Default::default()` is only worth offering for a type that has a `Default`, and the definition
/// that settles that is usually in a different file. So every file is parsed first, every syntax
/// tree is held, and the index over them is complete before any mutant is emitted.
///
/// A worker keeps the trees it parsed and mutates those same files in the second phase. A syntax
/// tree is not `Send` — `syn` spans carry a handle that only the thread that made them may touch —
/// so the trees cannot be pooled and redistributed. What crosses the barrier between the phases is
/// the index, which is only names. That is also why the two phases share one scope rather than
/// running as two consecutive ones: the trees cannot outlive the thread that built them.
///
/// The barrier's party count is fixed at spawn, so *every* worker must reach both waits on every
/// path, including the ones this code did not plan for. Failures are recorded and fall through;
/// panics are caught, held, and resumed once both waits are behind them.
fn scan(
    files: &[&TargetFile],
    declaration_files: &[(&Utf8Path, &CfgSet)],
    roots: &[Utf8PathBuf],
    selection: &Selection,
    cfgs: &Cfgs,
    exclude_trait_impls: &[String],
) -> Result<Scan> {
    let workers = thread::available_parallelism().map_or(1, NonZero::get).min(files.len().max(1));
    let shared = Shared {
        next: AtomicUsize::new(0),
        partials: Mutex::new(Vec::new()),
        skipped: Mutex::new(Vec::new()),
        barrier: Barrier::new(workers),
        defaults: OnceLock::new(),
    };

    let mut collected: Vec<(usize, Parsed)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_worker| {
                let shared = &shared;

                scope.spawn(move || work(files, shared, selection, cfgs, exclude_trait_impls))
            })
            .collect();

        let mut collected = Vec::new();
        let mut failure: Option<(usize, Error)> = None;

        for handle in handles {
            // A panic in a worker is a bug in this crate, not something a user can act on, so it
            // is propagated rather than turned into a diagnostic that blames their code. It can
            // only get here because the worker held it until both barriers were behind it.
            match handle.join().unwrap_or_else(|payload| resume_unwind(payload)) {
                Ok(mine) => collected.extend(mine),

                // Several files can be unreadable or unparseable at once, and which worker noticed
                // first is a race. The earliest in file order is reported so the message does not
                // change between runs.
                Err((at, error)) => {
                    if failure.as_ref().is_none_or(|(seen, _)| at < *seen) {
                        failure = Some((at, error));
                    }
                }
            }
        }

        match failure {
            Some((_at, error)) => Err(error),
            None => Ok(collected),
        }
    })?;

    collected.sort_by_key(|(index, _parsed)| *index);

    // A file reachable only through a test-only or inactive module declaration is absent from the
    // production population, whatever it looks like from the inside, and its mutants are dropped
    // rather than reported. A mutated assertion is a broken test, not a gap in one, and inactive
    // code is not built for any test to catch.
    let mut declared: Vec<(Utf8PathBuf, Vec<modules::Declaration>)> = collected
        .iter()
        .map(|(index, parsed)| {
            let path = files.get(*index).map_or_else(Utf8PathBuf::new, |file| file.absolute.clone());

            (path, parsed.declared.clone())
        })
        .collect();
    let selected: HashSet<&Utf8Path> = files.iter().map(|file| file.absolute.as_path()).collect();

    let extra_decl_files: Vec<(&Utf8Path, &CfgSet)> = declaration_files
        .iter()
        .filter(|&&(path, _cfg)| !selected.contains(path))
        .map(|&(path, cfg)| (path, cfg))
        .collect();

    let declaration_skips: Vec<(Utf8PathBuf, String)> = if extra_decl_files.is_empty() {
        Vec::new()
    } else {
        let Declarations {
            declared: extra_declared,
            skipped: extra_skipped,
        } = parse_declarations_parallel(&extra_decl_files)?;

        declared.extend(extra_declared);
        extra_skipped
    };

    let excluded = modules::excluded_files(roots, &declared);
    let total = collected.iter().map(|(_index, parsed)| parsed.mutants.len()).sum();
    let mut mutants = Vec::with_capacity(total);
    let mut digests: HashMap<Utf8PathBuf, String> = HashMap::default();
    let mut suppressed = 0;
    let mut idle = Vec::new();

    for (index, parsed) in collected {
        if files.get(index).is_some_and(|file| excluded.contains(&file.absolute)) {
            continue;
        }

        if let Some(file) = files.get(index) {
            let _replaced = digests.insert(file.path.clone(), parsed.digest);
        }
        mutants.extend(parsed.mutants);
        suppressed += parsed.suppressed;
        idle.extend(parsed.idle);
    }

    // Keyed by filesystem path rather than by claim order for the same reason the earliest failure
    // is the one
    // reported: which worker claimed which file is a race, and a diagnostic that reorders itself
    // between runs is one nobody can diff. Sorting the selected-file and declaration-only skips
    // together by the same key also makes the list independent of which scan read a file, so
    // narrowing a selection moves a skip between scans without moving it in the report.
    let selected_skips = shared.skipped.into_inner().unwrap_or_else(PoisonError::into_inner);
    let mut unanalyzable: Vec<(Utf8PathBuf, String)> = selected_skips
        .into_iter()
        .map(|(at, message)| {
            let path = files.get(at).map_or_else(Utf8PathBuf::new, |file| file.absolute.clone());

            (path, message)
        })
        .chain(declaration_skips)
        .collect();

    unanalyzable.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    unanalyzable.dedup();

    Ok(Scan {
        mutants,
        suppressed,
        idle,
        skipped: unanalyzable.into_iter().map(|(_path, message)| message).collect(),
        digests,
    })
}

type DeclarationParse = (usize, Result<DeclarationOutcome>);

/// What parsing one declaration-only file yielded: its declarations, or the reason it was skipped.
enum DeclarationOutcome {
    /// The file parsed, and declares these modules.
    Declared(Utf8PathBuf, Vec<modules::Declaration>),

    /// The file could not be analyzed, and this diagnostic names it and says why.
    Skipped(Utf8PathBuf, String),
}

/// Declarations gathered from files outside the selection, with the ones that could not be read.
struct Declarations {
    declared: Vec<(Utf8PathBuf, Vec<modules::Declaration>)>,
    skipped: Vec<(Utf8PathBuf, String)>,
}

/// Parses declaration-only files with bounded parallelism.
///
/// Each file is read and parsed solely to extract module declarations — no mutation is performed.
/// The parallelism is bounded by `available_parallelism` to avoid exceeding system thread limits.
/// Results are returned in filesystem-path order.
///
/// A file this tool cannot analyze but `rustc` can build is recorded as a skip rather than a
/// failure, exactly as the selected-file scan records it. Otherwise narrowing a selection would
/// move such a file from the mutating scan to this declaration-only scan and turn a partial
/// measurement of an otherwise valid workspace into a failed run, without a line of the workspace
/// having changed. Its declarations are lost with it, so a module only that file declares is
/// treated as absent — the same shape as the file having been unreadable to a selection that never
/// mentioned it.
///
/// # Errors
///
/// Returns the first non-skippable file-read or parse error encountered, in path order.
fn parse_declarations_parallel(files: &[(&Utf8Path, &CfgSet)]) -> Result<Declarations> {
    if files.is_empty() {
        return Ok(Declarations {
            declared: Vec::new(),
            skipped: Vec::new(),
        });
    }

    let workers = thread::available_parallelism().map_or(1, NonZero::get).min(files.len());

    let mut results: Vec<DeclarationParse> = if workers <= 1 {
        let mut results = Vec::with_capacity(files.len());

        for (index, &(path, cfg)) in files.iter().enumerate() {
            results.push((index, parse_declarations_of(path, cfg)));
        }

        results
    } else {
        let next = AtomicUsize::new(0);

        thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_worker| {
                    let next = &next;

                    scope.spawn(move || {
                        let mut mine = Vec::new();

                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&(path, cfg)) = files.get(index) else {
                                break;
                            };

                            mine.push((index, parse_declarations_of(path, cfg)));
                        }

                        mine
                    })
                })
                .collect();

            let mut all = Vec::new();

            for handle in handles {
                all.extend(handle.join().unwrap_or_else(|payload| resume_unwind(payload)));
            }

            all
        })
    };

    results.sort_by_key(|(index, _result)| *index);

    // Check for errors after restoring input order, so scheduling cannot choose the diagnostic.
    let mut gathered = Declarations {
        declared: Vec::with_capacity(results.len()),
        skipped: Vec::new(),
    };

    for (_index, result) in results {
        match result? {
            DeclarationOutcome::Declared(path, declarations) => gathered.declared.push((path, declarations)),
            DeclarationOutcome::Skipped(path, message) => gathered.skipped.push((path, message)),
        }
    }

    // Declaration consumers use path order, independent of how the caller ordered its pairs.
    gathered.declared.sort_by(|left, right| left.0.cmp(&right.0));
    gathered.skipped.sort_by(|left, right| left.0.cmp(&right.0));

    Ok(gathered)
}

/// Reads one declaration-only file, turning a skippable failure into a named skip.
fn parse_declarations_of(path: &Utf8Path, cfg: &CfgSet) -> Result<DeclarationOutcome> {
    match SourceFile::read(path) {
        Ok(source) => Ok(DeclarationOutcome::Declared(
            path.to_owned(),
            modules::declarations(path, source.ast(), cfg),
        )),

        Err(error) if error.is_skippable() => Ok(DeclarationOutcome::Skipped(path.to_owned(), error.to_string())),

        Err(error) => Err(error.into()),
    }
}

/// What parsing and mutating a set of files produced.
struct Scan {
    mutants: Vec<Mutant>,
    suppressed: usize,
    idle: Vec<suppress::Idle>,
    skipped: Vec<String>,
    digests: HashMap<Utf8PathBuf, String>,
}

/// Everything the workers of one scan share, so that adding a channel does not widen every
/// signature between here and the loop that uses it.
struct Shared {
    /// The next file index to claim, so work is taken one file at a time rather than in blocks.
    next: AtomicUsize,

    /// Each worker's index of what the files it parsed declare, merged by the leader.
    partials: Mutex<Vec<collect::Defaults>>,

    /// Files stepped over, each with the index that orders it and the diagnostic that names it.
    skipped: Mutex<Vec<(usize, String)>>,

    /// Where every worker meets between the two phases, since one file's mutants depend on what the
    /// others declare.
    barrier: Barrier,

    /// The merged index, set once by the leader and read by all of them.
    defaults: OnceLock<collect::Defaults>,
}

/// What one file yielded when it was parsed.
/// One survey worker: parses whatever files it can claim, then mutates the ones it parsed.
///
/// Split out of `scan` because it is the whole of a worker's life and reads better whole, and
/// because the barrier discipline it implements is the point of the function rather than a detail
/// of the loop that spawns it.
///
/// The `usize` in the error is the index of the offending file, so `scan` can report the earliest
/// in file order rather than whichever worker happened to notice first.
fn work(
    files: &[&TargetFile],
    shared: &Shared,
    selection: &Selection,
    cfgs: &Cfgs,
    exclude_trait_impls: &[String],
) -> Result<Vec<(usize, Parsed)>, (usize, Error)> {
    let Shared {
        next,
        partials,
        skipped,
        barrier,
        defaults,
    } = shared;

    let mut mine: Vec<(usize, SourceFile)> = Vec::new();
    let mut index = collect::Defaults::default();
    let mut failure: Option<(usize, Error)> = None;

    // Phase one is guarded because the two waits below are not optional. `Barrier` has a fixed party
    // count and no poisoning, so a worker that unwound past them would leave every other worker
    // blocked in `wait` forever, `thread::scope` blocked joining those workers, and the panic that
    // should have been re-raised never raised at all — a silent hang, after the run has already paid
    // for discovery. The read-failure path below takes the same shape for the same reason; this is
    // that discipline extended to the failures the code cannot see coming.
    let unwound = catch_unwind(AssertUnwindSafe(|| {
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(file) = files.get(at) else { break };

            #[cfg(test)]
            #[cfg(not(miri))]
            tests::panic_probe(&file.path);

            match SourceFile::read(&file.absolute) {
                Ok(mut source) => {
                    // Report paths relative to the workspace root; that is what a user can act on
                    // and what a suppression or an expectation is keyed by.
                    source.set_path(file.path.clone());
                    index.absorb(collect::Defaults::of_in(source.ast(), cfgs.for_package(&file.package)));
                    mine.push((at, source));
                }

                // A file this tool cannot analyze but `rustc` can build is not a reason to
                // refuse the workspace; the rest of it is still worth measuring, and the file is
                // named so the score is read knowing what is missing from it. Every other read
                // failure stops the run, because it says the tree is not what it claims to be.
                Err(error) if error.is_skippable() => {
                    skipped.lock().unwrap_or_else(PoisonError::into_inner).push((at, error.to_string()));
                }

                Err(error) => {
                    failure = Some((at, error.into()));
                    break;
                }
            }
        }
    }))
    .err();

    partials.lock().unwrap_or_else(PoisonError::into_inner).push(mem::take(&mut index));

    // Guarded for the same reason, and more urgently: the leader has one more wait to reach, and it
    // is the only thread that can release the others.
    let merged = if barrier.wait().is_leader() {
        catch_unwind(AssertUnwindSafe(|| {
            let mut merged = collect::Defaults::default();

            for partial in partials.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
                merged.absorb(partial);
            }

            let _first = defaults.set(merged);
        }))
        .err()
    } else {
        None
    };

    let _released = barrier.wait();

    // Nobody can be left waiting now, so the panic goes back to being a panic and reaches the `join`
    // in `scan`, which is where a bug in this crate belongs.
    if let Some(payload) = unwound.or(merged) {
        resume_unwind(payload);
    }

    if let Some(failed) = failure {
        return Err(failed);
    }

    // Unset only if the leader unwound out of the merge, in which case the leader is resuming that
    // panic right now and it will reach `join`. There is nothing for this worker to mutate against,
    // and nothing useful for it to say.
    let Some(defaults) = defaults.get() else {
        return Ok(Vec::new());
    };

    let mut parsed = Vec::with_capacity(mine.len());

    for (at, source) in &mine {
        let Some(file) = files.get(*at) else { continue };

        match mutate(file, source, selection, cfgs, defaults, exclude_trait_impls) {
            Ok(one) => parsed.push((*at, one)),
            Err(error) => return Err((*at, error)),
        }
    }

    Ok(parsed)
}

struct Parsed {
    mutants: Vec<Mutant>,
    suppressed: usize,
    idle: Vec<suppress::Idle>,
    declared: Vec<modules::Declaration>,

    /// A digest of the exact bytes this file's mutants were derived from.
    digest: String,
}

/// Drains a set into the sorted vector the survey stores, so two scans agree on order.
fn sorted<T: Ord + core::hash::Hash>(values: HashSet<T>) -> Vec<T> {
    let mut ordered: Vec<T> = values.into_iter().collect();

    ordered.sort_unstable();
    ordered
}

/// Works out which workspace packages each workspace package can reach.
///
/// Built from the declared dependencies rather than from a resolved graph, so it costs nothing
/// beyond the metadata already loaded. Dependencies of every kind count, including dev: an
/// integration test links its package's dev-dependencies, and being over-inclusive here can only
/// cost time, never correctness — the reverse would silently skip a test that really does reach the
/// mutated code and turn a survivor into a false clean bill of health.
///
/// The metadata is loaded with `--no-deps`, so a dependency that is not itself a workspace member
/// has no entry to walk into. A registry dependency cannot lead back into the workspace and can be
/// ignored, but a *path* dependency outside the workspace can: `app -> facade -> core` is a real
/// chain that this graph cannot see. Rather than skip a test binary that does reach the mutated
/// code, a package with such a dependency reaches everything — and so does every package that can
/// reach it, which is both the same fail-open argument and what keeps a dependency's reach set a
/// subset of its dependent's, the property [`stages`](super::stages) sorts on.
///
/// Members are assigned integer IDs so graph traversal avoids allocating package names.
///
/// Reachability itself is computed once for the whole graph rather than once per starting package:
/// [`reachable_ids`] collapses the graph into strongly connected components and gives each
/// component's closure a single bitset, built in one bottom-up pass over the condensation instead
/// of a fresh breadth-first search per member. A workspace of `n` packages and `e` dependency edges
/// used to cost `O(n * (n + e))`; collapsing first costs `O(n + e)` to find the components and
/// `O(c^2 / 64)` words to union their closures, where `c <= n` is the component count — a real
/// saving whenever `c` is smaller than `n`, and never worse, since a graph with no cycles has
/// `c == n` and the bitset union pass still costs only `O(n^2 / 64)` words rather than `O(n^2)`
/// pointer-chasing queue operations.
fn reachable(metadata: &Metadata) -> HashMap<String, HashSet<String>> {
    let member_packages: Vec<&str> = metadata.workspace_packages().iter().map(|p| p.name.as_str()).collect();
    let member_count = member_packages.len();

    let name_to_id: HashMap<&str, usize> = member_packages.iter().enumerate().map(|(id, name)| (*name, id)).collect();

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); member_count];
    let mut opaque: Vec<bool> = vec![false; member_count];

    for package in metadata.workspace_packages() {
        let Some(&src_id) = name_to_id.get(package.name.as_str()) else {
            continue;
        };

        for dependency in &package.dependencies {
            if let Some(&dst_id) = name_to_id.get(dependency.name.as_str()) {
                edges[src_id].push(dst_id);
            } else if dependency.path.is_some() {
                opaque[src_id] = true;
            }
        }
    }

    reachable_ids(&edges, &opaque)
        .into_iter()
        .enumerate()
        .map(|(id, reached)| {
            let reachable_set: HashSet<String> = reached
                .into_iter()
                .map(|reached_id| member_packages[reached_id].to_owned())
                .collect();

            (member_packages[id].to_owned(), reachable_set)
        })
        .collect()
}

/// A fixed-size, word-packed set of small integers.
///
/// Used to hold one strongly connected component's reach set: a workspace has too few members to
/// justify pulling in a bitset crate, but the closures computed below are unioned often enough that
/// per-word operations matter more than the allocation they would need to avoid.
#[derive(Clone)]
struct Bitset {
    words: Vec<u64>,
}

impl Bitset {
    fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
        }
    }

    fn set(&mut self, index: usize) {
        self.words[index / 64] |= 1 << (index % 64);
    }

    /// Unions another set's bits into this one.
    fn or_with(&mut self, other: &Self) {
        for (word, other_word) in self.words.iter_mut().zip(&other.words) {
            *word |= *other_word;
        }
    }

    fn iter_set(&self) -> Vec<usize> {
        let mut out = Vec::new();

        for (word_index, word) in self.words.iter().enumerate() {
            let mut remaining = *word;

            while remaining != 0 {
                let bit = remaining.trailing_zeros();
                out.push(word_index * 64 + usize::try_from(bit).unwrap_or(0));
                remaining &= remaining - 1;
            }
        }

        out
    }
}

/// One call frame of the iterative Tarjan walk, replacing the recursive call [`strongly_connected_components`]
/// would otherwise need one stack frame per node for.
///
/// A workspace's dependency graph can have a long linear chain (`a -> b -> c -> ...`), and a
/// recursive implementation would need one native stack frame per link in it. Driving the descent
/// through an explicit stack removes that risk entirely, at the cost of tracking, per node
/// currently open, which of its edges has already been followed.
struct Frame {
    node: usize,
    edge_at: usize,
}

/// Collapses a directed graph into its strongly connected components, using Tarjan's algorithm.
///
/// Returns each node's component id. Components are numbered in the order they finish (are fully
/// popped off Tarjan's stack), which guarantees that every edge crossing from component `c` to a
/// different component `d` satisfies `d < c`: nothing a component points to can finish, and so be
/// numbered, after it does. [`reachable_ids`] relies on this to compute every component's closure
/// in one forward pass over increasing ids, without a separate topological sort.
fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<usize> {
    let node_count = edges.len();
    let mut index_of: Vec<Option<u32>> = vec![None; node_count];
    let mut low_link: Vec<u32> = vec![0; node_count];
    let mut on_stack: Vec<bool> = vec![false; node_count];
    let mut comp_of: Vec<usize> = vec![usize::MAX; node_count];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index: u32 = 0;
    let mut next_component: usize = 0;

    for start in 0..node_count {
        if index_of[start].is_some() {
            continue;
        }

        let mut call_stack: Vec<Frame> = vec![Frame { node: start, edge_at: 0 }];
        index_of[start] = Some(next_index);
        low_link[start] = next_index;
        next_index += 1;
        stack.push(start);
        on_stack[start] = true;

        while let Some(frame) = call_stack.last_mut() {
            let node = frame.node;

            if frame.edge_at < edges[node].len() {
                let neighbor = edges[node][frame.edge_at];
                frame.edge_at += 1;

                if let Some(neighbor_index) = index_of[neighbor] {
                    if on_stack[neighbor] {
                        low_link[node] = low_link[node].min(neighbor_index);
                    }
                } else {
                    index_of[neighbor] = Some(next_index);
                    low_link[neighbor] = next_index;
                    next_index += 1;
                    stack.push(neighbor);
                    on_stack[neighbor] = true;
                    call_stack.push(Frame {
                        node: neighbor,
                        edge_at: 0,
                    });
                }
            } else {
                call_stack.pop();

                // Guarded by pushing `start` above and never popping below it in this loop, so a
                // parent frame is always here to read the child's finished low-link from.
                if let Some(parent) = call_stack.last() {
                    low_link[parent.node] = low_link[parent.node].min(low_link[node]);
                }

                if low_link[node] == index_of[node].expect("just indexed above, on the way in") {
                    loop {
                        let member = stack.pop().expect("root of this component pushed before this loop started");
                        on_stack[member] = false;
                        comp_of[member] = next_component;

                        if member == node {
                            break;
                        }
                    }

                    next_component += 1;
                }
            }
        }
    }

    comp_of
}

/// The pure graph half of [`reachable`]: every node's id-keyed reachable set, from an edge list and
/// an opaque flag per node.
///
/// Split out from [`reachable`] so the graph algorithm can be tested directly against small,
/// hand-built and randomized graphs, without needing a real `cargo_metadata::Metadata` to drive it.
fn reachable_ids(edges: &[Vec<usize>], opaque: &[bool]) -> Vec<HashSet<usize>> {
    let node_count = edges.len();
    let comp_of = strongly_connected_components(edges);
    let component_count = comp_of.iter().copied().max().map_or(0, |max| max + 1);

    let mut comp_edges: Vec<HashSet<usize>> = vec![HashSet::default(); component_count];
    let mut comp_opaque: Vec<bool> = vec![false; component_count];
    let mut comp_members: Vec<Vec<usize>> = vec![Vec::new(); component_count];

    for node in 0..node_count {
        let comp = comp_of[node];
        comp_members[comp].push(node);

        if opaque[node] {
            comp_opaque[comp] = true;
        }

        for &neighbor in &edges[node] {
            let neighbor_comp = comp_of[neighbor];

            if neighbor_comp != comp {
                let _added = comp_edges[comp].insert(neighbor_comp);
            }
        }
    }

    // Every component's own closure over components: itself, plus every successor's closure. Built
    // in increasing id order, which the finish-order numbering above guarantees is also a
    // reachability order — by the time `comp` is processed, everything smaller it points to already
    // has its own finished closure to union in.
    let mut comp_closure: Vec<Bitset> = Vec::with_capacity(component_count);

    for (comp, successors) in comp_edges.iter().enumerate() {
        let mut closure = Bitset::new(component_count);
        closure.set(comp);

        for &successor in successors {
            let successor_closure = comp_closure[successor].clone();
            closure.or_with(&successor_closure);
        }

        comp_closure.push(closure);
    }

    // A component reaches everything once any component in its own closure is opaque: the missing
    // edges that make a component's own graph incomplete make it impossible to prove it does *not*
    // reach a package, and the same is true of anything that can reach that component.
    let all_nodes: HashSet<usize> = (0..node_count).collect();
    let effective_opaque: Vec<bool> = (0..component_count)
        .map(|comp| comp_closure[comp].iter_set().into_iter().any(|reached| comp_opaque[reached]))
        .collect();

    (0..node_count)
        .map(|node| {
            let comp = comp_of[node];

            if effective_opaque[comp] {
                all_nodes.clone()
            } else {
                comp_closure[comp]
                    .iter_set()
                    .into_iter()
                    .flat_map(|reached_comp| comp_members[reached_comp].iter().copied())
                    .collect()
            }
        })
        .collect()
}

/// Works out which configuration predicates hold for each package of the workspace.
///
/// Asking `rustc` is one process for the whole run, and the feature closure is arithmetic over
/// metadata that has already been loaded, so this is cheap enough to do unconditionally.
///
/// The predicates describe the build cargo will run rather than a bare host compile: its target,
/// its profile's `debug_assertions` and whatever `--cfg` its flags carry all decide which code is
/// compiled, and evaluating a different build would classify code the compiler does see as absent
/// and drop its mutants.
///
/// A `rustc` that cannot be run, or a build no single set of predicates describes, leaves every set
/// unconditional, which is exactly how the tool behaved before it evaluated predicates at all:
/// nothing is stripped, and a user on an unusual toolchain gets a noisier report rather than a
/// failed run.
fn configuration(root: &Utf8Path, cargo: &CargoOptions, enabled: &HashMap<String, Vec<String>>) -> Cfgs {
    let build = cargo.cfg_build(root);

    let Ok(target) = crate::cfg::for_build(&build) else {
        return Cfgs::unconditional();
    };

    Cfgs::new(&target, enabled)
}

/// Places a package's source file relative to the workspace root, refusing one that lies outside it.
///
/// `Utf8Path::join` with an absolute argument replaces the base, so an absolute `TargetFile::path`
/// bypasses every containment check in the filesystem layer and the write lands in the user's real
/// source tree rather than the scratch copy. Cargo accepts a member outside the root when the member
/// names its workspace, so this is reachable, and it is refused rather than repaired into a path
/// that means something else.
///
/// # Errors
///
/// Returns an error if `absolute` is not under `root`.
fn placed_under(root: &Utf8Path, absolute: &Utf8Path, package: &str) -> Result<Utf8PathBuf> {
    let Ok(inside) = absolute.strip_prefix(root) else {
        return Err(error!(
            "package `{package}` has the source file `{absolute}`, which lies outside the workspace root `{root}`; \
             gamma cannot mutate a file it cannot place inside its scratch copy"
        ));
    };

    Ok(Utf8PathBuf::from(normalize_separators(inside.as_str())))
}

/// Loads cargo metadata for the tree at `dir`.
///
/// The feature selection has to match the one the build will use. Metadata decides which targets
/// exist and which files are walked, so discovering under one feature set and compiling under
/// another would place guards in files the compiler never sees.
pub fn load_metadata(dir: &Utf8Path, features: &FeatureArgs) -> Result<Metadata> {
    let mut command = MetadataCommand::new();

    let _builder = command.current_dir(dir).no_deps();

    if features.all_features {
        let _builder = command.features(CargoOpt::AllFeatures);
    }

    if features.no_default_features {
        let _builder = command.features(CargoOpt::NoDefaultFeatures);
    }

    if !features.features.is_empty() {
        // Cargo accepts a comma-separated list in one argument and repetition across several, so
        // the entries are split apart here and handed over as the flat list they denote.
        let named: Vec<String> = features
            .features
            .iter()
            .flat_map(|entry| entry.split([',', ' ']))
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        let _builder = command.features(CargoOpt::SomeFeatures(named));
    }

    command
        .exec()
        .map_err(|cause| error!("could not read cargo metadata for `{dir}`").caused_by(cause))
}

/// The local filesystem inputs Cargo's dependency graph exposes.
#[derive(Default)]
struct ExternalPathInputs {
    roots: Vec<Utf8PathBuf>,
    has_build_scripts: bool,
}

/// Finds local packages Cargo resolves outside the workspace root.
///
/// The ordinary metadata pass intentionally uses `--no-deps`, because discovery only needs
/// workspace targets. Cache provenance needs the opposite answer as well: a path dependency's
/// source is a filesystem input even though it is not a workspace target and can change without a
/// workspace file moving. Registry and git packages are identified by their locked source and are
/// covered by the lockfile; only local packages have no source identifier to carry that change.
/// Build scripts are different: any package's script can read arbitrary paths that metadata does
/// not enumerate, so their presence makes a snapshot incomplete.
fn external_path_inputs(dir: &Utf8Path, features: &FeatureArgs, root: &Utf8Path) -> Result<ExternalPathInputs> {
    let mut command = MetadataCommand::new();
    let _builder = command.current_dir(dir);

    if features.all_features {
        let _builder = command.features(CargoOpt::AllFeatures);
    }

    if features.no_default_features {
        let _builder = command.features(CargoOpt::NoDefaultFeatures);
    }

    if !features.features.is_empty() {
        let named: Vec<String> = features
            .features
            .iter()
            .flat_map(|entry| entry.split([',', ' ']))
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        let _builder = command.features(CargoOpt::SomeFeatures(named));
    }

    let metadata = command
        .exec()
        .map_err(|cause| error!("could not read the external Cargo inputs for `{dir}`").caused_by(cause))?;
    let workspace = crate::paths::physical(root)?;
    let mut roots = Vec::new();
    let mut has_build_scripts = false;

    for package in metadata.packages {
        has_build_scripts |= package.targets.iter().any(|target| target.kind.contains(&TargetKind::CustomBuild));

        if package.source.is_some() {
            continue;
        }

        let Some(directory) = Utf8Path::new(package.manifest_path.as_str()).parent() else {
            return Err(error!(
                "Cargo reported a package manifest without a parent at `{}`",
                package.manifest_path
            ));
        };
        let directory = crate::paths::physical(directory)?;

        if !directory.starts_with(&workspace) {
            roots.push(directory);
        }
    }

    roots.sort();
    roots.dedup();

    Ok(ExternalPathInputs { roots, has_build_scripts })
}

/// Names the packages a run mutates, following cargo's own selection rules.
///
/// Cargo acts on the package that owns the directory it was invoked from, and on the whole
/// workspace only when asked. Mutating every member by default would be a much worse surprise than
/// it is for `cargo build`: a run over an unasked-for member costs a full build and test suite per
/// mutant, in code the caller may not own, and a failure anywhere ends the run.
///
/// At the workspace root there is no owning package, so the default members decide, exactly as they
/// do for a bare `cargo test`. `workspace_default_members` needs cargo 1.71, and dereferencing it on
/// anything older panics, so an unavailable list falls back to the owning package, or to every
/// member — which is what cargo itself does for a virtual manifest that declares no `default-members`.
fn selected_packages(metadata: &Metadata, args: &SelectArgs) -> HashSet<String> {
    let members = || {
        metadata
            .workspace_packages()
            .iter()
            .map(|package| package.name.to_string())
            .collect()
    };
    let defaults = || -> Option<HashSet<String>> {
        metadata.workspace_default_members.is_available().then(|| {
            metadata
                .workspace_default_packages()
                .iter()
                .map(|package| package.name.to_string())
                .collect()
        })
    };

    if !args.packages.is_empty() {
        return args.packages.iter().cloned().collect();
    }

    if args.workspace {
        return members();
    }

    // At the workspace root cargo's own default members are the answer, and that is the one place
    // `owning_package` disagrees: a root which is *itself* a package and also declares
    // `default-members` resolves to the root package alone, so gamma mutates one crate while a bare
    // `cargo test` in the same directory runs another. The oracle is capped by this same selection,
    // so the divergence changes verdicts rather than only cost.
    if at_workspace_root(metadata, &args.dir)
        && let Some(selected) = defaults()
    {
        return selected;
    }

    if let Some(owner) = owning_package(metadata, &args.dir) {
        return once(owner).collect();
    }

    defaults().unwrap_or_else(members)
}

/// Returns whether `dir` is the workspace root itself rather than somewhere beneath it.
fn at_workspace_root(metadata: &Metadata, dir: &Utf8Path) -> bool {
    let resolve = |path: &Utf8Path| path.canonicalize_utf8().unwrap_or_else(|_unresolved| path.to_owned());

    resolve(dir) == resolve(Utf8Path::new(metadata.workspace_root.as_str()))
}

/// Returns the workspace member whose directory contains `dir`, if any.
///
/// The deepest match wins, since a package may be nested inside another package's directory and the
/// inner one is the one cargo would act on. A directory that resolves to no member — the root of a
/// virtual manifest, or a directory outside the workspace entirely — has no owner.
fn owning_package(metadata: &Metadata, dir: &Utf8Path) -> Option<String> {
    let absolute = dir.canonicalize_utf8().unwrap_or_else(|_unresolved| dir.to_owned());
    let mut best: Option<(usize, String)> = None;

    for package in metadata.workspace_packages() {
        let Some(home) = Utf8Path::new(package.manifest_path.as_str()).parent() else {
            continue;
        };

        let home = home.canonicalize_utf8().unwrap_or_else(|_unresolved| home.to_owned());

        if !absolute.starts_with(&home) {
            continue;
        }

        let depth = home.components().count();

        if best.as_ref().is_none_or(|(deepest, _name)| depth > *deepest) {
            best = Some((depth, package.name.to_string()));
        }
    }

    best.map(|(_depth, name)| name)
}

/// Returns the first `--package` name that no workspace member answers to.
///
/// A misspelled package name would otherwise select nothing and report a clean run over an empty
/// population, which reads exactly like a workspace with no gaps in its tests.
fn unknown_packages<'args>(metadata: &Metadata, args: &'args SelectArgs) -> Option<&'args str> {
    let known: HashSet<&str> = metadata.workspace_packages().iter().map(|package| package.name.as_str()).collect();

    args.packages.iter().map(String::as_str).find(|wanted| !known.contains(wanted))
}

/// Returns whether a target contains code worth mutating.
///
/// Test, bench and example targets are excluded: mutating a test measures the tests' tests, and
/// mutating an example measures nothing at all, since examples are usually not run by the suite.
///
/// Proc-macro targets are excluded for a sharper reason: no mutant of one can ever be killed. A
/// proc macro runs inside `rustc`, while some *other* crate is being compiled, but a run builds the
/// tree once and only then selects one mutant per test process. By the time a test is watching, the
/// macro has long since finished its work, so every mutant of it survives however good the suite
/// is. Including them would charge a project the full cost of building and testing each one and
/// then hand back a pile of survivors that say nothing about its tests — which is exactly the kind
/// of unearned noise that teaches people to stop reading the score.
///
/// The way to get a proc macro under mutation is to keep its logic in an ordinary library that the
/// macro crate delegates to, which is what this project does with `cargo-gamma-attrs-impl`.
///
/// The guard runtime is excluded for a third reason, and it is the plainest of the three: a mutant
/// is only ever reached through a call to the runtime that decides whether it is active, so
/// mutating the runtime would place that call inside the very crate that defines it. The result
/// does not compile, and no amount of test quality changes that. This only bites a workspace that
/// vendors or develops the runtime itself — which is to say, this one — but the exclusion belongs
/// here rather than in a local ignore file, because it follows from how mutants are switched on
/// rather than from anything particular to this repository.
///
/// A binary whose `required-features` are not all enabled is excluded too, because cargo will not
/// build it. Surveying it costs the run a target's worth of mutants that cannot compile, reported
/// as unviable and retried through the rollback loop, all of it about a target the build never had.
/// `enabled` is what the feature closure worked out for the target's own package, and `None` — a
/// package the closure never described — keeps the target, since proving a gate unmet is what
/// having no answer makes impossible.
///
/// This decides targets rather than files, and the walk that follows a surviving target takes its
/// whole source directory: a gated `[[bin]]` whose file sits beside `lib.rs` is still reached
/// through the library. That is the same over-inclusion the walk carries throughout, and the safe
/// direction to err in.
fn is_mutable_target(target: &Target, enabled: Option<&Vec<String>>) -> bool {
    if target.name == crate::exec::RUNTIME_CRATE {
        return false;
    }

    let kind = target
        .kind
        .iter()
        .any(|kind| matches!(kind.to_string().as_str(), "lib" | "rlib" | "cdylib" | "bin"));

    // Library targets carry no `required-features` — cargo rejects a manifest that gives them
    // any — so this is the binary gate and nothing else.
    kind && enabled.is_none_or(|on| {
        target.required_features.iter().all(|required| {
            // A `dependency/feature` requirement is about another package's feature table, which
            // this closure does not key by, so it is left as satisfied rather than guessed at.
            required.contains('/') || on.iter().any(|feature| feature == required)
        })
    })
}

/// Names every test target the workspace declares.
///
/// The name is what cargo reports as `target.name` for the binary it builds, and so is what a
/// `--include-test` or `--exclude-test` pattern is written against. Bench and example targets are
/// listed when they carry `test = true`, because cargo builds and runs those as test binaries too,
/// and a run that cannot name them cannot take them out of the oracle.
///
/// Names are deduplicated because two workspace members may each have a `tests/integration.rs`,
/// and a pattern naming it means both. That is the honest reading: these patterns select targets,
/// and `--test-package` is what selects by package.
fn test_targets(metadata: &Metadata) -> Vec<String> {
    let mut names: Vec<String> = metadata
        .workspace_packages()
        .iter()
        .flat_map(|package| package.targets.iter())
        .filter(|target| target.test)
        .map(|target| target.name.clone())
        .collect();

    names.sort();
    names.dedup();
    names
}

/// Returns the first `--file` or `--exclude-file` pattern that matches no source file.
///
/// A pattern that matches nothing is nearly always a mistake — a typo, a stale path after a move,
/// or separators written for the wrong platform. Left alone it produces an empty run that reports
/// no mutants and exits successfully, which reads in CI exactly like a clean bill of health. The
/// same reasoning already makes an unmatched `--mutators` selector an error.
///
/// `walked` names every mutable source file in the workspace, not only the files this run selects,
/// so a workspace-wide pattern stays valid on a run narrowed with `--package`.
struct FilePatterns<'args> {
    include: Vec<(&'args str, Glob)>,
    exclude: Vec<(&'args str, Glob)>,
}

impl<'args> FilePatterns<'args> {
    fn new(args: &'args SelectArgs) -> Self {
        let compile = |patterns: &'args [String]| patterns.iter().map(|pattern| (pattern.as_str(), Glob::new(pattern))).collect();

        Self {
            include: compile(&args.files),
            exclude: compile(&args.exclude_files),
        }
    }

    fn unmatched(&self, walked: &[Utf8PathBuf]) -> Option<&'args str> {
        self.include
            .iter()
            .chain(&self.exclude)
            .find(|(_pattern, compiled)| !walked.iter().any(|path| compiled.matches(path.as_str())))
            .map(|(pattern, _compiled)| *pattern)
    }

    fn includes(&self, path: &Utf8Path) -> bool {
        let text = path.as_str();

        if self.exclude.iter().any(|(_pattern, compiled)| compiled.matches(text)) {
            return false;
        }

        self.include.is_empty() || self.include.iter().any(|(_pattern, compiled)| compiled.matches(text))
    }
}

/// Returns whether a file passes the include and exclude patterns.
#[cfg(test)]
fn is_included(path: &Utf8Path, args: &SelectArgs) -> bool {
    FilePatterns::new(args).includes(path)
}

/// Lists every `.rs` file under a directory, in a deterministic order.
///
/// Every failure is reported rather than skipped, which is the opposite of what a walk usually
/// does. This is the sole producer of the candidate file list for the whole population, and a walk
/// error is per *entry*: an unreadable subdirectory yields one error and then simply produces no
/// descendants, so a swallow deletes that entire subtree from the population. Nothing downstream
/// can notice — inclusion filtering, target mutability and the score denominator are all computed
/// over whatever survives — and the run reports a *higher* score with no warning and no non-zero
/// exit, which is the one direction this tool must never fail in.
///
/// A path that is not UTF-8 is refused only when it names a Rust source file. Such a file would
/// have been mutated and now cannot even be named, while a file of any other kind was never part
/// of the population and its spelling is nobody's business here.
fn walk_rust_files(directory: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut found: Vec<Utf8PathBuf> = Vec::new();

    for entry in WalkDir::new(directory) {
        let entry = entry.map_err(|cause| error!("could not list the source files under `{directory}`").caused_by(cause))?;

        if entry.file_type().is_dir() {
            continue;
        }

        let rust = entry.path().extension() == Some(OsStr::new("rs"));

        match Utf8PathBuf::from_path_buf(entry.into_path()) {
            Ok(path) if rust => found.push(path),
            Ok(_other) => {}
            Err(path) if rust => {
                return Err(error!(
                    "the source file `{}` under `{directory}` has a path that is not UTF-8, so it cannot be mutated",
                    path.display()
                ));
            }
            Err(_other) => {}
        }
    }

    found.sort();

    Ok(found)
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::testing::discover_fixture::counting_mutant;

    /// The name a test gives a file it wants a phase-1 survey worker to panic on.
    ///
    /// Injected by name rather than by an armed flag so that nothing has to be disarmed and no
    /// other test running at the same time can trip it.
    const PANIC_PROBE: &str = "gamma_panic_probe.rs";

    /// Panics if this is the file a test planted to make a survey worker die mid-phase.
    pub(super) fn panic_probe(path: &Utf8Path) {
        assert!(
            !path.as_str().ends_with(PANIC_PROBE),
            "a survey worker panicking during phase one, on purpose"
        );
    }

    #[test]
    fn excludes_beat_includes() {
        let args = SelectArgs {
            files: vec!["src/**/*.rs".to_owned()],
            exclude_files: vec!["generated.rs".to_owned()],
            ..SelectArgs::default()
        };

        assert!(is_included(Utf8Path::new("src/lexer.rs"), &args));
        assert!(!is_included(Utf8Path::new("src/generated.rs"), &args));
    }

    #[test]
    fn no_include_patterns_means_everything() {
        assert!(is_included(Utf8Path::new("anything.rs"), &SelectArgs::default()));
    }

    #[test]
    fn a_package_reaches_itself() {
        // Otherwise every mutant in a leaf crate would be reported as unreachable by its own tests.
        let metadata = load_metadata(Utf8Path::new(env!("CARGO_MANIFEST_DIR")), &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);

        for (package, reachable_from) in &reach {
            assert!(reachable_from.contains(package), "{package} does not reach itself");
        }
    }

    #[test]
    fn a_dependent_reaches_what_it_depends_on() {
        let metadata = load_metadata(Utf8Path::new(env!("CARGO_MANIFEST_DIR")), &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);

        // The binary crate is deliberately thin and defers everything to the library, so it must
        // reach it; the reverse must not hold, or the filter would never exclude anything here.
        let from_binary = reach.get("cargo-gamma").expect("the binary crate is a workspace member");

        assert!(from_binary.contains("cargo-gamma-lib"), "{from_binary:?}");

        let from_library = reach.get("cargo-gamma-lib").expect("the library is a workspace member");

        assert!(!from_library.contains("cargo-gamma"), "{from_library:?}");
    }

    /// Exhaustively checks [`reachable_ids`] against the same breadth-first search the previous
    /// implementation ran per starting node, so the collapse-then-close rewrite is only trusted
    /// once it is shown to answer identically to the algorithm it replaced.
    fn brute_force_reachable(edges: &[Vec<usize>], opaque: &[bool]) -> Vec<HashSet<usize>> {
        let node_count = edges.len();

        (0..node_count)
            .map(|start| {
                let mut seen = vec![false; node_count];
                let mut queue = std::collections::VecDeque::from([start]);
                seen[start] = true;
                let mut hits_opaque = false;

                while let Some(current) = queue.pop_front() {
                    if opaque[current] {
                        hits_opaque = true;
                    }

                    for &neighbor in &edges[current] {
                        if !seen[neighbor] {
                            seen[neighbor] = true;
                            queue.push_back(neighbor);
                        }
                    }
                }

                if hits_opaque {
                    (0..node_count).collect()
                } else {
                    seen.iter()
                        .enumerate()
                        .filter(|(_id, reached)| **reached)
                        .map(|(id, _)| id)
                        .collect()
                }
            })
            .collect()
    }

    /// A minimal, deterministic pseudo-random generator, so the randomized graph test below needs
    /// no external `rand` dependency and reproduces the same graphs on every run.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn a_chain_gives_every_node_a_strictly_smaller_id_than_its_predecessor() {
        // 0 -> 1 -> 2 -> 3, no cycles: every node is its own singleton component, and the finish
        // order of a depth-first walk numbers a leaf before the node that points to it.
        let edges = vec![vec![1], vec![2], vec![3], vec![]];
        let comp = strongly_connected_components(&edges);

        assert_ne!(comp[0], comp[1]);
        assert_ne!(comp[1], comp[2]);
        assert_ne!(comp[2], comp[3]);
        assert!(comp[3] < comp[2]);
        assert!(comp[2] < comp[1]);
        assert!(comp[1] < comp[0]);
    }

    #[test]
    fn a_full_cycle_collapses_into_one_component_that_reaches_itself_and_its_own_dependency() {
        // 0 -> 1 -> 2 -> 0 is one strongly connected component; 2 -> 3 means the cycle, as a
        // whole, also reaches the dependency any one of its members can walk to.
        let edges = vec![vec![1], vec![2], vec![0, 3], vec![]];
        let opaque = [false; 4];
        let reach = reachable_ids(&edges, &opaque);

        for member in [0, 1, 2] {
            assert_eq!(reach[member], [0, 1, 2, 3].into_iter().collect::<HashSet<_>>(), "member {member}");
        }

        assert_eq!(reach[3], std::iter::once(3).collect::<HashSet<_>>());
    }

    #[test]
    fn a_dense_graph_matches_the_brute_force_oracle() {
        // A handful of criss-crossing edges and no cycle, curated rather than random so a failure
        // here is easy to reason about by hand.
        let edges = vec![
            vec![1, 2],
            vec![3],
            vec![3, 4],
            vec![5],
            vec![5],
            std::iter::once(6).collect(),
            vec![],
        ];
        let opaque = [false; 7];

        assert_eq!(reachable_ids(&edges, &opaque), brute_force_reachable(&edges, &opaque));
    }

    #[test]
    fn an_opaque_node_taints_itself_and_everything_that_can_reach_it() {
        // 0 -> 1 -> 2, and 2 has an unresolvable path dependency: 2 must reach everything, and so
        // must 0 and 1, since both can reach 2.
        let edges = vec![vec![1], vec![2], vec![]];
        let opaque = [false, false, true];
        let reach = reachable_ids(&edges, &opaque);

        for member in [0, 1, 2] {
            assert_eq!(reach[member], [0, 1, 2].into_iter().collect::<HashSet<_>>(), "member {member}");
        }
    }

    #[test]
    fn an_opaque_member_of_a_cycle_taints_the_whole_component() {
        // 0 <-> 1 form one component; only 1 is directly opaque, but the collapse means neither
        // member can be told apart from the other, so both fall back to reaching every workspace
        // member, exactly as a single opaque node does on its own. Node 2 is untouched: nothing in
        // its own graph, nor anything that can reach it, is opaque.
        let edges = vec![vec![1], vec![0], vec![]];
        let opaque = [false, true, false];
        let reach = reachable_ids(&edges, &opaque);

        assert_eq!(reach[0], [0, 1, 2].into_iter().collect::<HashSet<_>>());
        assert_eq!(reach[1], [0, 1, 2].into_iter().collect::<HashSet<_>>());
        assert_eq!(reach[2], std::iter::once(2).collect::<HashSet<_>>());
    }

    #[test]
    fn randomized_graphs_with_cycles_and_opaque_nodes_match_the_brute_force_oracle() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;

        for trial in 0..200 {
            let node_count = 1 + usize::try_from(xorshift(&mut state) % 12).unwrap_or(0);
            let mut edges: Vec<Vec<usize>> = vec![Vec::new(); node_count];

            for (from, out_edges) in edges.iter_mut().enumerate() {
                for to in 0..node_count {
                    if from != to && xorshift(&mut state).is_multiple_of(3) {
                        out_edges.push(to);
                    }
                }
            }

            let opaque: Vec<bool> = (0..node_count).map(|_| xorshift(&mut state).is_multiple_of(10)).collect();

            assert_eq!(
                reachable_ids(&edges, &opaque),
                brute_force_reachable(&edges, &opaque),
                "trial {trial} with {node_count} nodes: {edges:?}, opaque {opaque:?}"
            );
        }
    }

    #[test]
    fn external_path_dependencies_and_local_build_scripts_are_cache_inputs() {
        let directory = crate::testing::workdir("survey-external-inputs-");
        let container = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        let root = container.join("workspace");
        let dependency = container.join("dependency");

        fs::create_dir_all(root.join("src")).expect("workspace source");
        fs::create_dir_all(dependency.join("src")).expect("dependency source");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\n\n[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndependency = { path = \"../dependency\" }\n",
        )
        .expect("workspace manifest");
        fs::write(root.join("src/lib.rs"), "pub fn workspace() {}\n").expect("workspace source");
        fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("dependency manifest");
        fs::write(dependency.join("src/lib.rs"), "pub fn dependency() {}\n").expect("dependency source");
        fs::write(dependency.join("build.rs"), "fn main() {}\n").expect("dependency build script");

        let inputs = external_path_inputs(&root, &FeatureArgs::default(), &root).expect("metadata");

        assert_eq!(inputs.roots, vec![crate::paths::physical(&dependency).expect("dependency path")]);
        assert!(
            inputs.has_build_scripts,
            "a local build script can read external paths Cargo metadata does not enumerate"
        );
    }

    #[test]
    fn a_registry_build_script_makes_the_snapshot_uncacheable() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut command = MetadataCommand::new();
        let _builder = command.current_dir(root);
        let metadata = command.exec().expect("workspace metadata");
        let blake3 = metadata
            .packages
            .iter()
            .find(|package| package.name == "blake3")
            .expect("the workspace resolves blake3");

        assert!(blake3.source.is_some(), "blake3 must be a registry package");
        assert!(
            blake3.targets.iter().any(|target| target.kind.contains(&TargetKind::CustomBuild)),
            "blake3 must expose its build script in metadata"
        );

        let workspace = Utf8Path::new(metadata.workspace_root.as_str());
        let inputs = external_path_inputs(workspace, &FeatureArgs::default(), workspace).expect("metadata");

        assert!(
            inputs.has_build_scripts,
            "a registry build script can read uncaptured inputs, so its snapshot must be incomplete"
        );
    }

    #[test]
    fn discovery_refuses_inline_and_file_cargo_configuration_before_metadata() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            ..SelectArgs::default()
        };

        for extra in [
            vec!["--config".to_owned(), "build.target = \"wasm32-wasip1\"".to_owned()],
            vec!["--config=outside.toml".to_owned()],
        ] {
            let failure = Survey::for_build(
                &args,
                None,
                &CargoOptions {
                    extra,
                    ..CargoOptions::default()
                },
            )
            .expect_err("unmodelled Cargo configuration must stop discovery");

            assert!(failure.is_usage(), "{failure}");
            assert!(failure.to_string().contains("--config"), "{failure}");
        }
    }

    #[test]
    fn discovery_propagates_a_configuration_failure() {
        let (_directory, root) = workspace();
        let config = root.join("gamma.toml");

        fs::create_dir_all(config.parent().expect("configuration parent")).expect("configuration parent");
        fs::write(&config, "cargo-args = [\n").expect("malformed configuration");
        let failure = Survey::new(
            &SelectArgs {
                dir: root,
                ..SelectArgs::default()
            },
            None,
        )
        .expect_err("configuration failure must not become default discovery settings");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("gamma.toml"), "{failure}");
    }

    #[test]
    fn a_narrow_survey_keeps_unselected_dependency_roots() {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"3\"\n");
        write(
            &root,
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nb = { path = \"../b\" }\n",
        );
        write(&root, "a/src/lib.rs", "pub fn a() -> bool { b::b() }\n");
        write(
            &root,
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&root, "b/src/lib.rs", "pub fn b() -> bool { true }\n");

        let survey = Survey::for_build(
            &SelectArgs {
                dir: root,
                packages: vec!["a".to_owned()],
                ..SelectArgs::default()
            },
            None,
            &CargoOptions::default(),
        )
        .expect("the narrowed workspace should survey");

        assert_eq!(survey.selected, ["a"]);
        assert_eq!(survey.specs.get("a").map(|(path, _version)| path.as_str()), Some("a"));
        assert_eq!(survey.specs.get("b").map(|(path, _version)| path.as_str()), Some("b"));
    }

    #[test]
    fn a_package_that_depends_through_a_non_member_reaches_the_whole_workspace() {
        // Regression, issue-006. `cargo metadata --no-deps` does not list packages outside the
        // workspace, so an `app -> facade -> core` chain through a path dependency that is not a
        // member is invisible. Concluding "app does not reach core" from a graph with a hole in it
        // means never running app's tests against a mutant in core, and scoring that mutant
        // uncovered when a test does in fact cover it. Reaching everything is the fail-open answer.
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"app\"]\nexclude = [\"facade\"]\nresolver = \"3\"\n",
        );
        write(
            &root,
            "core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(&root, "core/src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n");

        // Outside the workspace on purpose: this is the package the metadata cannot see through.
        write(
            &root,
            "facade/Cargo.toml",
            "[package]\nname = \"facade\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\ncore = { path = \"../core\" }\n\n[workspace]\n",
        );
        write(&root, "facade/src/lib.rs", "pub use core::add;\n");

        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nfacade = { path = \"../facade\" }\n",
        );
        write(&root, "app/src/lib.rs", "pub fn go() -> i32 { facade::add(1, 2) }\n");

        let metadata = load_metadata(&root, &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);
        let from_app = reach.get("app").expect("app is a workspace member");

        assert!(from_app.contains("core"), "{from_app:?}");

        // The package with no such dependency keeps its exact reach: fail-open must not become
        // "everything reaches everything", which would run every binary for every mutant.
        let from_core = reach.get("core").expect("core is a workspace member");

        assert!(!from_core.contains("app"), "{from_core:?}");
    }

    /// `stages()` orders packages by the size of their reach sets, which is a topological order
    /// only because a dependency's reach set is a subset of its dependent's. An opaque package
    /// reaching every member breaks that on its own: a dependent that cannot reach some third
    /// member has the *smaller* set and sorts ahead of the package it depends on. So the union
    /// travels to the dependents too, and the sort keeps its premise.
    #[test]
    fn a_dependent_of_an_opaque_package_is_never_ordered_before_it() {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"opaque\", \"dependent\", \"unrelated\"]\nexclude = [\"outside\"]\nresolver = \"3\"\n",
        );

        // Outside the workspace, so the metadata cannot see through it and `opaque` reaches
        // everything rather than risk missing a path back into the workspace.
        write(
            &root,
            "outside/Cargo.toml",
            "[package]\nname = \"outside\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        );
        write(&root, "outside/src/lib.rs", "pub fn f() {}\n");

        write(
            &root,
            "opaque/Cargo.toml",
            "[package]\nname = \"opaque\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\noutside = { path = \"../outside\" }\n",
        );
        write(&root, "opaque/src/lib.rs", "pub fn f() {}\n");

        write(
            &root,
            "dependent/Cargo.toml",
            "[package]\nname = \"dependent\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nopaque = { path = \"../opaque\" }\n",
        );
        write(&root, "dependent/src/lib.rs", "pub fn f() {}\n");

        write(
            &root,
            "unrelated/Cargo.toml",
            "[package]\nname = \"unrelated\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(&root, "unrelated/src/lib.rs", "pub fn f() {}\n");

        let metadata = load_metadata(&root, &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);
        let of = |name: &str| reach.get(name).cloned().unwrap_or_default();

        assert!(of("dependent").is_superset(&of("opaque")), "{reach:?}");

        let packages = vec!["opaque".to_owned(), "dependent".to_owned(), "unrelated".to_owned()];
        let stages = crate::discover::stages(&packages, &reach);
        let place = |name: &str| {
            stages
                .iter()
                .position(|stage| stage.iter().any(|member| member == name))
                .expect("every package is placed somewhere")
        };

        assert!(place("opaque") <= place("dependent"), "{stages:?}");
    }

    /// Cargo acts on the package that owns the directory it was invoked from, and mutation testing
    /// has far more reason to follow that rule than `cargo build` does: an unasked-for member costs
    /// a full build and test suite per mutant.
    #[test]
    fn a_run_from_inside_a_member_selects_that_member_alone() {
        let home = TempDir::new().expect("could not create a temporary directory");
        let root = Utf8Path::from_path(home.path()).expect("the temporary path is not UTF-8");

        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"2\"\n",
        );
        write(
            root,
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "alpha/src/lib.rs", "pub fn a() {}\n");
        write(
            root,
            "beta/Cargo.toml",
            "[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "beta/src/lib.rs", "pub fn b() {}\n");

        let metadata = load_metadata(root, &FeatureArgs::default()).expect("metadata");

        let inside = SelectArgs {
            dir: root.join("alpha"),
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &inside), once("alpha".to_owned()).collect());

        // A subdirectory of a member is still that member, which is where anyone actually stands.
        let deeper = SelectArgs {
            dir: root.join("alpha/src"),
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &deeper), once("alpha".to_owned()).collect());
    }

    /// `--workspace` is what widens the run, and `--package` still names the selection outright.
    #[test]
    fn the_whole_workspace_is_selected_only_when_asked_for() {
        let home = TempDir::new().expect("could not create a temporary directory");
        let root = Utf8Path::from_path(home.path()).expect("the temporary path is not UTF-8");

        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"2\"\n",
        );
        write(
            root,
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "alpha/src/lib.rs", "pub fn a() {}\n");
        write(
            root,
            "beta/Cargo.toml",
            "[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "beta/src/lib.rs", "pub fn b() {}\n");

        let metadata = load_metadata(root, &FeatureArgs::default()).expect("metadata");
        let both: HashSet<String> = ["alpha".to_owned(), "beta".to_owned()].into_iter().collect();

        let whole = SelectArgs {
            dir: root.join("alpha"),
            workspace: true,
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &whole), both);

        let named = SelectArgs {
            dir: root.join("alpha"),
            packages: vec!["beta".to_owned()],
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &named), once("beta".to_owned()).collect());

        // At the root of a virtual manifest no package owns the directory, and with no
        // `default-members` declared cargo means all of them.
        let outside = SelectArgs {
            dir: root.to_owned(),
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &outside), both);
    }

    /// At the workspace root the default members decide, exactly as they do for a bare `cargo test`.
    #[test]
    fn the_workspace_root_honours_the_default_members() {
        let home = TempDir::new().expect("could not create a temporary directory");
        let root = Utf8Path::from_path(home.path()).expect("the temporary path is not UTF-8");

        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"alpha\", \"beta\"]\ndefault-members = [\"alpha\"]\nresolver = \"2\"\n",
        );
        write(
            root,
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "alpha/src/lib.rs", "pub fn a() {}\n");
        write(
            root,
            "beta/Cargo.toml",
            "[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "beta/src/lib.rs", "pub fn b() {}\n");

        let metadata = load_metadata(root, &FeatureArgs::default()).expect("metadata");

        let at_root = SelectArgs {
            dir: root.to_owned(),
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &at_root), once("alpha".to_owned()).collect());
    }

    /// The sibling of the test above, with a root that is itself a package. Cargo's own
    /// `default_members` answers `["alpha"]` here too, but an `owning_package` lookup consulted
    /// first finds the root package and pre-empts it — so a bare `cargo gamma` mutates only the
    /// root while a bare `cargo test` runs `alpha`.
    #[test]
    fn a_root_package_workspace_honours_the_default_members() {
        let home = TempDir::new().expect("could not create a temporary directory");
        let root = Utf8Path::from_path(home.path()).expect("the temporary path is not UTF-8");

        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"host\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [workspace]\nmembers = [\"alpha\"]\ndefault-members = [\"alpha\"]\nresolver = \"2\"\n",
        );
        write(root, "src/lib.rs", "pub fn h() {}\n");
        write(
            root,
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(root, "alpha/src/lib.rs", "pub fn a() {}\n");

        let metadata = load_metadata(root, &FeatureArgs::default()).expect("metadata");

        let at_root = SelectArgs {
            dir: root.to_owned(),
            ..SelectArgs::default()
        };

        assert_eq!(selected_packages(&metadata, &at_root), once("alpha".to_owned()).collect());
    }

    /// Cargo accepts a member outside the workspace root. `TargetFile::path` is documented as
    /// relative, and the filesystem layer's containment rests on that: `Utf8Path::join` with an
    /// absolute argument *replaces* the base, so an absolute path here sends every instrumented
    /// write into the user's real source tree instead of the scratch copy.
    #[test]
    fn a_member_outside_the_workspace_root_is_refused_rather_than_given_an_absolute_path() {
        let home = TempDir::new().expect("could not create a temporary directory");
        let base = Utf8Path::from_path(home.path()).expect("the temporary path is not UTF-8");
        let root = base.join("w");

        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"../sibling\"]\nresolver = \"2\"\n");
        write(
            base,
            "sibling/Cargo.toml",
            "[package]\nname = \"sibling\"\nversion = \"0.0.0\"\nedition = \"2024\"\nworkspace = \"../w\"\n",
        );
        write(base, "sibling/src/lib.rs", "pub fn s() -> i32 { 1 + 2 }\n");

        let args = SelectArgs {
            dir: root,
            ..SelectArgs::default()
        };

        let failure = Survey::new(&args, None).expect_err("a member outside the root must be refused");

        assert!(failure.to_string().contains("outside the workspace root"), "{failure}");
    }

    #[test]
    fn a_registry_dependency_does_not_make_a_package_opaque() {
        // Regression, issue-006. Only a *path* dependency can lead back into the workspace. Marking
        // a package opaque for an ordinary crates.io dependency would make almost every real
        // workspace reach everything, undoing the scoping this whole graph exists for.
        let metadata = load_metadata(Utf8Path::new(env!("CARGO_MANIFEST_DIR")), &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);
        let from_library = reach.get("cargo-gamma-lib").expect("the library is a workspace member");

        assert!(
            !from_library.contains("cargo-gamma"),
            "this crate has many registry dependencies: {from_library:?}"
        );
    }

    /// A mutant whose package never had a file walked for it — because the mutant's own package
    /// name is wrong, or the two lists have simply drifted apart — must not silently inflate
    /// another package's counts. If it landed anywhere, the per-package report would blame the
    /// wrong crate for a mutant it never produced.
    #[test]
    fn a_mutant_from_a_package_no_file_was_walked_for_does_not_inflate_anothers_count() {
        let files = [TargetFile {
            path: Utf8PathBuf::from("core/src/lib.rs"),
            absolute: Utf8PathBuf::from("/tree/core/src/lib.rs"),
            package: "core".to_owned(),
        }];
        let mutants = [counting_mutant("core"), counting_mutant("ghost")];

        let mut lines = Vec::new();
        report_by_package(&files, &mutants, &mut |line: &str| lines.push(line.to_owned()));

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("core, 1 mutant in 1"), "{lines:?}");
    }

    ///
    /// `core` is a plain library, `app` is a binary that depends on it and also carries an example
    /// and an integration test — which is what makes it useful here, since those are exactly the
    /// target kinds the survey has to walk past.
    fn workspace() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"3\"\n",
        );

        write(
            &root,
            "core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n",
        );
        write(&root, "core/src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        write(&root, "core/src/generated.rs", "pub fn scale(x: i32) -> i32 {\n    x * 2\n}\n");

        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
        );
        write(&root, "app/src/main.rs", "fn main() {\n    let _ = 1 + 1;\n}\n");
        write(&root, "app/examples/demo.rs", "fn main() {\n    let _ = 2 + 2;\n}\n");
        write(&root, "app/tests/it.rs", "#[test]\nfn works() {\n    assert_eq!(1 + 1, 2);\n}\n");

        (directory, root)
    }

    /// Scanning package by package must reach exactly the population one whole-workspace scan does.
    ///
    /// The per-package selection is an index rather than a filter, because a filter walks every
    /// file in the workspace once per package. An index can disagree with the list it indexes in
    /// two ways a filter cannot — by missing a file, and by ordering them differently — and the
    /// second is the silent one, because path order is what makes the population deterministic.
    #[test]
    fn scanning_a_package_at_a_time_reaches_the_same_population_as_scanning_the_workspace() {
        let (_directory, root) = wide_workspace();
        let survey = survey(&root, SelectArgs::default());
        let selection = Selection::parse("all").expect("every mutator resolves");

        let mut ordinals = 0;
        let whole = survey.scan(None, &selection, &mut ordinals).expect("the fixture must scan");

        let mut ordinals = 0;
        let mut piecemeal: Vec<MutantId> = Vec::new();

        for package in survey.packages() {
            let scanned = survey
                .scan(Some(&package), &selection, &mut ordinals)
                .expect("the fixture must scan");

            piecemeal.extend(scanned.mutants.iter().map(|mutant| mutant.id.clone()));
        }

        let mut expected: Vec<MutantId> = whole.mutants.iter().map(|mutant| mutant.id.clone()).collect();
        let mut found = piecemeal.clone();

        expected.sort();
        found.sort();

        assert_eq!(found, expected, "package-by-package scanning reached a different population");

        // A package the workspace does not have is empty rather than the whole workspace, which is
        // what a lookup gets wrong if it falls back to the unfiltered list.
        let mut ordinals = 0;
        let absent = survey
            .scan(Some("nothing-by-this-name"), &selection, &mut ordinals)
            .expect("scanning nothing is not an error");

        assert!(absent.mutants.is_empty(), "{:?}", absent.mutants);
    }

    #[test]
    fn every_mutant_exclusion_rule_must_match() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "trait Diagnostic { fn value(&self) -> i32; }\n\
             struct Message;\n\
             impl Diagnostic for Message { fn value(&self) -> i32 { 1 + 2 } }\n",
        );

        let failure = Survey::new(
            &SelectArgs {
                dir: root,
                exclude_trait_impls: vec!["Diagnostic".to_owned(), "Diagnostc".to_owned()],
                ..SelectArgs::default()
            },
            None,
        )
        .expect_err("one unmatched entry must refuse the selection");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("entry 2"), "{failure}");
        assert!(failure.to_string().contains("Diagnostc"), "{failure}");
        assert!(failure.to_string().contains("matched no trait implementations"), "{failure}");
    }

    #[test]
    fn exclusion_validation_matches_implementations_without_selected_mutants() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "trait Diagnostic { fn write(&self); }\n\
             struct Message;\n\
             impl Diagnostic for Message { fn write(&self) {} }\n\
             pub fn compare(left: i32, right: i32) -> bool { left < right }\n",
        );

        let survey = survey(
            &root,
            SelectArgs {
                exclude_trait_impls: vec!["Diagnostic".to_owned()],
                ..SelectArgs::default()
            },
        );
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("relational").expect("selection"), &mut ordinals)
            .expect("the implementation exists even though it contains no selected mutant");

        assert!(!scanned.mutants.is_empty(), "the ordinary relational mutant remains selected");
    }

    #[test]
    fn package_scans_apply_exclusions_after_workspace_validation() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "trait Diagnostic { fn value(&self) -> i32; }\n\
             struct Message;\n\
             impl Diagnostic for Message { fn value(&self) -> i32 { 1 + 2 } }\n",
        );

        let survey = survey(
            &root,
            SelectArgs {
                exclude_trait_impls: vec!["Diagnostic".to_owned()],
                ..SelectArgs::default()
            },
        );
        let packages = survey.packages();

        assert_eq!(packages, ["app", "core"], "the non-matching package must be scanned first");

        let selection = Selection::parse("arith.add_to_sub").expect("selection");
        let mut ordinals = 0;
        let first = survey
            .scan(Some(&packages[0]), &selection, &mut ordinals)
            .expect("workspace validation already established the name");
        let second = survey
            .scan(Some(&packages[1]), &selection, &mut ordinals)
            .expect("the matching package is scanned normally");

        assert!(!first.mutants.is_empty(), "the first package still contributes ordinary mutants");
        let excluded: Vec<_> = second
            .mutants
            .iter()
            .filter(|mutant| {
                mutant.suppression.as_ref().is_some_and(|suppression| {
                    suppression.channel == Channel::Config
                        && suppression
                            .reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("trait implementation `Diagnostic`"))
                })
            })
            .collect();

        assert!(!excluded.is_empty(), "the excluded implementation remains visible");
        assert!(excluded.iter().all(|mutant| mutant.outcome == Outcome::Ignored));
        assert!(excluded.iter().all(|mutant| {
            mutant
                .suppression
                .as_ref()
                .is_some_and(|suppression| suppression.channel == Channel::Config)
        }));
        assert_eq!(second.suppressed, excluded.len());
    }

    #[test]
    fn exclusion_validation_is_independent_of_package_selection() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "struct Message;\n\
             impl core::fmt::Debug for Message {\n\
                 fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {\n\
                     formatter.write_str(\"message\")\n\
                 }\n\
             }\n",
        );

        let survey = Survey::new(
            &SelectArgs {
                dir: root,
                packages: vec!["app".to_owned()],
                exclude_trait_impls: vec!["Debug".to_owned()],
                ..SelectArgs::default()
            },
            None,
        )
        .expect("a workspace-wide exclusion may match outside the selected package");

        assert_eq!(survey.packages(), ["app"]);
    }

    /// A workspace whose shapes exercise the deduplication and graph-walking paths.
    ///
    /// `core` has both a library and a binary rooted in the same directory, so its files are
    /// walked twice; `mid` and `app` both depend on `core`, so the reachability walk meets it
    /// twice; and `core` reaches a module only through `#[cfg(test)]`.
    fn wide_workspace() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"mid\", \"app\"]\nresolver = \"3\"\n",
        );

        write(
            &root,
            "core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"core-cli\"\npath = \"src/main.rs\"\n",
        );
        write(
            &root,
            "core/src/lib.rs",
            "#[cfg(test)]\nmod helpers;\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );
        write(&root, "core/src/main.rs", "fn main() {\n    let _ = 1 + 1;\n}\n");
        write(&root, "core/src/helpers.rs", "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n");

        write(
            &root,
            "mid/Cargo.toml",
            "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
        );
        write(&root, "mid/src/lib.rs", "pub fn triple(x: i32) -> i32 {\n    x * 3\n}\n");

        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\nmid = { path = \"../mid\" }\n",
        );
        write(&root, "app/src/main.rs", "fn main() {\n    let _ = 2 + 2;\n}\n");

        (directory, root)
    }

    /// A workspace wide enough that the scan's parallelism and its collision handling both matter.
    ///
    /// The filler files are byte-identical on purpose. Identical text in different files is what
    /// exercises the duplicate screen, whose winner depends on visit order, and thirty-two of them
    /// is enough that the workers finish in a different order from one run to the next.
    fn repeatable_workspace() -> (TempDir, Utf8PathBuf) {
        let (directory, root) = wide_workspace();

        for at in 0..32 {
            write(
                &root,
                &format!("core/src/filler{at}.rs"),
                "pub fn value(x: i32) -> i32 {\n    x + 1\n}\n",
            );
        }

        // One suppressed site, so that the withheld set is part of what is being compared rather
        // than being uniformly empty.
        write(
            &root,
            "mid/src/quiet.rs",
            "#[gamma::skip(reason = \"the population has to be stable including what was withheld\")]\npub fn quiet(x: i32) -> i32 {\n    x - 1\n}\n",
        );

        (directory, root)
    }

    /// Everything about a mutant that two runs over an unchanged tree have to agree on.
    fn fingerprint(mutant: &Mutant) -> String {
        format!(
            "{}|{}|{}|{}|{:?}|{}|{}|{}|{}|{:?}|{:?}|{:?}",
            mutant.ordinal,
            mutant.id,
            mutant.file,
            mutant.package,
            mutant.span,
            mutant.line,
            mutant.column,
            mutant.mutator,
            mutant.item_path,
            mutant.shape,
            mutant.outcome,
            mutant.replacement,
        )
    }

    /// Two scans of an unchanged tree produce the same population, in the same order.
    ///
    /// Ordering is stabilized within a file and identifiers are stable for identical input, but
    /// neither of those covers the two places order is decided across files: the per-worker partial
    /// results are merged in completion order, and the duplicate screen resolves a collision in
    /// favour of whichever site it visited first. Both are load-bearing — ordinals are assigned
    /// from the merged order, and `--iterate`, sharding and every run-to-run comparison the tool
    /// invites are built on the population being the same population.
    ///
    /// The whole plan is compared, not just the identifiers: an ordinal, a span or a replacement
    /// that moved would leave the identifier set intact and still make two reports incomparable.
    ///
    /// Three scans rather than two, because a merge that depends on completion order does not
    /// necessarily disagree on the first attempt.
    #[test]
    fn scanning_an_unchanged_tree_twice_produces_the_same_population() {
        let (_directory, root) = repeatable_workspace();
        let selection = Selection::parse("all").expect("every mutator resolves");

        let mut populations = Vec::new();
        let mut counts = Vec::new();

        for _ in 0..3 {
            let survey = survey(&root, SelectArgs::default());
            let mut ordinals = 0;
            let scanned = survey.scan(None, &selection, &mut ordinals).expect("the fixture must scan");
            let plan = survey.into_plan(scanned);

            counts.push((
                plan.files.iter().map(|file| file.path.to_string()).collect::<Vec<_>>(),
                plan.suppressed,
                plan.sharded_out,
                plan.settled_out,
            ));
            populations.push(plan.mutants.iter().map(fingerprint).collect::<Vec<_>>());
        }

        assert!(
            !populations[0].is_empty(),
            "the fixture produced no mutants, so this proves nothing"
        );
        assert!(counts[0].1 > 0, "the fixture suppressed nothing, so the withheld set is untested");

        assert_eq!(populations[1], populations[0], "the second scan found a different population");
        assert_eq!(populations[2], populations[0], "the third scan found a different population");
        assert_eq!(counts[1], counts[0], "the second scan disagreed about the files or the counts");
        assert_eq!(counts[2], counts[0], "the third scan disagreed about the files or the counts");
    }

    /// A workspace wide enough to keep several survey workers busy, with one file that kills the
    /// worker unlucky enough to claim it.
    fn probe_workspace() -> (TempDir, Utf8PathBuf) {
        let (directory, root) = wide_workspace();

        for at in 0..64 {
            write(
                &root,
                &format!("core/src/filler{at}.rs"),
                "pub fn value(x: i32) -> i32 {\n    x + 1\n}\n",
            );
        }

        write(&root, &format!("core/src/{PANIC_PROBE}"), "pub fn nothing() {}\n");

        (directory, root)
    }

    /// A worker that dies while parsing still has two barrier waits ahead of it, and every other
    /// worker is blocked on them. If it unwinds straight past, they wait forever, `thread::scope`
    /// waits on them, and the panic is never re-raised — the run hangs with no diagnosis.
    ///
    /// Run under the shared watchdog, because the failure this guards against is a hang and an
    /// unbounded test would take the whole suite down with it rather than reporting.
    #[test]
    fn a_worker_that_panics_while_parsing_does_not_wedge_the_others_at_the_barrier() {
        let (directory, root) = probe_workspace();

        let panicked = crate::testing::within(crate::testing::WATCHDOG, "the scan", move || {
            catch_unwind(AssertUnwindSafe(|| {
                let survey = survey(&root, SelectArgs::default());
                let mut ordinals = 0;

                survey
                    .scan(None, &Selection::parse("@all").expect("selection"), &mut ordinals)
                    .map(|scanned| scanned.mutants.len())
            }))
            .is_err()
        });

        assert!(panicked, "the worker's panic must reach the caller, not be swallowed");

        // Kept alive until here rather than dropped at the top: the fixture tree has to outlive the
        // scan that reads it, and the watchdog's body owns everything it touches.
        drop(directory);
    }

    fn write(root: &Utf8Path, relative: &str, text: &str) {
        let path = root.join(relative);

        fs::create_dir_all(path.parent().expect("every fixture path has a parent").as_std_path())
            .expect("could not create the fixture directory");
        fs::write(path.as_std_path(), text).expect("could not write the fixture file");
    }

    /// The walk is the sole producer of the candidate file list, and a walk error is per entry: a
    /// directory that cannot be read yields one error and then no descendants at all. Swallowing it
    /// takes that whole subtree out of the population and out of the denominator, and reports a
    /// better score for it.
    #[test]
    fn a_directory_that_cannot_be_walked_is_reported_rather_than_passed_off_as_empty() {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");
        let missing = root.join("vanished");
        let error = walk_rust_files(&missing).expect_err("a directory that is not there cannot have been walked");

        assert!(error.to_string().contains("vanished"), "{error}");

        // The same walk over a directory that is really there still lists what it holds.
        write(&root, "here/one.rs", "pub fn f() {}\n");
        write(&root, "here/notes.txt", "not a source file\n");

        let listed = walk_rust_files(&root.join("here")).expect("a readable directory walks");

        assert_eq!(listed, vec![root.join("here/one.rs")]);
    }

    /// A source file whose path cannot be spelled as UTF-8 is one this run would have mutated and
    /// now cannot name, so it is refused rather than quietly left out of the population. A file of
    /// any other kind was never in the population and its spelling decides nothing.
    #[cfg(unix)]
    #[test]
    fn a_source_file_with_a_path_that_is_not_utf8_is_reported_rather_than_dropped() {
        use std::os::unix::ffi::OsStrExt;

        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(&root, "src/lib.rs", "pub fn f() {}\n");

        let odd = directory.path().join("src").join(OsStr::from_bytes(b"lat\xffin.rs"));

        fs::write(&odd, "pub fn g() {}\n").expect("the fixture file is written");

        let error = walk_rust_files(&root.join("src")).expect_err("a source file nobody can name is not skipped");

        assert!(error.to_string().contains("not UTF-8"), "{error}");

        // Renamed to something that is not a source file, it is out of the population anyway.
        fs::rename(&odd, directory.path().join("src").join(OsStr::from_bytes(b"lat\xffin.txt"))).expect("renamed");

        let listed = walk_rust_files(&root.join("src")).expect("a file that is not Rust says nothing about the walk");

        assert_eq!(listed, vec![root.join("src/lib.rs")]);
    }

    fn survey(root: &Utf8Path, args: SelectArgs) -> Survey {
        Survey::new(
            &SelectArgs {
                dir: root.to_owned(),
                ..args
            },
            None,
        )
        .expect("the fixture workspace must survey")
    }

    /// A malformed suppression directive is a mistake in the user's own source, and it has to stop
    /// the scan with the same named error `suppress::directives` itself would report, rather than
    /// being swallowed by the parallel scan and the mutant simply going unsuppressed with nothing
    /// to explain why: the directive is why the file was written, and a typo in it should not
    /// silently disable itself.
    #[test]
    fn an_unknown_suppression_directive_fails_the_scan_rather_than_being_ignored() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "#[gamma::note]\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;

        let error = survey
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect_err("the directive names no known intent");

        assert!(error.to_string().contains("unknown directive"), "{error}");
    }

    /// A file this tool cannot walk is left out and named, rather than taking the workspace with it.
    ///
    /// The nesting guard exists because a recursive descent over deep enough source runs out of
    /// stack, and `rustc` builds files far deeper than this tool can walk — a generated parser
    /// table or a macro-expanded literal is the ordinary case. So this is the one parse failure
    /// that says nothing about whether the workspace is sound, and refusing the run over it makes
    /// a valid workspace unmeasurable. The file is skipped, and the skip reaches the plan so that
    /// the score is read knowing which code is not in it: a file dropped in silence is
    /// indistinguishable from a file with nothing worth mutating.
    #[test]
    fn a_file_too_deep_to_walk_is_left_out_by_name_rather_than_stopping_the_scan() {
        let (_directory, root) = workspace();

        let deep = too_deep_source();

        write(&root, "core/src/deep.rs", &deep);
        write(
            &root,
            "core/src/lib.rs",
            "mod deep;\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;

        let scanned = survey
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect("a file this tool cannot walk does not make the workspace unmeasurable");

        assert_eq!(scanned.skipped.len(), 1, "{:?}", scanned.skipped);

        let note = scanned.skipped.first().expect("one file was skipped");

        assert!(note.contains("deep.rs"), "the skip does not name the file: {note}");
        assert!(note.contains("nests deeper"), "the skip does not say why: {note}");

        // The rest of the package is still measured, which is the whole point of skipping rather
        // than refusing.
        assert!(
            scanned.mutants.iter().any(|mutant| mutant.file.as_str().ends_with("lib.rs")),
            "{:?}",
            scanned.mutants.iter().map(|mutant| mutant.file.as_str()).collect::<Vec<_>>()
        );
    }

    /// Source deliberately deeper than the parser accepts, without reading the implementation
    /// limit: these regressions exercise how a refused file is handled, not where refusal starts.
    fn too_deep_source() -> String {
        let depth = 512;

        format!("pub fn deep() -> i32 {{\n    {}1{}\n}}\n", "(".repeat(depth), ")".repeat(depth))
    }

    /// The same deeply nested file, reached only as a module declaration, must be skipped rather
    /// than fail the run.
    ///
    /// Narrowing `--files` moves a file out of the mutating population and into the
    /// declaration-only scan, which exists purely to learn which modules are test-only or
    /// inactive. If that scan propagated the nesting-limit error, the very same buildable
    /// workspace would measure partially under a wide selection and refuse to run at all under a
    /// narrow one — a selection flag deciding whether the tree is sound. Both selections must
    /// complete, and both must name the same file for the same reason.
    #[test]
    fn a_file_too_deep_to_walk_is_skipped_the_same_way_when_only_its_declarations_are_needed() {
        let (_directory, root) = workspace();

        let deep = too_deep_source();

        write(&root, "core/src/deep.rs", &deep);
        write(
            &root,
            "core/src/lib.rs",
            "mod deep;\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let scan_with = |args: SelectArgs| {
            let survey = survey(&root, args);
            let mut ordinals = 0;

            survey
                .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
                .expect("a file this tool cannot walk does not make the workspace unmeasurable")
        };

        let wide = scan_with(SelectArgs::default());

        // Narrow enough that `core/src/deep.rs` is no longer mutated, and so is read only for the
        // module declarations it might contain.
        let narrow = scan_with(SelectArgs {
            files: vec!["core/src/lib.rs".to_owned()],
            ..SelectArgs::default()
        });

        assert_eq!(narrow.skipped, wide.skipped, "the same file must be reported either way");
        assert_eq!(narrow.skipped.len(), 1, "{:?}", narrow.skipped);

        let note = narrow.skipped.first().expect("one file was skipped");

        assert!(note.contains("deep.rs"), "the skip does not name the file: {note}");
        assert!(note.contains("nests deeper"), "the skip does not say why: {note}");

        // And the narrowed selection still measures what it did select, rather than being emptied
        // by the file it could not read.
        assert!(
            narrow.mutants.iter().any(|mutant| mutant.file.as_str().ends_with("lib.rs")),
            "{:?}",
            narrow.mutants.iter().map(|mutant| mutant.file.as_str()).collect::<Vec<_>>()
        );
    }

    /// A stated value the tool cannot honour stops the scan for the same reason a misspelled
    /// suppression does: the author wrote it believing it was working. The compiler catches this
    /// too, but not until a mutant is built, and a run that quietly collected the site's guessed
    /// values in the meantime would report a mutation score computed from mutants the author had
    /// already said were the wrong question.
    #[test]
    fn a_stated_value_that_cannot_be_honoured_fails_the_scan_rather_than_being_ignored() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "#[gamma::value(1, 2)]\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;

        let error = survey
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect_err("two expressions are not one value");

        assert!(error.to_string().contains("states one value"), "{error}");
        assert!(error.to_string().contains("core/src/lib.rs:1"), "{error}");
    }

    /// A stated value that is well formed is collected rather than reported.
    ///
    /// The other half of the check: a rule that fires on correct source is worse than no rule,
    /// because the only way out of it is to stop using the feature.
    #[test]
    fn a_stated_value_reaches_the_population_as_a_mutant_of_its_own() {
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "#[gamma::value(a - b)]\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("fn_value").expect("every mutator resolves"), &mut ordinals)
            .expect("a well-formed stated value is not an error");

        let stated: Vec<&Mutant> = scanned
            .mutants
            .iter()
            .filter(|mutant| &*mutant.mutator == "fn_value.stated")
            .collect();

        assert_eq!(stated.len(), 1, "{:?}", scanned.mutants);
        assert_eq!(stated[0].replacement, "a - b");
    }

    /// A file walked once per target still appears once, and test-only modules never appear.
    #[test]
    fn files_reached_twice_are_listed_once_and_test_only_modules_are_dropped() {
        let (_directory, root) = wide_workspace();
        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("@all").expect("selection"), &mut ordinals)
            .expect("scan");
        let plan = survey.into_plan(scanned);

        let listed: Vec<&Utf8PathBuf> = plan.files.iter().map(|file| &file.path).collect();
        let core_lib = Utf8PathBuf::from("core/src/lib.rs");

        assert_eq!(listed.iter().filter(|path| ****path == core_lib).count(), 1, "{listed:?}");

        // `helpers.rs` is only ever reached through `#[cfg(test)] mod helpers;`, so it is test
        // code however ordinary it looks, and none of its mutants belong in the population.
        assert!(
            !plan.mutants.iter().any(|mutant| mutant.file.as_str().ends_with("helpers.rs")),
            "{:?}",
            plan.mutants.iter().map(|mutant| &mutant.file).collect::<Vec<_>>()
        );
    }

    /// A package two others depend on is visited once, however many paths lead to it.
    #[test]
    fn a_package_reached_by_two_paths_is_walked_once() {
        let (_directory, root) = wide_workspace();
        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("@all").expect("selection"), &mut ordinals)
            .expect("scan");
        let plan = survey.into_plan(scanned);

        // `app` reaches `core` directly and again through `mid`; the graph walk has to terminate
        // and report it once rather than loop or double-count.
        let reached = plan.reach.get("app").expect("app should have a reachable set");

        assert!(reached.contains("core"), "{reached:?}");
        assert!(reached.contains("mid"), "{reached:?}");
    }

    /// The names `--include-test` and `--exclude-test` are checked against.
    #[test]
    fn every_test_target_in_the_workspace_is_named() {
        let (_directory, root) = workspace();
        let survey = survey(&root, SelectArgs::default());

        // `it` is the integration target under `app/tests`, and the lib and bin targets are named
        // because their own unit tests build into binaries of the same name. `demo` is an example,
        // which cargo does not build as a test unless the manifest says so.
        assert!(survey.tests.contains(&"it".to_owned()), "{:?}", survey.tests);
        assert!(survey.tests.contains(&"core".to_owned()), "{:?}", survey.tests);
        assert!(survey.tests.contains(&"app".to_owned()), "{:?}", survey.tests);
        assert!(!survey.tests.contains(&"demo".to_owned()), "{:?}", survey.tests);
    }

    /// A proc macro's code runs inside `rustc` while another crate is compiled, but a run builds
    /// once and only then selects a mutant per test process. No mutant of one can therefore ever be
    /// killed, so mutating it would charge for the work and hand back guaranteed survivors. An
    /// ordinary library in the same workspace must still be mutated, or this would be a way to lose
    /// real coverage.
    #[test]
    fn a_proc_macro_target_yields_no_mutants_while_its_neighbours_still_do() {
        let (_directory, root) = workspace();

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"app\", \"macros\"]\nresolver = \"3\"\n",
        );
        write(
            &root,
            "macros/Cargo.toml",
            "[package]\nname = \"macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
        );
        write(&root, "macros/src/lib.rs", "pub fn widen(x: i32) -> i32 {\n    x + 1\n}\n");

        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect("the fixture must scan");

        let mutated: HashSet<&str> = scanned.mutants.iter().map(|mutant| mutant.file.as_str()).collect();

        assert!(
            !mutated.iter().any(|file| file.contains("macros")),
            "a proc-macro target must not be mutated: {mutated:?}"
        );
        assert!(
            mutated.iter().any(|file| file.contains("core")),
            "ordinary libraries must still be mutated: {mutated:?}"
        );
    }

    /// Every mutant is reached through a call to the guard runtime, so mutating the runtime itself
    /// would put that call inside the crate that defines it and the tree would stop compiling. This
    /// only arises for a workspace that builds the runtime — this one, when it runs on itself — but
    /// the exclusion is keyed on the library's name, so a fixture declaring the same name proves it
    /// without needing the real crate. Its neighbours must still be mutated.
    #[test]
    fn the_guard_runtime_yields_no_mutants_while_its_neighbours_still_do() {
        let (_directory, root) = workspace();

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"app\", \"rt\"]\nresolver = \"3\"\n",
        );
        write(
            &root,
            "rt/Cargo.toml",
            &format!(
                "[package]\nname = \"rt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"{}\"\n",
                crate::exec::RUNTIME_CRATE
            ),
        );
        write(&root, "rt/src/lib.rs", "pub fn widen(x: i32) -> i32 {\n    x + 1\n}\n");

        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect("the fixture must scan");

        let mutated: HashSet<&str> = scanned.mutants.iter().map(|mutant| mutant.file.as_str()).collect();

        assert!(
            !mutated.iter().any(|file| file.contains("rt")),
            "the guard runtime must not be mutated: {mutated:?}"
        );
        assert!(
            mutated.iter().any(|file| file.contains("core")),
            "ordinary libraries must still be mutated: {mutated:?}"
        );
    }

    /// Test targets are collected across the whole workspace, since `--package` says what to
    /// mutate while these patterns say what judges it.
    #[test]
    fn test_targets_are_named_even_for_packages_left_unmutated() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["core".to_owned()],
            ..SelectArgs::default()
        };
        let survey = Survey::new(&args, None).expect("survey");

        assert!(survey.tests.contains(&"it".to_owned()), "{:?}", survey.tests);
    }

    #[test]
    fn a_file_filter_still_recognizes_a_test_only_module_declared_outside_the_filter() {
        let (_directory, root) = workspace();
        write(
            &root,
            "core/src/lib.rs",
            "#[cfg(test)]\n#[path = \"reader_tests.rs\"]\nmod tests;\n",
        );
        write(
            &root,
            "core/src/reader_tests.rs",
            "#[test]\nfn reads() {\n    assert_eq!(1 + 2, 3);\n}\n",
        );

        let survey = survey(
            &root,
            SelectArgs {
                files: vec!["core/src/reader_tests.rs".to_owned()],
                ..SelectArgs::default()
            },
        );
        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("@all").expect("selection"), &mut ordinals)
            .expect("scan");

        assert!(scanned.mutants.is_empty(), "{:?}", scanned.mutants);
    }

    #[test]
    fn a_package_that_is_not_in_the_workspace_is_a_usage_error() {
        // Silently surveying nothing would report a perfect score for a package name that was
        // simply mistyped, which is the worst possible way to learn about a typo.
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["nosuch".to_owned()],
            ..SelectArgs::default()
        };

        let error = Survey::new(&args, None).expect_err("an unknown package must not survey");

        assert!(error.to_string().contains("nosuch"), "{error}");
        assert!(error.is_usage(), "{error}");
    }

    #[test]
    fn naming_one_package_leaves_the_others_alone() {
        let (_directory, root) = workspace();
        let plan = survey(
            &root,
            SelectArgs {
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        assert!(plan.files.iter().all(|file| file.package == "core"), "{:?}", plan.files);
    }

    /// A workspace-wide exclusion must survive a run narrowed to one package.
    ///
    /// The patterns are written once, in `gamma.toml`, for the whole workspace, while `--package`
    /// narrows a single run. Checking them against only the narrowed files makes a correct config
    /// fail outright the moment someone runs a single package — which is what anyone iterating on
    /// one crate does, so the config that works in CI breaks on every local run.
    #[test]
    fn a_pattern_naming_another_package_is_still_matched_when_one_package_is_named() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["core".to_owned()],
            exclude_files: vec!["app/**".to_owned()],
            ..SelectArgs::default()
        };

        let plan = Survey::new(&args, None).expect("a pattern naming an unselected package must not be an error");

        assert!(plan.files.iter().all(|file| file.package == "core"), "{:?}", plan.files);
    }

    /// The wider walk must not weaken the check that catches a genuine typo.
    #[test]
    fn a_pattern_matching_nothing_in_the_whole_workspace_is_still_an_error() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["core".to_owned()],
            exclude_files: vec!["nosuch/**".to_owned()],
            ..SelectArgs::default()
        };

        let error = Survey::new(&args, None).expect_err("a pattern matching nothing must not survey");

        assert!(error.to_string().contains("nosuch/**"), "{error}");
        assert!(error.is_usage(), "{error}");
    }

    #[test]
    fn a_test_or_example_target_is_never_mutated() {
        // Mutating a test measures the tests' tests, and mutating an example measures nothing at
        // all, since the suite does not run examples.
        let (_directory, root) = workspace();
        let plan = survey(&root, SelectArgs::default());
        let paths: Vec<&str> = plan.files.iter().map(|file| file.path.as_str()).collect();

        assert!(!paths.iter().any(|path| path.contains("examples")), "{paths:?}");
        assert!(!paths.iter().any(|path| path.contains("tests")), "{paths:?}");
        assert!(paths.iter().any(|path| path.ends_with("main.rs")), "{paths:?}");
    }

    #[test]
    fn an_excluded_file_is_dropped_before_it_is_parsed() {
        let (_directory, root) = workspace();
        let plan = survey(
            &root,
            SelectArgs {
                exclude_files: vec!["**/generated.rs".to_owned()],
                ..SelectArgs::default()
            },
        );

        assert!(
            !plan.files.iter().any(|file| file.path.as_str().ends_with("generated.rs")),
            "{:?}",
            plan.files
        );
    }

    /// A diff path that cannot be read at all has to fail the survey up front, naming the diff,
    /// rather than falling through to a scan that silently behaves as though `--in-diff` had never
    /// been given: a run over the wrong set of lines because a diff path was mistyped is far harder
    /// to notice than an error at start-up.
    #[test]
    fn a_diff_that_cannot_be_read_fails_the_survey_rather_than_being_ignored() {
        let (_directory, root) = workspace();

        let error = Survey::new(
            &SelectArgs {
                dir: root.clone(),
                in_diff: Some(root.join("no-such.patch")),
                ..SelectArgs::default()
            },
            None,
        )
        .expect_err("the diff file does not exist");

        assert!(error.to_string().contains("no-such.patch"), "{error}");
    }

    #[test]
    fn a_file_the_diff_never_mentions_is_dropped_before_it_is_parsed() {
        // This is most of what makes `--in-diff` affordable on a pull request: the files the change
        // did not touch are skipped without ever being read, let alone parsed.
        let (_directory, root) = workspace();

        write(
            &root,
            "change.patch",
            "--- a/core/src/lib.rs\n+++ b/core/src/lib.rs\n@@ -1,2 +1,2 @@\n one\n+    a + b\n",
        );

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                ..SelectArgs::default()
            },
        );

        assert_eq!(plan.files.len(), 1, "{:?}", plan.files);
        assert!(plan.files[0].path.as_str().ends_with("lib.rs"), "{:?}", plan.files);
    }

    /// `diff.mnemonicPrefix` is a setting a developer turns on once and forgets, and every diff
    /// they produce afterwards carries `i/` and `w/` instead of `a/` and `b/`. Selecting nothing
    /// from such a diff is indistinguishable from a change that touched no code.
    #[test]
    fn a_diff_written_with_mnemonic_prefixes_selects_the_same_files() {
        let (_directory, root) = workspace();

        write(
            &root,
            "change.patch",
            "diff --git i/core/src/lib.rs w/core/src/lib.rs\n--- i/core/src/lib.rs\n+++ w/core/src/lib.rs\n@@ -1,2 +1,2 @@\n one\n+    a + b\n",
        );

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                ..SelectArgs::default()
            },
        );

        assert_eq!(plan.files.len(), 1, "{:?}", plan.files);
        assert!(plan.files[0].path.as_str().ends_with("lib.rs"), "{:?}", plan.files);
    }

    /// A diff naming files this workspace does not have was not understood, and a survey that
    /// shrugged and selected nothing would hand the run an empty population and a perfect score.
    #[test]
    fn a_diff_that_names_nothing_in_this_workspace_fails_the_survey() {
        let (_directory, root) = workspace();

        write(
            &root,
            "change.patch",
            "--- a/elsewhere/src/lib.rs\n+++ b/elsewhere/src/lib.rs\n@@ -1,2 +1,2 @@\n one\n+    a + b\n",
        );

        let error = Survey::new(
            &SelectArgs {
                dir: root.clone(),
                in_diff: Some(root.join("change.patch")),
                ..SelectArgs::default()
            },
            None,
        )
        .expect_err("a diff that names no file here must not pass for an empty change");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("elsewhere/src/lib.rs"), "{error}");
    }

    /// A diff that touched only files this run does not mutate is understood perfectly well, and
    /// selecting nothing is the right answer to it.
    #[test]
    fn a_diff_that_touches_no_source_file_selects_nothing_without_failing() {
        let (_directory, root) = workspace();

        write(
            &root,
            "change.patch",
            "--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -1,2 +1,2 @@\n one\n+two\n",
        );

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                ..SelectArgs::default()
            },
        );

        assert!(plan.files.is_empty(), "{:?}", plan.files);
    }

    #[test]
    fn a_changed_file_still_only_yields_mutants_on_the_changed_lines() {
        // A changed line usually sits among many that were not touched. Mutating the whole file
        // would report on code the change never went near.
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        );
        write(
            &root,
            "change.patch",
            "--- a/core/src/lib.rs\n+++ b/core/src/lib.rs\n@@ -1,2 +1,2 @@\n head\n+    a + b\n",
        );

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        let mut ordinals = 0;
        let scanned = plan
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect("the fixture must scan");

        assert!(!scanned.mutants.is_empty(), "the changed line yields nothing");
        // Whole-extent matching: every yielded mutant must span the touched line 2 -- `add`'s
        // whole-body value mutants run lines 1..3 and its `a + b` mutants sit on line 2 -- while
        // nothing from `sub`, four lines below and never touched, is pulled in.
        assert!(
            scanned.mutants.iter().all(|mutant| mutant.line <= 2 && mutant.end_line >= 2),
            "{:?}",
            scanned.mutants
        );
        assert!(
            scanned.mutants.iter().all(|mutant| &*mutant.item_path == "add"),
            "{:?}",
            scanned.mutants
        );
    }

    #[test]
    fn a_mutant_is_selected_by_its_whole_extent_not_just_its_first_line() {
        // A mutation site can span several lines -- a call, match, or binary expression broken
        // across them. A change that edits an interior line of such a site has changed that site,
        // so its mutants must be selected even though the line the diff touched is neither the one
        // the site starts on nor the one the report will name.
        let (_directory, root) = workspace();

        write(
            &root,
            "core/src/lib.rs",
            "pub fn add(a: i32, b: i32, c: i32) -> i32 {\n    a\n        + b\n        + c\n}\n",
        );
        // The only added line is line 3, `+ b`, an interior line of the `a + b + c` expression
        // whose site runs from line 2 to line 4.
        write(
            &root,
            "change.patch",
            "--- a/core/src/lib.rs\n+++ b/core/src/lib.rs\n@@ -2,3 +2,3 @@\n     a\n-        + x\n+        + b\n         + c\n",
        );

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        let mut ordinals = 0;
        let scanned = plan
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect("the fixture must scan");

        // Selecting a site by only its first line would attribute every mutant to line 2 and, since
        // the diff never touched line 2, drop them all.
        assert!(!scanned.mutants.is_empty(), "a site edited on an interior line was dropped");
        assert!(
            scanned.mutants.iter().any(|mutant| mutant.line == 2 && mutant.end_line >= 3),
            "no multi-line site reached the touched interior line: {:?}",
            scanned.mutants
        );
        // Every surviving mutant must genuinely span the touched line 3, start to end.
        assert!(
            scanned.mutants.iter().all(|mutant| mutant.line <= 3 && mutant.end_line >= 3),
            "{:?}",
            scanned.mutants
        );
    }

    #[test]
    fn a_mutant_an_earlier_report_settled_is_kept_with_its_verdict_and_never_run() {
        // Incremental execution exists so a second run costs only the mutants that were still open, and a
        // settled mutant therefore takes no ordinal and no shard slot. It stays in the population
        // wearing the verdict it earned, because the score is a claim about the population: a run
        // that dropped them would report on the subset it retried and call that the score.
        let (_directory, root) = workspace();
        let mut plan = survey(
            &root,
            SelectArgs {
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        let selection = Selection::parse("all").expect("every mutator resolves");
        let mut ordinals = 0;
        let first = plan.scan(None, &selection, &mut ordinals).expect("the fixture must scan");
        let settled: HashMap<MutantId, Outcome> = first.mutants.iter().map(|mutant| (mutant.id.clone(), Outcome::Killed)).collect();

        assert!(!settled.is_empty(), "the fixture yielded no mutants to settle");

        plan.settle(settled.clone());

        let mut ordinals = 0;
        let second = plan.scan(None, &selection, &mut ordinals).expect("the fixture must scan");

        assert_eq!(second.mutants.len(), first.mutants.len());
        assert_eq!(second.settled_out, settled.len());
        assert_eq!(ordinals, 0, "a settled mutant is not work and must take no ordinal");
        assert!(
            second.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed),
            "{:?}",
            second.mutants
        );
        assert_eq!(crate::model::Summary::of(&second.mutants).scored(), Some(100.0));
    }

    /// Only the mutants the report actually settled are carried; the rest are work again, and the
    /// population the second run scores is the same one the first run faced.
    #[test]
    fn an_unsettled_mutant_is_still_work_in_an_iterative_run() {
        let (_directory, root) = workspace();
        let mut plan = survey(
            &root,
            SelectArgs {
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        let selection = Selection::parse("all").expect("every mutator resolves");
        let mut ordinals = 0;
        let first = plan.scan(None, &selection, &mut ordinals).expect("the fixture must scan");
        let live: Vec<&Mutant> = first.mutants.iter().filter(|mutant| mutant.ordinal > 0).collect();

        assert!(live.len() > 1, "the fixture needs more than one live mutant");

        let settled: HashMap<MutantId, Outcome> = live.iter().skip(1).map(|mutant| (mutant.id.clone(), Outcome::Killed)).collect();

        plan.settle(settled.clone());

        let mut ordinals = 0;
        let second = plan.scan(None, &selection, &mut ordinals).expect("the fixture must scan");

        assert_eq!(second.mutants.len(), first.mutants.len());
        assert_eq!(second.settled_out, settled.len());
        assert_eq!(ordinals, 1, "only the mutant that is still open is work");
    }

    #[test]
    fn a_feature_selection_is_carried_into_the_metadata_it_surveys() {
        // Discovering under one feature set and compiling under another would place guards in files
        // the compiler never sees, so every form of feature selection has to reach cargo.
        let (_directory, root) = workspace();

        for features in [
            FeatureArgs {
                all_features: true,
                ..FeatureArgs::default()
            },
            FeatureArgs {
                no_default_features: true,
                ..FeatureArgs::default()
            },
            FeatureArgs {
                features: vec!["core/extra, ".to_owned()],
                ..FeatureArgs::default()
            },
        ] {
            let metadata = load_metadata(&root, &features).expect("the fixture workspace must produce metadata");

            assert_eq!(metadata.workspace_packages().len(), 2, "{features:?}");
        }
    }

    /// A workspace whose binary target only exists when a feature turns it on.
    ///
    /// The gated binary sits in a directory of its own, because the walk that follows a target is
    /// by directory: a `[[bin]]` beside `lib.rs` has its file reached through the library's own
    /// walk whether or not cargo builds the binary.
    fn gated_workspace() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"gated\"]\nresolver = \"3\"\n");
        write(
            &root,
            "gated/Cargo.toml",
            concat!(
                "[package]\nname = \"gated\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
                "[features]\ncli = []\n\n",
                "[lib]\nname = \"gated\"\npath = \"src/lib.rs\"\n\n",
                "[[bin]]\nname = \"gated-cli\"\npath = \"cli/main.rs\"\nrequired-features = [\"cli\"]\n\n",
                "[[bin]]\nname = \"gated-plain\"\npath = \"plain/main.rs\"\n",
            ),
        );
        write(
            &root,
            "gated/src/lib.rs",
            concat!(
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n",
                "#[cfg(feature = \"cli\")]\npub fn only_with_cli(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            ),
        );
        write(&root, "gated/cli/main.rs", "fn main() {\n    let _ = 1 + 1;\n}\n");
        write(&root, "gated/plain/main.rs", "fn main() {\n    let _ = 2 + 2;\n}\n");

        (directory, root)
    }

    /// Cargo does not build a binary whose `required-features` are off, so neither does the survey.
    ///
    /// Every mutant in such a target is unviable: the file is never compiled, so nothing can switch
    /// a guard in it on and nothing can kill it. They arrive as build errors instead, and the
    /// rollback loop pays for a round of them before withdrawing the lot.
    #[test]
    fn a_binary_whose_required_features_are_off_is_not_surveyed() {
        let (_directory, root) = gated_workspace();
        let survey = survey(&root, SelectArgs::default());
        let listed: Vec<&Utf8PathBuf> = survey.files.iter().map(|file| &file.path).collect();

        assert!(listed.contains(&&Utf8PathBuf::from("gated/src/lib.rs")), "{listed:?}");
        assert!(listed.contains(&&Utf8PathBuf::from("gated/plain/main.rs")), "{listed:?}");
        assert!(!listed.contains(&&Utf8PathBuf::from("gated/cli/main.rs")), "{listed:?}");
    }

    /// The same target, with the feature it asks for turned on, is built and therefore surveyed.
    #[test]
    fn a_binary_whose_required_features_are_on_is_surveyed_like_any_other() {
        let (_directory, root) = gated_workspace();
        let survey = survey(
            &root,
            SelectArgs {
                features: FeatureArgs {
                    all_features: true,
                    ..FeatureArgs::default()
                },
                ..SelectArgs::default()
            },
        );
        let listed: Vec<&Utf8PathBuf> = survey.files.iter().map(|file| &file.path).collect();

        assert!(listed.contains(&&Utf8PathBuf::from("gated/src/lib.rs")), "{listed:?}");
        assert!(listed.contains(&&Utf8PathBuf::from("gated/plain/main.rs")), "{listed:?}");
        assert!(listed.contains(&&Utf8PathBuf::from("gated/cli/main.rs")), "{listed:?}");
    }

    /// A feature selector reaches cargo through `-C`/`--cargo-arg` and through `gamma.toml`'s
    /// `cargo_args`, and cargo then really builds with it — so a closure that reads only the typed
    /// arguments describes a build nobody runs. The gated binary leaves the population whole and
    /// every item behind the feature is judged absent, both of which raise the score.
    #[test]
    fn a_feature_selector_in_the_passthrough_arguments_reaches_the_closure() {
        let (_directory, root) = gated_workspace();
        let selected = CargoOptions {
            extra: vec!["--features".to_owned(), "cli".to_owned()],
            ..CargoOptions::default()
        };

        let survey = Survey::for_build(
            &SelectArgs {
                dir: root.clone(),
                ..SelectArgs::default()
            },
            None,
            &selected,
        )
        .expect("the fixture workspace must survey");
        let listed: Vec<&Utf8PathBuf> = survey.files.iter().map(|file| &file.path).collect();

        assert!(listed.contains(&&Utf8PathBuf::from("gated/cli/main.rs")), "{listed:?}");

        let items = mutated_items(&root, &selected);

        assert!(items.iter().any(|item| item.contains("only_with_cli")), "{items:?}");

        // The control: with no selector anywhere, cargo builds neither, and neither does the survey.
        let plain = mutated_items(&root, &CargoOptions::default());

        assert!(!plain.iter().any(|item| item.contains("only_with_cli")), "{plain:?}");
    }

    /// A `dependency/feature` requirement is not something the per-package closure can answer, so
    /// the target is kept rather than dropped on a guess. Losing a target that is built is worse
    /// than keeping one that is not: the first silently shrinks the population, the second costs a
    /// rollback round.
    #[test]
    fn a_required_feature_decides_a_binary_only_when_the_closure_can_answer() {
        let (_directory, root) = gated_workspace();
        let metadata = load_metadata(&root, &FeatureArgs::default()).expect("the fixture workspace must produce metadata");
        let packages = metadata.workspace_packages();
        let package = packages.first().expect("the fixture workspace has one member");
        let gated = package
            .targets
            .iter()
            .find(|target| target.name == "gated-cli")
            .expect("the fixture declares the gated binary");

        assert!(
            !is_mutable_target(gated, Some(&Vec::new())),
            "cargo does not build it with the feature off"
        );
        assert!(is_mutable_target(gated, Some(&vec!["cli".to_owned()])));

        // A package the closure never described leaves the requirement unproven, and an unproven
        // requirement does not take a target out of the population.
        assert!(is_mutable_target(gated, None));

        let mut cross = (*gated).clone();

        cross.required_features = vec!["serde/derive".to_owned()];

        assert!(is_mutable_target(&cross, Some(&Vec::new())));
    }

    /// A workspace whose items are gated on the things the build decides.
    ///
    /// One item per facet: the profile's `debug_assertions`, its negation, the target, and a
    /// predicate that only a `--cfg` in the build's flags can satisfy. A survey that probed a bare
    /// host compile in the default profile would answer the first correctly and the rest wrong.
    fn predicate_workspace() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"probe\"]\nresolver = \"3\"\n");
        write(
            &root,
            "probe/Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &root,
            "probe/src/lib.rs",
            concat!(
                "#[cfg(debug_assertions)]\npub fn debug_only(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n",
                "#[cfg(not(debug_assertions))]\npub fn release_only(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n",
                "#[cfg(target_os = \"solaris\")]\npub fn solaris_only(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n",
                "#[cfg(gamma_probe)]\npub fn custom_only(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            ),
        );

        (directory, root)
    }

    /// The items that yield a mutant when this workspace is surveyed for this build.
    fn mutated_items(root: &Utf8Path, cargo: &CargoOptions) -> Vec<String> {
        mutated_items_with_assertions(root, cargo, None)
    }

    fn mutated_items_with_assertions(root: &Utf8Path, cargo: &CargoOptions, debug_assertions: Option<bool>) -> Vec<String> {
        let mut survey = Survey::for_build(
            &SelectArgs {
                dir: root.to_owned(),
                ..SelectArgs::default()
            },
            None,
            cargo,
        )
        .expect("the fixture workspace must survey");

        if let Some(debug_assertions) = debug_assertions {
            let mut features = HashMap::default();

            let _old = features.insert("probe".to_owned(), Vec::new());
            survey.cfgs = Cfgs::new(
                &crate::cfg::for_build(&crate::cfg::Build {
                    debug_assertions: Some(debug_assertions),
                    ..crate::cfg::Build::default()
                })
                .expect("the host compiler must describe its cfg predicates"),
                &features,
            );
        }

        let mut ordinals = 0;
        let scanned = survey
            .scan(None, &Selection::parse("arith.add_to_sub").expect("selection"), &mut ordinals)
            .expect("the fixture must scan");

        scanned.mutants.iter().map(|mutant| mutant.item_path.to_string()).collect()
    }

    /// The profile decides `debug_assertions`, so it decides which half of that gate is compiled.
    #[test]
    fn the_profile_decides_which_half_of_a_debug_assertions_gate_is_surveyed() {
        let (_directory, root) = predicate_workspace();
        let debug = mutated_items_with_assertions(&root, &CargoOptions::default(), Some(true));
        let release = mutated_items_with_assertions(
            &root,
            &CargoOptions {
                profile: Some("release".to_owned()),
                ..CargoOptions::default()
            },
            Some(false),
        );

        assert!(debug.iter().any(|item| item.contains("debug_only")), "{debug:?}");
        assert!(!debug.iter().any(|item| item.contains("release_only")), "{debug:?}");
        assert!(release.iter().any(|item| item.contains("release_only")), "{release:?}");
        assert!(!release.iter().any(|item| item.contains("debug_only")), "{release:?}");
    }

    /// A build for another target compiles that target's code, and so surveys it.
    ///
    /// The triple is one every toolchain knows how to describe and no CI host runs, so the item is
    /// absent by default and present only because the build asked for it. Nothing is compiled here:
    /// `rustc --print cfg` answers for a target whose standard library is not installed.
    #[test]
    fn a_cross_target_build_surveys_the_code_that_target_compiles() {
        let (_directory, root) = predicate_workspace();
        let host = mutated_items(&root, &CargoOptions::default());
        let elsewhere = mutated_items(
            &root,
            &CargoOptions {
                extra: vec!["--target".to_owned(), "x86_64-pc-solaris".to_owned()],
                ..CargoOptions::default()
            },
        );

        assert!(!host.iter().any(|item| item.contains("solaris_only")), "{host:?}");
        assert!(elsewhere.iter().any(|item| item.contains("solaris_only")), "{elsewhere:?}");
    }

    /// A predicate that only the build's own flags define is present exactly when they define it.
    #[test]
    fn a_custom_predicate_from_the_build_flags_is_surveyed_as_present() {
        const CHILD: &str = "CARGO_GAMMA_CONFIG_RUSTFLAGS_TEST_CHILD";

        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().expect("the test executable has a path"))
                .args([
                    "--exact",
                    "discover::survey::tests::a_custom_predicate_from_the_build_flags_is_surveyed_as_present",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env_remove("CARGO_ENCODED_RUSTFLAGS")
                .env_remove("RUSTFLAGS")
                .env_remove("CARGO_BUILD_RUSTFLAGS")
                .status()
                .expect("the isolated test process runs");

            assert!(status.success(), "{status}");
            return;
        }

        let (_directory, root) = predicate_workspace();
        let plain = mutated_items(&root, &CargoOptions::default());

        assert!(!plain.iter().any(|item| item.contains("custom_only")), "{plain:?}");

        write(&root, ".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"gamma_probe\"]\n");

        let flagged = mutated_items(&root, &CargoOptions::default());

        assert!(flagged.iter().any(|item| item.contains("custom_only")), "{flagged:?}");
    }

    #[test]
    fn metadata_that_cannot_be_read_names_the_directory() {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");
        let error = load_metadata(&root, &FeatureArgs::default()).expect_err("a directory with no manifest has no metadata");

        assert!(error.to_string().contains(root.as_str()), "{error}");
    }
}
