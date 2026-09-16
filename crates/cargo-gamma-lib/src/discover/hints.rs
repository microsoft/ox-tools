// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The score-neutral part of the record, in a file a workspace can check in.
//!
//! Everything the tool learns about how to be fast lives under `target/`, and CI deletes that on
//! every run. So every CI run is a cold run: the sweep is unguided and the killer map is empty on
//! exactly the runs that cost the most. This is the same knowledge in a form that survives a clean
//! checkout — reviewed, committed, and consulted automatically.
//!
//! Only tiers that cannot move a score are allowed in here, and the file is defined by that
//! property rather than by being a relocated record:
//!
//! - **Probes** — which test caught each mutant. Never believed; the named test is actually run,
//!   and a probe that does not convict costs one filtered test process.
//! - **Build order** — which mutants failed to compile for whoever promoted the file. Never
//!   believed either: every one of them is compiled and the compiler decides, exactly as it would
//!   have without the hint. All it changes is which mutants are offered to the compiler first.
//!
//! A verdict is *not* allowed in, and that is the whole line this file walks. Adopting a carried
//! kill would settle part of the score out of another run's knowledge, which is why local incremental
//! caching requires matching digests; a sidecar that quietly did it would make every reported score unfalsifiable. Unviability
//! is admitted only after being demoted to an ordering hint, because a checked-in envelope will
//! differ from the run reading it almost always — see [`Tier::Ordering`].

#[cfg(test)]
use core::cell::RefCell;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use saphyr_parser::{Event as YamlEvent, Parser as YamlParser};
use serde::{Deserialize, Serialize};
use tick::{SimpleClock, SystemTimeExt as _};

use super::input;
use super::record::{GeneralizedHints, Killer, RunRecord, Tier};
use crate::elements::Publication;
use crate::error::error;
use crate::model::{Mutant, MutantId, Outcome};
use crate::{HashMap, HashSet, Result};

/// The artifact's file name.
const FILE: &str = "gamma-hints.yaml";

/// The JSON artifact accepted only as migration input.
const LEGACY_FILE: &str = "gamma-hints.json";

/// What the artifact format is; a file written by any other version is ignored rather than read.
const VERSION: u32 = 3;

/// JSON formats accepted during the YAML transition.
const LEGACY_FLAT_VERSION: u32 = 1;
const LEGACY_GROUPED_VERSION: u32 = 2;

/// Producer prefix written into artifacts whose schema cargo-gamma owns.
const TOOL_PREFIX: &str = "cargo-gamma ";

/// Where the artifact lives for a workspace rooted at `root`.
#[must_use]
pub fn path(root: &Utf8Path) -> Utf8PathBuf {
    root.join(FILE)
}

/// Where the read-only migration input lives.
fn legacy_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join(LEGACY_FILE)
}

/// Repository provenance for one generated artifact.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HintContext {
    /// The repository revision at which this generation was produced.
    repo_sha: String,

    /// Its UTC generation date in ISO 8601 calendar form.
    generated_on: String,
}

impl HintContext {
    /// Captures provenance at the start of an explicit promotion.
    fn capture(root: &Utf8Path) -> Result<Self> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root.as_std_path())
            .args(["rev-parse", "--verify", "HEAD"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|cause| error!("could not resolve `HEAD` for hints provenance").caused_by(cause))?;

        if !output.status.success() {
            return Err(error!(
                "could not resolve `HEAD` for hints provenance; `cargo gamma hints` requires a repository with a committed HEAD"
            ));
        }

        let repo_sha = core::str::from_utf8(&output.stdout)
            .ok()
            .map(str::trim)
            .filter(|sha| matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| error!("git returned an invalid commit ID while resolving `HEAD` for hints provenance"))?
            .to_owned();

        Ok(Self {
            repo_sha,
            generated_on: SimpleClock::new_system()
                .system_time()
                .display_iso_8601()
                .to_string()
                .get(..10)
                .ok_or_else(|| error!("the system clock could not be formatted as an ISO 8601 calendar date"))?
                .to_owned(),
        })
    }
}

/// The checked-in hints for a workspace.
///
/// Ordered by source file and then by mutant id, which is not a detail. A population in the tens of
/// thousands, regenerated on a schedule, otherwise produces a diff nobody can review — and an
/// unreviewable file in version control is a liability rather than an asset. Both keys are needed:
/// the file is what makes a diff readable against a change, and the id is what makes the order
/// total.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hints {
    /// The format this was written in.
    version: u32,

    /// What wrote it, which is the provenance a reviewer asks for first.
    tool: String,

    /// The commit and date at which this artifact generation was published.
    context: HintContext,

    /// One entry per mutant with something to say about it, ordered by file and then by id.
    ///
    /// This is the semantic form used in memory. The version-3 wire form groups these entries by
    /// file and interns killers within each group.
    mutants: Vec<Hint>,

    /// Optional score-neutral tiers shared verbatim with the run record.
    generalized: GeneralizedHints,
}

/// What the artifact remembers about one mutant.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hint {
    /// The workspace-relative file the mutant lives in, which is the artifact's primary sort key.
    ///
    /// Carried even though nothing reads it at run time — the run keys by mutant id — because a
    /// diff of this file is read by people, and a list of content hashes with no file beside them
    /// tells a reviewer nothing about what changed.
    file: Utf8PathBuf,

    /// The mutant's content-addressed id.
    id: MutantId,

    /// The test that caught it, when one did.
    killer: Option<Killer>,

    /// Whether it failed to compile for the run that was promoted.
    ///
    /// A hint about *order*, never a filter. See [`Tier::Ordering`]: the mutant is built and judged
    /// exactly as it would have been, and all this decides is that it is offered to the compiler
    /// early, where a mutant that does turn out to be unviable costs one round instead of hiding
    /// behind another one for several.
    unviable: bool,
}

#[derive(Deserialize)]
struct Header {
    version: u32,
    tool: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyFlatHints {
    #[serde(rename = "version")]
    _version: u32,
    #[serde(rename = "tool")]
    _tool: String,
    #[serde(rename = "context")]
    _context: super::record::ContextDigest,
    mutants: Vec<LegacyHint>,
    #[serde(default)]
    generalized: GeneralizedHints,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyHint {
    file: Utf8PathBuf,
    id: MutantId,
    #[serde(default)]
    killer: Option<Killer>,
    #[serde(default)]
    unviable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyGroupedHints {
    #[serde(rename = "version")]
    _version: u32,
    #[serde(rename = "tool")]
    _tool: String,
    #[serde(rename = "context")]
    _context: super::record::ContextDigest,
    files: Vec<FileHints>,
    #[serde(default)]
    generalized: GeneralizedHints,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GroupedHints {
    version: u32,
    tool: String,
    context: HintContext,
    files: Vec<FileHints>,
    #[serde(default = "GeneralizedHints::empty_supported")]
    generalized: GeneralizedHints,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileHints {
    path: Utf8PathBuf,
    killers: Vec<Killer>,
    mutants: Vec<GroupedHint>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GroupedHint {
    id: MutantId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    killer: Option<u32>,
    #[serde(default, skip_serializing_if = "is_not_set")]
    unviable: bool,
}

/// Whether a flag is at its default, so that the common entry serializes without it.
#[expect(clippy::trivially_copy_pass_by_ref, reason = "the signature is dictated by serde")]
const fn is_not_set(flag: &bool) -> bool {
    !*flag
}

/// What a promotion changed, so the command can say it rather than claim it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Promotion {
    /// How many mutants the artifact now names.
    pub mutants: usize,

    /// How many of them carry a killing test.
    pub probes: usize,

    /// How many of them are offered to the build as likely to fail.
    pub ordering: usize,

    /// Number of generalized item, binary and reach-cluster entries.
    pub generalized: usize,

    /// Whether the bytes on disk changed.
    pub changed: bool,
}

/// How an incremental promotion changed logical hint entries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HintChanges {
    /// Entries that were not present before.
    pub added: usize,

    /// Entries whose identity was retained with different knowledge.
    pub updated: usize,

    /// Entries deliberately removed by whole-artifact replacement.
    pub removed: usize,

    /// Entries carried forward byte-for-byte.
    pub preserved: usize,
}

impl HintChanges {
    pub(crate) const fn is_empty(self) -> bool {
        self.added == 0 && self.updated == 0 && self.removed == 0
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Replacement {
    Incremental,
    WholeArtifact,
}

impl From<bool> for Replacement {
    fn from(replace: bool) -> Self {
        if replace { Self::WholeArtifact } else { Self::Incremental }
    }
}

impl Hints {
    /// Reads the artifact for a workspace, or returns an empty one.
    ///
    /// Every failure is an empty set of hints: missing, unreadable, not YAML, a format from another
    /// version, or a file some other tool put at that name. That is the same contract the run
    /// record has, and for the same reason — nothing here can move a verdict, so being unable to
    /// read it may only ever cost the time it would have saved. A run that failed over it would
    /// have turned an optimization into a dependency, which is exactly what a file living in
    /// version control must never become.
    #[must_use]
    pub fn load(root: &Utf8Path) -> Self {
        let current = path(root);

        match fs::metadata(current.as_std_path()) {
            Ok(_) => return Self::read(&current).unwrap_or_default(),
            Err(cause) if cause.kind() == ErrorKind::NotFound => {}
            Err(_unreadable) => return Self::default(),
        }

        Self::read_legacy(&legacy_path(root)).unwrap_or_default()
    }

    /// Reads the artifact and captures the exact YAML generation used by an explicit promotion.
    ///
    /// Automatic consumers remain best-effort through [`Hints::load`]. Promotion is different:
    /// replacing knowledge the command could not understand would make ordinary incremental
    /// promotion destructive. `replace` is the explicit opt-in to discard such knowledge.
    pub(crate) fn load_for_promotion(root: &Utf8Path, replace: bool) -> Result<(Self, Option<String>, Option<String>)> {
        let current = path(root);

        if let Some(text) = existing_text(&current)? {
            return match Self::parse(&text) {
                Some(hints) if replace || hints.generalized.supported().is_some() => Ok((hints, Some(text), None)),
                Some(_hints) => Err(error!(
                    "`{current}` contains generalized hints this cargo-gamma cannot preserve, so incremental promotion would not preserve them; use `--replace` to discard them explicitly"
                )),
                None if replace => Ok((Self::default(), Some(text), None)),
                None => Err(error!(
                    "`{current}` is not a supported cargo-gamma hints artifact, so incremental promotion would not preserve it; use `--replace` to discard it explicitly"
                )),
            };
        }

        let legacy = legacy_path(root);
        let Some(text) = existing_text(&legacy)? else {
            return Ok((Self::default(), None, None));
        };

        match Self::parse_legacy(&text) {
            Some(hints) => Ok((hints, None, Some(text))),
            None if replace => Ok((Self::default(), None, Some(text))),
            None => Err(error!(
                "`{legacy}` is not a supported cargo-gamma hints artifact, so incremental promotion would not preserve it; use `--replace` to discard it explicitly"
            )),
        }
    }

    /// Whether this workspace has never had a checked-in hints artifact.
    ///
    /// An unreadable, corrupt or foreign-version file is present even though it cannot provide
    /// hints, so it must not trigger advice to create the file that is already there.
    #[must_use]
    pub(crate) fn is_missing(root: &Utf8Path) -> bool {
        [path(root), legacy_path(root)]
            .iter()
            .all(|path| matches!(fs::metadata(path.as_std_path()), Err(cause) if cause.kind() == ErrorKind::NotFound))
    }

    /// Reads and validates the artifact at `path`, or nothing when it cannot be trusted.
    fn read(path: &Utf8Path) -> Option<Self> {
        let text = input::text(File::open(path.as_std_path()).ok()?).ok()??;

        Self::parse(&text)
    }

    fn parse(text: &str) -> Option<Self> {
        if yaml_requires_rejection(text) {
            return None;
        }

        let header = yaml_serde::from_str::<Header>(text).ok()?;

        if header.version != VERSION || !Self::valid_tool(&header.tool) {
            return None;
        }

        Self::from_grouped(yaml_serde::from_str(text).ok()?)
    }

    /// Reads either historical JSON schema while no YAML artifact exists.
    fn read_legacy(path: &Utf8Path) -> Option<Self> {
        let text = input::text(File::open(path.as_std_path()).ok()?).ok()??;

        Self::parse_legacy(&text)
    }

    fn parse_legacy(text: &str) -> Option<Self> {
        let header = serde_json::from_str::<Header>(text).ok()?;

        if !Self::valid_tool(&header.tool) {
            return None;
        }

        match header.version {
            LEGACY_GROUPED_VERSION => Self::from_legacy_grouped(serde_json::from_str(text).ok()?),
            LEGACY_FLAT_VERSION => Some(Self::from_legacy_flat(serde_json::from_str(text).ok()?)),
            _unsupported => None,
        }
    }

    fn from_legacy_flat(legacy: LegacyFlatHints) -> Self {
        let mut hints = Self {
            version: VERSION,
            tool: format!("{TOOL_PREFIX}{}", env!("CARGO_PKG_VERSION")),
            context: HintContext::default(),
            mutants: legacy
                .mutants
                .into_iter()
                .map(|hint| Hint {
                    file: hint.file,
                    id: hint.id,
                    killer: hint.killer,
                    unviable: hint.unviable,
                })
                .collect(),
            generalized: legacy.generalized,
        };
        hints.normalize();
        hints
    }

    fn from_legacy_grouped(grouped: LegacyGroupedHints) -> Option<Self> {
        Self::from_files(
            grouped.files,
            grouped.generalized,
            HintContext::default(),
            format!("{TOOL_PREFIX}{}", env!("CARGO_PKG_VERSION")),
        )
    }

    fn from_grouped(grouped: GroupedHints) -> Option<Self> {
        Self::from_files(grouped.files, grouped.generalized, grouped.context, grouped.tool)
    }

    fn from_files(files: Vec<FileHints>, generalized: GeneralizedHints, context: HintContext, tool: String) -> Option<Self> {
        let mut mutants = Vec::new();

        for file in files {
            for hint in file.mutants {
                let killer = match hint.killer {
                    Some(index) => Some(file.killers.get(usize::try_from(index).ok()?)?.clone()),
                    None => None,
                };
                mutants.push(Hint {
                    file: file.path.clone(),
                    id: hint.id,
                    killer,
                    unviable: hint.unviable,
                });
            }
        }

        let mut hints = Self {
            version: VERSION,
            tool,
            context,
            mutants,
            generalized,
        };
        hints.normalize();
        Some(hints)
    }

    fn normalize(&mut self) {
        self.mutants.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.id.cmp(&right.id))
                .then_with(|| left.killer.as_ref().map(killer_key).cmp(&right.killer.as_ref().map(killer_key)))
                .then_with(|| left.unviable.cmp(&right.unviable))
        });
        self.mutants.dedup();
    }

    /// The tests to try first, keyed by mutant id.
    #[must_use]
    pub fn probes(&self) -> HashMap<MutantId, Killer> {
        self.mutants
            .iter()
            .filter_map(|hint| hint.killer.clone().map(|killer| (hint.id.clone(), killer)))
            .collect()
    }

    /// The mutants to offer the compiler first, in the artifact's own order.
    #[must_use]
    pub fn ordering(&self) -> Vec<&str> {
        self.mutants
            .iter()
            .filter(|hint| hint.unviable)
            .map(|hint| hint.id.as_str())
            .collect()
    }

    /// Generalized tiers, empty when their independent schema version is unsupported.
    #[must_use]
    pub fn generalized(&self) -> GeneralizedHints {
        self.generalized
            .supported()
            .cloned()
            .unwrap_or_else(GeneralizedHints::empty_supported)
    }

    /// Whether it holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mutants.is_empty() && self.generalized.is_empty()
    }

    /// What this artifact holds, for a caller that wants to report it without writing it.
    #[must_use]
    pub fn counts(&self) -> Promotion {
        self.promotion(false)
    }

    /// Merges one selected promotion into the checked-in artifact.
    ///
    /// A package, feature, file, diff or mutator selection can prove only what it found, so ordinary
    /// promotion upserts those entries and leaves every absence alone. Explicit replacement
    /// discards the old artifact intentionally.
    ///
    /// # Errors
    ///
    /// Returns an error when incremental promotion cannot preserve the existing or incoming
    /// generalized-hints version, or when changed knowledge needs provenance but `HEAD` cannot be
    /// resolved to a valid commit ID.
    pub(crate) fn merged(existing: &Self, promoted: Self, replacement: impl Into<Replacement>, root: &Utf8Path) -> Result<Self> {
        let replace = matches!(replacement.into(), Replacement::WholeArtifact);

        if !replace && existing.generalized.supported().is_none() && (existing.generalized.version != 0 || !existing.generalized.is_empty())
        {
            return Err(error!(
                "the existing artifact contains a generalized hints version this cargo-gamma cannot preserve incrementally; use `--replace` to discard it explicitly"
            ));
        }

        let migrating = !path(root).exists() && legacy_path(root).exists();
        let mut merged = if replace {
            promoted
        } else {
            let mut output = existing.clone();

            for incoming in promoted.mutants {
                output.mutants.retain(|hint| hint.file != incoming.file || hint.id != incoming.id);
                output.mutants.push(incoming);
            }

            output.generalized = merge_generalized(&output.generalized, &promoted.generalized)?;
            output
        };

        merged.normalize();
        let knowledge_changed = !merged.same_knowledge(existing);

        if knowledge_changed || migrating || replace {
            merged.context = HintContext::capture(root)?;
            merged.version = VERSION;
            merged.tool = format!("{TOOL_PREFIX}{}", env!("CARGO_PKG_VERSION"));
        } else {
            merged.context.clone_from(&existing.context);
        }

        Ok(merged)
    }

    /// Counts logical entry changes without treating provenance refresh as scheduling knowledge.
    #[must_use]
    pub fn changes_from(&self, previous: &Self) -> HintChanges {
        let mut changes = HintChanges::default();

        compare_entries(
            &previous.mutants,
            &self.mutants,
            |left, right| left.file == right.file && left.id == right.id,
            &mut changes,
        );
        compare_entries(
            &previous.generalized.items,
            &self.generalized.items,
            |left, right| left.file == right.file && left.item == right.item,
            &mut changes,
        );
        compare_entries(
            &previous.generalized.binaries,
            &self.generalized.binaries,
            |left, right| left.file == right.file,
            &mut changes,
        );

        let previous_reach = reach_entries(&previous.generalized);
        let current_reach = reach_entries(&self.generalized);
        compare_entries(&previous_reach, &current_reach, |(left, _), (right, _)| left == right, &mut changes);

        changes
    }

    fn same_knowledge(&self, other: &Self) -> bool {
        self.mutants == other.mutants && self.generalized() == other.generalized()
    }

    /// Builds the artifact from a scratch record and the population as it stands now.
    ///
    /// The population is what makes this a promotion rather than a copy. A record accumulates
    /// entries for mutants that have since been edited away, and committing those would grow the
    /// file without bound and fill its diff with ids nobody can locate; joining against the mutants
    /// that exist today drops them, and gives every surviving entry the file it lives in, which is
    /// what the ordering is for.
    ///
    /// Verdicts are not consulted. The only knowledge read here cannot move a score: probes are
    /// rerun before use, and [`Tier::Ordering`] is unviability demoted to an order to build in.
    #[must_use]
    pub fn promoted(record: &RunRecord, population: &[Mutant]) -> Self {
        let probes = record.probes();

        // The one place the admission rule is applied, so that widening it means editing a function
        // whose name says what it decides.
        let unviable: HashSet<&str> = record
            .iter()
            .filter(|(_id, outcome)| tier_of(*outcome) == Some(Tier::Ordering))
            .map(|(id, _outcome)| id)
            .collect();

        let mut mutants: Vec<Hint> = population
            .iter()
            .filter_map(|mutant| {
                let hint = Hint {
                    file: mutant.file.to_path_buf(),
                    id: mutant.id.clone(),
                    killer: probes.get(&mutant.id).cloned(),
                    unviable: unviable.contains(mutant.id.as_str()),
                };

                (hint.killer.is_some() || hint.unviable).then_some(hint)
            })
            .collect();

        // By file, then by id. A population can hold the same id twice only if the same mutant was
        // scanned twice, which a shard or an overlapping selection can do, so the duplicates are
        // dropped after sorting rather than assumed away.
        mutants.sort_by(|left, right| left.file.cmp(&right.file).then_with(|| left.id.cmp(&right.id)));
        mutants.dedup_by(|left, right| left.file == right.file && left.id == right.id);

        Self {
            version: VERSION,
            tool: format!("{TOOL_PREFIX}{}", env!("CARGO_PKG_VERSION")),
            context: HintContext::default(),
            mutants,
            generalized: Self::generalized_for(&record.generalized(), population),
        }
    }

    fn valid_tool(tool: &str) -> bool {
        tool.strip_prefix(TOOL_PREFIX).is_some_and(|version| !version.trim().is_empty())
    }

    fn generalized_for(source: &GeneralizedHints, population: &[Mutant]) -> GeneralizedHints {
        let files: HashSet<&Utf8Path> = population.iter().map(|mutant| mutant.file.as_ref()).collect();
        let items: HashSet<(&Utf8Path, &str)> = population
            .iter()
            .map(|mutant| (mutant.file.as_ref(), mutant.item_path.as_ref()))
            .collect();
        let sites: HashSet<super::record::SiteIdentity> = population.iter().map(super::record::SiteIdentity::from_mutant).collect();
        let mut output = GeneralizedHints::empty_supported();
        output.items = source
            .items
            .iter()
            .filter(|entry| items.contains(&(entry.file.as_path(), entry.item.as_str())))
            .cloned()
            .collect();
        output
            .items
            .sort_by(|left, right| left.file.cmp(&right.file).then_with(|| left.item.cmp(&right.item)));
        output.binaries = source
            .binaries
            .iter()
            .filter(|entry| files.contains(entry.file.as_path()))
            .cloned()
            .collect();
        output.binaries.sort_by(|left, right| left.file.cmp(&right.file));

        let mut reach = Vec::new();
        for cluster in source.reach.iter().filter(|cluster| sites.contains(&cluster.site)) {
            let Some(old) = usize::try_from(cluster.test_set).ok().and_then(|index| source.test_sets.get(index)) else {
                continue;
            };
            let mut tests = old.clone();
            tests.sort_by(|left, right| killer_key(left).cmp(&killer_key(right)));
            tests.dedup();
            reach.push((cluster.site.clone(), tests));
        }
        reach.sort_by(|(left, _), (right, _)| site_key(left).cmp(&site_key(right)));
        output.test_sets = reach.iter().map(|(_, tests)| tests.clone()).collect();
        output
            .test_sets
            .sort_by(|left, right| left.iter().map(killer_key).cmp(right.iter().map(killer_key)));
        output.test_sets.dedup();

        for (site, tests) in reach {
            let index = output
                .test_sets
                .iter()
                .position(|set| set == &tests)
                .expect("every retained reach set was inserted above");
            if let Ok(test_set) = u32::try_from(index) {
                output.reach.push(super::record::ReachCluster { site, test_set });
            }
        }

        output
    }

    /// Writes the artifact to `path`, reads it back, and conditionally puts the old one back if it did not survive.
    ///
    /// The write is atomic — staged beside the destination and renamed onto it — so an interrupted
    /// promotion leaves the previous file rather than half of a new one. That matters more here
    /// than for a scratch file: this one is in version control, and a truncated YAML file that a
    /// later run silently treats as "no hints" is a slow run nobody can explain.
    ///
    /// Reading it back is the same discipline `suppress` applies to the source it edits. Verifying
    /// what was written is the only thing that distinguishes "the tool wrote a file" from "the file
    /// says what the tool meant", and the cost is one read of a file that was just written.
    ///
    /// # Errors
    ///
    /// Returns the reason when the file cannot be written, cannot be read back, does not parse back
    /// to what was written, or was already there and could not be read. Unlike every automatic path
    /// through this module, a promotion is something somebody asked for, so a failure is reported
    /// rather than absorbed.
    pub fn write(&self, path: &Utf8Path) -> Result<Promotion> {
        let before = existing_text(path)?;
        let legacy = if before.is_none() {
            existing_text(&legacy_path(path.parent().unwrap_or_else(|| Utf8Path::new("."))))?
        } else {
            None
        };

        self.write_from(path, before.as_deref(), legacy.as_deref())
    }

    /// Publishes against the exact generation from which this artifact was derived.
    pub(crate) fn write_from(&self, path: &Utf8Path, before: Option<&str>, legacy_before: Option<&str>) -> Result<Promotion> {
        let text = self.rendered()?;
        let workspace = path.parent().unwrap_or_else(|| Utf8Path::new("."));

        if u64::try_from(text.len()).unwrap_or(u64::MAX) > input::MAX_BYTES {
            return Err(error!(
                "the promoted hints are larger than the {} bytes cargo-gamma will retain",
                input::MAX_BYTES
            ));
        }

        if before == Some(text.as_str()) {
            match remove_legacy(workspace, legacy_before)? {
                LegacyRemoval::Removed => {}
                LegacyRemoval::RemovedUndurable(cause) => return Err(cause),
            }
            return Ok(self.promotion(false));
        }

        match crate::elements::write_if_unchanged(workspace, path, before, &text)
            .map_err(|cause| error!("could not write `{path}`").caused_by(cause))?
        {
            Publication::Conflict => {
                return Err(error!(
                    "`{path}` changed while these hints were being promoted; the newer generation was left alone"
                ));
            }
            Publication::Published => {}
            // The new hints are visible but the directory entry was not made durable. Keep the
            // failure rather than treating a successful read-back as a durable promotion.
            Publication::PublishedUndurable(cause) => return Err(cause),
        }

        after_publication(path);

        match Self::verified(path, self) {
            Ok(()) => {
                match remove_legacy(workspace, legacy_before) {
                    Ok(LegacyRemoval::Removed) => {}
                    Ok(LegacyRemoval::RemovedUndurable(cause)) => return Err(cause),
                    Err(cause) => return Err(restored(workspace, path, before, &text, cause)),
                }
                Ok(self.promotion(true))
            }
            Err(cause) => Err(restored(workspace, path, before, &text, cause)),
        }
    }

    /// The artifact as it goes to disk.
    fn rendered(&self) -> Result<String> {
        let mut text = yaml_serde::to_string(&GroupedHints::from(self))
            .map_err(|cause| error!("the hints could not be serialized; please report this").caused_by(cause))?;

        if !text.ends_with('\n') {
            text.push('\n');
        }

        Ok(text)
    }

    /// Reads back what was written and checks that it is what was meant.
    fn verified(path: &Utf8Path, intended: &Self) -> Result<()> {
        let Some(written) = Self::read(path) else {
            return Err(error!("`{path}` could not be read back after being written"));
        };

        if &written == intended {
            return Ok(());
        }

        Err(error!("`{path}` does not hold what was written to it"))
    }

    /// What this artifact would report having promoted.
    fn promotion(&self, changed: bool) -> Promotion {
        Promotion {
            mutants: self.mutants.len(),
            probes: self.mutants.iter().filter(|hint| hint.killer.is_some()).count(),
            ordering: self.mutants.iter().filter(|hint| hint.unviable).count(),
            generalized: self.generalized.items.len() + self.generalized.binaries.len() + self.generalized.reach.len(),
            changed,
        }
    }
}

/// Whether YAML is malformed or contains anchor or alias syntax.
///
/// The generated format never emits references. Refusing them keeps parsing cost proportional to
/// the bounded input bytes rather than to an alias-expanded graph.
fn yaml_requires_rejection(text: &str) -> bool {
    for event in YamlParser::new_from_str(text) {
        let Ok((event, _)) = event else {
            return true;
        };
        match event {
            YamlEvent::Alias(_) => return true,
            YamlEvent::Scalar(_, _, anchor, _) | YamlEvent::SequenceStart(anchor, _) | YamlEvent::MappingStart(anchor, _)
                if anchor != 0 =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn existing_text(path: &Utf8Path) -> Result<Option<String>> {
    match File::open(path.as_std_path()) {
        Ok(file) => input::text(file)
            .map_err(|cause| error!("`{path}` is already there and could not be read, so it must not be replaced").caused_by(cause))?
            .map_or_else(
                || {
                    Err(error!(
                        "`{path}` is larger than the {} bytes cargo-gamma will retain, so it cannot be safely replaced",
                        input::MAX_BYTES
                    ))
                },
                |text| Ok(Some(text)),
            ),
        Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(error!("`{path}` is already there and could not be read, so it must not be replaced").caused_by(cause)),
    }
}

enum LegacyRemoval {
    /// The legacy artifact was absent or its removal was durably committed.
    Removed,
    /// The legacy artifact was removed, but the parent-directory sync failed.
    RemovedUndurable(crate::error::Error),
}

#[cfg(test)]
thread_local! {
    static FAIL_LEGACY_REMOVAL_DURABILITY: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_next_legacy_removal_durability() {
    FAIL_LEGACY_REMOVAL_DURABILITY.set(true);
}

#[cfg(test)]
fn injected_legacy_removal_failure() -> bool {
    FAIL_LEGACY_REMOVAL_DURABILITY.replace(false)
}

#[cfg(not(test))]
const fn injected_legacy_removal_failure() -> bool {
    false
}

/// Removes the read-only JSON migration input after YAML is known to be complete.
fn remove_legacy(workspace: &Utf8Path, before: Option<&str>) -> Result<LegacyRemoval> {
    let Some(before) = before else {
        return Ok(LegacyRemoval::Removed);
    };
    let legacy = legacy_path(workspace);

    match crate::elements::remove_if_unchanged(workspace, &legacy, before)? {
        Publication::Published if injected_legacy_removal_failure() => Ok(LegacyRemoval::RemovedUndurable(error!(
            "could not remove `{legacy}`: injected directory sync failure"
        ))),
        Publication::Published => Ok(LegacyRemoval::Removed),
        Publication::Conflict => Err(error!(
            "`{legacy}` changed while its YAML replacement was being published; the YAML artifact is authoritative and the changed JSON file was left in place"
        )),
        Publication::PublishedUndurable(cause) => Ok(LegacyRemoval::RemovedUndurable(cause)),
    }
}

impl From<&Hints> for GroupedHints {
    fn from(hints: &Hints) -> Self {
        let mut files = Vec::new();
        let mut start = 0;

        while start < hints.mutants.len() {
            let path = hints.mutants[start].file.clone();
            let end = hints.mutants[start..]
                .iter()
                .position(|hint| hint.file != path)
                .map_or(hints.mutants.len(), |offset| start + offset);
            let group = &hints.mutants[start..end];
            let mut killers: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect();
            killers.sort_by(|left, right| killer_key(left).cmp(&killer_key(right)));
            killers.dedup();
            let mutants = group
                .iter()
                .map(|hint| GroupedHint {
                    id: hint.id.clone(),
                    killer: hint.killer.as_ref().map(|killer| {
                        let index = killers
                            .iter()
                            .position(|known| known == killer)
                            .expect("the killer pool was built from every killer in this file group");
                        u32::try_from(index).expect("a killer index cannot exceed the number of mutants retained in the bounded artifact")
                    }),
                    unviable: hint.unviable,
                })
                .collect();
            files.push(FileHints { path, killers, mutants });
            start = end;
        }

        Self {
            version: VERSION,
            tool: hints.tool.clone(),
            context: hints.context.clone(),
            files,
            generalized: hints.generalized.clone(),
        }
    }
}

fn merge_generalized(existing: &GeneralizedHints, incoming: &GeneralizedHints) -> Result<GeneralizedHints> {
    let mut output = match existing.supported() {
        Some(existing) => existing.clone(),
        None if existing.version == 0 && existing.is_empty() => GeneralizedHints::empty_supported(),
        None => return Err(error!("the existing generalized hints version cannot be merged")),
    };
    let incoming = match incoming.supported() {
        Some(incoming) => incoming.clone(),
        None if incoming.version == 0 && incoming.is_empty() => return Ok(output),
        None => return Err(error!("the promoted generalized hints version cannot be merged")),
    };
    let incoming_reach = reach_entries(&incoming);

    for hint in incoming.items {
        output.items.retain(|known| known.file != hint.file || known.item != hint.item);
        output.items.push(hint);
    }

    for hint in incoming.binaries {
        output.binaries.retain(|known| known.file != hint.file);
        output.binaries.push(hint);
    }

    let mut reach = reach_entries(&output);

    for (site, tests) in incoming_reach {
        reach.retain(|(known, _tests)| known != &site);
        reach.push((site, tests));
    }

    output
        .items
        .sort_by(|left, right| left.file.cmp(&right.file).then_with(|| left.item.cmp(&right.item)));
    output.binaries.sort_by(|left, right| left.file.cmp(&right.file));
    rebuild_reach(&mut output, reach);
    Ok(output)
}

fn reach_entries(generalized: &GeneralizedHints) -> Vec<(super::record::SiteIdentity, Vec<Killer>)> {
    generalized
        .supported()
        .into_iter()
        .flat_map(|hints| &hints.reach)
        .filter_map(|cluster| {
            let index = usize::try_from(cluster.test_set).ok()?;
            Some((cluster.site.clone(), generalized.test_sets.get(index)?.clone()))
        })
        .collect()
}

fn rebuild_reach(generalized: &mut GeneralizedHints, mut reach: Vec<(super::record::SiteIdentity, Vec<Killer>)>) {
    for (_site, tests) in &mut reach {
        tests.sort_by(|left, right| killer_key(left).cmp(&killer_key(right)));
        tests.dedup();
    }
    reach.sort_by(|(left, _), (right, _)| site_key(left).cmp(&site_key(right)));
    reach.dedup_by(|(left, _), (right, _)| left == right);

    generalized.test_sets = reach.iter().map(|(_site, tests)| tests.clone()).collect();
    generalized
        .test_sets
        .sort_by(|left, right| left.iter().map(killer_key).cmp(right.iter().map(killer_key)));
    generalized.test_sets.dedup();
    generalized.reach.clear();

    for (site, tests) in reach {
        let index = generalized
            .test_sets
            .iter()
            .position(|known| known == &tests)
            .expect("every retained reach set was inserted from the same normalized entries");
        if let Ok(test_set) = u32::try_from(index) {
            generalized.reach.push(super::record::ReachCluster { site, test_set });
        }
    }
}

fn compare_entries<T: PartialEq>(previous: &[T], current: &[T], same_key: impl Fn(&T, &T) -> bool, changes: &mut HintChanges) {
    for entry in previous {
        match current.iter().find(|candidate| same_key(entry, candidate)) {
            Some(candidate) if candidate == entry => changes.preserved += 1,
            Some(_updated) => changes.updated += 1,
            None => changes.removed += 1,
        }
    }

    changes.added += current
        .iter()
        .filter(|entry| !previous.iter().any(|candidate| same_key(candidate, entry)))
        .count();
}

fn killer_key(killer: &Killer) -> (&str, &str, &str) {
    (killer.package.as_str(), killer.target.as_str(), killer.test.as_str())
}

fn site_key(site: &super::record::SiteIdentity) -> (&Utf8Path, &str, &str, &str, u32) {
    (
        site.file.as_path(),
        site.item.as_str(),
        site.mutator.as_str(),
        site.normalized_text.as_str(),
        site.occurrence,
    )
}

/// Puts back whatever was at `path` before a promotion that could not be verified.
///
/// The restoration is generation-aware. A successful later promotion must survive a first
/// promotion's failed verification, so this restores only if the path still holds the exact bytes
/// this invocation published. The conditional helpers serialize cooperating processes with the
/// same locked generation protocol as the final comparison; a stale rollback therefore reports
/// its conflict instead of replacing or removing somebody else's artifact.
fn restored(
    workspace: &Utf8Path,
    path: &Utf8Path,
    before: Option<&str>,
    published: &str,
    cause: crate::error::Error,
) -> crate::error::Error {
    let restored = before.map_or_else(
        || crate::elements::remove_if_unchanged(workspace, path, published),
        |text| crate::elements::write_if_unchanged(workspace, path, Some(published), text),
    );

    match restored {
        Ok(Publication::Published) => cause,
        Ok(Publication::Conflict) => {
            error!("`{path}` changed after this promotion was published, so its later generation was left alone").caused_by(cause)
        }
        Ok(Publication::PublishedUndurable(failure)) => {
            error!("`{path}` was put back after a promotion that could not be verified, but its directory could not be synced ({failure})")
                .caused_by(cause)
        }
        Err(failure) => error!("`{path}` could not be put back after a promotion that could not be verified ({failure})").caused_by(cause),
    }
}

#[cfg(test)]
type PublicationHook = Box<dyn FnOnce(&Utf8Path)>;

#[cfg(test)]
thread_local! {
    static AFTER_PUBLICATION: RefCell<Option<PublicationHook>> = const { RefCell::new(None) };
}

/// Runs `hook` after this thread's next hints publication and before its read-back verification.
#[cfg(test)]
fn after_next_publication(hook: impl FnOnce(&Utf8Path) + 'static) {
    AFTER_PUBLICATION.with(|next| *next.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn after_publication(path: &Utf8Path) {
    let hook = AFTER_PUBLICATION.with(|next| next.borrow_mut().take());

    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
const fn after_publication(_path: &Utf8Path) {}

/// Whether an outcome is one the artifact is allowed to carry, and as which tier.
///
/// Stated here rather than at the call site so that widening it is a deliberate edit to a function
/// whose name says what the rule is. The rule is the whole safety argument of this file: a verdict
/// carried into version control and adopted automatically would settle part of a score out of
/// somebody else's run.
#[must_use]
const fn tier_of(outcome: Outcome) -> Option<Tier> {
    match outcome {
        Outcome::CompileError => Some(Tier::Ordering),
        _other => None,
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::super::record;
    use super::*;
    use crate::fixtures;
    use crate::testing::workdir;

    fn mutant(id: &str, file: &str) -> Mutant {
        Mutant {
            id: id.to_owned().into(),
            file: (Utf8PathBuf::from(file)).into(),
            ..fixtures::mutant()
        }
    }

    fn killer(test: &str) -> Killer {
        Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: test.to_owned(),
        }
    }

    /// A workspace holding one source file, and a record base pointing at the same directory.
    fn workspace(prefix: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = workdir(prefix);
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("the work directory should be UTF-8");

        fs::create_dir_all(root.join("src")).expect("the source directory should be creatable");
        fs::write(root.join("src/lib.rs"), "fn add() {}").expect("the source should be writable");

        (dir, root)
    }

    /// A build context, which the artifact carries as provenance and never reads as a gate.
    fn context_of() -> record::ContextDigest {
        record::context(&record::Context {
            toolchain: Some("1.90.0"),
            ..record::Context::default()
        })
        .expect("a named toolchain gives a context")
    }

    fn hint_context() -> HintContext {
        HintContext {
            repo_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            generated_on: "2026-09-16".to_owned(),
        }
    }

    /// A record holding one unviable mutant and one probe.
    fn recorded(root: &Utf8Path) -> RunRecord {
        let mut unviable = mutant("unviable", "src/lib.rs");

        unviable.outcome = Outcome::CompileError;

        RunRecord::from_run(root, &[unviable], &context_of(), &[root.join("src")]).store(root, root);
        RunRecord::store_probes(root, &core::iter::once(("killed".into(), killer("tests::caught"))).collect());

        RunRecord::load(root)
    }

    #[test]
    fn a_missing_artifact_is_no_hints_at_all() {
        let (_dir, root) = workspace("hints-absent-");

        assert!(Hints::load(&root).is_empty());
        assert!(Hints::is_missing(&root));
    }

    /// A file that cannot be parsed must cost the run nothing but the speed-up it would have given.
    #[test]
    fn a_corrupt_artifact_is_no_hints_at_all() {
        let (_dir, root) = workspace("hints-corrupt-");

        fs::write(path(&root).as_std_path(), "{ not json").expect("the artifact should be writable");

        assert!(Hints::load(&root).is_empty());
        assert!(!Hints::is_missing(&root));

        fs::write(
            path(&root).as_std_path(),
            serde_json::to_vec(&serde_json::json!({
                "version": VERSION,
                "tool": "cargo-gamma test",
                "context": hint_context(),
                "files": [{
                    "path": "src/lib.rs",
                    "killers": [],
                    "mutants": [{ "id": "abc", "killer": 0 }],
                }],
            }))
            .expect("malformed grouped YAML"),
        )
        .expect("the malformed artifact should be writable");
        assert!(Hints::load(&root).is_empty(), "an invalid killer-table reference was accepted");
    }

    #[test]
    fn yaml_references_are_rejected_without_confusing_quoted_punctuation() {
        assert!(yaml_requires_rejection("context: &shared\n  repo_sha: abc\ncopy: *shared\n"));
        assert!(yaml_requires_rejection("copy: *shared\n"));
        assert!(!yaml_requires_rejection("test: 'uses * and & literally'\n"));
        assert!(!yaml_requires_rejection("test: 'uses '' & literally'\n"));
        assert!(!yaml_requires_rejection("test: \"uses * and & literally\"\n"));
        assert!(!yaml_requires_rejection("test: 'uses\n  * and & literally'\n"));
        assert!(!yaml_requires_rejection("test: \"uses\n  * and & literally\"\n"));
        assert!(!yaml_requires_rejection("test: value&value # *comment\n"));
    }

    #[test]
    fn an_alias_bearing_artifact_is_ignored_before_deserialization() {
        let (_dir, root) = workspace("hints-alias-");
        let text = "version: 3\n\
                    tool: cargo-gamma test\n\
                    context: &context\n\
                      repo_sha: 0123456789abcdef0123456789abcdef01234567\n\
                      generated_on: '2026-09-16'\n\
                    files: []\n\
                    generalized: {}\n";

        fs::write(path(&root), text).expect("alias artifact");

        assert!(Hints::load(&root).is_empty());
    }

    #[test]
    fn the_checked_in_workspace_artifact_uses_the_current_readable_schema() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifact = path(&root);

        if !artifact.exists() {
            return;
        }

        let text = fs::read_to_string(&artifact).expect("checked-in hints are UTF-8");

        let grouped = yaml_serde::from_str::<GroupedHints>(&text).expect("checked-in hints use the current YAML schema");
        let canonical = yaml_serde::to_string(&grouped).expect("checked-in hints can be serialized");
        assert!(!yaml_requires_rejection(&canonical), "serialized hints contain YAML references");
        assert!(!yaml_requires_rejection(&text), "checked-in hints contain YAML references");
        assert_eq!(grouped.version, VERSION);
        assert!(Hints::valid_tool(&grouped.tool));
        assert!(!Hints::from_grouped(grouped).expect("valid references").is_empty());
    }

    /// Somebody else's YAML at this name is not this tool's file, and must not be read as one.
    #[test]
    fn a_foreign_artifact_is_no_hints_at_all() {
        let (_dir, root) = workspace("hints-foreign-");
        let foreign = Hints {
            version: VERSION,
            tool: "other".to_owned(),
            context: hint_context(),
            mutants: Vec::new(),
            generalized: GeneralizedHints::empty_supported(),
        };

        fs::write(
            path(&root).as_std_path(),
            serde_json::to_vec(&GroupedHints::from(&foreign)).expect("serializable"),
        )
        .expect("writable");

        assert!(Hints::load(&root).is_empty());
        assert!(!Hints::is_missing(&root));
    }

    #[test]
    fn promotion_carries_the_two_score_neutral_tiers_and_nothing_else() {
        let (_dir, root) = workspace("hints-promote-");
        let mut unviable = mutant("unviable", "src/lib.rs");

        unviable.outcome = Outcome::CompileError;

        let record = {
            let mut killed = mutant("killed", "src/lib.rs");

            killed.outcome = Outcome::Killed;
            killed.killed_by = Some("tests::caught".to_owned());

            RunRecord::from_run(&root, &[unviable, killed], &context_of(), &[root.join("src")]).store(&root, &root);
            RunRecord::store_probes(&root, &core::iter::once(("killed".into(), killer("tests::caught"))).collect());

            RunRecord::load(&root)
        };

        let hints = Hints::promoted(&record, &[mutant("killed", "src/lib.rs"), mutant("unviable", "src/lib.rs")]);

        assert_eq!(hints.probes().get("killed"), Some(&killer("tests::caught")));
        assert_eq!(hints.ordering(), vec!["unviable"]);

        // The kill itself must not be in the file in any form a run could adopt.
        let text = hints.rendered().expect("the hints should serialize");

        assert!(!text.contains("killed\":"), "{text}");
        assert!(!text.contains("outcome"), "a verdict reached the artifact: {text}");
    }

    #[test]
    fn promotion_carries_generalized_tiers_for_a_clean_checkout() {
        let (_dir, root) = workspace("hints-generalized-promote-");
        let record = recorded(&root);
        let generalized = GeneralizedHints {
            version: super::super::record::GENERALIZED_HINTS_VERSION,
            items: vec![super::super::record::ItemHints {
                file: "src/lib.rs".into(),
                item: "subject::changed".to_owned(),
                candidates: vec![super::super::record::RankedHint {
                    candidate: killer("tests::likely"),
                    hits: 3,
                    misses: 1,
                    measured_ms: 12,
                    samples: 4,
                    order: 0,
                }],
            }],
            ..GeneralizedHints::empty_supported()
        };
        RunRecord::store_knowledge(&root, record.probes(), Some(&generalized));

        let mut changed = mutant("new-id", "src/lib.rs");
        changed.item_path = "subject::changed".into();
        let promoted = Hints::promoted(&RunRecord::load(&root), &[changed]);
        promoted.write(&path(&root)).expect("generalized hints should be promotable");
        let clean = Hints::load(&root).generalized();

        assert_eq!(clean.items, generalized.items);
        assert_eq!(clean.items[0].candidates[0].candidate.test, "tests::likely");
    }

    #[test]
    fn an_unsupported_generalized_tier_is_ignored_without_losing_exact_hints() {
        let (_dir, root) = workspace("hints-generalized-version-");
        let unsupported = Hints {
            version: VERSION,
            tool: "cargo-gamma test".to_owned(),
            context: hint_context(),
            mutants: vec![Hint {
                file: "src/lib.rs".into(),
                id: "abc".into(),
                killer: Some(killer("tests::exact")),
                unviable: false,
            }],
            generalized: GeneralizedHints {
                version: 999,
                items: vec![super::super::record::ItemHints {
                    file: "src/lib.rs".into(),
                    item: "subject::f".to_owned(),
                    candidates: Vec::new(),
                }],
                ..GeneralizedHints::default()
            },
        };
        fs::write(path(&root), unsupported.rendered().expect("the artifact should serialize")).expect("the artifact should be writable");

        let loaded = Hints::load(&root);

        assert_eq!(loaded.probes().get("abc"), Some(&killer("tests::exact")));
        assert!(loaded.generalized().is_empty());
        let error = Hints::load_for_promotion(&root, false).expect_err("incremental promotion cannot round-trip a future tier");
        assert!(error.to_string().contains("generalized hints"), "{error}");
    }

    #[test]
    fn current_yaml_without_generalized_tiers_is_valid_for_incremental_promotion() {
        let (_dir, root) = workspace("hints-no-generalized-");
        let text = format!(
            concat!(
                "version: {}\n",
                "tool: cargo-gamma test\n",
                "context:\n",
                "  repo_sha: 0123456789abcdef0123456789abcdef01234567\n",
                "  generated_on: '2026-09-16'\n",
                "files: []\n",
            ),
            VERSION
        );
        fs::write(path(&root), &text).expect("artifact without generalized tiers");

        let (loaded, generation, legacy_generation) =
            Hints::load_for_promotion(&root, false).expect("omitted optional tiers are supported");

        assert!(loaded.is_empty());
        assert!(loaded.generalized.supported().is_some());
        assert_eq!(generation.as_deref(), Some(text.as_str()));
        assert!(legacy_generation.is_none());
    }

    #[test]
    fn current_yaml_does_not_depend_on_reading_the_legacy_path() {
        let (_dir, root) = workspace("hints-current-with-blocked-legacy-");
        let current = artifact(&[]);
        current.write(&path(&root)).expect("current YAML artifact");
        fs::create_dir(legacy_path(&root)).expect("legacy path blocker");

        let (loaded, generation, legacy_generation) = Hints::load_for_promotion(&root, false).expect("current YAML is authoritative");

        assert_eq!(loaded, current);
        assert!(generation.is_some());
        assert!(legacy_generation.is_none());
    }

    #[test]
    fn incremental_promotion_rejects_out_of_range_generalized_reach_references() {
        let (_dir, root) = workspace("hints-invalid-reach-");
        let mut malformed = artifact(&[("src/lib.rs", "abc", "tests::exact")]);
        malformed.generalized = GeneralizedHints {
            reach: vec![super::super::record::ReachCluster {
                site: super::super::record::SiteIdentity {
                    file: "src/lib.rs".into(),
                    item: "subject::f".to_owned(),
                    mutator: "expr.delete".to_owned(),
                    normalized_text: "call()".to_owned(),
                    occurrence: 0,
                },
                test_set: 1,
            }],
            ..GeneralizedHints::empty_supported()
        };
        fs::write(path(&root), malformed.rendered().expect("malformed references still serialize")).expect("malformed artifact");

        let loaded = Hints::load(&root);
        assert_eq!(loaded.probes().get("abc"), Some(&killer("tests::exact")));
        assert!(loaded.generalized().is_empty());

        let error = Hints::load_for_promotion(&root, false).expect_err("strict promotion must not discard an invalid reference");
        assert!(error.to_string().contains("generalized hints"), "{error}");
    }

    #[test]
    fn incremental_promotion_rejects_unknown_generalized_fields() {
        let (_dir, root) = workspace("hints-unknown-generalized-");
        let current = path(&root);
        let text = artifact(&[])
            .rendered()
            .expect("artifact")
            .replacen("generalized:\n", "generalized:\n  futureTier: preserved\n", 1);
        fs::write(&current, &text).expect("artifact with future generalized field");

        let error = Hints::load_for_promotion(&root, false).expect_err("strict promotion must preserve unknown generalized data");
        assert!(error.to_string().contains("incremental promotion would not preserve"), "{error}");
        assert_eq!(fs::read_to_string(current).expect("original generation"), text);
    }

    #[test]
    fn incremental_merge_refuses_an_unsupported_generalized_generation() {
        let (_dir, root) = workspace("hints-generalized-preserve-");
        commit_fixture(&root);
        let mut existing = artifact(&[("src/old.rs", "old", "tests::old")]);
        existing.generalized = GeneralizedHints {
            version: 999,
            items: vec![super::super::record::ItemHints {
                file: "src/future.rs".into(),
                item: "future::item".to_owned(),
                candidates: Vec::new(),
            }],
            ..GeneralizedHints::default()
        };
        let promoted = artifact(&[("src/new.rs", "new", "tests::new")]);

        let error = Hints::merged(&existing, promoted, false, &root).expect_err("unknown generalized data cannot be round-tripped");

        assert!(error.to_string().contains("cannot preserve incrementally"), "{error}");
    }

    /// A promoted hint whose mutant no longer exists would grow the file forever and fill its diff
    /// with ids nobody can locate.
    #[test]
    fn promotion_drops_hints_for_mutants_the_population_no_longer_holds() {
        let (_dir, root) = workspace("hints-gone-");
        let record = recorded(&root);

        let hints = Hints::promoted(&record, &[mutant("survivor", "src/lib.rs")]);

        assert!(hints.is_empty(), "a hint for a mutant nobody scanned was promoted");
    }

    /// The file is reviewed, so its order has to be one a reviewer can follow, and the same on
    /// every machine that regenerates it.
    #[test]
    fn promotion_orders_by_file_and_then_by_id() {
        let (_dir, root) = workspace("hints-order-");

        fs::write(root.join("src/other.rs"), "fn other() {}").expect("the source should be writable");

        let population = [
            mutant("zeta", "src/other.rs"),
            mutant("alpha", "src/other.rs"),
            mutant("mid", "src/lib.rs"),
        ];

        let mut unviable: Vec<Mutant> = population.to_vec();

        for entry in &mut unviable {
            entry.outcome = Outcome::CompileError;
        }

        RunRecord::from_run(&root, &unviable, &context_of(), &[root.join("src")]).store(&root, &root);

        let record = RunRecord::load(&root);
        let hints = Hints::promoted(&record, &population);
        let order: Vec<&str> = hints.mutants.iter().map(|hint| hint.id.as_str()).collect();

        assert_eq!(order, vec!["mid", "alpha", "zeta"]);

        // Regenerating from the same inputs has to produce the same bytes, or a scheduled
        // regeneration commits a diff on every run.
        let again = Hints::promoted(&record, &population);

        assert_eq!(hints.rendered().unwrap(), again.rendered().unwrap());
    }

    #[test]
    fn grouped_schema_emits_each_file_and_shared_killer_once_per_group() {
        let shared = killer("tests::shared");
        let hints = Hints {
            version: VERSION,
            tool: "cargo-gamma test".to_owned(),
            context: hint_context(),
            mutants: vec![
                Hint {
                    file: "src/lib.rs".into(),
                    id: "alpha".into(),
                    killer: Some(shared.clone()),
                    unviable: false,
                },
                Hint {
                    file: "src/lib.rs".into(),
                    id: "beta".into(),
                    killer: Some(shared),
                    unviable: true,
                },
            ],
            generalized: GeneralizedHints::empty_supported(),
        };
        let text = hints.rendered().expect("the grouped artifact serializes");
        let yaml: yaml_serde::Value = yaml_serde::from_str(&text).expect("the grouped artifact is YAML");

        assert_eq!(yaml["version"].as_u64(), Some(u64::from(VERSION)));
        assert_eq!(yaml["files"].as_sequence().expect("file groups").len(), 1);
        assert_eq!(text.matches("src/lib.rs").count(), 1, "{text}");
        assert_eq!(text.matches("tests::shared").count(), 1, "{text}");
        assert_eq!(yaml["files"][0]["killers"].as_sequence().expect("killer pool").len(), 1);
        assert_eq!(yaml["files"][0]["mutants"][0]["killer"].as_u64(), Some(0));
        assert_eq!(yaml["files"][0]["mutants"][1]["killer"].as_u64(), Some(0));
    }

    #[test]
    fn legacy_schema_loads_semantically_and_rewrites_as_grouped() {
        let (_dir, root) = workspace("hints-legacy-");
        commit_fixture(&root);
        let legacy = serde_json::json!({
            "version": LEGACY_FLAT_VERSION,
            "tool": "cargo-gamma legacy",
            "context": context_of(),
            "mutants": [
                { "file": "src/lib.rs", "id": "killed", "killer": killer("tests::caught") },
                { "file": "src/lib.rs", "id": "unviable", "unviable": true },
            ],
            "generalized": GeneralizedHints::empty_supported(),
        });
        fs::write(legacy_path(&root), serde_json::to_vec_pretty(&legacy).expect("legacy JSON")).expect("legacy artifact");
        let loaded = Hints::load(&root);

        assert_eq!(loaded.probes().get("killed"), Some(&killer("tests::caught")));
        assert_eq!(loaded.ordering(), ["unviable"]);
        let migrated = Hints::merged(&loaded, Hints::default(), false, &root).expect("legacy merge");
        assert!(migrated.write(&path(&root)).expect("legacy rewrite").changed);
        let yaml: yaml_serde::Value =
            yaml_serde::from_str(&fs::read_to_string(path(&root)).expect("rewritten artifact")).expect("new YAML");
        assert_eq!(yaml["version"].as_u64(), Some(u64::from(VERSION)));
        assert!(yaml.get("files").is_some());
        assert!(yaml.get("mutants").is_none());
        assert_eq!(yaml["context"].as_mapping().expect("context").len(), 2);
        assert!(!legacy_path(&root).exists());
    }

    #[test]
    fn legacy_migration_compares_against_the_json_generation_used_for_the_merge() {
        let (_dir, root) = workspace("hints-legacy-generation-");
        let legacy = serde_json::json!({
            "version": LEGACY_FLAT_VERSION,
            "tool": "cargo-gamma legacy",
            "context": context_of(),
            "mutants": [
                { "file": "src/lib.rs", "id": "old", "killer": killer("tests::old") },
            ],
        });
        let legacy_path = legacy_path(&root);
        fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).expect("legacy JSON")).expect("legacy artifact");
        let (loaded, yaml_generation, legacy_generation) = Hints::load_for_promotion(&root, false).expect("legacy promotion input");

        fs::write(&legacy_path, "newer legacy generation").expect("concurrent legacy update");
        let error = loaded
            .write_from(&path(&root), yaml_generation.as_deref(), legacy_generation.as_deref())
            .expect_err("stale migration must not supersede a newer JSON generation");

        assert!(
            error.to_string().contains("changed while its YAML replacement was being published"),
            "{error}"
        );
        assert!(!path(&root).exists(), "the stale YAML publication must be rolled back");
        assert_eq!(
            fs::read_to_string(legacy_path).expect("newer legacy bytes"),
            "newer legacy generation"
        );
    }

    #[test]
    fn legacy_migration_keeps_verified_yaml_when_json_removal_is_visible_but_undurable() {
        let (_dir, root) = workspace("hints-legacy-removal-sync-");
        let legacy = serde_json::json!({
            "version": LEGACY_FLAT_VERSION,
            "tool": "cargo-gamma legacy",
            "context": context_of(),
            "mutants": [
                { "file": "src/lib.rs", "id": "old", "killer": killer("tests::old") },
            ],
        });
        let legacy_path = legacy_path(&root);
        fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).expect("legacy JSON")).expect("legacy artifact");
        let (loaded, yaml_generation, legacy_generation) = Hints::load_for_promotion(&root, false).expect("legacy promotion input");
        let merged = Hints::merged(&loaded, Hints::default(), false, &root).expect("legacy merge");

        fail_next_legacy_removal_durability();
        let error = merged
            .write_from(&path(&root), yaml_generation.as_deref(), legacy_generation.as_deref())
            .expect_err("the failed directory sync must be reported");

        assert!(error.to_string().contains("injected directory sync failure"), "{error}");
        assert!(!legacy_path.exists(), "the JSON name was already removed");
        assert_eq!(
            Hints::load(&root),
            merged,
            "the verified YAML must remain as the only surviving copy"
        );
    }

    #[test]
    fn grouped_json_schema_remains_readable_during_migration() {
        let (_dir, root) = workspace("hints-legacy-grouped-");
        let legacy = serde_json::json!({
            "version": LEGACY_GROUPED_VERSION,
            "tool": "cargo-gamma legacy",
            "context": context_of(),
            "files": [{
                "path": "src/lib.rs",
                "killers": [killer("tests::caught")],
                "mutants": [{ "id": "killed", "killer": 0 }],
            }],
            "generalized": GeneralizedHints::empty_supported(),
        });

        fs::write(legacy_path(&root), serde_json::to_vec_pretty(&legacy).expect("legacy JSON")).expect("legacy artifact");

        assert_eq!(Hints::load(&root).probes().get("killed"), Some(&killer("tests::caught")));
    }

    #[test]
    fn a_present_yaml_artifact_is_authoritative_over_legacy_json() {
        let (_dir, root) = workspace("hints-yaml-precedence-");
        let legacy = serde_json::json!({
            "version": LEGACY_FLAT_VERSION,
            "tool": "cargo-gamma legacy",
            "context": context_of(),
            "mutants": [{ "file": "src/lib.rs", "id": "legacy", "killer": killer("tests::legacy") }],
        });

        fs::write(legacy_path(&root), serde_json::to_vec(&legacy).expect("legacy JSON")).expect("legacy artifact");
        fs::write(path(&root), "not: a cargo-gamma artifact\n").expect("current artifact");

        let loaded = Hints::load(&root);

        assert!(loaded.is_empty(), "a corrupt authoritative YAML file fell back to stale JSON");
        assert!(!loaded.probes().contains_key("legacy"));
    }

    #[test]
    fn unsupported_older_and_newer_versions_are_ignored() {
        let (_dir, root) = workspace("hints-unsupported-version-");

        for version in [0, VERSION + 1] {
            fs::write(
                path(&root),
                serde_json::to_vec(&serde_json::json!({
                    "version": version,
                    "tool": "cargo-gamma test",
                    "context": context_of(),
                    "files": [],
                }))
                .expect("unsupported YAML"),
            )
            .expect("unsupported artifact");
            assert!(Hints::load(&root).is_empty(), "version {version} was accepted");
        }
    }

    #[test]
    fn grouped_round_trip_preserves_all_semantics_and_counts() {
        let (_dir, root) = workspace("hints-round-trip-");
        let mut site = mutant("site", "src/lib.rs");
        site.item_path = "subject::changed".into();
        let mut hints = Hints {
            version: VERSION,
            tool: "cargo-gamma provenance".to_owned(),
            context: hint_context(),
            mutants: vec![
                Hint {
                    file: "src/lib.rs".into(),
                    id: "killed".into(),
                    killer: Some(killer("tests::caught")),
                    unviable: false,
                },
                Hint {
                    file: "src/lib.rs".into(),
                    id: "unviable".into(),
                    killer: None,
                    unviable: true,
                },
            ],
            generalized: GeneralizedHints {
                version: record::GENERALIZED_HINTS_VERSION,
                items: vec![record::ItemHints {
                    file: "src/lib.rs".into(),
                    item: "subject::changed".to_owned(),
                    candidates: vec![record::RankedHint {
                        candidate: killer("tests::item"),
                        hits: 3,
                        misses: 1,
                        measured_ms: 12,
                        samples: 4,
                        order: 5,
                    }],
                }],
                binaries: vec![record::FileBinaryHints {
                    file: "src/lib.rs".into(),
                    candidates: vec![record::RankedHint {
                        candidate: record::BinaryHint {
                            package: "subject".to_owned(),
                            target: "lib".to_owned(),
                        },
                        hits: 2,
                        misses: 0,
                        measured_ms: 8,
                        samples: 2,
                        order: 6,
                    }],
                }],
                test_sets: vec![vec![killer("tests::reach")]],
                reach: vec![record::ReachCluster {
                    site: record::SiteIdentity::from_mutant(&site),
                    test_set: 0,
                }],
            },
        };
        hints.normalize();
        hints.write(&path(&root)).expect("grouped artifact");
        let loaded = Hints::load(&root);

        assert_eq!(loaded, hints);
        assert_eq!(
            loaded.counts(),
            Promotion {
                mutants: 2,
                probes: 1,
                ordering: 1,
                generalized: 3,
                changed: false,
            }
        );
    }

    #[test]
    fn equivalent_input_orders_render_identical_bytes_and_minimal_reach_sets() {
        let site_a_mutant = mutant("site-a", "src/lib.rs");
        let site_a = record::SiteIdentity::from_mutant(&site_a_mutant);
        let mut second_mutant = mutant("site-b", "src/lib.rs");
        second_mutant.item_path = "subject::z".into();
        let site_b = record::SiteIdentity::from_mutant(&second_mutant);
        let shared = vec![killer("tests::z"), killer("tests::a"), killer("tests::a")];
        let generalized = GeneralizedHints {
            version: record::GENERALIZED_HINTS_VERSION,
            test_sets: vec![shared.clone(), shared.into_iter().rev().collect()],
            reach: vec![
                record::ReachCluster {
                    site: site_b.clone(),
                    test_set: 1,
                },
                record::ReachCluster {
                    site: site_a.clone(),
                    test_set: 0,
                },
            ],
            ..GeneralizedHints::empty_supported()
        };
        let population = [site_a_mutant, second_mutant];
        let first = Hints::generalized_for(&generalized, &population);
        let reversed = GeneralizedHints {
            test_sets: generalized.test_sets.iter().cloned().rev().collect(),
            reach: vec![
                record::ReachCluster { site: site_a, test_set: 1 },
                record::ReachCluster { site: site_b, test_set: 0 },
            ],
            ..generalized.clone()
        };
        let second = Hints::generalized_for(&reversed, &population);

        assert_eq!(first, second);
        assert_eq!(first.test_sets.len(), 1);
        assert_eq!(first.test_sets[0].len(), 2);

        let make = |generalized| Hints {
            version: VERSION,
            tool: "cargo-gamma test".to_owned(),
            context: hint_context(),
            mutants: vec![
                Hint {
                    file: "src/lib.rs".into(),
                    id: "beta".into(),
                    killer: Some(killer("tests::same")),
                    unviable: false,
                },
                Hint {
                    file: "src/lib.rs".into(),
                    id: "alpha".into(),
                    killer: Some(killer("tests::same")),
                    unviable: true,
                },
            ],
            generalized,
        };
        let mut left = make(first);
        let mut right = make(second);
        right.mutants.reverse();
        left.normalize();
        right.normalize();
        assert_eq!(left.rendered().unwrap(), right.rendered().unwrap());
    }

    #[test]
    fn a_written_artifact_reads_back_as_what_was_written() {
        let (_dir, root) = workspace("hints-write-");
        let record = recorded(&root);
        let population = vec![mutant("unviable", "src/lib.rs"), mutant("killed", "src/lib.rs")];
        let hints = Hints::promoted(&record, &population);
        let promotion = hints.write(&path(&root)).expect("the artifact should be writable");

        assert!(promotion.changed);
        assert_eq!(promotion.mutants, 2);
        assert_eq!(promotion.probes, 1);
        assert_eq!(promotion.ordering, 1);
        assert_eq!(Hints::load(&root), hints);
        assert!(!Hints::is_missing(&root));
    }

    #[test]
    fn explicit_promotion_rejects_semantically_unreadable_artifacts_unless_replacement_was_requested() {
        let (_dir, root) = workspace("hints-strict-load-");
        let current = path(&root);
        let cases = [
            "not: [valid".to_owned(),
            {
                let mut foreign = artifact(&[]);
                foreign.tool = "another-tool".to_owned();
                foreign.rendered().expect("foreign artifact")
            },
            {
                artifact(&[]).rendered().expect("future artifact").replacen(
                    &format!("version: {VERSION}"),
                    &format!("version: {}", VERSION + 1),
                    1,
                )
            },
        ];

        for text in cases {
            fs::write(&current, &text).expect("existing artifact");

            let error = Hints::load_for_promotion(&root, false).expect_err("incremental promotion must preserve unknown bytes");
            assert!(error.to_string().contains("incremental promotion would not preserve"), "{error}");
            assert_eq!(fs::read_to_string(&current).expect("existing bytes"), text);

            let (replacement, generation, legacy_generation) =
                Hints::load_for_promotion(&root, true).expect("explicit replacement may discard unknown knowledge");
            assert!(replacement.is_empty());
            assert_eq!(generation.as_deref(), Some(text.as_str()));
            assert!(legacy_generation.is_none());
        }
    }

    #[test]
    fn promotion_compares_against_the_generation_used_to_derive_it() {
        let (_dir, root) = workspace("hints-generation-");
        let current = path(&root);
        let original = artifact(&[("src/lib.rs", "original", "tests::original")]);
        original.write(&current).expect("original generation");
        let (_loaded, generation, legacy_generation) = Hints::load_for_promotion(&root, false).expect("promotion input");

        let newer = artifact(&[("src/lib.rs", "newer", "tests::newer")]);
        newer.write(&current).expect("newer generation");
        let stale = artifact(&[("src/lib.rs", "stale", "tests::stale")]);

        let error = stale
            .write_from(&current, generation.as_deref(), legacy_generation.as_deref())
            .expect_err("a stale promotion must not replace the newer generation");

        assert!(
            error.to_string().contains("changed while these hints were being promoted"),
            "{error}"
        );
        assert_eq!(Hints::load(&root), newer);
    }

    #[test]
    fn a_hints_write_uses_the_workspace_lock() {
        let (_dir, root) = workspace("hints-lock-");
        let record = recorded(&root);
        let hints = Hints::promoted(&record, &[mutant("unviable", "src/lib.rs")]);
        let _held = crate::exec::claim_workspace(&root).expect("the workspace lock should be available");

        let error = hints
            .write(&path(&root))
            .expect_err("the existing workspace claim must block the write");

        assert!(error.to_string().contains("already using"), "{error}");
    }

    /// A directory sync failure happens after the hints file is visible. Promotion still reports
    /// the durability failure instead of calling that visible generation a successful promotion.
    #[test]
    fn a_post_rename_hints_sync_failure_is_reported() {
        let (_dir, root) = workspace("hints-sync-failure-");
        let record = recorded(&root);
        let hints = Hints::promoted(&record, &[mutant("unviable", "src/lib.rs")]);

        crate::elements::fail_next_directory_sync();

        let error = hints.write(&path(&root)).expect_err("the post-rename sync fails");

        assert!(error.to_string().contains("injected directory sync failure"), "{error}");
        assert_eq!(Hints::load(&root), hints, "the published hints must still be readable");
    }

    /// The first writer publishes, then a second writer completes before the first can read back.
    /// The first must report its failed verification without restoring or removing the second
    /// writer's generation.
    #[test]
    fn a_failed_hints_rollback_leaves_a_later_successful_promotion_intact() {
        let (_dir, root) = workspace("hints-rollback-generation-");
        let record = recorded(&root);
        let first = Hints::promoted(&record, &[mutant("unviable", "src/lib.rs")]);
        let mut second = first.clone();

        second.tool = "cargo-gamma second writer".to_owned();

        let later = second.clone();
        after_next_publication(move |destination| {
            assert!(later.write(destination).expect("the later promotion").changed);
        });

        let error = first.write(&path(&root)).expect_err("the later generation changes read-back");

        assert!(error.to_string().contains("later generation was left alone"), "{error}");
        assert_eq!(Hints::load(&root), second, "the first rollback removed the later promotion");
    }

    /// "There is no file" and "there is a file I cannot read" are different answers, and reading
    /// both as absence turns the rollback into a delete of a checked-in artifact. A file this
    /// cannot restore is a file it must not replace.
    #[test]
    fn an_artifact_that_is_there_but_unreadable_is_not_replaced_and_not_deleted() {
        let (_dir, root) = workspace("hints-unreadable-");
        let destination = path(&root);

        // Bytes that are not UTF-8: present, readable as bytes, and refused by the text read —
        // portable, and needing no permission model to arrange.
        fs::create_dir_all(destination.parent().expect("a parent").as_std_path()).expect("the directory");
        fs::write(destination.as_std_path(), [0xff_u8, 0xfe, 0xfd]).expect("the artifact");

        let record = recorded(&root);
        let population = [mutant("unviable", "src/lib.rs")];
        let hints = Hints::promoted(&record, &population);

        let cause = hints.write(&destination).expect_err("an artifact that cannot be read back");

        assert!(cause.to_string().contains("must not be replaced"), "{cause}");
        assert_eq!(
            fs::read(destination.as_std_path()).expect("the artifact afterwards"),
            [0xff_u8, 0xfe, 0xfd],
            "a promotion that could not read the artifact removed it"
        );
    }

    /// Rewriting the same content is not a change, so a regeneration in CI does not look like one.
    #[test]
    fn writing_the_same_artifact_twice_reports_no_change() {
        let (_dir, root) = workspace("hints-idempotent-");
        let record = recorded(&root);
        let population = [mutant("unviable", "src/lib.rs")];
        let hints = Hints::promoted(&record, &population);

        assert!(hints.write(&path(&root)).expect("writable").changed);
        assert!(!hints.write(&path(&root)).expect("writable").changed);
    }

    fn artifact(entries: &[(&str, &str, &str)]) -> Hints {
        Hints {
            version: VERSION,
            tool: "cargo-gamma test".to_owned(),
            context: hint_context(),
            mutants: entries
                .iter()
                .map(|(file, id, test)| Hint {
                    file: (*file).into(),
                    id: (*id).into(),
                    killer: Some(killer(test)),
                    unviable: false,
                })
                .collect(),
            generalized: GeneralizedHints::empty_supported(),
        }
    }

    fn commit_fixture(root: &Utf8Path) {
        let git = |arguments: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git starts")
        };

        assert!(git(&["init", "--quiet"]).success());
        assert!(git(&["add", "."]).success());
        assert!(
            git(&[
                "-c",
                "user.name=cargo-gamma",
                "-c",
                "user.email=cargo-gamma@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ])
            .success()
        );
    }

    #[test]
    fn partial_promotion_upserts_selected_entries_and_preserves_every_absence() {
        let (_dir, root) = workspace("hints-merge-partial-");
        commit_fixture(&root);
        let existing = artifact(&[
            ("src/lib.rs", "alpha", "tests::old"),
            ("src/lib.rs", "stale", "tests::stale"),
            ("src/other.rs", "outside", "tests::outside"),
        ]);
        let promoted = artifact(&[("src/lib.rs", "alpha", "tests::new"), ("src/lib.rs", "beta", "tests::beta")]);
        let merged = Hints::merged(&existing, promoted, false, &root).expect("merge");
        let changes = merged.changes_from(&existing);

        assert_eq!(merged.mutants.len(), 4);
        assert_eq!(merged.probes().get("alpha"), Some(&killer("tests::new")));
        assert_eq!(merged.probes().get("stale"), Some(&killer("tests::stale")));
        assert_eq!(merged.probes().get("outside"), Some(&killer("tests::outside")));
        assert_eq!(
            changes,
            HintChanges {
                added: 1,
                updated: 1,
                removed: 0,
                preserved: 2,
            }
        );
    }

    #[test]
    fn incremental_promotion_merges_generalized_entries_and_reinterns_reach_sets() {
        let (_dir, root) = workspace("hints-merge-generalized-");
        commit_fixture(&root);
        let mut selected_site = mutant("selected", "src/lib.rs");
        selected_site.item_path = "subject::selected".into();
        let mut outside_site = mutant("outside", "src/other.rs");
        outside_site.item_path = "subject::outside".into();
        let ranked = |test: &str| record::RankedHint {
            candidate: killer(test),
            hits: 1,
            misses: 0,
            measured_ms: 1,
            samples: 1,
            order: 0,
        };
        let mut existing = artifact(&[]);
        existing.generalized = GeneralizedHints {
            version: record::GENERALIZED_HINTS_VERSION,
            items: vec![
                record::ItemHints {
                    file: "src/lib.rs".into(),
                    item: "subject::selected".to_owned(),
                    candidates: vec![ranked("tests::old")],
                },
                record::ItemHints {
                    file: "src/other.rs".into(),
                    item: "subject::outside".to_owned(),
                    candidates: vec![ranked("tests::outside")],
                },
            ],
            test_sets: vec![vec![killer("tests::old_reach")], vec![killer("tests::outside_reach")]],
            reach: vec![
                record::ReachCluster {
                    site: record::SiteIdentity::from_mutant(&selected_site),
                    test_set: 0,
                },
                record::ReachCluster {
                    site: record::SiteIdentity::from_mutant(&outside_site),
                    test_set: 1,
                },
            ],
            ..GeneralizedHints::empty_supported()
        };
        let mut promoted = artifact(&[]);
        promoted.generalized = GeneralizedHints {
            version: record::GENERALIZED_HINTS_VERSION,
            items: vec![record::ItemHints {
                file: "src/lib.rs".into(),
                item: "subject::selected".to_owned(),
                candidates: vec![ranked("tests::new")],
            }],
            test_sets: vec![vec![killer("tests::new_reach")]],
            reach: vec![record::ReachCluster {
                site: record::SiteIdentity::from_mutant(&selected_site),
                test_set: 0,
            }],
            ..GeneralizedHints::empty_supported()
        };

        let merged = Hints::merged(&existing, promoted, false, &root).expect("merge");
        let generalized = merged.generalized();

        assert_eq!(generalized.items.len(), 2);
        assert_eq!(generalized.items[0].candidates[0].candidate.test, "tests::new");
        assert_eq!(generalized.items[1].candidates[0].candidate.test, "tests::outside");
        assert_eq!(generalized.reach.len(), 2);
        assert_eq!(generalized.test_sets.len(), 2);
        assert!(generalized.test_sets.iter().any(|tests| tests == &[killer("tests::new_reach")]));
        assert!(generalized.test_sets.iter().any(|tests| tests == &[killer("tests::outside_reach")]));
    }

    #[test]
    fn explicit_replacement_discards_every_entry_outside_the_selected_population() {
        let (_dir, root) = workspace("hints-merge-replace-");
        commit_fixture(&root);
        let existing = artifact(&[("src/other.rs", "outside", "tests::outside")]);
        let promoted = artifact(&[("src/lib.rs", "alpha", "tests::new")]);

        let merged = Hints::merged(&existing, promoted, true, &root).expect("replace");

        assert_eq!(merged.mutants.len(), 1);
        assert_eq!(merged.probes().get("alpha"), Some(&killer("tests::new")));
        assert!(!merged.probes().contains_key("outside"));
    }

    #[test]
    fn no_op_incremental_promotion_preserves_the_original_generation_metadata() {
        let (_dir, root) = workspace("hints-merge-no-op-");
        let mut existing = artifact(&[("src/lib.rs", "alpha", "tests::same")]);
        existing.tool = "cargo-gamma 0.1.0".to_owned();
        let promoted = existing.clone();
        let merged = Hints::merged(&existing, promoted, false, &root).expect("a no-op does not need git provenance");

        assert_eq!(merged.context, existing.context);
        assert_eq!(merged.context.generated_on, "2026-09-16");
        assert_eq!(merged.tool, existing.tool);
    }

    /// The artifact must never carry a tier that could settle part of a score.
    #[test]
    fn only_unviability_is_admitted_as_a_tier() {
        assert_eq!(tier_of(Outcome::CompileError), Some(Tier::Ordering));

        for refused in [
            Outcome::Killed,
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::Ignored,
            Outcome::NotBuilt,
            Outcome::NoCoverage,
            Outcome::OutOfMemory,
            Outcome::Flaky,
            Outcome::Pending,
        ] {
            assert_eq!(tier_of(refused), None, "{refused:?} would have been carried");
        }
    }
}
