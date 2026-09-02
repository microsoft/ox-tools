// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build facts and checked hints from the last run.

use std::process::Command;
use std::slice::Iter;
use std::{env, fs};

use blake3::Hasher;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use super::Plan;
use super::killers::Killers;
use super::workspace_snapshot::WorkspaceSnapshot;
use crate::cfg::Build;
use crate::model::{Mutant, MutantId, Outcome};
use crate::{HashMap, HashSet};

/// What the cache format is; a file written by any other version is discarded rather than read.
///
/// A cache is allowed to be thrown away — that is what makes it a cache — so there is no migration
/// path here and there should never be one. Reading a format that has changed meaning is how a
/// cache stops being free.
///
/// Version 7 records external local Cargo inputs alongside the workspace snapshot.
///
/// A term added to the context does not move this number, and deliberately. The terms are separate
/// fields and a new one is optional, so an older file is not a foreign format — it is a record that
/// declines to answer the guard on the tiers that ask about that term, and keeps the two that ask
/// nothing. Discarding it instead would throw away the probes and the build order to defend a
/// question they do not depend on. What moves this number is a change of *meaning* in what is
/// already there, which no reader could detect for itself.
const VERSION: u32 = 9;

/// The file name under the gamma scratch base.
const FILE: &str = "last-gamma-run.json";

/// How much of a record a reader is willing to believe.
///
/// Production runs use [`Self::Free`] and therefore reuse only compiler unviability. Test verdicts
/// are nondeterministic observations and must not contribute to a later run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Only what moves no verdict: the mutants that would not compile.
    #[default]
    Free,

    /// Test verdicts as well as compiler outcomes.
    ///
    /// New run records contain no test verdicts, so this cannot add detection credit.
    Settled,
}

/// One term of the build context, digested on its own so that a tier can name what it depends on.
///
/// Separate terms rather than one digest because the terms do not all matter to the same things.
/// A mutant's compilation and verdict both depend on the compiler, while probes and ordering are
/// never believed. A single digest cannot express that difference without making safe hints cold
/// whenever a developer's machine and CI differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Term {
    /// The feature selection, including `--all-features` and `--no-default-features`.
    Features,

    /// The cargo profile, which decides `debug_assertions` among other things.
    Profile,

    /// Whichever of the rustflags variables cargo will actually read.
    Rustflags,

    /// The triple the build compiles for, as the command line and the environment ask for it.
    ///
    /// Separate from [`Self::Extra`] even though a passthrough `--target` is part of both, because
    /// the target has a second spelling — `CARGO_BUILD_TARGET` — that no passthrough argument
    /// carries. A term fed only by the arguments would let the variable move the whole compilation
    /// without moving the key, and unviability found for one architecture would be believed for
    /// another.
    Target,

    /// The build settings Cargo configuration and workspace files hold, digested together.
    ///
    /// The complete bytes of every Cargo configuration Cargo reads, together with the
    /// configuration's `build.target`, `build.rustflags` and target tables, and the profile bodies
    /// of both the configuration and the manifest: everything
    /// [`Build::resolve`](crate::cfg::Build::resolve) reads that neither the command line nor the
    /// environment carries. One term rather than several because nothing here can be reported more
    /// usefully apart — a reader told "the configuration differs" looks at their configuration —
    /// and because the set of tables is open, so a term per table could not be a fixed list.
    Config,

    /// The arbitrary passthrough build arguments, which can carry `--cfg`, `-C` or `--target`.
    Extra,

    /// The compiler, cargo and wrapper identities.
    Toolchain,

    /// This tool's own version, which decides what a mutant id denotes.
    Tool,

    /// The test filtering and execution arguments.
    Tests,

    /// The execution policy that decides how a test result becomes a verdict.
    Policy,

    /// Environment inherited by test binaries that gamma does not set itself.
    Environment,
}

impl Term {
    /// Every term there is, which is what a tier requiring the whole context asks for.
    ///
    /// Spelled once, so that adding a term to [`ContextDigest`] and forgetting it here is the only
    /// way to weaken an invalidation rule by accident — and the test below pins that this list and
    /// the digest agree.
    pub const ALL: &'static [Self] = &[
        Self::Features,
        Self::Profile,
        Self::Rustflags,
        Self::Target,
        Self::Config,
        Self::Extra,
        Self::Toolchain,
        Self::Tool,
        Self::Tests,
        Self::Policy,
        Self::Environment,
    ];

    /// The term's name, as the diagnostics spell it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Features => "features",
            Self::Profile => "profile",
            Self::Rustflags => "rustflags",
            Self::Target => "target",
            Self::Config => "config",
            Self::Extra => "extra",
            Self::Toolchain => "toolchain",
            Self::Tool => "tool",
            Self::Tests => "tests",
            Self::Policy => "policy",
            Self::Environment => "environment",
        }
    }
}

/// A kind of knowledge the record holds, paired with what a run must match before it may be used.
///
/// This is the per-tier invalidation rule, and it is deliberately stated as data rather than as a
/// condition spread across the readers: a tier that quietly widened what it accepts would be a
/// mutant dropped from the denominator, and there is no test that could find that in a condition
/// nobody named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The mutants that would not compile, used to keep them out of the build entirely.
    ///
    /// A claim about what compiles, so it depends on everything that decides what compiles. Being
    /// wrong here withholds a mutant that might have survived, which turns a real gap in the suite
    /// into a better score — the one direction this tool must never be wrong in.
    Unviability,

    /// Test verdicts read from a record.
    ///
    /// The writer does not persist these because matching inputs cannot prove deterministic tests.
    Verdict,

    /// Which mutants failed to compile last time, offered to the build as an order to try them in.
    ///
    /// Depends on nothing either, and for the same shape of reason: being wrong about the order
    /// costs the order and nothing else. No mutant is withheld, settled or excluded on this, so a
    /// hint from another toolchain, another feature set or another machine is free to be wrong.
    Ordering,
}

impl Tier {
    /// The context terms a run has to agree with the record on before it may use this tier.
    #[must_use]
    pub const fn requires(self) -> &'static [Term] {
        match self {
            Self::Unviability => &[
                Term::Features,
                Term::Profile,
                Term::Rustflags,
                Term::Target,
                Term::Config,
                Term::Extra,
                Term::Toolchain,
                Term::Tool,
                Term::Policy,
                Term::Environment,
            ],
            Self::Verdict => &[
                Term::Features,
                Term::Profile,
                Term::Rustflags,
                Term::Target,
                Term::Config,
                Term::Extra,
                Term::Toolchain,
                Term::Tool,
                Term::Tests,
                Term::Policy,
                Term::Environment,
            ],
            Self::Ordering => &[],
        }
    }

    /// Whether a record written under `recorded` may be read for this tier by a run under `current`.
    ///
    /// A term neither side states is not agreement. [`ContextDigest::states`] exists because two of
    /// the terms are answers only a workspace can give, so a digest built before the workspace was
    /// located leaves them open — and reading "neither of us knows" as "we agree" would admit
    /// exactly the records this guard is for.
    #[must_use]
    pub fn admits(self, recorded: &ContextDigest, current: &ContextDigest) -> bool {
        self.requires()
            .iter()
            .all(|term| recorded.states(*term) && current.states(*term) && recorded.term(*term) == current.term(*term))
    }
}

/// A digest of each thing other than the sources that decides what compiles, kept term by term.
///
/// Written into the record rather than derived on read, because the question a reader asks is not
/// "what is this run's context" but "did the run that wrote this agree with mine about the terms my
/// tier depends on" — and that can only be answered by a file that states its terms separately.
///
/// An absent term is an empty string, which is a value like any other: two runs that both passed no
/// rustflags agree about rustflags. An *unstated* term is different and is spelled `None`: the
/// target and the configuration are answers only a located workspace can give, while the inherited
/// environment was not recorded before that term existed. A digest built without one of those
/// answers leaves it open, and a tier that requires it refuses rather than matching on the gap.
/// That is also what an older record — written before a term existed — looks like, which costs it
/// the tiers whose guard it cannot answer and leaves it the two that need no guard.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextDigest {
    /// The feature selection, including the two flags that override it.
    features: String,

    /// The cargo profile.
    profile: String,

    /// The ambient rustflags cargo will read.
    rustflags: String,

    /// The triple the build compiles for, or nothing when this digest does not state one.
    #[serde(default)]
    target: Option<String>,

    /// Cargo configuration and workspace build settings, or nothing when they had not been read yet.
    #[serde(default)]
    config: Option<String>,

    /// The passthrough build arguments.
    extra: String,

    /// The compiler's version string.
    toolchain: String,

    /// This tool's own version.
    tool: String,

    /// The test filtering and execution arguments.
    #[serde(default)]
    tests: String,

    /// The policy that turns test execution into a verdict.
    #[serde(default)]
    policy: String,

    /// The ambient environment test binaries inherit, or nothing for an older record.
    #[serde(default)]
    environment: Option<String>,
}

impl ContextDigest {
    /// The digest of one term, which is the empty string for a term this digest does not state.
    #[must_use]
    pub fn term(&self, term: Term) -> &str {
        match term {
            Term::Features => &self.features,
            Term::Profile => &self.profile,
            Term::Rustflags => &self.rustflags,
            Term::Target => self.target.as_deref().unwrap_or_default(),
            Term::Config => self.config.as_deref().unwrap_or_default(),
            Term::Extra => &self.extra,
            Term::Toolchain => &self.toolchain,
            Term::Tool => &self.tool,
            Term::Tests => &self.tests,
            Term::Policy => &self.policy,
            Term::Environment => self.environment.as_deref().unwrap_or_default(),
        }
    }

    /// Whether this digest has an answer for a term at all.
    ///
    /// Asked before [`Self::term`] is compared, because the empty string that an unstated term
    /// reads as is also a perfectly good digest of nothing, and the two must not be confused: one
    /// says "no rustflags were set", the other says "nobody has looked".
    #[must_use]
    pub const fn states(&self, term: Term) -> bool {
        match term {
            Term::Target => self.target.is_some(),
            Term::Config => self.config.is_some(),
            Term::Environment => self.environment.is_some(),
            _stated_by_the_command_line => true,
        }
    }

    /// The terms this run and `other` disagree about, in [`Term::ALL`] order.
    ///
    /// Reported rather than merely counted so that a diagnostic can say *which* axis moved. "The
    /// record did not apply" sends a reader looking through their whole configuration; "the
    /// toolchain differs" is something they can act on, or decide not to.
    ///
    /// A term one of the two does not state is not a disagreement to report. It is a difference the
    /// gate does act on — see [`Tier::admits`] — but naming it here would tell a reader that their
    /// target moved when what actually happened is that one of the digests was taken before the
    /// workspace was in hand.
    #[must_use]
    pub fn differences(&self, other: &Self) -> Vec<Term> {
        Term::ALL
            .iter()
            .copied()
            .filter(|term| self.states(*term) && other.states(*term) && self.term(*term) != other.term(*term))
            .collect()
    }

    /// This context with the terms only a located workspace can answer filled in.
    ///
    /// Separate from [`context`] because the two are known at different moments. The command line
    /// settles the features, the profile and the rest before anything has been read from disk,
    /// while the target tables and the profile bodies live in files whose location is the workspace
    /// root — which the record is handed when it is written and when it is read, and not before.
    /// Both sides of every comparison go through here, so the gate compares like with like.
    ///
    /// Public because the gate is not the only caller that has to compare like with like:
    /// [`Self::differences`] skips any term one side leaves unstated, so a caller that reports
    /// *why* the cache did not apply and passes an unresolved digest would silently never name the
    /// configuration — the one axis a reader is least likely to guess at.
    #[must_use]
    pub fn resolved_at(&self, root: &Utf8Path) -> Self {
        let settings = Build::settings(root);
        let parts: Vec<&[u8]> = settings.iter().map(String::as_bytes).collect();

        Self {
            config: Some(term(Term::Config, &parts)),
            ..self.clone()
        }
    }
}

/// What an earlier run established about each mutant, and what those findings depended on.
///
/// Of everything a run learns, which mutants failed to compile is both the most expensive to
/// rediscover and the safest to reuse. Expensive because unviability is found by building, blaming
/// and building again, and each round is another rebuild of the instrumented tree. Safe because an
/// unviable mutant is excluded from the score outright, so carrying one forward moves no verdict —
/// unlike a kill, which is a claim about the test suite and is only adopted when asked for.
///
/// Nothing here is discarded wholesale. The file is read whatever context it was written under, and
/// each [`Tier`] decides for itself whether this run agrees with it about the terms that tier
/// depends on. A toolchain bump therefore costs the unviability and keeps the probes and the build
/// order.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RunRecord {
    /// The format this was written in.
    version: u32,

    /// A digest, term by term, of every policy and toolchain setting that can affect a verdict.
    ///
    /// Features are the reason this exists. A mutant that cannot compile with one feature set may
    /// compile perfectly well with another, so a cache written under `--all-features` says nothing
    /// about a default-features run. Kept per term rather than as one digest so that a tier
    /// depending on some of the terms is not invalidated by the ones it does not depend on.
    context: ContextDigest,

    /// For each source file that holds a recorded mutant, its digest and what was settled there.
    files: Vec<RecordedFile>,

    /// Every workspace input Cargo could have consulted before this run started.
    ///
    /// This is intentionally wider than the mutable files. A changed helper, manifest, lockfile,
    /// build script, test or cargo configuration can change a verdict without changing a mutant's
    /// identity, so any difference rejects every carried outcome.
    #[serde(default)]
    inputs: WorkspaceSnapshot,

    /// Workspace package roots whose contents can affect each package's compilation.
    #[serde(default)]
    compilation_roots: HashMap<String, Vec<Utf8PathBuf>>,

    /// What caught each mutant last time, so the sweep can try that test first.
    ///
    /// Outside [`Self::files`], and deliberately: a verdict is *believed*, so it is gated on the
    /// build context and on the digest of the file it was found in. A hint is never believed. Every
    /// one of them is a guess the run immediately checks by running the named test, and a guess
    /// that does not convict costs one filtered process and is thrown away. Nothing here can move a
    /// verdict, so nothing here needs invalidating — and invalidating it would only make the map
    /// cold on exactly the runs, after an edit or a feature change, where it is worth the most.
    #[serde(default)]
    hints: HashMap<MutantId, Killer>,
}

/// The test that caught a mutant, and the binary it lives in.
///
/// The binary is named by package and target rather than by path because a path is not stable
/// across runs: the binaries a run judges live in a scratch tree that is rebuilt each time, so a
/// recorded path would miss every time and the map would be permanently cold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Killer {
    /// The package whose test binary caught it.
    pub package: String,

    /// The cargo target within that package.
    pub target: String,

    /// The test the harness named when it failed.
    pub test: String,
}

impl Killer {
    /// Whether this names the given binary.
    #[must_use]
    pub fn names(&self, package: &str, target: &str) -> bool {
        self.package == package && self.target == target
    }
}

/// The recorded mutants of one source file, and the digest of the file they were judged in.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedFile {
    /// Workspace-relative path, as the mutants spell it.
    path: Utf8PathBuf,

    /// The workspace package this file belongs to.
    #[serde(default)]
    package: String,

    /// A digest of the file's bytes as they were when these mutants failed to compile.
    digest: String,

    /// The file's length in bytes when those mutants failed to compile.
    ///
    /// A cheap rejection ahead of the digest: a file of a different length is certainly a different
    /// file, and saying so costs a `stat` where the digest costs a full read. It can only reject —
    /// a matching length still has to be hashed, because two different files of the same length are
    /// ordinary rather than exotic, and accepting one would drop a mutant that might have survived
    /// out of the denominator.
    size: u64,

    /// What this run settled about the mutants in this file.
    mutants: Vec<Entry>,
}

/// One mutant's settled verdict, as the run that reached it left it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// The mutant's content-addressed id.
    id: MutantId,

    /// The verdict that was reached.
    outcome: Outcome,

    /// The test that did the killing, when one did.
    ///
    /// Written for the same reason the JSON report carries `killedBy`: the mutant's identity hashes
    /// the file, the item path, the mutator and the replacement, none of which change when somebody
    /// deletes the test that caught it. Without a name to check, a kill cannot be revalidated and
    /// must not be carried.
    ///
    /// Distinct from the entry in [`RunRecord::hints`], which names the same test for a different
    /// purpose and on different terms: this one is evidence the verdict is still true, that one is a
    /// guess about where to look first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    killed_by: Option<String>,

    /// The workspace-relative file that declared `killed_by` before execution.
    ///
    /// The name alone is insufficient: moving a test with the same qualified name to another file
    /// can change which target Cargo builds and what it links. This is present only when the
    /// pre-execution scan could identify one unambiguously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    killer_file: Option<Utf8PathBuf>,

    /// What the mutant cost to judge, in milliseconds.
    #[serde(default)]
    elapsed_ms: u64,
}

/// Every recorded verdict in a [`RunRecord`], with files in stored order and entries within each
/// file in stored order.
///
/// Returned by [`RunRecord::iter`], and by `IntoIterator` on `&RunRecord`. Named rather than
/// returned as `impl Iterator` so that `&RunRecord` can name it as its `IntoIter`, which is what
/// lets a `for` loop over a record work.
#[derive(Debug, Clone)]
pub struct Entries<'a> {
    files: Iter<'a, RecordedFile>,

    /// The iterator over entries in the current file, absent before the first file is reached.
    mutants: Option<Iter<'a, Entry>>,
}

impl<'a> Iterator for Entries<'a> {
    type Item = (&'a str, Outcome);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(entry) = self.mutants.as_mut().and_then(Iterator::next) {
            return Some((entry.id.as_str(), entry.outcome));
        }

        for file in self.files.by_ref() {
            let mut mutants = file.mutants.iter();

            if let Some(entry) = mutants.next() {
                self.mutants = Some(mutants);

                return Some((entry.id.as_str(), entry.outcome));
            }
        }

        None
    }
}

impl<'a> IntoIterator for &'a RunRecord {
    type Item = (&'a str, Outcome);
    type IntoIter = Entries<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Whether an outcome is safe to reuse without executing the mutant.
///
/// Test outcomes are observations, not proof that the suite is deterministic. Recording only
/// compiler unviability ensures a cached observation can never contribute detection credit.
const fn settled_verdict(outcome: Outcome) -> bool {
    matches!(outcome, Outcome::CompileError)
}

impl RunRecord {
    /// Captures the workspace before it is copied or executed.
    #[cfg(test)]
    #[must_use]
    fn snapshot(root: &Utf8Path, scratch_base: &Utf8Path) -> WorkspaceSnapshot {
        WorkspaceSnapshot::capture(root, &[scratch_base.to_path_buf()])
    }

    /// Captures a workspace together with local path dependencies Cargo resolves outside it.
    #[must_use]
    pub(crate) fn snapshot_with_external(
        root: &Utf8Path,
        scratch_base: &Utf8Path,
        external_roots: &[Utf8PathBuf],
        untracked_build_script_inputs: bool,
    ) -> WorkspaceSnapshot {
        WorkspaceSnapshot::capture_with_external(root, &[scratch_base.to_path_buf()], external_roots, untracked_build_script_inputs)
    }

    /// Reads the record, or returns an empty one.
    ///
    /// Every failure is an empty cache rather than an error. A cache that cannot be read has cost
    /// the run nothing but the time it takes to rebuild, which is the property that lets it be
    /// deleted, corrupted or written by a different version without anyone having to care.
    ///
    /// The build context is not checked here, because there is no longer one answer to check it
    /// against: each [`Tier`] names the terms it depends on, and [`Self::settled`],
    /// [`Self::probes`] and [`Self::ordering`] apply their own. A file written under another
    /// toolchain is a real record with a smaller usable part, not a foreign one.
    #[must_use]
    pub fn load(base: &Utf8Path) -> Self {
        Self::load_raw(base).unwrap_or_default()
    }

    /// Reads the record from disk, or nothing when it is absent, unreadable or a foreign format.
    fn load_raw(base: &Utf8Path) -> Option<Self> {
        let text = fs::read_to_string(base.join(FILE)).ok()?;
        let record = serde_json::from_str::<Self>(&text).ok()?;

        (record.version == VERSION).then_some(record)
    }

    /// What caught each mutant last time, whatever this run's build context is.
    ///
    /// No context is checked: a probe from another feature set is a wasted test process rather than
    /// a wrong verdict, while discarding it would make the map cold on the runs it is worth the most
    /// on.
    #[must_use]
    pub const fn probes(&self) -> &HashMap<MutantId, Killer> {
        &self.hints
    }

    /// Every verdict the record holds, paired with the mutant it belongs to.
    ///
    /// Named `iter` rather than for what it yields, because that is the spelling a caller looks
    /// for first: this is the record's one iteration entry point, and a domain noun would hide it.
    /// `&RunRecord` implements [`IntoIterator`] over the same items, so a `for` loop reaches them
    /// without naming a method at all.
    ///
    /// Offered without any invalidation of its own, because the callers are the tiers that need
    /// none: the artifact promotion, which admits only what cannot move a score, and the build
    /// order. Anything that *believes* a verdict goes through [`Self::settled`] instead, which
    /// applies both the source digest and the tier's context terms.
    #[must_use]
    pub fn iter(&self) -> Entries<'_> {
        Entries {
            files: self.files.iter(),
            mutants: None,
        }
    }

    /// Whether this record holds anything that the [`Tier::Unviability`] rules govern.
    ///
    /// Asked so that a run can tell the reader which term of the context cost it the cache, and stay
    /// quiet when the record held no unviability to lose in the first place.
    #[must_use]
    pub fn holds_unviability(&self) -> bool {
        self.files
            .iter()
            .flat_map(|file| file.mutants.iter())
            .any(|entry| entry.outcome == Outcome::CompileError)
    }

    /// The mutants this record saw fail to compile, offered as an order to build in.
    ///
    /// [`Tier::Ordering`] requires no term of the context and no source digest, which is what
    /// separates it from [`Self::settled`]: this is not a claim that these mutants are unviable
    /// now, only that they are the ones worth compiling first. Nothing downstream may withhold,
    /// settle or exclude a mutant on the strength of it — every one of them is built, and the
    /// compiler decides, exactly as it would have without the hint.
    #[must_use]
    pub fn ordering(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .iter()
            .filter(|(_id, outcome)| *outcome == Outcome::CompileError)
            .map(|(id, _outcome)| id)
            .collect();

        // Sorted so that two runs over the same record hand the build the same order. The build
        // itself keys by ordinal, but a hint set that iterated a map would still make the reported
        // counts and any future tie-break depend on hash order.
        ids.sort_unstable();
        ids.dedup();

        ids
    }

    /// The context this record was written under, which is provenance rather than a gate.
    #[must_use]
    pub const fn context(&self) -> &ContextDigest {
        &self.context
    }

    /// Replaces the hints in the record on disk, leaving every verdict where it is.
    ///
    /// Written by the sweep rather than at the end of the run, and written even when the sweep
    /// failed: a run that stopped partway still learned which test caught every mutant it got to,
    /// and discarding that would make an abandoned run cost the next one as much as it cost this
    /// one.
    ///
    /// A failure is reported as a deferred note rather than failing the run.
    pub fn store_probes(base: &Utf8Path, probes: &HashMap<MutantId, Killer>) {
        let mut record = Self::load_raw(base).unwrap_or_default();

        record.version = VERSION;
        record.hints.clone_from(probes);

        let Ok(text) = serde_json::to_string(&record) else {
            return;
        };

        if let Err(failure) = crate::elements::write(&base.join(FILE), &text) {
            crate::notes::note(format!("could not save run-record probes: {failure}"));
        }
    }

    /// What this record settles, given the sources as they are now and how far it is believed.
    ///
    /// A changed workspace input contributes nothing. The pre-execution snapshot covers every
    /// workspace file Cargo can consult, so a changed helper, manifest, lockfile, build script,
    /// cargo configuration or test rejects the whole record rather than rebinding an old outcome
    /// to new bytes.
    ///
    /// The other invalidation is the build context, applied per tier rather than to the file as a
    /// whole. Both tiers require every applicable term: compiler and execution-policy changes can
    /// alter verdicts as surely as source changes can.
    ///
    /// The run's context is resolved against `root` before either tier is asked, because the terms
    /// that describe the workspace's own settings are the ones a caller cannot have filled in: it
    /// built its context from a command line, and this is the first place the workspace is known.
    /// The recorded side was resolved the same way when it was written.
    ///
    /// Deliberately conservative in one direction only. A file that cannot be read now is treated
    /// as changed, so its mutants are re-tried; the cost of that is time, and the cost of the other
    /// choice is signal.
    ///
    /// Returns the verdicts alongside the number of kills that were refused because the test that
    /// did the killing is no longer declared anywhere in the workspace. The file digest cannot
    /// catch that: a kill is a claim about the suite, and the suite is not the file the mutant
    /// lives in.
    #[must_use]
    pub fn settled(
        &self,
        root: &Utf8Path,
        trust: Trust,
        killers: &Killers,
        context: &ContextDigest,
    ) -> (HashMap<MutantId, Outcome>, usize) {
        let current_inputs = self.inputs.recapture(root);

        self.settled_against(root, trust, killers, context, &current_inputs)
    }

    pub(crate) fn settled_against(
        &self,
        root: &Utf8Path,
        trust: Trust,
        killers: &Killers,
        context: &ContextDigest,
        current_inputs: &WorkspaceSnapshot,
    ) -> (HashMap<MutantId, Outcome>, usize) {
        let mut settled = HashMap::default();
        let mut declined = 0;

        if !current_inputs.is_complete() {
            return (settled, declined);
        }

        let workspace_unchanged = self.inputs == *current_inputs;
        let current = context.resolved_at(root);
        let unviability = Tier::Unviability.admits(&self.context, &current);
        let verdicts = Tier::Verdict.admits(&self.context, &current);

        if !(unviability || verdicts && workspace_unchanged) {
            return (settled, declined);
        }

        let files_by_path: HashMap<&Utf8Path, &RecordedFile> = self.files.iter().map(|file| (file.path.as_path(), file)).collect();

        for file in &self.files {
            if !is_unchanged(file, current_inputs) {
                continue;
            }

            for entry in &file.mutants {
                let admitted = if entry.outcome == Outcome::CompileError {
                    unviability
                        && (workspace_unchanged
                            || self
                                .compilation_roots
                                .get(&file.package)
                                .is_some_and(|roots| self.inputs.matches_compilation_inputs(current_inputs, roots)))
                } else {
                    verdicts && workspace_unchanged && trust == Trust::Settled
                };

                if !admitted || entry.outcome == Outcome::Timeout {
                    continue;
                }

                if entry.outcome == Outcome::Killed && !still_killed(entry, killers, &files_by_path, root, current_inputs) {
                    declined += 1;

                    continue;
                }

                let _previous = settled.insert(entry.id.clone(), entry.outcome);
            }
        }

        (settled, declined)
    }

    /// Captures a record immediately, for callers outside an execution session.
    ///
    /// The run command uses [`Self::from_snapshot`] instead, with the snapshot captured before the
    /// workspace was copied. This constructor serves operations that create synthetic records in
    /// one uninterrupted step.
    #[cfg(test)]
    #[must_use]
    pub fn from_run(root: &Utf8Path, mutants: &[Mutant], context: &ContextDigest, _source_dirs: &[Utf8PathBuf]) -> Self {
        let inputs = WorkspaceSnapshot::capture(root, &[root.join(FILE)]);
        let killers = Killers::scan(&inputs.rust_files(root));

        Self::from_snapshot(root, mutants, context, inputs, &killers).unwrap_or_default()
    }

    /// Builds a record from outcomes bound to their pre-execution workspace snapshot.
    ///
    /// No current workspace bytes are used to stamp an outcome. If anything Cargo could have read
    /// changed after that snapshot, there is no safe attribution and the caller receives `None`.
    /// This costs the next run work rather than letting an old execution verdict acquire new
    /// provenance.
    #[cfg(test)]
    #[must_use]
    fn from_snapshot(
        root: &Utf8Path,
        mutants: &[Mutant],
        context: &ContextDigest,
        inputs: WorkspaceSnapshot,
        killers: &Killers,
    ) -> Option<Self> {
        let compilation_roots = mutants
            .iter()
            .map(|mutant| ((*mutant.package).to_owned(), vec![Utf8PathBuf::new()]))
            .collect();

        Self::from_snapshot_with_roots(root, mutants, context, inputs, killers, compilation_roots)
    }

    #[must_use]
    pub(crate) fn from_plan_snapshot(plan: &Plan, context: &ContextDigest, inputs: WorkspaceSnapshot, killers: &Killers) -> Option<Self> {
        let mut compilation_roots = HashMap::default();

        for package in plan.specs.keys() {
            let mut dependencies = plan.reach.get(package).cloned().unwrap_or_default();
            let _self = dependencies.insert(package.clone());
            let mut roots: Vec<Utf8PathBuf> = dependencies
                .iter()
                .filter_map(|dependency| plan.specs.get(dependency).map(|(root, _version)| root.clone()))
                .collect();

            roots.sort();
            roots.dedup();
            let _previous = compilation_roots.insert(package.clone(), roots);
        }

        Self::from_snapshot_with_roots(&plan.root, &plan.mutants, context, inputs, killers, compilation_roots)
    }

    fn from_snapshot_with_roots(
        root: &Utf8Path,
        mutants: &[Mutant],
        context: &ContextDigest,
        inputs: WorkspaceSnapshot,
        killers: &Killers,
        compilation_roots: HashMap<String, Vec<Utf8PathBuf>>,
    ) -> Option<Self> {
        if !inputs.matches_current(root) {
            return None;
        }

        let packages: HashMap<Utf8PathBuf, String> = mutants
            .iter()
            .map(|mutant| (mutant.file.to_path_buf(), (*mutant.package).to_owned()))
            .collect();
        let mut by_file: HashMap<Utf8PathBuf, Vec<Entry>> = inputs
            .files
            .iter()
            .filter(|file| file.path.extension() == Some("rs"))
            .map(|file| (file.path.clone(), Vec::new()))
            .collect();

        for mutant in mutants.iter().filter(|mutant| settled_verdict(mutant.outcome)) {
            let file = mutant.file.to_path_buf();
            let _known = inputs.file(&file)?;
            let killer_file = if mutant.outcome == Outcome::Killed {
                mutant.killed_by.as_deref().and_then(|name| {
                    killers
                        .verdict_file_for(name)
                        .and_then(|path| path.strip_prefix(root).ok())
                        .map(Utf8Path::to_path_buf)
                })
            } else {
                None
            };

            by_file.entry(file).or_default().push(Entry {
                id: mutant.id.clone(),
                outcome: mutant.outcome,
                killed_by: mutant.killed_by.clone(),
                killer_file,
                elapsed_ms: mutant.elapsed_ms,
            });
        }

        let mut files: Vec<RecordedFile> = by_file
            .into_iter()
            .filter_map(|(path, mut mutants)| {
                let input = inputs.file(&path)?;

                mutants.sort_by(|left, right| left.id.cmp(&right.id));
                mutants.dedup_by(|left, right| left.id == right.id);

                Some(RecordedFile {
                    package: packages.get(&path).cloned().unwrap_or_default(),
                    path,
                    digest: input.digest.clone(),
                    size: input.size,
                    mutants,
                })
            })
            .collect();

        files.sort_by(|left, right| left.path.cmp(&right.path));

        Some(Self {
            version: VERSION,
            context: context.resolved_at(root),
            files,
            inputs,
            compilation_roots,
            hints: HashMap::default(),
        })
    }

    /// Writes the cache over whatever an earlier run left, keeping what this run did not revisit.
    ///
    /// A run knows only about the files it looked at. `--in-diff`, a single package and a shard all
    /// narrow that to a handful, and a plain overwrite would throw away every entry for everything
    /// else — so a sequence of narrow runs, which is exactly the workflow narrowing exists for,
    /// would never accumulate a cache at all.
    ///
    /// This run wins outright for any file it has an entry for: it just built those mutants, so its
    /// answer is the current one and the older answer is not evidence against it. Every other file
    /// is carried forward only if it is still on disk unchanged, which is the same rule
    /// [`RunRecord::settled`] applies when reading — an entry that would not be believed is not
    /// worth keeping, and dropping it here is what keeps the union bounded by the files that still
    /// exist rather than by every file that ever did.
    ///
    /// A run that could not write its cache has still produced every verdict it was asked for, so
    /// a failure is reported as a deferred note rather than making the optimization a dependency.
    pub fn store(&self, base: &Utf8Path, root: &Utf8Path) {
        let earlier = Self::load_raw(base).unwrap_or_default();
        let merged = self.absorbing(&earlier);

        if !merged.inputs.matches_current(root) {
            return;
        }

        let Ok(text) = serde_json::to_string(&merged) else {
            return;
        };

        if let Err(failure) = crate::elements::write(&base.join(FILE), &text) {
            crate::notes::note(format!("could not save run record: {failure}"));
        }
    }

    /// This cache, plus the entries of `earlier` for files this run never visited.
    fn absorbing(&self, earlier: &Self) -> Self {
        let workspace_unchanged = earlier.inputs == self.inputs;
        let unviability = Tier::Unviability.admits(&earlier.context, &self.context);
        let verdicts = Tier::Verdict.admits(&earlier.context, &self.context) && workspace_unchanged;
        let mut carried = Vec::new();

        for file in &earlier.files {
            let compilation_unchanged = earlier
                .compilation_roots
                .get(&file.package)
                .is_some_and(|roots| earlier.inputs.matches_compilation_inputs(&self.inputs, roots));
            let mut admitted = file.clone();

            admitted.mutants.retain(|entry| {
                if entry.outcome == Outcome::CompileError {
                    unviability && compilation_unchanged
                } else {
                    verdicts
                }
            });

            if !admitted.mutants.is_empty() {
                carried.push(admitted);
            }
        }

        let carried_map: HashMap<&Utf8PathBuf, &RecordedFile> = carried.iter().map(|f| (&f.path, f)).collect();

        let mut seen = HashSet::default();
        let mut files: Vec<RecordedFile> = Vec::new();

        for file in &self.files {
            let _ = seen.insert(&file.path);
            if !file.mutants.is_empty() {
                files.push(file.clone());
            } else if let Some(earlier_file) = carried_map.get(&file.path) {
                if !earlier_file.mutants.is_empty() && is_unchanged(earlier_file, &self.inputs) {
                    files.push(RecordedFile {
                        path: file.path.clone(),
                        package: earlier_file.package.clone(),
                        digest: file.digest.clone(),
                        size: file.size,
                        mutants: earlier_file.mutants.clone(),
                    });
                } else {
                    files.push(file.clone());
                }
            } else {
                files.push(file.clone());
            }
        }

        for file in &carried {
            if !seen.contains(&file.path) && is_unchanged(file, &self.inputs) {
                files.push(file.clone());
            }
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));
        let mut compilation_roots = self.compilation_roots.clone();

        for file in &files {
            if !compilation_roots.contains_key(&file.package)
                && let Some(roots) = earlier.compilation_roots.get(&file.package)
            {
                let _previous = compilation_roots.insert(file.package.clone(), roots.clone());
            }
        }

        Self {
            version: VERSION,
            context: self.context.clone(),
            files,
            inputs: self.inputs.clone(),
            compilation_roots,
            hints: if self.hints.is_empty() {
                earlier.hints.clone()
            } else {
                self.hints.clone()
            },
        }
    }

    /// How many mutants this record holds.
    ///
    /// Only the tests ask. A run counts what the record actually spared it against the population
    /// instead, because an entry whose mutant is no longer there spared nobody anything.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.files.iter().map(|file| file.mutants.len()).sum()
    }
}

/// Whether a kill this record holds is still a kill.
///
/// A mutant's identity says nothing about the tests, so a kill is accepted only when its recorded
/// killer still has the same identity and contents.
fn still_killed(
    entry: &Entry,
    killers: &Killers,
    files_by_path: &HashMap<&Utf8Path, &RecordedFile>,
    root: &Utf8Path,
    current_inputs: &WorkspaceSnapshot,
) -> bool {
    let Some(name) = entry.killed_by.as_deref() else {
        return false;
    };
    let Some(recorded_path) = entry.killer_file.as_deref() else {
        return false;
    };
    let Some(test_file_path) = killers.verdict_file_for(name) else {
        return false;
    };
    let Ok(current_path) = test_file_path.strip_prefix(root) else {
        return false;
    };

    if current_path != recorded_path {
        return false;
    }

    let Some(recorded) = files_by_path.get(recorded_path) else {
        return false;
    };

    killers.file_digest(test_file_path) == Some(recorded.digest.as_str()) && is_unchanged(recorded, current_inputs)
}

/// Whether the current workspace snapshot has a recorded file exactly as it was when judged.
///
/// The snapshot already read and hashed every included file. Reusing that result avoids a second
/// content pass while keeping absence or an incomplete capture on the conservative side.
fn is_unchanged(file: &RecordedFile, current_inputs: &WorkspaceSnapshot) -> bool {
    current_inputs
        .file(&file.path)
        .is_some_and(|current| current.size == file.size && current.digest == file.digest)
}

/// A hex digest of some bytes.
pub(crate) fn digest(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new();
    let _ = hasher.update(bytes);

    hasher.finalize().to_hex().to_string()
}

/// Reports the compiler, cargo and wrapper identities, or nothing if either tool cannot be asked.
///
/// Failing to name the toolchain is not an error, but it is not harmless either, and the earlier
/// spelling of this — an empty string — got that backwards. An empty answer does not make the cache
/// *less* useful; it removes an invalidation axis entirely, so two runs under different compilers
/// hash the same and the second believes the first. `RUSTC` pointing at a wrapper that does not
/// answer `--version` would make that permanent and silent. `None` says so, and [`context`] turns
/// it into "do not use a cache at all", which costs a run some time instead of a mutant.
#[must_use]
pub fn toolchain() -> Option<String> {
    let program = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let rustc = Command::new(&program)
        .arg("-vV")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())?;
    let cargo_program = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo = Command::new(&cargo_program)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())?;
    let wrapper = env::var_os("RUSTC_WRAPPER").unwrap_or_default();
    let workspace_wrapper = env::var_os("RUSTC_WORKSPACE_WRAPPER").unwrap_or_default();

    Some(format!(
        "rustc={}\ncargo={}\nrustc_wrapper={}\nrustc_workspace_wrapper={}\n{rustc}\n{cargo}",
        program.to_string_lossy(),
        cargo_program.to_string_lossy(),
        wrapper.to_string_lossy(),
        workspace_wrapper.to_string_lossy()
    ))
}

/// The flags cargo will read from the ambient environment, if any.
///
/// Cargo takes the global ones from the first variable that has anything to say rather than merging
/// them: `CARGO_ENCODED_RUSTFLAGS`, then `RUSTFLAGS`, then `CARGO_BUILD_RUSTFLAGS`, which is the
/// variable spelling of `build.rustflags` and decides the build exactly as the other two do. Only
/// one of them is ever in force and only that one belongs in the key. A `--cfg` passed any of these
/// ways selects different code entirely — the design notes `--cfg loom` as an expected scenario —
/// so a cache blind to it carries unviability from a tree that was never the tree being built.
///
/// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` sits at its own precedence level rather than in that chain, so
/// it is added to the key rather than competing for it. Every such variable is added, not the one
/// naming the triple in force: the triple is not settled at the point this is read, and being
/// over-inclusive costs a cache that could have been kept while being under-inclusive costs a
/// mutant silently withheld from the denominator.
///
/// The file spelling of the same settings is not here, because it is not in the environment: the
/// configuration's own `build.rustflags` and target tables reach the key through [`Term::Config`].
#[must_use]
pub fn rustflags() -> Option<String> {
    let mut targeted: Vec<(String, String)> = env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;

            (name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS"))
                .then(|| Some((name, value.into_string().ok()?)))
                .flatten()
        })
        .collect();

    targeted.sort();

    rustflags_in(
        env::var("CARGO_ENCODED_RUSTFLAGS").ok(),
        env::var("RUSTFLAGS").ok(),
        env::var("CARGO_BUILD_RUSTFLAGS").ok(),
        &targeted,
    )
}

/// Picks the flags out of the three global variables, in the order cargo consults them, and appends
/// the target-specific ones, which cargo reads independently of that order.
///
/// Taken as values rather than read here so that a test can vary them without writing the process
/// environment.
fn rustflags_in(
    encoded: Option<String>,
    plain: Option<String>,
    configured: Option<String>,
    targeted: &[(String, String)],
) -> Option<String> {
    let chosen = encoded.or(plain).or(configured);

    if targeted.is_empty() {
        return chosen;
    }

    let mut text = chosen.unwrap_or_default();

    for (name, value) in targeted {
        text.push('\n');
        text.push_str(name);
        text.push('=');
        text.push_str(value);
    }

    Some(text)
}

/// Everything other than the sources that decides whether a mutant compiles.
///
/// Gathered into one value rather than passed as seven arguments so that adding an axis is a
/// compile error at every construction site, which is the failure this type exists to prevent: the
/// cache is *believed*, so a missing axis is a mutant silently dropped from the denominator —
/// a real gap in the suite reported as a better score.
#[derive(Debug, Default, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "captures discrete boolean CLI and build flags for context digest"
)]
pub struct Context<'a> {
    /// The feature arguments, as the command line gave them.
    pub features: &'a [String],

    /// Whether every feature was asked for.
    pub all_features: bool,

    /// Whether the default features were suppressed.
    pub no_default_features: bool,

    /// The cargo profile, which decides `debug_assertions` among other things.
    pub profile: Option<&'a str>,

    /// The arbitrary passthrough build arguments, which can carry `--cfg`, `-C` or `--target`.
    pub extra: &'a [String],

    /// Whichever of the two rustflags variables cargo will actually read.
    pub rustflags: Option<&'a str>,

    /// The compiler, cargo and wrapper identities, or `None` when either tool could not be asked.
    pub toolchain: Option<&'a str>,

    /// Test packages filter.
    pub test_packages: &'a [String],

    /// Test names to include.
    pub include_tests: &'a [String],

    /// Test names to exclude.
    pub exclude_tests: &'a [String],

    /// Whether to test the whole workspace.
    pub test_workspace: bool,

    /// Whether to run reachable test binaries whole instead of selecting individual test cases.
    pub whole_test_binaries: bool,

    /// Whether to run test binaries through `cargo nextest`.
    pub nextest: bool,

    /// Arguments added to every test binary with `--cargo-test-arg`.
    pub cargo_test_args: &'a [String],

    /// Arguments after the command-line `--` separator.
    pub test_args: &'a [String],

    /// Whether the healthy baseline was measured.
    pub baseline: bool,

    /// Whether a failing test was confirmed without a mutant.
    pub confirm: bool,

    /// Whether silence can produce a stall verdict.
    pub stall: bool,

    /// The multiplier for baseline-derived test timeouts.
    pub test_timeout_multiplier: Option<f64>,

    /// The lower bound for baseline-derived test timeouts.
    pub minimum_test_timeout: Option<f64>,

    /// Requested memory-control mode.
    pub memory: Option<crate::exec::MemoryControl>,

    /// The baseline-memory multiplier for a mutant's limit.
    pub memory_multiplier: Option<f64>,

    /// The fixed memory headroom for a mutant's limit.
    pub memory_headroom: Option<u64>,

    /// An explicit memory limit for a mutant.
    pub memory_limit: Option<u64>,

    /// An explicit memory limit for the baseline.
    pub baseline_memory_limit: Option<u64>,

    /// Whether memory-control relaunching was disabled.
    pub no_relaunch: bool,

    /// Whether ignored workspace files were copied into the build.
    pub copy_ignored: bool,

    /// Requested mutant concurrency, which can affect timeout scheduling.
    pub jobs: Option<usize>,

    /// Fixed build timeout.
    pub build_timeout: Option<f64>,

    /// Build timeout multiplier.
    pub build_timeout_multiplier: Option<f64>,

    /// Maximum rollback build rounds.
    pub rollback_rounds: u32,
}

/// Digests a [`Context`], or returns nothing when it cannot be trusted to be complete.
///
/// Features first, because they are the axis that actually moves: a workspace routinely has code
/// that only exists under one of them, and a cache that ignored them would carry unviability from a
/// run that never compiled the code in question.
///
/// The tool's own version is a term because [`Mutant::id`](crate::model::Mutant) — what the cache is
/// keyed by — hashes the item path, the mutator, the normalized site text, the occurrence and the
/// replacement index, every one of which is a property of *this tool*. An upgrade can change what an
/// id denotes while the sources, the features and the toolchain are all unchanged. `VERSION` guards
/// the file's format; this guards the meaning of the keys inside it.
///
/// Each term is digested on its own rather than folded into one hash, which is what lets a [`Tier`]
/// require some terms and not others. Within a term the parts are length-prefixed, so that no
/// rearrangement of them can produce the same digest as a different one — `["ab", "c"]` and
/// `["a", "bc"]` are different feature selections.
///
/// A `None` toolchain gives `None` here, which is what makes a compiler that cannot be asked
/// suppress the cache rather than silently drop out of the key.
///
/// [`Term::Config`] is left unstated, because the settings it covers live in the workspace and this
/// is called before the workspace has been located. `ContextDigest::resolved_at` fills it in
/// where the root is in hand, which is both ends of every comparison the gate makes.
#[must_use]
pub fn context(of: &Context<'_>) -> Option<ContextDigest> {
    let environment = inherited_environment();

    context_in(of, env::var("CARGO_BUILD_TARGET").ok().as_deref(), &environment)
}

/// Digests a context against a given `CARGO_BUILD_TARGET`, so a test can vary the one variable
/// this reads for itself.
///
/// The variable is taken as a value rather than looked up where it is needed because the workspace
/// forbids writing the process environment. Everything else the digest covers arrives through
/// [`Context`] or through the workspace root.
fn context_in(of: &Context<'_>, build_target: Option<&str>, environment: &[(Vec<u8>, Vec<u8>)]) -> Option<ContextDigest> {
    let toolchain = of.toolchain?;

    let mut features: Vec<&[u8]> = of.features.iter().map(String::as_bytes).collect();
    let flags = [u8::from(of.all_features), u8::from(of.no_default_features)];

    features.push(&flags);

    let targets = Build::requested_targets(of.extra, build_target);
    let named: Vec<&[u8]> = targets.iter().map(String::as_bytes).collect();

    let mut test_parts: Vec<&[u8]> = Vec::new();
    for pkg in of.test_packages {
        test_parts.push(pkg.as_bytes());
    }
    test_parts.push(b":inc:");
    for inc in of.include_tests {
        test_parts.push(inc.as_bytes());
    }
    test_parts.push(b":exc:");
    for exc in of.exclude_tests {
        test_parts.push(exc.as_bytes());
    }
    // The tag differs from the bit spelling that preceded it so that records written under the
    // opposite default are invalidated rather than believed.
    test_parts.push(b":case-reachability-default:");
    let test_flags = [u8::from(of.test_workspace), u8::from(of.whole_test_binaries), u8::from(of.nextest)];
    test_parts.push(&test_flags);

    let mut policy_parts: Vec<&[u8]> = Vec::new();
    policy_parts.push(b":cargo-test:");
    policy_parts.extend(of.cargo_test_args.iter().map(String::as_bytes));
    policy_parts.push(b":post--:");
    policy_parts.extend(of.test_args.iter().map(String::as_bytes));
    let policy = format!(
        "baseline={};confirm={};stall={};timeout_multiplier={:?};timeout_floor={:?};memory={:?};\
         memory_multiplier={:?};memory_headroom={:?};memory_limit={:?};baseline_memory_limit={:?};\
         no_relaunch={};copy_ignored={};jobs={:?};build_timeout={:?};build_timeout_multiplier={:?};\
         rollback_rounds={}",
        of.baseline,
        of.confirm,
        of.stall,
        of.test_timeout_multiplier,
        of.minimum_test_timeout,
        of.memory,
        of.memory_multiplier,
        of.memory_headroom,
        of.memory_limit,
        of.baseline_memory_limit,
        of.no_relaunch,
        of.copy_ignored,
        of.jobs,
        of.build_timeout,
        of.build_timeout_multiplier,
        of.rollback_rounds
    );
    policy_parts.push(policy.as_bytes());

    let mut environment_parts = Vec::with_capacity(environment.len().saturating_mul(2));
    for (name, value) in environment {
        environment_parts.push(name.as_slice());
        environment_parts.push(value.as_slice());
    }

    Some(ContextDigest {
        features: term(Term::Features, &features),
        profile: term(Term::Profile, &[of.profile.unwrap_or_default().as_bytes()]),
        rustflags: term(Term::Rustflags, &[of.rustflags.unwrap_or_default().as_bytes()]),
        target: Some(term(Term::Target, &named)),
        config: None,
        extra: term(Term::Extra, &of.extra.iter().map(String::as_bytes).collect::<Vec<&[u8]>>()),
        toolchain: term(Term::Toolchain, &[toolchain.as_bytes()]),
        tool: term(Term::Tool, &[env!("CARGO_PKG_VERSION").as_bytes()]),
        tests: term(Term::Tests, &test_parts),
        policy: term(Term::Policy, &policy_parts),
        environment: Some(term(Term::Environment, &environment_parts)),
    })
}

/// The process environment that reaches a test binary without gamma naming it.
///
/// Test commands inherit their parent's environment and then overlay only gamma's control
/// variables. Hash every inherited name and value rather than trying to guess which application
/// variables a test, fixture, subprocess or build-produced helper consumes. The digest is sorted
/// and length-prefixed by [`term`], so its value is stable across enumeration order and never
/// serializes the environment's raw contents into the run record.
fn inherited_environment() -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut variables: Vec<(Vec<u8>, Vec<u8>)> = env::vars_os()
        .map(|(name, value)| (name.as_encoded_bytes().to_vec(), value.as_encoded_bytes().to_vec()))
        .collect();

    variables.sort_unstable();
    variables
}

/// Which term covers each input the build resolution reads, so the two modules cannot drift apart.
///
/// [`Build::INPUTS`](crate::cfg::Build::INPUTS) is where those inputs are named, beside the code
/// that reads them; this says what the record does about each one. The pair is checked by a test
/// rather than by the type system because the inputs are strings in a configuration format and
/// there is no type that could hold them — but an input added there and not here fails that test,
/// which is the drift this exists to stop. Unviability is withheld from the denominator on the
/// strength of these terms, so an input nobody covers is a mutant nobody counts.
#[cfg(test)]
const COVERAGE: &[(&str, Term)] = &[
    ("CARGO_BUILD_TARGET", Term::Target),
    ("CARGO_ENCODED_RUSTFLAGS", Term::Rustflags),
    ("RUSTFLAGS", Term::Rustflags),
    ("CARGO_BUILD_RUSTFLAGS", Term::Rustflags),
    ("CARGO_TARGET_<triple>_RUSTFLAGS", Term::Rustflags),
    // Only ever a way of naming a file. Whatever it selects is read into the settings themselves,
    // so the values are covered wherever the home happens to be.
    ("CARGO_HOME", Term::Config),
    ("cargo config files", Term::Config),
    ("build.target", Term::Config),
    ("build.rustflags", Term::Config),
    ("target.*.rustflags", Term::Config),
    ("profile.*", Term::Config),
];

/// Digests the parts of one context term.
///
/// The term's own name goes in first, so two terms that happen to hold the same bytes — an unset
/// profile and unset rustflags are both the empty string — still digest differently. Nothing
/// compares one term's digest against another's, so this changes no decision; it exists so that a
/// reader comparing two envelopes by eye, or a future term that is derived rather than compared
/// field by field, cannot be misled by a coincidence.
fn term(name: Term, parts: &[&[u8]]) -> String {
    let mut hasher = Hasher::new();

    let named = name.name().as_bytes();
    let _ = hasher.update(&(named.len() as u64).to_le_bytes());
    let _ = hasher.update(named);

    for part in parts {
        // Length-prefixed, so that no rearrangement of the parts can produce the same digest as a
        // different one.
        let _ = hasher.update(&(part.len() as u64).to_le_bytes());
        let _ = hasher.update(part);
    }

    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::fixtures;
    use crate::testing::workdir;

    fn mutant(id: &str, file: &str, outcome: Outcome) -> Mutant {
        Mutant {
            id: id.to_owned().into(),
            file: (Utf8PathBuf::from(file)).into(),
            mutator: ("arith".to_owned()).into(),
            item_path: ("subject::add".to_owned()).into(),
            original: "+".to_owned().into(),
            replacement: "-".to_owned().into(),
            outcome,
            ..fixtures::mutant()
        }
    }

    fn workspace(prefix: &str, body: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = workdir(prefix);
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("the work directory should be UTF-8");

        fs::create_dir_all(root.join("src")).expect("the source directory should be creatable");
        fs::write(root.join("src/lib.rs"), body).expect("the source should be writable");

        (dir, root)
    }

    /// Indexes the tests declared by a file written into the workspace for the purpose.
    fn killers(root: &Utf8PathBuf, body: &str) -> Killers {
        let path = root.join("src/tests.rs");

        fs::write(&path, body).expect("the test source should be writable");

        Killers::scan(&[path])
    }

    fn killed(id: &str, by: Option<&str>) -> Mutant {
        Mutant {
            killed_by: by.map(str::to_owned),
            elapsed_ms: 42,
            ..mutant(id, "src/lib.rs", Outcome::Killed)
        }
    }

    /// The build context every test writes and reads under unless it is varying one.
    fn envelope() -> ContextDigest {
        context(&plain()).expect("a named toolchain gives a context")
    }

    fn from_run(root: &Utf8Path, mutants: &[Mutant], context: &ContextDigest) -> RunRecord {
        RunRecord::from_run(root, mutants, context, &[root.join("src"), root.join("tests")])
    }

    fn package_mutant(id: &str, package: &str, file: &str, outcome: Outcome) -> Mutant {
        Mutant {
            package: package.to_owned().into(),
            ..mutant(id, file, outcome)
        }
    }

    fn cache_plan(root: &Utf8Path, mutants: Vec<Mutant>, a_depends_on_b: bool) -> Plan {
        let mut a_reach = HashSet::default();
        let _inserted = a_reach.insert("a".to_owned());
        if a_depends_on_b {
            let _inserted = a_reach.insert("b".to_owned());
        }
        let mut b_reach = HashSet::default();
        let _inserted = b_reach.insert("b".to_owned());
        let mut reach = HashMap::default();
        let _previous = reach.insert("a".to_owned(), a_reach);
        let _previous = reach.insert("b".to_owned(), b_reach);
        let mut specs = HashMap::default();
        let _previous = specs.insert("a".to_owned(), (Utf8PathBuf::from("crates/a"), "0.1.0".to_owned()));
        let _previous = specs.insert("b".to_owned(), (Utf8PathBuf::from("crates/b"), "0.1.0".to_owned()));

        Plan {
            root: root.to_owned(),
            files: Vec::new(),
            mutants,
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: HashMap::default(),
            skipped: Vec::new(),
            reach,
            specs,
        }
    }

    fn cache_workspace(prefix: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = workdir(prefix);
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("the work directory should be UTF-8");

        for package in ["a", "b"] {
            fs::create_dir_all(root.join(format!("crates/{package}/src"))).expect("package source directory");
            fs::write(
                root.join(format!("crates/{package}/Cargo.toml")),
                format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\n"),
            )
            .expect("package manifest");
            fs::write(
                root.join(format!("crates/{package}/src/lib.rs")),
                format!("pub fn {package}() -> bool {{ true }}\n"),
            )
            .expect("package source");
        }

        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n").expect("workspace manifest");
        fs::write(root.join("Cargo.lock"), "# lock\n").expect("workspace lockfile");

        (dir, root)
    }

    fn from_plan(plan: &Plan) -> RunRecord {
        let inputs = WorkspaceSnapshot::capture(&plan.root, &[]);

        RunRecord::from_plan_snapshot(plan, &envelope(), inputs, &Killers::default())
            .expect("an unchanged workspace should produce a record")
    }

    /// The same context under a different compiler, and identical in every other term.
    fn under_another_toolchain() -> ContextDigest {
        context(&Context {
            toolchain: Some("1.91.0"),
            ..plain()
        })
        .expect("a named toolchain gives a context")
    }

    /// The same context under a different feature selection, and identical in every other term.
    fn under_another_feature_set() -> ContextDigest {
        let features = ["extra".to_owned()];

        context(&Context {
            features: &features,
            ..plain()
        })
        .expect("a named toolchain gives a context")
    }

    /// No observed test verdict can become durable evidence about a later run.
    #[test]
    fn only_compiler_unviability_is_recorded() {
        let (_dir, root) = workspace("record-unsettled-", "fn add() {}");
        let population = [
            mutant("killed", "src/lib.rs", Outcome::Killed),
            mutant("survivor", "src/lib.rs", Outcome::Survived),
            mutant("flake", "src/lib.rs", Outcome::Flaky),
            mutant("unbuilt", "src/lib.rs", Outcome::NotBuilt),
            mutant("hungry", "src/lib.rs", Outcome::OutOfMemory),
            mutant("uncovered", "src/lib.rs", Outcome::NoCoverage),
            mutant("pending", "src/lib.rs", Outcome::Pending),
            mutant("slow", "src/lib.rs", Outcome::Timeout),
            mutant("skipped", "src/lib.rs", Outcome::Ignored),
            mutant("unviable", "src/lib.rs", Outcome::CompileError),
        ];

        let record = from_run(&root, &population, &envelope());
        let (settled, _declined) = record.settled(&root, Trust::Settled, &Killers::default(), &envelope());

        assert_eq!(settled.len(), 1, "{settled:?}");
        assert_eq!(settled.get("unviable"), Some(&Outcome::CompileError));
    }

    #[test]
    fn a_cached_mutant_is_settled_when_its_file_is_unchanged() {
        let (_dir, root) = workspace("record-hit-", "fn add() {}");
        let cache = from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope());

        assert_eq!(
            cache.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.get("abc"),
            Some(&Outcome::CompileError)
        );
    }

    /// The invalidation that makes this safe. A mutant that could not compile against one version
    /// of the surrounding types may compile against the next, and assuming otherwise would drop a
    /// mutant that might have survived out of the denominator.
    #[test]
    fn a_cached_mutant_is_retried_when_its_file_changed() {
        let (_dir, root) = workspace("record-miss-", "fn add() {}");
        let cache = from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope());

        fs::write(root.join("src/lib.rs"), "fn add(a: i32) -> i32 { a }").expect("the source should be rewritable");

        assert!(
            cache.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty(),
            "an edited file must not carry its unviability"
        );
    }

    /// A file that has been deleted or moved is treated as changed, because the only alternative is
    /// to assume something about source nobody can read.
    #[test]
    fn a_cached_mutant_is_retried_when_its_file_is_gone() {
        let (_dir, root) = workspace("record-gone-", "fn add() {}");
        let cache = from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope());

        fs::remove_file(root.join("src/lib.rs")).expect("the source should be removable");

        assert!(cache.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty());
    }

    #[test]
    fn an_unrelated_package_change_preserves_unviability_but_not_test_verdicts() {
        let (_dir, root) = cache_workspace("record-package-input-");
        let unviable = package_mutant("unviable", "a", "crates/a/src/lib.rs", Outcome::CompileError);
        let survived = package_mutant("survived", "a", "crates/a/src/lib.rs", Outcome::Survived);
        let plan = cache_plan(&root, vec![unviable, survived], false);
        let record = from_plan(&plan);

        fs::write(root.join("crates/b/src/lib.rs"), "pub fn b() -> bool { false }\n").expect("unrelated source edit");

        let settled = record.settled(&root, Trust::Settled, &Killers::default(), &envelope()).0;

        assert_eq!(settled.get("unviable"), Some(&Outcome::CompileError));
        assert!(!settled.contains_key("survived"));
    }

    #[test]
    fn a_narrow_run_writes_back_safe_unviability_after_an_unrelated_change() {
        let (_dir, root) = cache_workspace("record-package-writeback-");
        let unviable = package_mutant("unviable", "a", "crates/a/src/lib.rs", Outcome::CompileError);
        let survived = package_mutant("survived", "a", "crates/a/src/lib.rs", Outcome::Survived);
        let earlier_plan = cache_plan(&root, vec![unviable, survived], false);
        let earlier = from_plan(&earlier_plan);

        fs::write(root.join("crates/b/src/lib.rs"), "pub fn b() -> bool { false }\n").expect("unrelated source edit");
        let fresh = package_mutant("fresh", "b", "crates/b/src/lib.rs", Outcome::CompileError);
        let current_plan = cache_plan(&root, vec![fresh], false);
        let current = from_plan(&current_plan);
        let merged = current.absorbing(&earlier);
        let settled = merged.settled(&root, Trust::Settled, &Killers::default(), &envelope()).0;

        assert_eq!(settled.get("unviable"), Some(&Outcome::CompileError));
        assert_eq!(settled.get("fresh"), Some(&Outcome::CompileError));
        assert!(!settled.contains_key("survived"));
    }

    /// The denominator safety property behind fine-grained invalidation: a source edit that makes
    /// an old compile failure viable must force that mutant back through compilation.
    #[test]
    fn a_package_source_change_never_carries_stale_unviability() {
        let (_dir, root) = cache_workspace("record-package-source-");
        let unviable = package_mutant("unviable", "a", "crates/a/src/lib.rs", Outcome::CompileError);
        let plan = cache_plan(&root, vec![unviable], false);
        let record = from_plan(&plan);

        fs::write(root.join("crates/a/src/lib.rs"), "pub fn a() -> bool { false }\n").expect("package source edit");

        assert!(
            record.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty(),
            "a newly viable mutant must remain in the denominator"
        );
    }

    #[test]
    fn a_workspace_dependency_change_invalidates_dependent_unviability() {
        let (_dir, root) = cache_workspace("record-dependency-input-");
        let unviable = package_mutant("unviable", "a", "crates/a/src/lib.rs", Outcome::CompileError);
        let plan = cache_plan(&root, vec![unviable], true);
        let record = from_plan(&plan);

        fs::write(root.join("crates/b/src/lib.rs"), "pub fn b() -> bool { false }\n").expect("dependency source edit");

        assert!(record.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty());
    }

    #[test]
    fn every_global_and_package_compilation_input_invalidates_unviability() {
        let edits = [
            ("Cargo.toml", "[workspace]\nmembers = []\n"),
            ("Cargo.lock", "# changed lock\n"),
            (".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"changed\"]\n"),
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n"),
            ("rust-toolchain", "1.72.0\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.2.0\"\n"),
            ("crates/a/build.rs", "fn main() { println!(\"cargo:rustc-cfg=changed\"); }\n"),
        ];

        for (path, contents) in edits {
            let (_dir, root) = cache_workspace("record-compilation-input-");
            let unviable = package_mutant("unviable", "a", "crates/a/src/lib.rs", Outcome::CompileError);
            let plan = cache_plan(&root, vec![unviable], false);
            let record = from_plan(&plan);
            let changed = root.join(path);

            fs::create_dir_all(changed.parent().expect("fixture paths have parents")).expect("fixture parent");
            fs::write(changed, contents).expect("compilation input edit");

            assert!(
                record.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty(),
                "{path} retained stale unviability"
            );
        }
    }

    #[test]
    fn every_workspace_cargo_input_invalidates_a_carried_outcome() {
        let edits = [
            ("src/helper.rs", "pub fn answer() -> u32 { 1 }\n", "pub fn answer() -> u32 { 2 }\n"),
            ("tests/behaviour.rs", "#[test]\nfn answer() {}\n", "#[test]\nfn answer_now() {}\n"),
            (
                "Cargo.toml",
                "[package]\nname = \"subject\"\n",
                "[package]\nname = \"subject-two\"\n",
            ),
            ("Cargo.lock", "version = 4\n", "version = 4\n# changed\n"),
            (
                ".cargo/config.toml",
                "[build]\ntarget-dir = \"build\"\n",
                "[build]\ntarget-dir = \"other\"\n",
            ),
            (
                "build.rs",
                "fn main() {}\n",
                "fn main() { println!(\"cargo:rerun-if-changed=x\"); }\n",
            ),
        ];

        for (path, before, after) in edits {
            let (_dir, root) = workspace("record-workspace-input-", "fn add() {}");
            let file = root.join(path);
            fs::create_dir_all(file.parent().expect("fixture paths have parents").as_std_path()).expect("fixture parent");
            fs::write(file.as_std_path(), before).expect("fixture input");
            let record = from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope());

            fs::write(file.as_std_path(), after).expect("changed input");

            assert!(
                record.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty(),
                "{path} changed without invalidating the outcome"
            );
        }
    }

    #[test]
    fn an_external_path_dependency_change_carries_neither_verdicts_nor_unviability() {
        let directory = workdir("record-external-path-");
        let container = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the work directory should be UTF-8");
        let root = container.join("workspace");
        let dependency = container.join("dependency");

        fs::create_dir_all(root.join("src")).expect("workspace source directory");
        fs::create_dir_all(dependency.join("src")).expect("dependency source directory");
        fs::write(root.join("src/lib.rs"), "fn add() {}\n").expect("workspace source");
        fs::write(root.join("src/tests.rs"), "#[test]\nfn caught() {}\n").expect("workspace test");
        fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\n",
        )
        .expect("dependency manifest");
        fs::write(dependency.join("src/lib.rs"), "pub fn answer() -> u8 { 1 }\n").expect("dependency source");

        let inputs = RunRecord::snapshot_with_external(&root, &root.join("target/gamma"), core::slice::from_ref(&dependency), false);
        let index = Killers::scan(&[root.join("src/tests.rs")]);
        let record = RunRecord::from_snapshot(
            &root,
            &[
                mutant("unviable", "src/lib.rs", Outcome::CompileError),
                killed("killed", Some("caught")),
            ],
            &envelope(),
            inputs,
            &index,
        )
        .expect("the external dependency belongs to the pre-execution snapshot");

        fs::write(dependency.join("src/lib.rs"), "pub fn answer() -> u8 { 2 }\n").expect("changed dependency source");

        assert!(
            record.settled(&root, Trust::Settled, &index, &envelope()).0.is_empty(),
            "an external dependency change must recompile and retest every recorded outcome"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_external_symlink_referent_cannot_create_a_reusable_record() {
        let directory = workdir("record-external-symlink-");
        let container = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the work directory should be UTF-8");
        let root = container.join("workspace");
        let external = container.join("external.rs");

        fs::create_dir_all(root.join("src")).expect("workspace source directory");
        fs::write(root.join("src/lib.rs"), "fn add() {}\n").expect("workspace source");
        fs::write(root.join("src/tests.rs"), "#[test]\nfn caught() {}\n").expect("workspace test");
        fs::write(&external, "pub fn external() {}\n").expect("external source");
        std::os::unix::fs::symlink(&external, root.join("src/linked.rs")).expect("external source link");

        let index = Killers::scan(&[root.join("src/tests.rs")]);
        let inputs = RunRecord::snapshot(&root, &root.join("target/gamma"));

        assert!(
            RunRecord::from_snapshot(
                &root,
                &[
                    mutant("unviable", "src/lib.rs", Outcome::CompileError),
                    killed("killed", Some("caught")),
                ],
                &envelope(),
                inputs,
                &index,
            )
            .is_none(),
            "a symlink referent outside the workspace must carry neither a verdict nor an unviability"
        );
    }

    #[test]
    fn an_edit_after_the_pre_execution_snapshot_records_no_outcome() {
        let (_dir, root) = workspace("record-mid-run-edit-", "fn add() {}");
        let snapshot = RunRecord::snapshot(&root, &root.join("target/gamma"));
        let barrier = Arc::new(Barrier::new(2));
        let editor_barrier = Arc::clone(&barrier);
        let edited = root.join("src/lib.rs");

        let editor = thread::spawn(move || {
            let _waiting = editor_barrier.wait();
            fs::write(edited, "fn add() { panic!() }").expect("changed source");
            let _written = editor_barrier.wait();
        });

        let _snapshot_taken = barrier.wait();
        let _edit_finished = barrier.wait();
        editor.join().expect("editor thread");

        assert!(
            RunRecord::from_snapshot(
                &root,
                &[mutant("abc", "src/lib.rs", Outcome::CompileError)],
                &envelope(),
                snapshot,
                &Killers::scan(&[])
            )
            .is_none(),
            "post-run bytes must never be stamped onto a pre-edit outcome"
        );
    }

    /// The edit-and-revert case, which content alone cannot see.
    ///
    /// A file changed during the run and put back before it ends is byte-identical to what was
    /// captured, so every digest still agrees — yet the outcomes were judged against the
    /// intermediate bytes and are worthless. The modification time is what distinguishes the two,
    /// because putting the content back is itself a write.
    ///
    /// The reverted file's time is stamped explicitly rather than left to the write, so that the
    /// test measures the snapshot's comparison instead of the host filesystem's timestamp
    /// granularity — a real revert advances the time exactly this way, only by an amount the
    /// filesystem chooses.
    #[test]
    fn a_revert_to_the_original_bytes_does_not_restore_the_snapshot() {
        let (_dir, root) = workspace("record-aba-edit-", "fn add() {}");
        let source = root.join("src/lib.rs");
        let original = fs::read(source.as_std_path()).expect("fixture source");
        let snapshot = RunRecord::snapshot(&root, &root.join("target/gamma"));

        fs::write(source.as_std_path(), "fn add() { panic!() }").expect("mid-run edit");
        fs::write(source.as_std_path(), &original).expect("revert");
        stamp_later(&source);

        assert_eq!(
            fs::read(source.as_std_path()).expect("reverted source"),
            original,
            "the fixture must end byte-identical, or the test proves nothing new"
        );
        assert!(
            RunRecord::from_snapshot(
                &root,
                &[mutant("abc", "src/lib.rs", Outcome::CompileError)],
                &envelope(),
                snapshot,
                &Killers::scan(&[])
            )
            .is_none(),
            "a workspace edited and put back was still edited while the outcomes were produced"
        );
    }

    /// Moves a file's modification time safely past every time the run could have recorded.
    fn stamp_later(path: &Utf8Path) {
        let file = fs::File::options()
            .write(true)
            .open(path.as_std_path())
            .expect("fixture file opens for writing");

        file.set_modified(std::time::SystemTime::now() + core::time::Duration::from_secs(2))
            .expect("fixture filesystem records modification times");
    }

    #[test]
    fn target_and_scratch_artifacts_do_not_invalidate_the_workspace_snapshot() {
        let (_dir, root) = workspace("record-artifacts-", "fn add() {}");
        let scratch = root.join(".gamma-work/gamma");
        let snapshot = RunRecord::snapshot(&root, &scratch);
        fs::create_dir_all(root.join("target/debug").as_std_path()).expect("target directory");
        fs::create_dir_all(scratch.as_std_path()).expect("scratch directory");
        fs::write(root.join("target/debug/artifact"), "generated").expect("target artifact");
        fs::write(scratch.join("tree"), "generated").expect("scratch artifact");

        let record = RunRecord::from_snapshot(
            &root,
            &[mutant("abc", "src/lib.rs", Outcome::CompileError)],
            &envelope(),
            snapshot,
            &Killers::scan(&[]),
        )
        .expect("generated artifacts are not workspace inputs");

        assert_eq!(
            record.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.get("abc"),
            Some(&Outcome::CompileError)
        );
    }

    /// Only unviability is free. A kill is a claim about the test suite and carrying one unasked
    /// would inflate the score; a survivor is the finding the whole exercise exists for.
    #[test]
    fn no_other_verdict_is_ever_carried_for_free() {
        let (_dir, root) = workspace("record-only-", "fn add() {}");
        let population = [
            mutant("killed", "src/lib.rs", Outcome::Killed),
            mutant("survived", "src/lib.rs", Outcome::Survived),
            mutant("timeout", "src/lib.rs", Outcome::Timeout),
            mutant("unviable", "src/lib.rs", Outcome::CompileError),
        ];

        let settled = from_run(&root, &population, &envelope())
            .settled(&root, Trust::Free, &Killers::default(), &envelope())
            .0;

        assert_eq!(settled.len(), 1);
        assert_eq!(settled.get("unviable"), Some(&Outcome::CompileError));
    }

    #[test]
    fn a_timeout_is_not_stored_for_a_later_run() {
        let (_dir, root) = workspace("record-timeout-", "fn add() {}");
        let record = from_run(&root, &[mutant("timeout", "src/lib.rs", Outcome::Timeout)], &envelope());

        assert_eq!(record.len(), 0);
        assert!(record.settled(&root, Trust::Settled, &Killers::scan(&[]), &envelope()).0.is_empty());
    }

    #[test]
    fn a_cache_survives_a_round_trip_through_the_scratch_directory() {
        let (_dir, root) = workspace("record-round-", "fn add() {}");

        from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        assert_eq!(RunRecord::load(&root).len(), 1);
    }

    #[test]
    fn every_completed_record_publication_is_parseable() {
        let (_dir, root) = workspace("record-atomic-round-", "fn add() {}");

        for generation in 0..8 {
            let id = format!("mutant-{generation}");
            from_run(&root, &[mutant(&id, "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);
            let stored = fs::read_to_string(root.join(FILE)).expect("completed record");
            let _record: RunRecord = serde_json::from_str(&stored).expect("completed record parses");

            RunRecord::store_probes(
                &root,
                &core::iter::once((
                    id.into(),
                    Killer {
                        package: "subject".to_owned(),
                        target: "lib".to_owned(),
                        test: format!("caught_{generation}"),
                    },
                ))
                .collect(),
            );
            let stored = fs::read_to_string(root.join(FILE)).expect("completed probe record");
            let _record: RunRecord = serde_json::from_str(&stored).expect("completed probe record parses");
        }
    }

    #[test]
    fn failed_record_publications_preserve_the_prior_generation() {
        let (_dir, root) = workspace("record-atomic-fail-", "fn add() {}");

        from_run(&root, &[mutant("original", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);
        let before = fs::read_to_string(root.join(FILE)).expect("original record");

        crate::elements::before_next_publication(|scratch| {
            fs::remove_file(scratch).expect("remove staged record");
        });
        from_run(&root, &[mutant("replacement", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);
        assert_eq!(fs::read_to_string(root.join(FILE)).expect("prior record"), before);

        crate::elements::before_next_publication(|scratch| {
            fs::remove_file(scratch).expect("remove staged probes");
        });
        RunRecord::store_probes(
            &root,
            &core::iter::once((
                "replacement".into(),
                Killer {
                    package: "subject".to_owned(),
                    target: "lib".to_owned(),
                    test: "caught".to_owned(),
                },
            ))
            .collect(),
        );
        assert_eq!(fs::read_to_string(root.join(FILE)).expect("prior record"), before);
    }

    /// Unviability written under one feature set says nothing about a run under another: it is a
    /// claim about what compiles, and a partial answer there is indistinguishable from a wrong one.
    #[test]
    fn unviability_from_a_different_context_is_not_settled() {
        let (_dir, root) = workspace("record-context-", "fn add() {}");

        from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        let settled = RunRecord::load(&root)
            .settled(&root, Trust::Free, &Killers::default(), &under_another_feature_set())
            .0;

        assert!(settled.is_empty(), "unviability crossed a feature change");
    }

    /// Deleting the cache may only ever cost time. If a missing one could change a verdict it has
    /// become state the score depends on.
    #[test]
    fn a_missing_cache_is_an_empty_one() {
        let (_dir, root) = workspace("record-absent-", "fn add() {}");

        assert_eq!(RunRecord::load(&root).len(), 0);
    }

    /// So is a corrupt one, for the same reason.
    #[test]
    fn a_corrupt_cache_is_an_empty_one() {
        let (_dir, root) = workspace("record-corrupt-", "fn add() {}");

        fs::write(root.join(FILE), "{ this is not json").expect("the cache should be writable");

        assert_eq!(RunRecord::load(&root).len(), 0);
    }

    /// A run that adopted the cache has to write it back, or the cache is warm only every other
    /// run: the adopted mutants are never rebuilt, so they are never rediscovered either.
    #[test]
    fn adopted_mutants_are_written_back() {
        let (_dir, root) = workspace("record-writeback-", "fn add() {}");
        let population = [
            mutant("adopted", "src/lib.rs", Outcome::CompileError),
            mutant("fresh", "src/lib.rs", Outcome::CompileError),
        ];

        from_run(&root, &population, &envelope()).store(&root, &root);

        assert_eq!(RunRecord::load(&root).len(), 2);
    }

    /// A narrowed run keeps what it did not look at, which is the whole point of narrowing.
    ///
    /// `--in-diff`, a single package and a shard each know about a handful of files. Overwriting the
    /// file with only those would leave every later narrow run starting cold, so a sequence of them
    /// — the workflow narrowing exists to serve — would never accumulate a cache at all.
    #[test]
    fn a_narrowed_run_keeps_the_entries_it_never_looked_at() {
        let (_dir, root) = workspace("record-narrow-", "fn add() {}");

        fs::write(root.join("src/other.rs"), "fn other() {}").expect("the source should be writable");

        from_run(&root, &[mutant("first", "src/other.rs", Outcome::CompileError)], &envelope()).store(&root, &root);
        from_run(&root, &[mutant("second", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        let settled = RunRecord::load(&root)
            .settled(&root, Trust::Free, &Killers::default(), &envelope())
            .0;

        assert_eq!(
            settled.get("first"),
            Some(&Outcome::CompileError),
            "the earlier run's file was dropped"
        );
        assert_eq!(settled.get("second"), Some(&Outcome::CompileError));
    }

    /// A narrowed run cannot bless an older outcome merely by writing a fresh snapshot around it.
    ///
    /// The helper contains no mutant of either run, which is why checking only each entry's source
    /// file would retain `first`; Cargo can still compile it into the test binary and change the
    /// outcome. The old pre-execution snapshot must match before any old entry joins the new one.
    #[test]
    fn a_narrowed_run_drops_prior_entries_after_any_workspace_input_changes() {
        let (_dir, root) = workspace("record-narrow-snapshot-", "fn add() {}");

        fs::write(root.join("src/helper.rs"), "pub fn helper() -> u8 { 1 }").expect("helper source is writable");
        fs::write(root.join("src/other.rs"), "fn other() {}").expect("other source is writable");

        from_run(&root, &[mutant("first", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        fs::write(root.join("src/helper.rs"), "pub fn helper() -> u8 { 2 }").expect("helper source is writable");

        from_run(&root, &[mutant("second", "src/other.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        let settled = RunRecord::load(&root)
            .settled(&root, Trust::Free, &Killers::default(), &envelope())
            .0;

        assert_eq!(settled.get("first"), None, "the changed helper recertified an old outcome");
        assert_eq!(settled.get("second"), Some(&Outcome::CompileError));
    }

    /// Retention stops exactly where belief does: an entry for a file that has changed is dropped.
    ///
    /// Carrying it would be carrying a stale compile-error verdict, which takes a mutant that might
    /// now survive out of the denominator and turns a real gap in the suite into a better score.
    #[test]
    fn a_carried_entry_is_dropped_once_its_file_changes() {
        let (_dir, root) = workspace("record-stale-", "fn add() {}");

        fs::write(root.join("src/other.rs"), "fn other() {}").expect("the source should be writable");

        from_run(&root, &[mutant("first", "src/other.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        fs::write(root.join("src/other.rs"), "fn other() -> usize { 0 }").expect("the source should be writable");

        from_run(&root, &[mutant("second", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        let reloaded = RunRecord::load(&root);

        assert_eq!(reloaded.len(), 1, "a stale entry was carried forward");
        assert_eq!(
            reloaded
                .settled(&root, Trust::Free, &Killers::default(), &envelope())
                .0
                .get("second"),
            Some(&Outcome::CompileError)
        );
    }

    /// A file this run looked at is this run's to describe, however much the earlier one held.
    ///
    /// The older answer is not evidence against a build that just happened, so it is replaced rather
    /// than merged — otherwise a mutant that has since become viable would live in the cache forever
    /// under a file whose digest never changed.
    #[test]
    fn this_runs_answer_replaces_the_earlier_one_for_a_file_it_visited() {
        let (_dir, root) = workspace("record-replace-", "fn add() {}");
        let earlier = [
            mutant("gone", "src/lib.rs", Outcome::CompileError),
            mutant("kept", "src/lib.rs", Outcome::CompileError),
        ];

        from_run(&root, &earlier, &envelope()).store(&root, &root);
        from_run(&root, &[mutant("kept", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);

        let settled = RunRecord::load(&root)
            .settled(&root, Trust::Free, &Killers::default(), &envelope())
            .0;

        assert_eq!(settled.get("gone"), None, "a mutant this run found viable was carried anyway");
        assert_eq!(settled.get("kept"), Some(&Outcome::CompileError));
    }

    /// A file whose length has changed is rejected without being read.
    ///
    /// The length is a `stat` where the digest is a full read, so it is checked first — and it may
    /// only ever reject, because two different files of the same length are ordinary rather than
    /// exotic and accepting one would drop a mutant that might have survived.
    #[test]
    fn a_file_that_has_grown_is_rejected() {
        let (_dir, root) = workspace("record-grown-", "fn add() {}");
        let cache = from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope());

        fs::write(root.join("src/lib.rs"), "fn add() {} // and more").expect("the source should be writable");

        assert!(cache.settled(&root, Trust::Free, &Killers::default(), &envelope()).0.is_empty());
    }

    /// A context over nothing but a toolchain, for varying one axis at a time.
    fn plain() -> Context<'static> {
        Context {
            toolchain: Some("1.90.0"),
            baseline: true,
            confirm: true,
            stall: true,
            ..Context::default()
        }
    }

    #[test]
    fn a_changed_inherited_environment_refuses_carried_unviability() {
        let (_dir, root) = workspace("record-unviability-environment-", "fn add() {}");
        let before =
            context_in(&plain(), None, &[(b"SUBJECT_BUILD_MODE".to_vec(), b"before".to_vec())]).expect("a named toolchain gives a context");
        let after =
            context_in(&plain(), None, &[(b"SUBJECT_BUILD_MODE".to_vec(), b"after".to_vec())]).expect("a named toolchain gives a context");
        let record = from_run(&root, &[unviable("abc")], &before);

        assert_eq!(
            record.settled(&root, Trust::Free, &Killers::default(), &before).0.get("abc"),
            Some(&Outcome::CompileError),
            "the unchanged inherited environment may reuse unviability"
        );
        assert!(
            record.settled(&root, Trust::Free, &Killers::default(), &after).0.is_empty(),
            "a changed inherited environment must recompile an unviable mutant"
        );
    }

    /// Every axis that decides whether a mutant compiles has to reach the digest.
    ///
    /// The cache is believed rather than re-checked, so an axis missing from the key is not a slower
    /// run — it is a mutant withheld from the denominator on the strength of a build that never
    /// happened, which turns a real gap in the suite into a better score.
    ///
    /// The `let Context { .. }` below is the point of this test and not decoration. An enumeration
    /// of the fields that exist can only ever check what somebody already implemented; a `..` in
    /// that pattern, or a table written against the current signature, would let a field be added
    /// to `Context` and left out of `context` with every test still green. Destructuring without
    /// `..` makes adding an axis a compile error here, so the axis cannot be forgotten — it can
    /// only be deliberately dismissed by whoever adds the line that dismisses it.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive test verifying each context axis distinguishes the digest"
    )]
    fn the_context_distinguishes_the_things_that_decide_what_compiles() {
        let features = ["extra".to_owned()];
        let extra = ["--cfg=loom".to_owned()];
        let test_packages = ["pkg_a".to_owned()];
        let include_tests = ["test_a".to_owned()];
        let exclude_tests = ["test_b".to_owned()];
        let cargo_test_args = ["--nocapture".to_owned()];
        let test_args = ["--skip".to_owned(), "slow".to_owned()];

        // Adding a field to `Context` fails to compile here until it is given a varied value below.
        // The bindings are named rather than wildcards only because a pattern of all wildcards is
        // one clippy offers to replace with `..`, which would silently disarm the whole guard.
        let Context {
            features: _f,
            all_features: _a,
            no_default_features: _n,
            profile: _p,
            extra: _e,
            rustflags: _r,
            toolchain: _t,
            test_packages: _tp,
            include_tests: _it,
            exclude_tests: _et,
            test_workspace: _tw,
            whole_test_binaries: _wtb,
            nextest: _nx,
            cargo_test_args: _cta,
            test_args: _ta,
            baseline: _b,
            confirm: _c,
            stall: _s,
            test_timeout_multiplier: _ttm,
            minimum_test_timeout: _mtt,
            memory: _m,
            memory_multiplier: _mm,
            memory_headroom: _mh,
            memory_limit: _ml,
            baseline_memory_limit: _bml,
            no_relaunch: _nr,
            copy_ignored: _ci,
            jobs: _j,
            build_timeout: _bt,
            build_timeout_multiplier: _btm,
            rollback_rounds: _rr,
        } = plain();

        let varied: &[(&str, Context<'_>)] = &[
            (
                "features",
                Context {
                    features: &features,
                    ..plain()
                },
            ),
            (
                "--all-features",
                Context {
                    all_features: true,
                    ..plain()
                },
            ),
            (
                "--no-default-features",
                Context {
                    no_default_features: true,
                    ..plain()
                },
            ),
            (
                "the profile, which turns debug_assertions off",
                Context {
                    profile: Some("release"),
                    ..plain()
                },
            ),
            (
                "passthrough build arguments, which can carry --cfg, -C or --target",
                Context { extra: &extra, ..plain() },
            ),
            (
                "ambient rustflags, which can select different code entirely",
                Context {
                    rustflags: Some("--cfg loom"),
                    ..plain()
                },
            ),
            (
                "the compiler",
                Context {
                    toolchain: Some("1.91.0"),
                    ..plain()
                },
            ),
            (
                "test packages",
                Context {
                    test_packages: &test_packages,
                    ..plain()
                },
            ),
            (
                "include tests",
                Context {
                    include_tests: &include_tests,
                    ..plain()
                },
            ),
            (
                "exclude tests",
                Context {
                    exclude_tests: &exclude_tests,
                    ..plain()
                },
            ),
            (
                "test workspace",
                Context {
                    test_workspace: true,
                    ..plain()
                },
            ),
            (
                "whole test binaries",
                Context {
                    whole_test_binaries: true,
                    ..plain()
                },
            ),
            ("nextest", Context { nextest: true, ..plain() }),
            (
                "cargo test arguments",
                Context {
                    cargo_test_args: &cargo_test_args,
                    ..plain()
                },
            ),
            (
                "post-separator test arguments",
                Context {
                    test_args: &test_args,
                    ..plain()
                },
            ),
            (
                "baseline mode",
                Context {
                    baseline: false,
                    ..plain()
                },
            ),
            ("confirmation mode", Context { confirm: false, ..plain() }),
            ("stall detection", Context { stall: false, ..plain() }),
            (
                "test timeout multiplier",
                Context {
                    test_timeout_multiplier: Some(1.5),
                    ..plain()
                },
            ),
            (
                "minimum test timeout",
                Context {
                    minimum_test_timeout: Some(30.0),
                    ..plain()
                },
            ),
            (
                "memory mode",
                Context {
                    memory: Some(crate::exec::MemoryControl::Off),
                    ..plain()
                },
            ),
            (
                "memory multiplier",
                Context {
                    memory_multiplier: Some(3.0),
                    ..plain()
                },
            ),
            (
                "memory headroom",
                Context {
                    memory_headroom: Some(1024),
                    ..plain()
                },
            ),
            (
                "memory limit",
                Context {
                    memory_limit: Some(2048),
                    ..plain()
                },
            ),
            (
                "baseline memory limit",
                Context {
                    baseline_memory_limit: Some(4096),
                    ..plain()
                },
            ),
            (
                "memory relaunch",
                Context {
                    no_relaunch: true,
                    ..plain()
                },
            ),
            (
                "ignored-file copying",
                Context {
                    copy_ignored: true,
                    ..plain()
                },
            ),
            ("parallel scheduling", Context { jobs: Some(2), ..plain() }),
            (
                "build timeout",
                Context {
                    build_timeout: Some(60.0),
                    ..plain()
                },
            ),
            (
                "build timeout multiplier",
                Context {
                    build_timeout_multiplier: Some(2.0),
                    ..plain()
                },
            ),
            (
                "rollback rounds",
                Context {
                    rollback_rounds: 1,
                    ..plain()
                },
            ),
        ];

        let base = context(&plain()).expect("a named toolchain gives a context");
        let mut digests = HashSet::default();

        let _ = digests.insert(base.clone());

        for (axis, varied) in varied {
            let digest = context(varied).expect("a named toolchain gives a context");

            assert_ne!(base, digest, "{axis}");

            // Pairwise as well as against the base: two axes that hashed the same as each other
            // would each pass the check above and still leave one of them unable to invalidate the
            // other's cache.
            assert!(digests.insert(digest), "{axis} digests the same as another axis");
        }

        assert_eq!(base, context(&plain()).unwrap(), "the same context digests the same");
    }

    /// The tool's own version is an axis too, and it is the one no test can vary.
    ///
    /// A mutant id hashes the item path, the mutator, the normalized site text, the occurrence and
    /// the replacement index — every one of them a property of *this tool* — so an upgrade can
    /// change what an id denotes while the sources, the features and the toolchain are all
    /// unchanged. It cannot be varied from a test, because it is an `env!` read at compile time, so
    /// what is asserted instead is that it is a term of its own: a context that spells the version
    /// out among its passthrough arguments must not digest the same as one that does not.
    #[test]
    fn the_tool_version_is_a_term_of_the_digest_rather_than_loose_bytes() {
        let spelled = [env!("CARGO_PKG_VERSION").to_owned()];

        assert_ne!(
            context(&plain()).unwrap(),
            context(&Context {
                extra: &spelled,
                ..plain()
            })
            .unwrap()
        );
    }

    /// A compiler that cannot be asked its version suppresses the cache rather than being hashed as
    /// an empty string.
    ///
    /// Hashing the absence would not make the cache less useful; it would remove the compiler from
    /// the key, so two runs under different toolchains would match and the second would believe the
    /// first. `RUSTC` pointing at a wrapper that does not answer `--version` would make that
    /// permanent and silent.
    #[test]
    fn a_toolchain_that_cannot_be_named_yields_no_context_at_all() {
        assert_eq!(
            context(&Context {
                toolchain: None,
                ..plain()
            }),
            None
        );
    }

    /// Rearranging the parts must not collide, which is what the length prefixes buy.
    #[test]
    fn two_different_contexts_cannot_digest_the_same_by_running_their_parts_together() {
        let split = ["ab".to_owned(), "c".to_owned()];
        let joined = ["a".to_owned(), "bc".to_owned()];

        assert_ne!(
            context(&Context {
                features: &split,
                ..plain()
            }),
            context(&Context {
                features: &joined,
                ..plain()
            })
        );
    }

    /// The whole of F2 in one assertion: a compiler bump costs unviability and nothing else.
    ///
    /// The old envelope discarded the file wholesale, which made a record shared between a
    /// developer's machine and CI deliver nothing while appearing to work — silently, which is
    /// worse than an absent file.
    #[test]
    fn a_toolchain_change_discards_unviability_and_keeps_the_probes() {
        let (_dir, root) = workspace("record-tiers-", "fn add() {}");
        let index = killers(&root, "#[test]\nfn caught() {}\n");

        from_run(&root, &[mutant("abc", "src/lib.rs", Outcome::CompileError)], &envelope()).store(&root, &root);
        RunRecord::store_probes(
            &root,
            &core::iter::once((
                "abc".into(),
                Killer {
                    package: "subject".to_owned(),
                    target: "lib".to_owned(),
                    test: "caught".to_owned(),
                },
            ))
            .collect(),
        );

        let record = RunRecord::load(&root);
        let settled = record.settled(&root, Trust::Free, &index, &under_another_toolchain()).0;

        assert!(settled.is_empty(), "unviability survived a compiler it was never checked against");
        assert_eq!(
            RunRecord::load(&root).probes().get("abc").map(|killer| killer.test.as_str()),
            Some("caught"),
            "the probe was thrown away with the unviability"
        );
        assert_eq!(record.ordering(), vec!["abc"], "the demoted unviability was thrown away too");
    }

    /// A compiler change can alter test behaviour, so it invalidates verdicts as well as builds.
    #[test]
    fn a_toolchain_change_rechecks_the_verdicts() {
        let (_dir, root) = workspace("record-verdict-toolchain-", "fn add() {}");
        let index = killers(&root, "#[test]\nfn caught() {}\n");

        let record = from_run(&root, &[killed("abc", Some("caught"))], &envelope());
        let settled = record.settled(&root, Trust::Settled, &index, &under_another_toolchain()).0;

        assert!(settled.is_empty());
    }

    /// Every execution axis invalidates a verdict, `--profile` most obviously: a `debug_assert!`
    /// that caught a mutant in one profile does not exist in the other.
    #[test]
    fn every_required_term_invalidates_a_verdict() {
        let (_dir, root) = workspace("record-verdict-terms-", "fn add() {}");
        let index = killers(&root, "#[test]\nfn caught() {}\n");
        let record = from_run(&root, &[killed("abc", Some("caught"))], &envelope());

        let extra = ["--cfg=loom".to_owned()];
        let test_packages = ["pkg_b".to_owned()];
        let varied: &[(&str, Context<'_>)] = &[
            (
                "the profile",
                Context {
                    profile: Some("release"),
                    ..plain()
                },
            ),
            (
                "rustflags",
                Context {
                    rustflags: Some("--cfg loom"),
                    ..plain()
                },
            ),
            ("passthrough arguments", Context { extra: &extra, ..plain() }),
            (
                "test filtering",
                Context {
                    test_packages: &test_packages,
                    ..plain()
                },
            ),
        ];

        for (axis, context_of) in varied {
            let digest = context(context_of).expect("a named toolchain gives a context");

            assert!(
                record.settled(&root, Trust::Settled, &index, &digest).0.is_empty(),
                "a verdict crossed a change of {axis}"
            );
        }

        assert!(
            record
                .settled(&root, Trust::Settled, &index, &under_another_feature_set())
                .0
                .is_empty(),
            "a verdict crossed a feature change"
        );
    }

    /// Every term a run digests has to be a term a tier can require, or a tier requiring "all of
    /// them" would silently stop covering the one that was added.
    #[test]
    fn every_digested_term_is_one_a_tier_can_require() {
        // Resolved against a workspace, so that every term holds a digest: an unstated one reads as
        // the empty string, which would let two of them collide with each other unnoticed.
        let (_dir, root) = workspace("record-terms-", "fn add() {}");
        let digest = envelope().resolved_at(&root);
        let named: HashSet<&str> = Term::ALL.iter().map(|term| digest.term(*term)).collect();

        // Each term is digested from different bytes, so a term missing from `Term::ALL` shows up
        // as a value nobody can name. The digest is a struct, so the compiler enforces the other
        // direction — `term` cannot answer for a field that does not exist.
        assert_eq!(named.len(), Term::ALL.len(), "two terms digest the same, so one cannot invalidate");

        assert_eq!(
            Tier::Unviability.requires(),
            &[
                Term::Features,
                Term::Profile,
                Term::Rustflags,
                Term::Target,
                Term::Config,
                Term::Extra,
                Term::Toolchain,
                Term::Tool,
                Term::Policy,
                Term::Environment,
            ]
        );
        assert!(Tier::Ordering.requires().is_empty());
        assert!(
            Tier::Verdict.requires().contains(&Term::Toolchain),
            "verdicts are rechecked after a compiler change"
        );
        assert!(
            Tier::Verdict.requires().contains(&Term::Tests),
            "the verdict tier depends on the tests"
        );
        assert!(
            Tier::Verdict.requires().contains(&Term::Policy),
            "verdicts depend on their execution policy"
        );
        assert!(
            Tier::Verdict.requires().contains(&Term::Environment),
            "verdicts depend on what test processes inherit"
        );
    }

    /// An unviable mutant, which is what [`Tier::Unviability`] governs.
    fn unviable(id: &str) -> Mutant {
        mutant(id, "src/lib.rs", Outcome::CompileError)
    }

    /// What a record settles for a run that trusts it as far as it is free to.
    fn free(record: &RunRecord, root: &Utf8Path, context: &ContextDigest) -> HashMap<MutantId, Outcome> {
        record.settled(root, Trust::Free, &Killers::default(), context).0
    }

    /// `CARGO_BUILD_TARGET` is the spelling of the target that no passthrough argument carries, and
    /// the one a record could cross without noticing: every source digest matches, no rustflags are
    /// set on either side, and the mutants would come back for an architecture they were never
    /// compiled against.
    #[test]
    fn a_record_written_for_one_target_is_not_read_for_another() {
        let (_dir, root) = workspace("record-target-", "fn add() {}");
        let here = context_in(&plain(), None, &[]).expect("a named toolchain gives a context");
        let elsewhere = context_in(&plain(), Some("x86_64-unknown-linux-musl"), &[]).expect("a named toolchain gives a context");

        let record = from_run(&root, &[unviable("abc")], &here);

        assert_eq!(
            free(&record, &root, &here).get("abc"),
            Some(&Outcome::CompileError),
            "the record has to apply to the run that wrote it"
        );
        assert!(
            free(&record, &root, &elsewhere).is_empty(),
            "unviability crossed a change of target, so a mutant is out of the denominator for a build it never saw"
        );
    }

    /// The same for the target's other spelling, which reaches the digest through the arguments the
    /// run passes cargo rather than through the environment.
    #[test]
    fn a_record_written_for_one_passthrough_target_is_not_read_for_another() {
        let (_dir, root) = workspace("record-target-argument-", "fn add() {}");
        let extra = ["--target".to_owned(), "wasm32-unknown-unknown".to_owned()];

        let here = context_in(&plain(), None, &[]).expect("a named toolchain gives a context");
        let elsewhere = context_in(&Context { extra: &extra, ..plain() }, None, &[]).expect("a named toolchain gives a context");

        let record = from_run(&root, &[unviable("abc")], &here);

        assert!(
            free(&record, &root, &elsewhere).is_empty(),
            "unviability crossed a change of target"
        );
        assert_ne!(
            here.term(Term::Target),
            elsewhere.term(Term::Target),
            "the target term is what has to notice, so that the diagnostic can name it"
        );
    }

    /// The scenario `rustflags` names as expected — a `--cfg` added to the build — written in the
    /// file rather than in the variable. The record is loaded from disk in between, because that is
    /// the shape of the run this defends: the flag is edited between two invocations.
    #[test]
    fn a_record_written_before_a_configured_rustflag_is_not_read_after_it() {
        let (_dir, root) = workspace("record-configured-rustflags-", "fn add() {}");
        let envelope = envelope();

        from_run(&root, &[unviable("abc")], &envelope).store(&root, &root);

        assert_eq!(
            free(&RunRecord::load(&root), &root, &envelope).get("abc"),
            Some(&Outcome::CompileError),
            "the record has to apply while nothing has changed"
        );

        fs::create_dir_all(root.join(".cargo").as_std_path()).expect("the configuration directory should be creatable");
        fs::write(
            root.join(".cargo/config.toml").as_std_path(),
            "[build]\nrustflags = [\"--cfg\", \"loom\"]\n",
        )
        .expect("the configuration should be writable");

        assert!(
            free(&RunRecord::load(&root), &root, &envelope).is_empty(),
            "unviability crossed a configured rustflag, which selects different code entirely"
        );
    }

    /// Every setting the workspace holds has to move the key, or the list of them below is a claim
    /// nobody checks.
    ///
    /// One case per file-borne input in [`Build::INPUTS`], because the digest is taken over a
    /// rendering of those settings and a rendering that dropped one of them would still look like a
    /// digest.
    #[test]
    fn a_change_to_any_setting_the_workspace_holds_moves_the_config_term() {
        let edits: &[(&str, &str, &str)] = &[
            (
                "build.target",
                ".cargo/config.toml",
                "[build]\ntarget = \"wasm32-unknown-unknown\"\n",
            ),
            (
                "build.rustflags",
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--cfg\", \"loom\"]\n",
            ),
            (
                "target.*.rustflags",
                ".cargo/config.toml",
                "[target.'cfg(unix)']\nrustflags = [\"--cfg\", \"loom\"]\n",
            ),
            ("profile.*", "Cargo.toml", "[profile.dev]\ndebug-assertions = false\n"),
        ];

        for (input, file, body) in edits {
            let (_dir, root) = workspace("record-settings-", "fn add() {}");
            let path = root.join(file);
            let before = envelope().resolved_at(&root);

            fs::create_dir_all(path.parent().expect("every fixture path has a parent").as_std_path())
                .expect("the directory should be creatable");
            fs::write(path.as_std_path(), *body).expect("the setting should be writable");

            assert_ne!(
                before.term(Term::Config),
                envelope().resolved_at(&root).term(Term::Config),
                "a change to {input} left the key where it was"
            );
        }
    }

    /// The two modules that decide what compiles must not drift apart.
    ///
    /// `cfg::build` is where a build input is read and [`Build::INPUTS`] is where it is named;
    /// [`COVERAGE`] is what this module does about each one. An input the resolution reads and no
    /// term covers is unviability carried across a change of build, which is a mutant dropped from
    /// the denominator — so the two lists are checked against each other rather than trusted to
    /// stay in step.
    #[test]
    fn every_input_the_build_resolution_reads_is_covered_by_a_term() {
        let uncovered: Vec<&str> = Build::INPUTS
            .iter()
            .copied()
            .filter(|input| !COVERAGE.iter().any(|(name, _term)| name == input))
            .collect();

        assert!(
            uncovered.is_empty(),
            "these decide what compiles and no term covers them: {uncovered:?}"
        );

        let unrequired: Vec<&str> = COVERAGE
            .iter()
            .filter(|(_name, term)| !Term::ALL.contains(term))
            .map(|(name, _term)| *name)
            .collect();

        assert!(
            unrequired.is_empty(),
            "these are covered by a term no tier can require, so the cover is not one: {unrequired:?}"
        );

        // The other direction, which catches the cover that outlived the input: a name here that
        // `cfg::build` no longer reads is either a stale entry or, worse, a rename that left the
        // real input uncovered while this list still looked complete.
        let stale: Vec<&str> = COVERAGE
            .iter()
            .filter(|(name, _term)| !Build::INPUTS.contains(name))
            .map(|(name, _term)| *name)
            .collect();

        assert!(stale.is_empty(), "these are covered and nothing reads them: {stale:?}");
    }

    /// A record written before these terms existed states neither of them, which is the case the
    /// format number does not move for: it loses the tiers whose guard it cannot answer and keeps
    /// the two that ask nothing, where discarding it would have cost all three.
    #[test]
    fn a_record_that_states_none_of_the_newer_terms_keeps_only_the_tiers_that_ask_nothing() {
        let (_dir, root) = workspace("record-older-terms-", "fn add() {}");

        from_run(&root, &[unviable("abc")], &envelope()).store(&root, &root);

        let text = fs::read_to_string(root.join(FILE).as_std_path()).expect("the record should be readable");
        let mut written: serde_json::Value = serde_json::from_str(&text).expect("the record is JSON");
        let context = written
            .get_mut("context")
            .and_then(serde_json::Value::as_object_mut)
            .expect("the record states its context");

        let _target = context.remove("target");
        let _config = context.remove("config");

        fs::write(
            root.join(FILE).as_std_path(),
            serde_json::to_string(&written).expect("the record serializes"),
        )
        .expect("the record should be writable");

        let older = RunRecord::load(&root);

        assert!(
            free(&older, &root, &envelope()).is_empty(),
            "unviability was adopted from a record that cannot say what build it was written under"
        );
        assert_eq!(older.ordering(), ["abc"], "the tiers that require no term still apply");
    }

    /// A term neither side has an answer for is not agreement, and the tier that requires it has to
    /// refuse. A record written before the term existed looks exactly like this.
    #[test]
    fn a_term_nobody_states_admits_nothing() {
        let unresolved = envelope();

        assert!(!unresolved.states(Term::Config));
        assert!(
            !Tier::Unviability.admits(&unresolved, &unresolved),
            "an unstated term must not match itself"
        );
        assert!(
            unresolved.differences(&unresolved).is_empty(),
            "a term neither side states is not an axis that moved"
        );
        assert!(
            Tier::Ordering.admits(&unresolved, &unresolved),
            "the tiers that require nothing are unaffected by what nobody states"
        );
    }

    /// Cargo reads the three global rustflags variables in order and takes the first that says
    /// anything, so the key holds whichever one is actually in force. `CARGO_BUILD_RUSTFLAGS` is the
    /// variable spelling of `build.rustflags` and decides the build exactly as the other two do.
    #[test]
    fn the_flags_in_the_key_are_the_ones_cargo_will_read() {
        let of = |value: &str| Some(value.to_owned());

        assert_eq!(rustflags_in(None, None, of("--cfg loom"), &[]).as_deref(), Some("--cfg loom"));
        assert_eq!(rustflags_in(None, of("plain"), of("configured"), &[]).as_deref(), Some("plain"));
        assert_eq!(
            rustflags_in(of("encoded"), of("plain"), of("configured"), &[]).as_deref(),
            Some("encoded")
        );
        assert_eq!(rustflags_in(None, None, None, &[]), None);
    }

    /// A target-specific variable is its own precedence level, so it is added to the key rather
    /// than competing with the global three for it.
    ///
    /// Cargo applies `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` where the global variables are unset, so a
    /// key that only ever held the first global that spoke would be identical across a change to it
    /// — and unviability would then be carried across a flag that decides what compiles.
    #[test]
    fn a_target_specific_rustflags_variable_moves_the_key() {
        let of = |value: &str| Some(value.to_owned());
        let targeted = [(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_owned(),
            "--cfg live".to_owned(),
        )];

        let alone = rustflags_in(None, None, None, &targeted);

        assert_ne!(alone, None, "a target-specific flag on its own said nothing");
        assert_ne!(
            alone,
            rustflags_in(None, None, None, &[]),
            "the key did not move when the only rustflag in force was a target-specific one"
        );

        assert_ne!(
            rustflags_in(of("encoded"), None, None, &targeted),
            rustflags_in(of("encoded"), None, None, &[]),
            "a global variable hid a target-specific one that cargo would also apply"
        );

        assert_ne!(
            rustflags_in(None, None, None, &targeted),
            rustflags_in(None, None, None, &[(targeted[0].0.clone(), "--cfg other".to_owned())]),
            "a change to the value of a target-specific variable left the key where it was"
        );
    }

    /// A record from the previous format is ignored rather than half-believed.    ///
    /// Version 3 hashed every term into one digest, so nothing in it can answer the question a tier
    /// asks — "did you agree with me about the profile" — and inventing an answer would be adopting
    /// knowledge under a rule it was never written under.
    #[test]
    fn a_record_from_the_previous_format_is_ignored() {
        let (_dir, root) = workspace("record-old-format-", "fn add() {}");

        fs::write(
            root.join(FILE),
            r#"{"version":3,"context":"deadbeef","files":[{"path":"src/lib.rs","digest":"x","size":11,"mutants":[{"id":"abc","outcome":"unviable"}]}],"hints":{}}"#,
        )
        .expect("the cache should be writable");

        let record = RunRecord::load(&root);

        assert_eq!(record.len(), 0);
        assert!(record.ordering().is_empty());
        assert!(RunRecord::load(&root).probes().is_empty());
    }

    /// The ordering tier answers whatever the context is, because being wrong about an order costs
    /// the order and nothing else.
    #[test]
    fn the_ordering_tier_survives_every_term_and_is_stable() {
        let (_dir, root) = workspace("record-ordering-", "fn add() {}");
        let population = [
            mutant("zeta", "src/lib.rs", Outcome::CompileError),
            mutant("alpha", "src/lib.rs", Outcome::CompileError),
            mutant("killed", "src/lib.rs", Outcome::Killed),
        ];

        let record = from_run(&root, &population, &envelope());

        assert_eq!(
            record.ordering(),
            vec!["alpha", "zeta"],
            "only unviability orders, and it is sorted"
        );
    }
}
