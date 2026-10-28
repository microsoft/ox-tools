// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek as _, Write as _};

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_engine::ops::collect::Confidence;
use serde::{Deserialize, Serialize};

use super::killers::Killers;
use super::measure::{Built, Oracle};
use super::session::{Phases, Session};
use crate::discover::{ContextDigest, Plan, RunRecord, WorkspaceSnapshot};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::{Result, elements as publication};

const VERSION: u32 = 2;
const SNAPSHOT: &str = "testing-session.json";
const JOURNAL: &str = "testing-verdicts.jsonl";
const PHASE: &str = "testing-phase.json";
const MAX_STATE_BYTES: u64 = 512 * 1024 * 1024;

#[cfg(feature = "internals")]
#[doc(hidden)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub mod native_test_util;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RecordPreparation {
    #[serde(default)]
    pub(crate) report_info: Option<crate::elements::RunInfo>,
    pub(crate) base: Utf8PathBuf,
    pub(crate) context: ContextDigest,
    pub(crate) inputs: WorkspaceSnapshot,
    pub(crate) killers: crate::discover::Killers,
    /// Whether `--re` narrowed the population, so publication must retain unselected evidence.
    ///
    /// Absent from state written before name filtering existed, which was never filtered.
    #[serde(default)]
    pub(crate) name_filtered: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Preparing,
    Testing,
    FinishedTesting,
    Completed,
}

#[derive(Serialize, Deserialize)]
struct Status {
    phase: Phase,
    snapshot: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TestingSettings {
    pub(crate) jobs: usize,
    #[serde(default)]
    pub(crate) mutators: Option<String>,
    pub(super) timeout_floor: core::time::Duration,
    pub(super) confirm: bool,
    pub(super) narrowed: bool,
    pub(super) incremental: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct TestingState {
    version: u32,
    tool: String,
    platform: String,
    pub(crate) plan: Plan,
    #[serde(default)]
    confidences: crate::HashMap<crate::model::MutantId, String>,
    pub(crate) session: Session,
    pub(crate) oracle: Oracle,
    pub(crate) workspace: serde_json::Value,
    pub(crate) settings: TestingSettings,
    pub(crate) killers: Killers,
    pub(crate) record: RecordPreparation,
    pub(crate) stuck: Vec<String>,
    pub(crate) dropped: Vec<String>,
    artifact_roots: Vec<Utf8PathBuf>,
    artifacts: Vec<Artifact>,
}

#[derive(Serialize, Deserialize)]
struct Artifact {
    path: Utf8PathBuf,
    digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generated_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<ArtifactMetadata>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactMetadata {
    kind: ArtifactKind,
    executable: u32,
    link: Option<Utf8PathBuf>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ArtifactKind {
    File,
    Directory,
    FileSymlink,
    Symlink,
    Other,
}

impl ArtifactMetadata {
    fn read(path: &Utf8Path) -> Result<Self> {
        Self::read_with(
            path,
            |follow| inspect_artifact(path, follow),
            || {
                Utf8PathBuf::from_path_buf(fs::read_link(path).map_err(|cause| {
                    error!("could not read prepared symlink `{path}`; restore it or start a new campaign").caused_by(cause)
                })?)
                .map_err(|_path| error!("prepared symlink `{path}` has a non-UTF-8 target"))
            },
        )
    }

    fn read_with(
        path: &Utf8Path,
        inspect: impl Fn(bool) -> std::io::Result<(ArtifactKind, u32)>,
        read_link: impl FnOnce() -> Result<Utf8PathBuf>,
    ) -> Result<Self> {
        let (mut kind, mut executable) = inspect(false)
            .map_err(|cause| error!("could not inspect prepared artifact `{path}`; restore it or start a new campaign").caused_by(cause))?;
        let link = if kind == ArtifactKind::Symlink {
            match inspect(true) {
                Ok((ArtifactKind::File, bits)) => {
                    kind = ArtifactKind::FileSymlink;
                    executable = bits;
                }
                Ok((ArtifactKind::Directory, _)) => {
                    return Err(error!(
                        "prepared directory symlink `{path}` cannot be frozen for resumable testing; \
                         replace it with a real directory and start a new campaign"
                    ));
                }
                Ok(_) => {
                    executable = 0;
                }
                Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
                    executable = 0;
                }
                Err(cause) => {
                    return Err(
                        error!("could not inspect prepared symlink target `{path}`; restore it or start a new campaign").caused_by(cause),
                    );
                }
            }
            Some(read_link()?)
        } else {
            None
        };
        if !matches!(kind, ArtifactKind::File | ArtifactKind::FileSymlink) {
            executable = 0;
        }
        Ok(Self { kind, executable, link })
    }
}

fn inspect_artifact(path: &Utf8Path, follow: bool) -> std::io::Result<(ArtifactKind, u32)> {
    let metadata = if follow { fs::metadata(path)? } else { fs::symlink_metadata(path)? };
    let kind = if metadata.file_type().is_symlink() {
        ArtifactKind::Symlink
    } else if metadata.is_file() {
        ArtifactKind::File
    } else if metadata.is_dir() {
        ArtifactKind::Directory
    } else {
        ArtifactKind::Other
    };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111
    };
    #[cfg(not(unix))]
    let executable = 0;
    Ok((kind, executable))
}

#[derive(Debug)]
pub(crate) struct TestingJournal {
    file: File,
    pub(super) previous_sweep: Option<super::session::SweepCost>,
    pub(super) prior_confirmations: std::collections::BTreeSet<crate::model::MutantId>,
    pub(super) initial_hints: Option<super::session::InitialHints>,
    /// Independent statistic, never a publication signal for corresponding journal contents.
    /// Checkpoint callbacks run serially on the sweep coordinator. Final reporting follows
    /// `test_all` joining its workers and completing those callbacks, so relaxed counting suffices;
    /// journal-write completion establishes durability separately.
    pub(super) live_refresh_failures: core::sync::atomic::AtomicUsize,
}

#[derive(Serialize, Deserialize)]
struct WireEntry<'a> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mutant: Option<Cow<'a, Mutant>>,
    probe: Option<Cow<'a, crate::discover::Killer>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sweep: Option<Cow<'a, super::session::SweepCost>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic: Option<Cow<'a, str>>,
}

#[derive(Serialize, Deserialize)]
struct Committed<'a> {
    #[serde(flatten)]
    verdict: WireEntry<'a>,
    checksum: String,
}

#[expect(
    clippy::large_enum_variant,
    reason = "one short-lived journal entry is encoded or replayed at a time; retain the inline payload rather than allocating per record"
)]
enum JournalEntry {
    Verdict {
        mutant: Mutant,
        probe: Option<crate::discover::Killer>,
        sweep: Option<super::session::SweepCost>,
        diagnostic: Option<String>,
    },
    Accounting {
        sweep: super::session::SweepCost,
        diagnostic: Option<String>,
    },
}

impl JournalEntry {
    fn wire(&self) -> WireEntry<'_> {
        match self {
            Self::Verdict {
                mutant,
                probe,
                sweep,
                diagnostic,
            } => WireEntry {
                mutant: Some(Cow::Borrowed(mutant)),
                probe: probe.as_ref().map(Cow::Borrowed),
                sweep: sweep.as_ref().map(Cow::Borrowed),
                diagnostic: diagnostic.as_deref().map(Cow::Borrowed),
            },
            Self::Accounting { sweep, diagnostic } => WireEntry {
                mutant: None,
                probe: None,
                sweep: Some(Cow::Borrowed(sweep)),
                diagnostic: diagnostic.as_deref().map(Cow::Borrowed),
            },
        }
    }
}

fn decode_committed(line: &[u8], path: &Utf8Path) -> Result<JournalEntry> {
    let committed: Committed = serde_json::from_slice(line)
        .map_err(|cause| error!("testing verdicts `{path}` are corrupt; start a new campaign").caused_by(cause))?;
    let encoded =
        serde_json::to_vec(&committed.verdict).map_err(|cause| error!("could not validate testing verdicts `{path}`").caused_by(cause))?;
    if blake3::hash(&encoded).to_hex().as_str() != committed.checksum {
        return Err(error!(
            "testing verdicts `{path}` failed their integrity check; start a new campaign"
        ));
    }
    let WireEntry {
        mutant,
        probe,
        sweep,
        diagnostic,
    } = committed.verdict;
    match (mutant, probe, sweep) {
        (Some(mutant), probe, sweep) => Ok(JournalEntry::Verdict {
            mutant: mutant.into_owned(),
            probe: probe.map(Cow::into_owned),
            sweep: sweep.map(Cow::into_owned),
            diagnostic: diagnostic.map(Cow::into_owned),
        }),
        (None, None, Some(sweep)) => Ok(JournalEntry::Accounting {
            sweep: sweep.into_owned(),
            diagnostic: diagnostic.map(Cow::into_owned),
        }),
        _ => Err(error!("testing verdicts `{path}` contain an invalid accounting checkpoint")),
    }
}

fn encode_committed(verdict: &JournalEntry) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&verdict.wire()).map_err(|cause| error!("could not encode a testing verdict").caused_by(cause))?;
    let checksum = blake3::hash(&bytes).to_hex();
    assert_eq!(bytes.pop(), Some(b'}'), "WireEntry serializes as a JSON object");
    bytes.extend_from_slice(b",\"checksum\":\"");
    bytes.extend_from_slice(checksum.as_bytes());
    bytes.extend_from_slice(b"\"}\n");
    Ok(bytes)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Utf8Path) -> Result<T> {
    read_json_with_limit(path, MAX_STATE_BYTES)
}

fn read_json_with_limit<T: serde::de::DeserializeOwned>(path: &Utf8Path, limit: u64) -> Result<T> {
    let file = File::open(path).map_err(|cause| error!("could not read testing state `{path}`; start a new campaign").caused_by(cause))?;
    read_json_from(path, file, limit)
}

fn read_json_from<T: serde::de::DeserializeOwned>(path: &Utf8Path, reader: impl Read, limit: u64) -> Result<T> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| error!("could not read testing state `{path}`").caused_by(cause))?;
    if bytes.len() as u64 > limit {
        return Err(error!("testing state `{path}` exceeds the supported size; start a new campaign"));
    }
    serde_json::from_slice(&bytes).map_err(|cause| error!("testing state `{path}` is invalid; start a new campaign").caused_by(cause))
}

fn read_verdict_line(path: &Utf8Path, reader: &mut impl BufRead, line: &mut Vec<u8>, limit: u64) -> Result<usize> {
    let length = reader
        .by_ref()
        .take(limit + 1)
        .read_until(b'\n', line)
        .map_err(|cause| error!("could not read testing verdicts `{path}`").caused_by(cause))?;
    Ok(length)
}

fn phase(base: &Utf8Path) -> Result<Option<Phase>> {
    let path = base.join(PHASE);
    match fs::symlink_metadata(&path) {
        Ok(_) => read_json::<Status>(&path).map(|status| Some(status.phase)),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(error!("could not inspect testing phase `{path}`").caused_by(cause)),
    }
}

fn set_phase(base: &Utf8Path, phase: Phase) -> Result<()> {
    let snapshot = if matches!(phase, Phase::Testing | Phase::FinishedTesting) {
        Some(fingerprint(&base.join(SNAPSHOT))?)
    } else {
        None
    };
    let contents =
        serde_json::to_string(&Status { phase, snapshot }).map_err(|cause| error!("could not encode testing phase").caused_by(cause))?;
    publication::write(&base.join(PHASE), &contents)
}

pub(crate) fn invalidate_testing(base: &Utf8Path) -> Result<()> {
    set_phase(base, Phase::Preparing)
}

pub(crate) fn finish_testing_record(base: &Utf8Path) -> Result<()> {
    if phase(base)? == Some(Phase::FinishedTesting) {
        set_phase(base, Phase::Completed)?;
    }
    Ok(())
}

pub(crate) fn partial_testing_record(base: &Utf8Path) -> Result<Option<RunRecord>> {
    if !matches!(phase(base)?, Some(Phase::Testing | Phase::FinishedTesting)) {
        return Ok(None);
    }
    let (state, _) = TestingState::read_checkpoint(base)?;
    partial_record(&state.plan, state.record, state.killers, &RunRecord::load_incomplete(base)).map(Some)
}

fn partial_record(plan: &Plan, preparation: RecordPreparation, mut killers: Killers, incomplete: &RunRecord) -> Result<RunRecord> {
    let mut record = RunRecord::from_completed_plan_snapshot(plan, &preparation.context, preparation.inputs, &preparation.killers)
        .ok_or_else(|| {
            error!(
                "could not construct partial campaign evidence from `{}`; start a new campaign",
                preparation.base
            )
        })?;
    killers.absorb_generalized(incomplete);
    killers.stage(&mut record, &plan.mutants);
    Ok(record)
}

impl TestingState {
    pub(crate) fn is_settled(&self) -> bool {
        self.plan.mutants.iter().all(|mutant| mutant.outcome != Outcome::Pending)
    }

    pub(crate) fn complete_testing(&self) -> Result<()> {
        if !self.is_settled() {
            return Err(error!("cannot publish a testing checkpoint with pending mutants"));
        }
        TestingJournal::complete(&self.record.base)
    }

    pub(super) fn start(
        plan: &Plan,
        built: &Built,
        killers: Killers,
        settings: TestingSettings,
        record: &RecordPreparation,
        omissions: (&[String], &[String]),
    ) -> Result<Self> {
        let state = Self {
            version: VERSION,
            tool: env!("CARGO_PKG_VERSION").to_owned(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            plan: plan.clone(),
            confidences: frozen_confidences(plan),
            session: built.session.clone(),
            oracle: built.oracle.clone(),
            workspace: built.work.testing_snapshot()?,
            settings,
            killers,
            record: record.clone(),
            stuck: omissions.0.to_vec(),
            dropped: omissions.1.to_vec(),
            artifact_roots: built.work.testing_roots()?,
            artifacts: capture_artifacts(built)?,
        };
        publication::write_streamed(&record.base.join(SNAPSHOT), |writer| {
            serde_json::to_writer(writer, &state).map_err(std::io::Error::other)
        })?;
        publication::write(&record.base.join(JOURNAL), "")?;
        if !super::workspace::remember_campaign_base(&plan.root, &record.base) {
            return Err(error!("could not publish the campaign location; testing has not started"));
        }
        set_phase(&record.base, Phase::Testing)?;
        Ok(state)
    }

    pub(crate) fn load(base: &Utf8Path) -> Result<(Self, u64)> {
        if !matches!(phase(base)?, Some(Phase::Testing | Phase::FinishedTesting)) {
            return Err(error!(
                "cannot resume testing: the previous campaign at `{base}` was not interrupted during testing. \
                 Start a new campaign without `--resume-testing`"
            ));
        }
        Self::read_checkpoint(base)
    }

    fn read_checkpoint(base: &Utf8Path) -> Result<(Self, u64)> {
        Self::read_checkpoint_with_limit(base, MAX_STATE_BYTES)
    }

    fn read_checkpoint_with_limit(base: &Utf8Path, limit: u64) -> Result<(Self, u64)> {
        let status: Status = read_json(&base.join(PHASE))?;
        if status.snapshot.as_deref() != Some(fingerprint(&base.join(SNAPSHOT))?.as_str()) {
            return Err(error!(
                "testing snapshot at `{base}` failed its integrity check; start a new campaign"
            ));
        }
        let mut state: Self = read_json(&base.join(SNAPSHOT))?;
        if state.version != VERSION
            || state.tool != env!("CARGO_PKG_VERSION")
            || state.platform != format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        {
            return Err(error!(
                "testing state at `{base}` was written by an incompatible version or platform; start a new campaign"
            ));
        }
        if !super::workspace::same_existing_path(&state.record.base, base) || !state.plan.root.is_absolute() || state.settings.jobs == 0 {
            return Err(error!(
                "testing state at `{base}` has invalid campaign identity or settings; start a new campaign"
            ));
        }
        let mut ids = crate::HashSet::default();
        let mut ordinals = crate::HashMap::default();
        for (index, mutant) in state.plan.mutants.iter().enumerate() {
            if !ids.insert(mutant.id.clone())
                || (mutant.ordinal == 0 && mutant.outcome == Outcome::Pending)
                || (mutant.ordinal != 0 && ordinals.insert(mutant.ordinal, index).is_some())
            {
                return Err(error!(
                    "testing state at `{base}` has ambiguous mutant identities; start a new campaign"
                ));
            }
        }
        let safe_path = |path: &Utf8Path| {
            !path.as_str().is_empty()
                && path
                    .as_std_path()
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)))
        };
        if state.plan.files.iter().any(|file| !safe_path(&file.path)) || state.plan.mutants.iter().any(|mutant| !safe_path(&mutant.file)) {
            return Err(error!("testing state at `{base}` contains a source path outside the workspace"));
        }
        if !RunRecord::contains_captured_sources(&state.plan, &state.record.inputs) {
            return Err(error!("testing state at `{base}` does not match its captured source generation"));
        }
        restore_confidences(&mut state.plan, &state.confidences, &ids, base)?;
        let path = base.join(JOURNAL);
        let file = File::open(&path).map_err(|cause| error!("could not read testing verdicts `{path}`").caused_by(cause))?;
        let mut reader = BufReader::new(file);
        let valid_bytes = replay_verdicts(
            &path,
            &mut reader,
            limit,
            &mut state.plan.mutants,
            &mut state.session.phases,
            &mut state.killers,
            &ordinals,
        )?;
        // A completed sweep can leave unjudged mutants pending. Until ledger publication, resume
        // must preserve its settled prefix and retry those mutants rather than reject the phase.
        Ok((state, valid_bytes))
    }

    pub(crate) fn validate_artifacts(&self) -> Result<()> {
        let workspace = self
            .workspace
            .get("root")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| error!("prepared testing workspace has no source root; start a new campaign"))?;
        let output = Utf8Path::new(workspace).join("target");
        let libraries: Vec<Utf8PathBuf> = match self.workspace.get("libraries") {
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|cause| error!("prepared testing workspace has invalid library paths; start a new campaign").caused_by(cause))?,
            None => Vec::new(),
        };
        for artifact in &self.artifacts {
            // Older checkpoints also captured temporary files left by baseline tests.
            if artifact
                .generated_output
                .unwrap_or_else(|| generated_output(&artifact.path, &output, &self.artifact_roots))
                && !self.session.binaries.iter().any(|binary| binary.path == artifact.path)
                && !is_loaded_library(&artifact.path, &libraries)
            {
                continue;
            }
            validate_artifact(artifact, ArtifactMetadata::read, artifact_digest)?;
        }
        Ok(())
    }
}

fn replay_verdicts(
    path: &Utf8Path,
    reader: &mut impl BufRead,
    limit: u64,
    mutants: &mut [Mutant],
    phases: &mut Phases,
    killers: &mut Killers,
    ordinals: &crate::HashMap<u32, usize>,
) -> Result<u64> {
    let mut valid_bytes = 0_u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let length = read_verdict_line(path, reader, &mut line, limit)?;
        if length == 0 {
            break;
        }
        if length as u64 > limit {
            return Err(error!("testing verdict in `{path}` exceeds the supported size"));
        }
        // An interrupted append has no committed newline. Only that final fragment is ignored.
        if line.last() != Some(&b'\n') {
            break;
        }
        let settled = decode_committed(&line, path)?;
        valid_bytes += length as u64;
        let (settled_mutant, probe) = match settled {
            JournalEntry::Accounting { sweep, .. } => {
                restore_sweep_cost(phases, Some(sweep));
                continue;
            }
            JournalEntry::Verdict { mutant, probe, sweep, .. } => {
                restore_sweep_cost(phases, sweep);
                (mutant, probe)
            }
        };
        let index = ordinals
            .get(&settled_mutant.ordinal)
            .ok_or_else(|| error!("testing verdicts `{path}` name an unknown mutant"))?;
        let mutant = &mut mutants[*index];
        if mutant.id != settled_mutant.id || mutant.outcome != Outcome::Pending || settled_mutant.outcome == Outcome::Pending {
            return Err(error!("testing verdicts `{path}` contain conflicting or unfinished outcomes"));
        }
        mutant.outcome = settled_mutant.outcome;
        mutant.elapsed_ms = settled_mutant.elapsed_ms;
        mutant.killed_by = settled_mutant.killed_by;
        mutant.killer = settled_mutant.killer;
        mutant.note = settled_mutant.note;
        match probe {
            Some(probe) => killers.record(mutant.id.clone(), probe),
            None => killers.forget(&mutant.id),
        }
    }
    Ok(valid_bytes)
}

impl TestingJournal {
    #[cfg(test)]
    pub(super) fn replace_writer_for_test(&mut self, file: File) {
        self.file = file;
    }

    pub(super) fn open(base: &Utf8Path, valid_bytes: Option<u64>) -> Result<Self> {
        let path = base.join(JOURNAL);
        if let Some(length) = valid_bytes {
            let repair = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|cause| error!("could not reopen testing verdicts `{path}` for recovery").caused_by(cause))?;
            repair
                .set_len(length)
                .and_then(|()| repair.sync_all())
                .map_err(|cause| error!("could not discard the interrupted verdict in `{path}`").caused_by(cause))?;
        }
        // Publishing is serialized by the coordinator; Windows needs write access to roll back failed appends.
        let file = OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|cause| error!("could not open testing verdicts `{path}`").caused_by(cause))?;
        Ok(Self {
            file,
            previous_sweep: None,
            prior_confirmations: std::collections::BTreeSet::new(),
            initial_hints: None,
            live_refresh_failures: core::sync::atomic::AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(super) fn publish(&self, mutant: &Mutant, killers: &Killers) -> Result<()> {
        self.publish_with_cost(mutant, killers, None)
    }

    pub(super) fn publish_with_cost(&self, mutant: &Mutant, killers: &Killers, sweep: Option<super::session::SweepCost>) -> Result<()> {
        if mutant.outcome == Outcome::Pending {
            return Ok(());
        }
        let verdict = JournalEntry::Verdict {
            mutant: mutant.clone(),
            probe: killers.hint(&mutant.id).cloned(),
            sweep,
            diagnostic: None,
        };
        self.publish_entry(&verdict)
    }

    pub(super) fn publish_cost(&self, sweep: super::session::SweepCost) -> Result<()> {
        self.publish_entry(&JournalEntry::Accounting { sweep, diagnostic: None })
    }

    pub(super) fn publish_diagnostic(&self, sweep: super::session::SweepCost, diagnostic: String) -> Result<()> {
        self.publish_entry(&JournalEntry::Accounting {
            sweep,
            diagnostic: Some(diagnostic),
        })
    }

    fn publish_entry(&self, verdict: &JournalEntry) -> Result<()> {
        let bytes = encode_committed(verdict)?;
        let mut file = &self.file;
        let length = file
            .seek(std::io::SeekFrom::End(0))
            .map_err(|cause| error!("could not locate the durable testing prefix; the campaign has stopped").caused_by(cause))?;
        append_checkpoint(
            &bytes,
            |bytes| file.write_all(bytes),
            || self.file.sync_data(),
            || self.file.set_len(length),
        )
    }

    pub(super) fn complete(base: &Utf8Path) -> Result<()> {
        set_phase(base, Phase::FinishedTesting)
    }
}

/// Replaces cumulative counter snapshots but unions confirmation identities, which may be journal
/// deltas to keep growth linear. Adding counters would double-count; replacing identities loses history.
fn restore_sweep_cost(phases: &mut super::session::Phases, cost: Option<super::session::SweepCost>) {
    if let Some(mut cost) = cost {
        if let Some(previous) = &mut phases.sweep {
            let mut confirmed = core::mem::take(&mut previous.confirmed_initial_killers);
            confirmed.extend(core::mem::take(&mut cost.confirmed_initial_killers));
            cost.confirmed_initial_killers = confirmed;
        }
        phases.sweep = Some(cost);
    }
}

fn frozen_confidences(plan: &Plan) -> crate::HashMap<crate::model::MutantId, String> {
    plan.mutants
        .iter()
        .filter(|mutant| mutant.confidence != Confidence::Proven)
        .map(|mutant| (mutant.id.clone(), mutant.confidence.as_str().to_owned()))
        .collect()
}

fn restore_confidences(
    plan: &mut Plan,
    confidences: &crate::HashMap<crate::model::MutantId, String>,
    ids: &crate::HashSet<crate::model::MutantId>,
    base: &Utf8Path,
) -> Result<()> {
    if confidences.keys().any(|id| !ids.contains(id)) {
        return Err(error!(
            "testing state at `{base}` contains confidence for an unknown mutant; start a new campaign"
        ));
    }
    for mutant in &mut plan.mutants {
        if let Some(confidence) = confidences.get(&mutant.id) {
            mutant.confidence = match confidence.as_str() {
                "proven" => Confidence::Proven,
                "optimistic" => Confidence::Optimistic,
                "explicit" => Confidence::Explicit,
                _ => {
                    return Err(error!(
                        "testing state at `{base}` contains invalid mutant confidence; start a new campaign"
                    ));
                }
            };
        }
    }
    Ok(())
}

fn is_loaded_library(path: &Utf8Path, libraries: &[Utf8PathBuf]) -> bool {
    matches!(path.extension(), Some("dll" | "so" | "dylib"))
        && path
            .parent()
            .is_some_and(|parent| libraries.iter().any(|library| parent == library))
}

fn append_checkpoint(
    bytes: &[u8],
    write: impl FnOnce(&[u8]) -> std::io::Result<()>,
    mut sync: impl FnMut() -> std::io::Result<()>,
    truncate: impl FnOnce() -> std::io::Result<()>,
) -> Result<()> {
    if let Err(cause) = write(bytes).and_then(|()| sync()) {
        truncate().and_then(|()| sync()).map_err(|rollback| {
            error!(
                "could not persist testing progress ({cause}) or restore the durable journal prefix; \
                    restore the journal before resuming"
            )
            .caused_by(rollback)
        })?;
        return Err(error!("could not persist testing progress; the campaign has stopped").caused_by(cause));
    }
    Ok(())
}

fn capture_artifacts(built: &Built) -> Result<Vec<Artifact>> {
    capture_artifacts_with(built, |directory| fs::read_dir(directory))
}

fn capture_artifacts_with<I: IntoIterator<Item = std::io::Result<fs::DirEntry>>>(
    built: &Built,
    mut enumerate: impl FnMut(&Utf8Path) -> std::io::Result<I>,
) -> Result<Vec<Artifact>> {
    let roots = built.work.testing_roots()?;
    let output = built.work.root.join("target");
    let mut paths = tree_paths(&roots, &output)?;
    for binary in &built.session.binaries {
        let _ = paths.insert(binary.path.clone());
    }
    // Dynamic libraries are loaded after the executable starts and must belong to the same build.
    for directory in &built.work.libraries {
        if !directory.is_dir() {
            continue;
        }
        for entry in enumerate(directory).map_err(|cause| error!("could not inspect test libraries `{directory}`").caused_by(cause))? {
            let entry = entry.map_err(|cause| error!("could not inspect test libraries `{directory}`").caused_by(cause))?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|_path| error!("test library path is not UTF-8"))?;
            if matches!(path.extension(), Some("dll" | "so" | "dylib")) && path.is_file() {
                let _ = paths.insert(path);
            }
        }
    }
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let metadata = ArtifactMetadata::read(&path)?;
            let digest = artifact_digest(&path, Some(&metadata))?;
            Ok(Artifact {
                generated_output: Some(generated_output(&path, &output, &roots)),
                path,
                digest,
                metadata: Some(metadata),
            })
        })
        .collect()
}

fn generated_output(path: &Utf8Path, output: &Utf8Path, roots: &[Utf8PathBuf]) -> bool {
    let Some(workspace) = output.parent() else { return false };
    path.ancestors()
        .take_while(|ancestor| ancestor.starts_with(workspace))
        .filter(|ancestor| ancestor.file_name() == Some("target"))
        .any(|target| {
            let is_output = target == output || target.parent().is_some_and(|parent| parent.join("Cargo.toml").is_file());
            is_output && !roots.iter().any(|root| root.starts_with(target) && path.starts_with(root))
        })
}

fn tree_paths(roots: &[Utf8PathBuf], output: &Utf8Path) -> Result<crate::HashSet<Utf8PathBuf>> {
    tree_paths_with_entries(roots, output, core::convert::identity)
}

fn tree_paths_with_entries<E: std::error::Error + Send + Sync + 'static>(
    roots: &[Utf8PathBuf],
    output: &Utf8Path,
    mut inspect_entry: impl FnMut(walkdir::Result<walkdir::DirEntry>) -> std::result::Result<walkdir::DirEntry, E>,
) -> Result<crate::HashSet<Utf8PathBuf>> {
    let mut paths = crate::HashSet::default();
    for root in roots {
        let metadata =
            fs::symlink_metadata(root).map_err(|cause| error!("could not inspect prepared testing tree `{root}`").caused_by(cause))?;
        if metadata.is_file() || metadata.file_type().is_symlink() {
            let _ = paths.insert(root.clone());
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .follow_root_links(false)
            .into_iter()
            .filter_entry(|entry| Utf8Path::from_path(entry.path()).is_none_or(|path| !generated_output(path, output, roots)))
        {
            let entry =
                inspect_entry(entry).map_err(|cause| error!("could not inspect prepared testing tree `{root}`").caused_by(cause))?;
            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                let path = Utf8PathBuf::from_path_buf(entry.into_path())
                    .map_err(|_path| error!("prepared testing tree `{root}` contains a non-UTF-8 path"))?;
                let _ = paths.insert(path);
            }
        }
    }
    Ok(paths)
}

fn artifact_digest(path: &Utf8Path, metadata: Option<&ArtifactMetadata>) -> Result<String> {
    if let Some(link) = metadata
        .filter(|metadata| metadata.kind == ArtifactKind::Symlink)
        .and_then(|metadata| metadata.link.as_ref())
    {
        return Ok(blake3::hash(link.as_str().as_bytes()).to_hex().to_string());
    }
    if metadata.is_some_and(|metadata| !matches!(metadata.kind, ArtifactKind::File | ArtifactKind::FileSymlink)) {
        return Err(error!(
            "prepared artifact `{path}` is not a regular file; restore it or start a new campaign"
        ));
    }
    fingerprint(path)
}

fn validate_artifact(
    artifact: &Artifact,
    inspect: impl FnOnce(&Utf8Path) -> Result<ArtifactMetadata>,
    digest: impl FnOnce(&Utf8Path, Option<&ArtifactMetadata>) -> Result<String>,
) -> Result<()> {
    let metadata = inspect(&artifact.path)?;
    if !artifact_metadata_matches(artifact.metadata.as_ref(), &metadata) || digest(&artifact.path, Some(&metadata))? != artifact.digest {
        return Err(error!(
            "cannot resume testing: prepared artifact `{}` changed; restore the campaign artifacts or start a new campaign",
            artifact.path
        ));
    }
    Ok(())
}

fn artifact_metadata_matches(expected: Option<&ArtifactMetadata>, actual: &ArtifactMetadata) -> bool {
    match expected {
        Some(expected) => expected == actual,
        // Legacy snapshots did not capture metadata; retain their original byte-validation contract.
        None => matches!(actual.kind, ArtifactKind::File | ArtifactKind::FileSymlink),
    }
}

fn fingerprint(path: &Utf8Path) -> Result<String> {
    let file = File::open(path)
        .map_err(|cause| error!("could not read prepared artifact `{path}`; restore it or start a new campaign").caused_by(cause))?;
    fingerprint_from(path, file)
}

fn fingerprint_from(path: &Utf8Path, mut reader: impl Read) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let length = reader
            .read(&mut buffer)
            .map_err(|cause| error!("could not read prepared artifact `{path}`").caused_by(cause))?;
        if length == 0 {
            break;
        }
        let _ = hasher.update(&buffer[..length]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod survivor_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::error::Error as _;
    use core::time::Duration;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt as _;

    use super::*;
    use crate::discover::TargetFile;
    use crate::exec::{OrderingHints, Workspace};
    use crate::fixtures;

    #[cfg(not(miri))]
    mod replay_model {
        use std::io::Cursor;

        use super::*;
        use crate::exec::{AttemptCost, SweepAccounting, SweepCost};

        struct Expected {
            end: usize,
            mutant: Option<Mutant>,
            probe: Option<crate::discover::Killer>,
            cost: Option<(usize, u64, u8, bool)>,
            malformed: bool,
        }

        fn cost(initial: &[Mutant; 2], (launches, execution_ms, confirmations, complete): (usize, u64, u8, bool)) -> SweepCost {
            SweepCost {
                launches,
                confirmed_initial_killers: initial
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| confirmations & (1 << index) != 0)
                    .map(|(_, mutant)| mutant.id.clone())
                    .collect(),
                accounting: Some(SweepAccounting {
                    complete,
                    exact: AttemptCost {
                        launches,
                        execution_ms,
                        ..AttemptCost::default()
                    },
                    ..SweepAccounting::default()
                }),
                ..SweepCost::default()
            }
        }

        fn generate(
            initial: &[Mutant; 2],
            initial_probe: &crate::discover::Killer,
            instructions: &[u8; 16],
            count: u8,
        ) -> (Vec<u8>, Vec<Expected>) {
            let mut journal = Vec::new();
            let mut expected = Vec::new();
            for pair in instructions.chunks_exact(2).take(usize::from(count % 9)) {
                let opcode = pair[0] % 8;
                let value = pair[1];
                let snapshot = (matches!(opcode, 0 | 1 | 7) || value & 16 != 0).then_some((
                    usize::from(value % 8),
                    u64::from(value) * 13,
                    value & 3,
                    value & 128 != 0,
                ));
                let mut mutant = matches!(opcode, 2 | 3 | 5 | 6).then(|| {
                    let mut mutant = initial[usize::from(opcode % 2)].clone();
                    mutant.outcome = if value & 1 == 0 { Outcome::Killed } else { Outcome::Survived };
                    mutant.elapsed_ms = u64::from(value) * 17;
                    mutant.note = Some(format!("verdict-{value}"));
                    if opcode == 5 {
                        match value % 4 {
                            0 => mutant.id = "unknown".into(),
                            1 => mutant.ordinal = 99,
                            2 => mutant.outcome = Outcome::Pending,
                            _ => mutant.ordinal = 0,
                        }
                    }
                    mutant
                });
                let probe = (mutant.is_some() && value & 8 != 0).then(|| crate::discover::Killer {
                    test: format!("probe-{value}"),
                    ..initial_probe.clone()
                });
                if let Some(mutant) = &mut mutant {
                    mutant.killed_by = probe.as_ref().map(|probe| probe.test.clone());
                    mutant.killer = probe.as_ref().map(|probe| crate::model::KillerIdentity {
                        package: probe.package.clone(),
                        target: probe.target.clone(),
                        test: probe.test.clone(),
                    });
                }
                let malformed = opcode == 4;
                let encoded = if malformed {
                    b"{bad}\n".to_vec()
                } else {
                    let entry = match &mutant {
                        Some(mutant) => JournalEntry::Verdict {
                            mutant: mutant.clone(),
                            probe: probe.clone(),
                            sweep: snapshot.map(|snapshot| cost(initial, snapshot)),
                            diagnostic: None,
                        },
                        None => JournalEntry::Accounting {
                            sweep: cost(initial, snapshot.unwrap()),
                            diagnostic: (opcode == 7).then(|| "generated diagnostic".into()),
                        },
                    };
                    encode_committed(&entry).unwrap()
                };
                assert!(encoded.len() <= 4096, "each generated record is bounded");
                journal.extend(encoded);
                expected.push(Expected {
                    end: journal.len(),
                    mutant,
                    probe,
                    cost: snapshot,
                    malformed,
                });
            }
            assert!(journal.len() <= 32 * 1024, "at most eight bounded records are generated");
            (journal, expected)
        }

        #[test]
        fn bounded_journal_sequences_match_an_independent_replay_model() {
            let mut first = fixtures::mutant();
            first.ordinal = 1;
            first.outcome = Outcome::Pending;
            let mut second = first.clone();
            second.id = "second".into();
            second.ordinal = 2;
            let initial = [first, second];
            let ordinals = initial.iter().enumerate().map(|(index, mutant)| (mutant.ordinal, index)).collect();
            let initial_probe = crate::discover::Killer {
                package: "subject".into(),
                target: "lib:subject".into(),
                test: "initial".into(),
            };
            bolero::check!()
                .with_type::<(u8, [u8; 16], u16, u8)>()
                .for_each(|(count, instructions, cut, chunk)| {
                    let (journal, expected) = generate(&initial, &initial_probe, instructions, *count);
                    for end in [0, usize::from(*cut) % (journal.len() + 1), journal.len()] {
                        let mut model_mutants = initial.clone();
                        let mut model_probes = [Some(initial_probe.clone()), Some(initial_probe.clone())];
                        let mut model_cost = (11, 13, 1, false);
                        let mut confirmations = std::collections::BTreeSet::from([initial[0].id.clone()]);
                        let mut valid_bytes = 0;
                        let mut invalid = false;
                        for record in &expected {
                            if record.end > end {
                                break;
                            }
                            if record.malformed {
                                invalid = true;
                                break;
                            }
                            if let Some(mutant) = &record.mutant {
                                let index = model_mutants
                                    .iter()
                                    .position(|original| original.id == mutant.id && original.ordinal == mutant.ordinal);
                                let Some(index) = index else {
                                    invalid = true;
                                    break;
                                };
                                if model_mutants[index].outcome != Outcome::Pending || mutant.outcome == Outcome::Pending {
                                    invalid = true;
                                    break;
                                }
                                model_mutants[index] = mutant.clone();
                                model_probes[index].clone_from(&record.probe);
                            }
                            if let Some(snapshot) = record.cost {
                                model_cost = snapshot;
                                for (index, mutant) in initial.iter().enumerate() {
                                    if snapshot.2 & (1 << index) != 0 {
                                        let _inserted = confirmations.insert(mutant.id.clone());
                                    }
                                }
                            }
                            valid_bytes = record.end;
                        }

                        for capacity in [1 + usize::from(*chunk % 32), end.max(1)] {
                            let mut reader = BufReader::with_capacity(capacity, Cursor::new(&journal[..end]));
                            let mut mutants = initial.clone();
                            let mut phases = Phases {
                                sweep: Some(cost(&initial, (11, 13, 1, false))),
                                ..Phases::default()
                            };
                            let mut killers = Killers::default();
                            for mutant in &initial {
                                killers.record(mutant.id.clone(), initial_probe.clone());
                            }
                            let replayed = replay_verdicts(
                                Utf8Path::new("generated-verdicts.jsonl"),
                                &mut reader,
                                4096,
                                &mut mutants,
                                &mut phases,
                                &mut killers,
                                &ordinals,
                            );
                            assert_eq!(replayed.is_err(), invalid);
                            if invalid {
                                continue;
                            }
                            assert_eq!(replayed.unwrap(), valid_bytes as u64);
                            assert_eq!(reader.stream_position().unwrap(), end as u64, "finite input must be consumed");
                            assert_eq!(
                                serde_json::to_value(&mutants).unwrap(),
                                serde_json::to_value(&model_mutants).unwrap()
                            );
                            let restored = phases.sweep.unwrap();
                            let accounting = restored.accounting.unwrap();
                            assert_eq!(restored.launches, model_cost.0);
                            assert_eq!(accounting.exact.launches, model_cost.0);
                            assert_eq!(accounting.exact.execution_ms, model_cost.1);
                            assert_eq!(accounting.complete, model_cost.3);
                            assert_eq!(restored.confirmed_initial_killers, confirmations);
                            for (mutant, expected_probe) in initial.iter().zip(&model_probes) {
                                assert_eq!(killers.hint(&mutant.id), expected_probe.as_ref());
                            }
                        }
                    }
                });
        }
    }

    #[test]
    fn validated_journal_kinds_preserve_wire_bytes_and_reject_invalid_accounting() {
        let path = Utf8Path::new("in-memory-verdicts.jsonl");
        let mutant = fixtures::mutant();
        let probe = crate::discover::Killer {
            package: "subject".into(),
            target: "lib:subject".into(),
            test: "regression".into(),
        };
        let sweep = super::super::session::SweepCost::default();
        let mutant_json = serde_json::to_string(&mutant).unwrap();
        let probe_json = serde_json::to_string(&probe).unwrap();
        let sweep_json = serde_json::to_string(&sweep).unwrap();
        for (entry, payload) in [
            (
                JournalEntry::Verdict {
                    mutant: mutant.clone(),
                    probe: None,
                    sweep: None,
                    diagnostic: None,
                },
                format!(r#"{{"mutant":{mutant_json},"probe":null}}"#),
            ),
            (
                JournalEntry::Verdict {
                    mutant,
                    probe: Some(probe),
                    sweep: Some(sweep.clone()),
                    diagnostic: Some("retained diagnostic".into()),
                },
                format!(r#"{{"mutant":{mutant_json},"probe":{probe_json},"sweep":{sweep_json},"diagnostic":"retained diagnostic"}}"#),
            ),
            (
                JournalEntry::Accounting {
                    sweep: sweep.clone(),
                    diagnostic: None,
                },
                format!(r#"{{"probe":null,"sweep":{sweep_json}}}"#),
            ),
            (
                JournalEntry::Accounting {
                    sweep,
                    diagnostic: Some("retained diagnostic".into()),
                },
                format!(r#"{{"probe":null,"sweep":{sweep_json},"diagnostic":"retained diagnostic"}}"#),
            ),
        ] {
            let checksum = blake3::hash(payload.as_bytes()).to_hex();
            let expected = format!("{},\"checksum\":\"{checksum}\"}}\n", payload.strip_suffix('}').unwrap());
            let encoded = encode_committed(&entry).unwrap();
            assert_eq!(encoded, expected.as_bytes());
            assert_eq!(encode_committed(&decode_committed(&encoded, path).unwrap()).unwrap(), encoded);
        }
        for payload in [
            r#"{"probe":null}"#.to_owned(),
            format!(r#"{{"probe":{probe_json}}}"#),
            format!(r#"{{"probe":{probe_json},"sweep":{sweep_json}}}"#),
        ] {
            let checksum = blake3::hash(payload.as_bytes()).to_hex();
            let encoded = format!("{},\"checksum\":\"{checksum}\"}}\n", payload.strip_suffix('}').unwrap());
            assert!(
                decode_committed(encoded.as_bytes(), path)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("invalid accounting checkpoint")
            );
        }
    }

    #[test]
    fn telemetry_journal_accepts_legacy_checksums_and_accounting_only_partial_checkpoints() {
        use crate::discover::Killer;
        use crate::exec::{AttemptCost, SweepAccounting, SweepCost};

        let path = Utf8Path::new("in-memory-verdicts.jsonl");
        let mutant = fixtures::mutant();
        let payload = format!(r#"{{"mutant":{},"probe":null}}"#, serde_json::to_string(&mutant).unwrap());
        let line = format!(
            r#"{{"mutant":{},"probe":null,"checksum":"{}"}}"#,
            serde_json::to_string(&mutant).unwrap(),
            blake3::hash(payload.as_bytes()).to_hex()
        );
        let JournalEntry::Verdict { mutant: legacy, sweep, .. } = decode_committed(line.as_bytes(), path).unwrap() else {
            panic!("a legacy mutant record must decode as a verdict");
        };
        assert_eq!(legacy.id, mutant.id);
        assert!(sweep.is_none());

        let original = Killer {
            package: "original-package".into(),
            target: "test:original-target".into(),
            test: "original-test".into(),
        };
        let initial = super::super::session::InitialHints {
            population: 3,
            killers: std::iter::once((mutant.id.clone(), original.clone())).collect(),
        };
        let mut phases = Phases {
            initial_hints: Some(initial),
            ..Phases::default()
        };
        for launches in [2, 7, 7] {
            let verdict = JournalEntry::Accounting {
                sweep: SweepCost {
                    launches,
                    confirmed_initial_killers: std::iter::once(mutant.id.clone()).collect(),
                    accounting: Some(SweepAccounting {
                        complete: true,
                        initial_population: Some(3),
                        initial_exact_hints: Some(1),
                        initial_killer_confirmed: Some(1),
                        live_refresh_failures: usize::from(launches == 7),
                        exact: AttemptCost {
                            launches,
                            execution_ms: launches as u64 * 10,
                            ..AttemptCost::default()
                        },
                        ..SweepAccounting::default()
                    }),
                    ..SweepCost::default()
                },
                diagnostic: (launches == 7).then(|| "live diagnostics unavailable: permission denied".into()),
            };
            let bytes = encode_committed(&verdict).unwrap();
            let JournalEntry::Accounting { sweep, diagnostic } = decode_committed(&bytes, path).unwrap() else {
                panic!("partial costs must not invent a mutant verdict");
            };
            assert_eq!(
                diagnostic.is_some(),
                launches == 7,
                "file-only failure evidence is checksummed and readable"
            );
            restore_sweep_cost(&mut phases, Some(sweep));
            phases = serde_json::from_value(serde_json::to_value(phases).unwrap()).unwrap();
        }
        let cost = phases.sweep.take().unwrap();
        assert_eq!(cost.launches, 7, "journal costs are snapshots, not deltas");
        assert_eq!(cost.accounting.as_ref().unwrap().exact.execution_ms, 70);
        let resumed = SweepCost {
            launches: 3,
            confirmed_initial_killers: std::iter::once(mutant.id.clone()).collect(),
            accounting: Some(SweepAccounting {
                complete: true,
                initial_population: Some(3),
                initial_exact_hints: Some(1),
                initial_killer_confirmed: Some(1),
                exact: AttemptCost {
                    launches: 3,
                    execution_ms: 30,
                    ..AttemptCost::default()
                },
                ..SweepAccounting::default()
            }),
            ..SweepCost::default()
        };
        restore_sweep_cost(&mut phases, SweepCost::combine(Some(cost), Some(resumed)));
        restore_sweep_cost(&mut phases, None);
        let completed = phases.sweep.as_ref().unwrap();
        assert_eq!(completed.launches, 10);
        let accounting = completed.accounting.as_ref().unwrap();
        assert_eq!((accounting.exact.launches, accounting.exact.execution_ms), (10, 100));
        assert_eq!(
            accounting.initial_killer_confirmed,
            Some(1),
            "observed identities deduplicate across resume"
        );
        assert_eq!(accounting.live_refresh_failures, 1);
        assert_eq!(
            phases.initial_hints.as_ref().unwrap().killers[&mutant.id],
            original,
            "completed publication retains the initial, not learned, hint"
        );
        assert!(decode_committed(line.replace("\"probe\":null", "\"probe\":{}").as_bytes(), path).is_err());
    }

    #[test]
    fn testing_settings_preserve_selection_and_accept_legacy_snapshots() {
        let settings = TestingSettings {
            jobs: 7,
            mutators: Some("arith.add_to_sub,relational.lt_to_le".to_owned()),
            timeout_floor: Duration::from_secs(20),
            confirm: true,
            narrowed: true,
            incremental: false,
        };
        let mut encoded = serde_json::to_value(&settings).unwrap();
        let loaded: TestingSettings = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(loaded.jobs, 7);
        assert_eq!(loaded.mutators, settings.mutators);
        encoded.as_object_mut().unwrap().remove("mutators");
        let legacy: TestingSettings = serde_json::from_value(encoded).unwrap();
        assert_eq!(legacy.jobs, 7);
        assert!(legacy.mutators.is_none());
    }

    #[test]
    fn incremental_confirmation_records_have_bounded_size_and_linear_total_growth() {
        use crate::exec::{SweepAccounting, SweepCost};

        fn sizes(count: usize) -> (usize, usize) {
            let mut total = 0;
            let mut largest = 0;
            for index in 0..count {
                let verdict = JournalEntry::Accounting {
                    sweep: SweepCost {
                        confirmed_initial_killers: [format!("{index:012x}").into()].into(),
                        accounting: Some(SweepAccounting {
                            complete: true,
                            initial_killer_confirmed: Some(index + 1),
                            ..SweepAccounting::default()
                        }),
                        ..SweepCost::default()
                    },
                    diagnostic: None,
                };
                let bytes = encode_committed(&verdict).unwrap();
                let JournalEntry::Accounting { sweep, .. } = decode_committed(&bytes, Utf8Path::new("bounded.jsonl")).unwrap() else {
                    panic!("confirmation deltas must decode as accounting");
                };
                assert_eq!(sweep.confirmed_initial_killers.len(), 1);
                total += bytes.len();
                largest = largest.max(bytes.len());
            }
            (total, largest)
        }
        let (small, _) = sizes(128);
        let (large, largest) = sizes(256);
        assert!(
            largest < 4096,
            "a checkpoint must not grow with the accumulated confirmation set: {largest}"
        );
        assert!(
            large <= small * 205 / 100,
            "doubling observations must approximately double bytes: {small} -> {large}"
        );
    }

    #[test]
    fn artifact_capture_includes_binary_and_dynamic_library_bytes_but_excludes_nonlibraries_and_generated_output() {
        let (_directory, state) = fixture();
        let root = state.plan.root.join("prepared");
        let libraries = state.plan.root.join("outside-libraries");
        fs::create_dir_all(root.join("gamma-rt")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(libraries.join("directory.dll")).unwrap();
        fs::write(root.join("source.rs"), "source").unwrap();
        fs::write(root.join("gamma-rt/lib.rs"), "runtime").unwrap();
        fs::write(root.join("target/debug/object"), "generated").unwrap();
        for name in ["first.dll", "second.so", "third.dylib", "ignored.txt"] {
            fs::write(libraries.join(name), name).unwrap();
        }
        let binary = state.plan.root.join("test-binary");
        fs::write(&binary, "executable").unwrap();
        let mut work = Workspace::adopt(root.clone(), state.plan.root.join("cargo-target"));
        work.libraries = vec![state.plan.root.join("missing-libraries"), libraries.clone()];
        let mut session = state.session;
        session.binaries = vec![
            crate::testing::test_binary(binary.as_str()),
            crate::testing::test_binary(binary.as_str()),
        ];
        let built = Built {
            work,
            session,
            oracle: state.oracle,
        };
        let artifacts = capture_artifacts(&built).unwrap();
        let mut expected = [
            root.join("source.rs"),
            root.join("gamma-rt/lib.rs"),
            binary,
            libraries.join("first.dll"),
            libraries.join("second.so"),
            libraries.join("third.dylib"),
        ];
        expected.sort();
        assert_eq!(
            artifacts.iter().map(|artifact| &artifact.path).collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>()
        );
        for artifact in artifacts {
            let bytes = fs::read(&artifact.path).unwrap();
            assert_eq!(artifact.digest, blake3::hash(&bytes).to_hex().to_string());
            assert!(artifact.metadata.is_some());
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the in-memory campaign keeps its three learned tiers and conflicting verdict evidence together"
    )]
    fn partial_records_promote_interrupted_generalized_learning_without_replacing_journal_evidence() {
        use crate::discover::{
            BinaryHint, FileBinaryHints, GeneralizedHints, Hints, ItemHints, Killer, RankedHint, ReachCluster, SiteIdentity,
        };

        let root = Utf8PathBuf::from("workspace");
        let source = include_str!("../../tests/fixtures/completed-recovery/lib.rs");
        let plan = Plan {
            root: root.clone(),
            files: vec![TargetFile {
                path: "src/lib.rs".into(),
                absolute: root.join("src/lib.rs"),
                package: "subject".into(),
                source: Some(source.into()),
            }],
            mutants: vec![
                fixtures::mutant(),
                fixtures::mutant_at("pending", "src/lib.rs", 1, "relational.lt_to_le", Outcome::Pending),
            ],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: crate::HashMap::from_iter([("src/lib.rs".into(), crate::discover::digest(source.as_bytes()))]),
            skipped: Vec::new(),
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let preparation = RecordPreparation {
            report_info: None,
            context: ContextDigest::default(),
            inputs: serde_json::from_value(serde_json::json!({
                "complete": true,
                "files": [{"path": "src/lib.rs", "digest": crate::discover::digest(source.as_bytes()), "size": source.len()}]
            }))
            .unwrap(),
            killers: crate::discover::Killers::default(),
            name_filtered: false,
            base: "state".into(),
        };
        let journal_killer = Killer {
            package: "subject".into(),
            target: "lib:subject".into(),
            test: "journal_test".into(),
        };
        let learned_killer = Killer {
            test: "learned_test".into(),
            ..journal_killer.clone()
        };
        let generalized = GeneralizedHints {
            items: vec![ItemHints {
                file: "src/lib.rs".into(),
                item: "f".into(),
                candidates: vec![RankedHint {
                    candidate: learned_killer.clone(),
                    seeds: 1,
                    hits: 2,
                    misses: 0,
                    measured_ms: 3,
                    samples: 2,
                    order: 1,
                }],
            }],
            binaries: vec![FileBinaryHints {
                file: "src/lib.rs".into(),
                candidates: vec![RankedHint {
                    candidate: BinaryHint {
                        package: "subject".into(),
                        target: "lib:subject".into(),
                    },
                    seeds: 1,
                    hits: 2,
                    misses: 0,
                    measured_ms: 3,
                    samples: 2,
                    order: 1,
                }],
            }],
            test_sets: vec![vec![learned_killer.clone()]],
            reach: vec![ReachCluster {
                site: SiteIdentity::from_mutant(&plan.mutants[0]),
                test_set: 0,
            }],
            ..GeneralizedHints::empty_supported()
        };
        let mut incomplete_plan = plan.clone();
        incomplete_plan.mutants[0].outcome = Outcome::Survived;
        incomplete_plan.mutants[1].outcome = Outcome::Killed;
        let mut incomplete = RunRecord::from_completed_plan_snapshot(
            &incomplete_plan,
            &preparation.context,
            preparation.inputs.clone(),
            &preparation.killers,
        )
        .unwrap();
        incomplete.replace_knowledge(
            crate::HashMap::from_iter([("m1".into(), learned_killer.clone()), ("pending".into(), learned_killer)]),
            generalized.clone(),
        );
        let mut killers = Killers::default();
        killers.record("m1".into(), journal_killer.clone());
        let record = partial_record(&plan, preparation, killers, &incomplete).unwrap();
        assert_eq!(record.generalized(), generalized);
        assert_eq!(record.probes(), &crate::HashMap::from_iter([("m1".into(), journal_killer)]));
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            vec![("m1", Outcome::Killed), ("pending", Outcome::Pending)]
        );
        let (promoted, omitted) = Hints::promoted_record(&record);
        assert_eq!(omitted, 0);
        assert_eq!(promoted.generalized(), generalized);
    }

    fn fixture() -> (tempfile::TempDir, TestingState) {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
        let base = root.join("state");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&base).unwrap();
        fs::write(root.join("src/lib.rs"), "a").unwrap();
        let mut pending = fixtures::mutant();
        pending.outcome = Outcome::Pending;
        let mut second = pending.clone();
        second.id = "m2".into();
        second.ordinal = 2;
        let mut ignored = pending.clone();
        ignored.id = "m3".into();
        ignored.ordinal = 0;
        ignored.outcome = Outcome::Ignored;
        let plan = Plan {
            root: root.clone(),
            files: vec![TargetFile {
                path: "src/lib.rs".into(),
                absolute: root.join("src/lib.rs"),
                package: "subject".into(),
                source: Some("a".into()),
            }],
            mutants: vec![pending, second, ignored],
            suppressed: 1,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: std::iter::once(("src/lib.rs".into(), crate::discover::digest(b"a"))).collect(),
            skipped: Vec::new(),
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let session = Session {
            baseline: Duration::from_secs(2),
            baseline_wall: Duration::from_secs(1),
            tests: Some(1),
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::from_secs(3),
            peak: None,
            metered: false,
            unbounded: None,
            withdrawn: 0,
            census: Vec::new(),
            rounds: 1,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            scratch: base.clone(),
            filtered: 0,
            widened: false,
            ordering: OrderingHints::default(),
            phases: Phases::default(),
        };
        let state = TestingState {
            version: VERSION,
            tool: env!("CARGO_PKG_VERSION").into(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            confidences: frozen_confidences(&plan),
            plan,
            session,
            oracle: Oracle {
                packages: Vec::new(),
                package_local: true,
                whole_workspace: false,
            },
            workspace: serde_json::json!({ "root": root.join("src") }),
            settings: TestingSettings {
                jobs: 1,
                mutators: None,
                timeout_floor: Duration::from_secs(20),
                confirm: true,
                narrowed: true,
                incremental: false,
            },
            killers: Killers::default(),
            record: RecordPreparation {
                report_info: fixtures::report_with(Some((1, 3)), 123, Vec::new()).config,
                context: ContextDigest::default(),
                inputs: RunRecord::snapshot_with_external(&root, &base, &[], false),
                killers: crate::discover::Killers::default(),
                name_filtered: false,
                base,
            },
            stuck: Vec::new(),
            dropped: Vec::new(),
            artifact_roots: vec![root.join("src")],
            artifacts: vec![Artifact {
                generated_output: None,
                path: root.join("src/lib.rs"),
                digest: fingerprint(&root.join("src/lib.rs")).unwrap(),
                metadata: Some(ArtifactMetadata::read(&root.join("src/lib.rs")).unwrap()),
            }],
        };
        save(&state);
        (directory, state)
    }

    fn save(state: &TestingState) {
        publication::write(&state.record.base.join(SNAPSHOT), &serde_json::to_string(state).unwrap()).unwrap();
        publication::write(&state.record.base.join(JOURNAL), "").unwrap();
        set_phase(&state.record.base, Phase::Testing).unwrap();
    }

    fn native_non_utf8_name() -> std::ffi::OsString {
        #[cfg(unix)]
        {
            std::ffi::OsString::from_vec(vec![b'n', 0xff, b'.', b's', b'o'])
        }
        #[cfg(windows)]
        {
            std::ffi::OsString::from_wide(&[0xd800, u16::from(b'.'), u16::from(b'd'), u16::from(b'l'), u16::from(b'l')])
        }
    }

    fn prepared_start() -> (tempfile::TempDir, TestingState, Built) {
        let (directory, state) = fixture();
        let root = state.plan.root.join("prepared");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir(root.join("gamma-rt")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("src/lib.rs"), "a").unwrap();
        let binary = super::super::test_binary::TestBinary {
            path: root.join("target/subject"),
            package: "subject".into(),
            tests: Some(4),
            ..crate::testing::test_binary("subject")
        };
        fs::write(&binary.path, "prepared executable bytes").unwrap();
        let mut session = state.session.clone();
        session.binaries.push(binary);
        let built = Built {
            work: Workspace::adopt(root.clone(), root.join("target")),
            session,
            oracle: state.oracle.clone(),
        };
        fs::write(state.record.base.join(".cargo-gamma-owner"), state.plan.root.as_str()).unwrap();
        set_phase(&state.record.base, Phase::Preparing).unwrap();
        (directory, state, built)
    }

    #[test]
    fn start_rejects_non_utf8_runtime_artifacts_after_serializing_the_native_environment() {
        let (_directory, state, mut built) = prepared_start();
        built.work.set_test_environment("subject", "OUT_DIR", native_non_utf8_name());
        built
            .work
            .testing_snapshot()
            .expect("OsString values serialize losslessly before testing_paths validates UTF-8");
        let Err(failure) = TestingState::start(&state.plan, &built, Killers::default(), state.settings, &state.record, (&[], &[])) else {
            panic!("an unrepresentable runtime artifact must stop campaign preparation")
        };
        assert_eq!(failure.message(), "a prepared test artifact has a non-UTF-8 path");
        assert_eq!(phase(&state.record.base).unwrap(), Some(Phase::Preparing));
    }

    #[test]
    fn starting_testing_freezes_killers_and_artifacts_before_opening_an_empty_journal() {
        let (_directory, state, built) = prepared_start();
        let binary = &built.session.binaries[0];
        let mut killers = Killers::default();
        let killer = crate::discover::Killer {
            package: binary.package.clone(),
            target: binary.killer_target(),
            test: "historical-test".into(),
        };
        killers.record(state.plan.mutants[0].id.clone(), killer.clone());
        fs::write(state.record.base.join(JOURNAL), "old journal must be replaced").unwrap();
        let started = TestingState::start(
            &state.plan,
            &built,
            killers,
            state.settings,
            &state.record,
            (&["stuck package".into()], &["dropped package".into()]),
        )
        .unwrap();
        assert_eq!(fs::read(state.record.base.join(JOURNAL)).unwrap(), b"");
        assert_eq!(phase(&state.record.base).unwrap(), Some(Phase::Testing));
        assert_eq!(started.killers.hint(&state.plan.mutants[0].id), Some(&killer));
        assert_eq!(started.stuck, ["stuck package"]);
        assert_eq!(started.dropped, ["dropped package"]);
        assert!(started.artifacts.iter().any(
            |artifact| artifact.path == binary.path && artifact.digest == blake3::hash(b"prepared executable bytes").to_hex().as_str()
        ));
        let (loaded, valid_bytes) = TestingState::load(&state.record.base).unwrap();
        assert_eq!(valid_bytes, 0);
        assert_eq!(loaded.killers.hint(&state.plan.mutants[0].id), Some(&killer));
    }

    #[test]
    fn start_failures_preserve_preparing_phase_at_each_independent_persistence_boundary() {
        for stage in ["artifacts", "snapshot", "journal", "location"] {
            let (_directory, state, built) = prepared_start();
            let path = state.record.base.join(if stage == "snapshot" { SNAPSHOT } else { JOURNAL });
            match stage {
                "artifacts" => fs::remove_file(&built.session.binaries[0].path).unwrap(),
                "snapshot" | "journal" => {
                    fs::remove_file(&path).unwrap();
                    fs::create_dir(&path).unwrap();
                }
                "location" => fs::remove_file(state.record.base.join(".cargo-gamma-owner")).unwrap(),
                _ => unreachable!("the boundary matrix is closed"),
            }
            let Err(failure) = TestingState::start(&state.plan, &built, Killers::default(), state.settings, &state.record, (&[], &[]))
            else {
                panic!("{stage} failure must not open the testing phase")
            };
            if stage == "artifacts" {
                assert!(failure.message().starts_with("could not inspect prepared artifact"), "{failure}");
            } else if stage == "location" {
                assert_eq!(
                    failure.message(),
                    "could not publish the campaign location; testing has not started"
                );
            } else {
                assert_eq!(failure.message(), format!("could not write `{path}`"));
            }
            assert_eq!(phase(&state.record.base).unwrap(), Some(Phase::Preparing));
        }
    }

    #[cfg(windows)]
    #[test]
    fn starting_testing_propagates_the_final_phase_replacement_failure() {
        const SHARE_READ: u32 = 1;
        let (_directory, state, built) = prepared_start();
        let path = state.record.base.join(PHASE);
        let lock = OpenOptions::new().read(true).share_mode(SHARE_READ).open(&path).unwrap();
        let Err(failure) = TestingState::start(&state.plan, &built, Killers::default(), state.settings, &state.record, (&[], &[])) else {
            panic!("successful snapshot and journal publication do not hide the final phase failure")
        };
        assert_eq!(failure.message(), format!("could not write `{path}`"));
        drop(lock);
        assert_eq!(phase(&state.record.base).unwrap(), Some(Phase::Preparing));
        assert_eq!(fs::read(state.record.base.join(JOURNAL)).unwrap(), b"");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the coordinated failure and recovery share one owned worker and journal scenario"
    )]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem-backed verdict journals")]
    fn concurrent_checkpoint_failure_joins_workers_and_resumes_only_the_durable_prefix() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Condvar, Mutex, mpsc};

        use super::super::stall::Stall;
        use super::super::sweep::{self, Judgement, Sweep};
        use super::super::test_binary::{Reachability, TestScope};

        let (_directory, mut state) = fixture();
        let template = state.plan.mutants[0].clone();
        state.plan.mutants = (0..40)
            .map(|index| {
                let mut mutant = template.clone();
                mutant.id = format!("m{index}").into();
                mutant.ordinal = index + 1;
                mutant
            })
            .collect();
        save(&state);
        let base = &state.record.base;
        let journal = TestingJournal::open(base, None).unwrap();
        let failing = TestingJournal {
            file: File::open(base.join(JOURNAL)).unwrap(),
            previous_sweep: None,
            prior_confirmations: std::collections::BTreeSet::new(),
            initial_hints: None,
            live_refresh_failures: core::sync::atomic::AtomicUsize::new(0),
        };
        let work = Workspace::adopt(state.plan.root.clone(), state.plan.root.join("target"));
        let binary = crate::testing::test_binary(state.plan.root.join("fake-binary").as_str());
        let binaries = vec![binary];
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&state.plan, &binaries, &scope);
        let options = Sweep {
            timeout_floor: Duration::ZERO,
            stall: Stall::NONE,
            jobs: 4,
            meter: false,
            confirm: true,
        };
        let mut events = crate::testing::Recorder::default();
        let mut calls = 0;
        let claimed = AtomicUsize::new(0);
        let release = (Mutex::new(0), Condvar::new());
        let (running, active) = mpsc::channel();
        let failure = sweep::test_all_with_hooks(
            &work,
            &mut state.plan,
            &reach,
            options,
            false,
            &mut state.killers,
            &mut events,
            (
                |mutant, killers| {
                    calls += 1;
                    if calls == 1 {
                        let mut phases: Vec<_> = (0..4)
                            .map(|_| {
                                active
                                    .recv_timeout(Duration::from_secs(10))
                                    .expect("all four workers become active")
                            })
                            .collect();
                        phases.sort_unstable();
                        assert_eq!(phases, [2, 3, 4, 5]);
                    } else {
                        assert_eq!(
                            active.recv_timeout(Duration::from_secs(10)).expect("the released worker resumes"),
                            6
                        );
                    }
                    let persisted = if calls == 1 {
                        journal.publish(mutant, killers)
                    } else {
                        failing.publish(mutant, killers)
                    };
                    *release.0.lock().unwrap() = if calls == 1 { 2 } else { usize::MAX };
                    release.1.notify_all();
                    persisted
                },
                |_| {
                    let phase = claimed.fetch_add(1, Ordering::SeqCst) + 1;
                    if phase >= 2 {
                        running.send(phase).unwrap();
                        let (_guard, deadline) = release
                            .1
                            .wait_timeout_while(release.0.lock().unwrap(), Duration::from_secs(10), |released| *released < phase)
                            .unwrap();
                        assert!(!deadline.timed_out(), "the coordinator releases each active worker");
                    }
                    Some(Judgement::Reached(Outcome::NoCoverage, None, None))
                },
                || {},
            ),
        )
        .unwrap_err();
        assert!(failure.to_string().contains("could not persist testing progress"));
        assert_eq!(calls, 2, "the coordinator must not attempt later checkpoints");
        assert_eq!(events.mutants, 1, "failed or later verdicts must not be published");
        let committed = fs::read(base.join(JOURNAL)).unwrap();
        let (mut recovered, length) = TestingState::load(base).unwrap();
        assert_eq!(length, committed.len() as u64);
        assert_eq!(
            recovered
                .plan
                .mutants
                .iter()
                .filter(|mutant| mutant.outcome != Outcome::Pending)
                .count(),
            1
        );
        assert_eq!(
            recovered
                .plan
                .mutants
                .iter()
                .filter(|mutant| mutant.outcome == Outcome::Pending)
                .count(),
            39
        );
        let journal = TestingJournal::open(base, Some(length)).unwrap();
        let reach = Reachability::build(&recovered.plan, &binaries, &scope);
        sweep::test_all_with_hooks(
            &work,
            &mut recovered.plan,
            &reach,
            options,
            false,
            &mut recovered.killers,
            &mut crate::testing::Recorder::default(),
            (
                |mutant, killers| journal.publish(mutant, killers),
                |_| Some(Judgement::Reached(Outcome::NoCoverage, None, None)),
                || {},
            ),
        )
        .unwrap();
        assert!(fs::read(base.join(JOURNAL)).unwrap().starts_with(&committed));
        let (finished, _) = TestingState::load(base).unwrap();
        assert!(finished.plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::NoCoverage));
    }

    fn failure(base: &Utf8Path) -> String {
        match TestingState::load(base) {
            Ok(_) => panic!("invalid state was accepted"),
            Err(failure) => failure.to_string(),
        }
    }

    #[test]
    fn checkpoint_identity_and_source_paths_are_validated_independently() {
        for case in [
            "jobs",
            "relative-root",
            "foreign-base",
            "duplicate-id",
            "duplicate-ordinal",
            "pending-zero",
        ] {
            let (_directory, mut state) = fixture();
            let base = state.record.base.clone();
            match case {
                "jobs" => state.settings.jobs = 0,
                "relative-root" => state.plan.root = "relative".into(),
                "foreign-base" => state.record.base = state.plan.root.join("foreign"),
                "duplicate-id" => state.plan.mutants[1].id = state.plan.mutants[0].id.clone(),
                "duplicate-ordinal" => state.plan.mutants[1].ordinal = state.plan.mutants[0].ordinal,
                "pending-zero" => state.plan.mutants[0].ordinal = 0,
                _ => unreachable!(),
            }
            publication::write(&base.join(SNAPSHOT), &serde_json::to_string(&state).unwrap()).unwrap();
            set_phase(&base, Phase::Testing).unwrap();
            let category = if case.starts_with("duplicate") || case == "pending-zero" {
                "ambiguous mutant identities"
            } else {
                "invalid campaign identity or settings"
            };
            assert_eq!(
                failure(&base),
                format!("testing state at `{base}` has {category}; start a new campaign"),
                "{case}"
            );
        }
        for invalid in ["", "../outside.rs", "src/../../outside.rs"] {
            for mutant_path in [false, true] {
                let (_directory, mut state) = fixture();
                if mutant_path {
                    state.plan.mutants[1].file = Utf8PathBuf::from(invalid).into();
                } else {
                    let mut valid = state.plan.files[0].clone();
                    valid.path = invalid.into();
                    state.plan.files.push(valid);
                }
                save(&state);
                assert_eq!(
                    failure(&state.record.base),
                    format!(
                        "testing state at `{}` contains a source path outside the workspace",
                        state.record.base
                    ),
                    "{invalid:?}, mutant={mutant_path}"
                );
            }
        }
        let (_directory, mut state) = fixture();
        let mut ignored = state.plan.mutants[2].clone();
        ignored.id = "another-ignored".into();
        state.plan.mutants.push(ignored);
        save(&state);
        assert_eq!(TestingState::load(&state.record.base).unwrap().0.plan.mutants.len(), 4);
    }

    #[test]
    fn journal_replay_restores_all_verdict_fields_and_rejects_unfinished_commits() {
        let (_directory, state) = fixture();
        let mut mutant = state.plan.mutants[0].clone();
        mutant.outcome = Outcome::Killed;
        mutant.elapsed_ms = 731;
        mutant.killed_by = Some("suite::regression".into());
        mutant.killer = Some(crate::model::KillerIdentity {
            package: "subject".into(),
            target: "lib:subject".into(),
            test: "suite::regression".into(),
        });
        mutant.note = Some("retained evidence".into());
        let journal = TestingJournal::open(&state.record.base, None).unwrap();
        journal.publish(&mutant, &state.killers).unwrap();
        let bytes = fs::read(state.record.base.join(JOURNAL)).unwrap();
        let (loaded, valid) = TestingState::load(&state.record.base).unwrap();
        assert_eq!(valid, bytes.len() as u64);
        let restored = &loaded.plan.mutants[0];
        assert_eq!(restored.outcome, Outcome::Killed);
        assert_eq!(restored.elapsed_ms, 731);
        assert_eq!(restored.killed_by, mutant.killed_by);
        assert_eq!(restored.killer, mutant.killer);
        assert_eq!(restored.note, mutant.note);
        drop(journal);
        mutant.outcome = Outcome::Pending;
        let verdict = JournalEntry::Verdict {
            mutant,
            probe: None,
            sweep: None,
            diagnostic: None,
        };
        let verdict = verdict.wire();
        let encoded = serde_json::to_vec(&verdict).unwrap();
        let mut bytes = serde_json::to_vec(&Committed {
            verdict,
            checksum: blake3::hash(&encoded).to_hex().to_string(),
        })
        .unwrap();
        bytes.push(b'\n');
        fs::write(state.record.base.join(JOURNAL), bytes).unwrap();
        assert_eq!(
            failure(&state.record.base),
            format!(
                "testing verdicts `{}` contain conflicting or unfinished outcomes",
                state.record.base.join(JOURNAL)
            )
        );
    }

    #[test]
    fn partial_records_refuse_input_snapshots_that_do_not_contain_the_retained_sources() {
        let (_directory, mut state) = fixture();
        let mut inputs = serde_json::to_value(&state.record.inputs).unwrap();
        inputs["files"] = serde_json::json!([]);
        state.record.inputs = serde_json::from_value(inputs).unwrap();
        save(&state);
        let Err(failure) = partial_testing_record(&state.record.base) else {
            panic!("partial evidence cannot pair source outcomes with a missing source generation")
        };
        assert_eq!(
            failure.message(),
            format!(
                "testing state at `{}` does not match its captured source generation",
                state.record.base
            )
        );
    }

    #[test]
    fn prepared_tree_roots_include_later_directories_and_propagate_late_walk_failures() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().canonicalize().unwrap()).unwrap();
        let standalone = root.join("standalone");
        let nested = root.join("nested");
        fs::write(&standalone, "first file").unwrap();
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("later"), "later file").unwrap();
        let output = root.join("target");
        let paths = tree_paths(&[standalone.clone(), nested.clone()], &output).unwrap();
        assert_eq!(paths, crate::HashSet::from_iter([standalone.clone(), nested.join("later")]));
        let mut file_walks = 0;
        let paths = tree_paths_with_entries(core::slice::from_ref(&standalone), &output, |_entry| {
            file_walks += 1;
            Err::<walkdir::DirEntry, _>(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unnecessary file reinspection",
            ))
        })
        .expect("an already inspected file root does not need a second, fallible tree traversal");
        assert_eq!(file_walks, 0);
        assert_eq!(paths, crate::HashSet::from_iter([standalone]));
        let mut entries = 0;
        let failure = tree_paths_with_entries(core::slice::from_ref(&nested), &output, |entry| {
            entries += 1;
            let entry = entry.map_err(std::io::Error::other)?;
            if entry.depth() == 1 {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "late walk denial"))
            } else {
                Ok(entry)
            }
        })
        .expect_err("the root was inspected successfully before its child enumeration failed");
        assert_eq!(entries, 2);
        assert_eq!(failure.message(), format!("could not inspect prepared testing tree `{nested}`"));
        assert_eq!(fs::read_to_string(nested.join("later")).unwrap(), "later file");
        let missing = root.join("missing-artifact-root");
        let failure =
            tree_paths(core::slice::from_ref(&missing), &output).expect_err("the tree collector cannot inspect a missing artifact root");
        assert_eq!(failure.message(), format!("could not inspect prepared testing tree `{missing}`"));
    }

    #[test]
    fn artifact_capture_propagates_both_library_directory_and_late_entry_failures() {
        for entry_failure in [false, true] {
            let (_directory, state, mut built) = prepared_start();
            let libraries = state.plan.root.join("libraries");
            fs::create_dir(&libraries).unwrap();
            built.work.libraries = vec![libraries.clone()];
            let mut calls = 0;
            let Err(failure) = capture_artifacts_with(&built, |directory| {
                calls += 1;
                assert_eq!(directory, libraries.as_path());
                let failure = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "scripted library enumeration");
                if entry_failure { Ok(vec![Err(failure)]) } else { Err(failure) }
            }) else {
                panic!("an incomplete dynamic-library inventory cannot become a resumable campaign")
            };
            assert_eq!(calls, 1);
            assert_eq!(failure.message(), format!("could not inspect test libraries `{libraries}`"));
        }
    }

    #[test]
    fn prepared_tree_and_external_library_inventory_reject_native_non_utf8_names() {
        let name = native_non_utf8_name();
        let (_directory, state, mut built) = prepared_start();
        let libraries = state.plan.root.join("external-libraries");
        fs::create_dir(&libraries).unwrap();
        fs::write(libraries.as_std_path().join(&name), "library").unwrap();
        built.work.libraries = vec![libraries.clone()];
        let Err(failure) = capture_artifacts(&built) else {
            panic!("native library names must be representable in the persisted UTF-8 inventory")
        };
        assert_eq!(failure.message(), "test library path is not UTF-8");
        let failure = tree_paths(core::slice::from_ref(&libraries), &built.work.root.join("target"))
            .expect_err("prepared source names must also be representable in the persisted inventory");
        assert_eq!(
            failure.message(),
            format!("prepared testing tree `{libraries}` contains a non-UTF-8 path")
        );
    }

    #[test]
    fn state_readers_preserve_the_first_failure_and_never_read_past_the_size_sentinel() {
        struct FailsAtEnd(std::io::Cursor<&'static [u8]>);
        impl Read for FailsAtEnd {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.position() == u64::try_from(self.0.get_ref().len()).unwrap() {
                    Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "scripted terminal read"))
                } else {
                    self.0.read(buf)
                }
            }
        }
        let path = Utf8Path::new("bounded-state.json");
        let failure = read_json_from::<serde_json::Value>(path, FailsAtEnd(std::io::Cursor::new(b"null")), 3)
            .expect_err("the first excess byte proves oversize without requesting another byte");
        assert_eq!(
            failure.message(),
            format!("testing state `{path}` exceeds the supported size; start a new campaign")
        );
        let failure = read_json_from::<serde_json::Value>(path, FailsAtEnd(std::io::Cursor::new(b"null")), 5)
            .expect_err("the terminal reader failure occurs before reaching the sentinel");
        assert_eq!(failure.message(), format!("could not read testing state `{path}`"));
        let failure = fingerprint_from(path, FailsAtEnd(std::io::Cursor::new(b"artifact")))
            .expect_err("an incomplete artifact hash must not be returned after an I/O error");
        assert_eq!(failure.message(), format!("could not read prepared artifact `{path}`"));
        assert_eq!(
            fingerprint_from(path, std::io::Cursor::new(b"artifact")).unwrap(),
            blake3::hash(b"artifact").to_hex().to_string()
        );
        let mut reader = BufReader::with_capacity(4, FailsAtEnd(std::io::Cursor::new(b"null")));
        let mut line = Vec::new();
        assert_eq!(read_verdict_line(path, &mut reader, &mut line, 3).unwrap(), 4);
        assert_eq!(line, b"null");
        let mut reader = BufReader::with_capacity(4, FailsAtEnd(std::io::Cursor::new(b"null")));
        line.clear();
        let failure =
            read_verdict_line(path, &mut reader, &mut line, 5).expect_err("the incomplete journal line terminates with a real read error");
        assert_eq!(failure.message(), format!("could not read testing verdicts `{path}`"));
    }

    #[test]
    fn finishing_a_record_changes_only_the_finished_testing_phase_and_preserves_phase_errors() {
        for (initial, expected) in [
            (Phase::Preparing, Phase::Preparing),
            (Phase::Testing, Phase::Testing),
            (Phase::FinishedTesting, Phase::Completed),
            (Phase::Completed, Phase::Completed),
        ] {
            let (_directory, state) = fixture();
            let base = &state.record.base;
            set_phase(base, initial).unwrap();
            let snapshot = fs::read(base.join(SNAPSHOT)).unwrap();
            finish_testing_record(base).unwrap();
            assert_eq!(phase(base).unwrap(), Some(expected));
            assert_eq!(fs::read(base.join(SNAPSHOT)).unwrap(), snapshot);
        }
        let (_directory, state) = fixture();
        let base = &state.record.base;
        fs::write(base.join(PHASE), "not JSON").unwrap();
        let failure = finish_testing_record(base).expect_err("a corrupt phase must not be overwritten by completion");
        assert_eq!(
            failure.message(),
            format!("testing state `{}` is invalid; start a new campaign", base.join(PHASE))
        );
        let Err(failure) = TestingState::load(base) else {
            panic!("loading must preserve the corrupt phase error")
        };
        assert_eq!(
            failure.message(),
            format!("testing state `{}` is invalid; start a new campaign", base.join(PHASE))
        );
        fs::remove_file(base.join(SNAPSHOT)).unwrap();
        fs::create_dir(base.join(SNAPSHOT)).unwrap();
        let failure = set_phase(base, Phase::Testing).expect_err("a directory cannot be fingerprinted as a snapshot");
        assert!(
            failure
                .message()
                .starts_with(&format!("could not read prepared artifact `{}`", base.join(SNAPSHOT))),
            "{failure}"
        );
    }

    #[test]
    fn checkpoint_and_partial_record_loaders_propagate_invalid_phase_and_snapshot_documents() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        fs::write(base.join(PHASE), "not JSON").unwrap();
        let Err(failure) = TestingState::read_checkpoint(base) else {
            panic!("checkpoint recovery must parse its phase")
        };
        assert_eq!(
            failure.message(),
            format!("testing state `{}` is invalid; start a new campaign", base.join(PHASE))
        );
        fs::write(base.join(SNAPSHOT), "not JSON").unwrap();
        set_phase(base, Phase::Testing).unwrap();
        let Err(failure) = TestingState::read_checkpoint(base) else {
            panic!("a matching checksum does not make malformed JSON valid")
        };
        assert_eq!(
            failure.message(),
            format!("testing state `{}` is invalid; start a new campaign", base.join(SNAPSHOT))
        );
        let Err(failure) = partial_testing_record(base) else {
            panic!("partial reporting must propagate snapshot recovery failure")
        };
        assert_eq!(
            failure.message(),
            format!("testing state `{}` is invalid; start a new campaign", base.join(SNAPSHOT))
        );
    }

    #[test]
    fn journal_recovery_distinguishes_unknown_ordinals_empty_records_and_invalid_repair_lengths() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        let path = base.join(JOURNAL);
        let mut unknown = state.plan.mutants[0].clone();
        unknown.ordinal = 99;
        unknown.outcome = Outcome::Survived;
        let journal = TestingJournal::open(base, None).unwrap();
        journal.publish(&unknown, &state.killers).unwrap();
        drop(journal);
        let Err(failure) = TestingState::read_checkpoint(base) else {
            panic!("a checksummed verdict cannot invent a new ordinal")
        };
        assert_eq!(failure.message(), format!("testing verdicts `{path}` name an unknown mutant"));
        fs::write(&path, "\n").unwrap();
        let Err(failure) = TestingState::read_checkpoint(base) else {
            panic!("an empty committed line is not end-of-file")
        };
        assert_eq!(
            failure.message(),
            format!("testing verdicts `{path}` are corrupt; start a new campaign")
        );
        fs::write(&path, "unchanged").unwrap();
        let failure = TestingJournal::open(base, Some(u64::MAX)).expect_err("u64::MAX cannot be represented as a native file length");
        assert_eq!(failure.message(), format!("could not discard the interrupted verdict in `{path}`"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "unchanged");
        fs::remove_file(&path).unwrap();
        for repair in [None, Some(0)] {
            let failure = TestingJournal::open(base, repair).expect_err("recovery does not invent a missing journal");
            assert_eq!(
                failure.message(),
                if repair.is_some() {
                    format!("could not reopen testing verdicts `{path}` for recovery")
                } else {
                    format!("could not open testing verdicts `{path}`")
                }
            );
            assert!(!path.exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn finishing_a_record_preserves_the_finished_phase_when_its_replacement_is_denied() {
        const SHARE_READ: u32 = 1;
        let (_directory, state) = fixture();
        let base = &state.record.base;
        set_phase(base, Phase::FinishedTesting).unwrap();
        let path = base.join(PHASE);
        let lock = OpenOptions::new().read(true).share_mode(SHARE_READ).open(&path).unwrap();
        let failure = finish_testing_record(base).expect_err("reading succeeds but replacing the phase is denied by the shared handle");
        assert_eq!(failure.message(), format!("could not write `{path}`"));
        drop(lock);
        assert_eq!(phase(base).unwrap(), Some(Phase::FinishedTesting));
    }

    #[test]
    fn bounded_json_accepts_exact_capacity_but_rejects_a_valid_prefix_with_one_excess_byte() {
        let path = Utf8Path::new("bounded.json");
        assert_eq!(read_json_from::<u32>(path, std::io::Cursor::new(b"123"), 3).unwrap(), 123);
        assert_eq!(read_json_from::<u32>(path, std::io::Cursor::new(b"123"), 4).unwrap(), 123);
        for bytes in [b"123 ".as_slice(), b"1234".as_slice()] {
            let failure = read_json_from::<u32>(path, std::io::Cursor::new(bytes), 3).unwrap_err();
            assert_eq!(
                failure.message(),
                format!("testing state `{path}` exceeds the supported size; start a new campaign")
            );
        }
        let failure = read_json_from::<u32>(path, std::io::Cursor::new(b"0"), 0).unwrap_err();
        assert_eq!(
            failure.message(),
            format!("testing state `{path}` exceeds the supported size; start a new campaign")
        );
        assert_eq!(read_json_from::<u32>(path, std::io::Cursor::new(b"0"), 1).unwrap(), 0);
    }

    #[test]
    fn the_native_json_reader_rejects_a_directory() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = Utf8Path::from_path(directory.path()).unwrap().join("bounded.json");
        let failure = read_json_with_limit::<u32>(path.parent().unwrap(), 3).expect_err("a directory cannot be read as a JSON state file");
        assert!(failure.message().starts_with("could not read testing state"));
    }

    #[test]
    fn journal_line_limit_counts_the_committing_newline_and_rejects_oversized_uncommitted_prefixes() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        let mut mutant = state.plan.mutants[0].clone();
        mutant.outcome = Outcome::Survived;
        let journal = TestingJournal::open(base, None).unwrap();
        journal.publish(&mutant, &state.killers).unwrap();
        drop(journal);
        let bytes = fs::read(base.join(JOURNAL)).unwrap();
        let limit = bytes.len() as u64;
        let (loaded, valid) = TestingState::read_checkpoint_with_limit(base, limit).unwrap();
        assert_eq!(valid, limit);
        assert_eq!(loaded.plan.mutants[0].outcome, Outcome::Survived);
        let failure = TestingState::read_checkpoint_with_limit(base, limit - 1).err().unwrap();
        assert_eq!(
            failure.message(),
            format!("testing verdict in `{}` exceeds the supported size", base.join(JOURNAL))
        );
        fs::write(base.join(JOURNAL), b"1234").unwrap();
        let failure = TestingState::read_checkpoint_with_limit(base, 3).err().unwrap();
        assert_eq!(
            failure.message(),
            format!("testing verdict in `{}` exceeds the supported size", base.join(JOURNAL))
        );
    }

    #[test]
    fn phase_io_failures_are_not_misclassified_as_missing_campaigns() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let base = Utf8Path::from_path(directory.path()).unwrap();
        assert_eq!(phase(base).unwrap(), None);
        fs::write(base.join(PHASE), "invalid").unwrap();
        assert!(phase(base).unwrap_err().message().contains("is invalid"));
        fs::remove_file(base.join(PHASE)).unwrap();
        let invalid_base = base.join("invalid\0parent");
        let invalid_phase = invalid_base.join(PHASE);
        assert_eq!(
            fs::symlink_metadata(&invalid_phase).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let failure = phase(&invalid_base).expect_err("an invalid phase path is an I/O error, not a missing campaign");
        assert_eq!(failure.message(), format!("could not inspect testing phase `{invalid_phase}`"));
        assert_eq!(
            failure.source().unwrap().downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let failure = set_phase(base, Phase::Testing).expect_err("a testing phase requires a prepared snapshot");
        assert!(failure.message().contains("testing-session.json"));
        let failure = read_json::<serde_json::Value>(&base.join("missing")).expect_err("a missing state file cannot be read");
        assert!(failure.message().starts_with("could not read testing state"));
    }

    #[test]
    fn failed_append_and_sync_restore_exactly_the_durable_prefix() {
        use core::error::Error as _;
        use std::cell::{Cell, RefCell};
        for fail_write in [false, true] {
            let prefix = b"durable\n".to_vec();
            let stored = RefCell::new(prefix.clone());
            let synced = Cell::new(0);
            let error = append_checkpoint(
                b"next-verdict\n",
                |bytes| {
                    let appended = if fail_write { &bytes[..4] } else { bytes };
                    stored.borrow_mut().extend_from_slice(appended);
                    if fail_write {
                        Err(std::io::Error::from(std::io::ErrorKind::WriteZero))
                    } else {
                        Ok(())
                    }
                },
                || {
                    synced.set(synced.get() + 1);
                    if !fail_write && synced.get() == 1 {
                        Err(std::io::Error::from(std::io::ErrorKind::Other))
                    } else {
                        Ok(())
                    }
                },
                || {
                    stored.borrow_mut().truncate(prefix.len());
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(*stored.borrow(), prefix);
            assert_eq!(synced.get(), if fail_write { 1 } else { 2 });
            assert_eq!(error.message(), "could not persist testing progress; the campaign has stopped");
            assert_eq!(
                error.source().unwrap().downcast_ref::<std::io::Error>().unwrap().kind(),
                if fail_write {
                    std::io::ErrorKind::WriteZero
                } else {
                    std::io::ErrorKind::Other
                }
            );
            append_checkpoint(
                b"retried\n",
                |bytes| {
                    stored.borrow_mut().extend_from_slice(bytes);
                    Ok(())
                },
                || Ok(()),
                || panic!("a successful checkpoint must not roll back"),
            )
            .unwrap();
            assert_eq!(*stored.borrow(), b"durable\nretried\n");
        }
    }

    #[test]
    fn checkpoint_rollback_failures_are_explicit() {
        use core::error::Error as _;
        use std::cell::Cell;
        for fail_truncate in [false, true] {
            let synced = Cell::new(0);
            let error = append_checkpoint(
                b"verdict\n",
                |_| Ok(()),
                || {
                    synced.set(synced.get() + 1);
                    Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "sync denied"))
                },
                || {
                    if fail_truncate {
                        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "truncate denied"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert_eq!(synced.get(), if fail_truncate { 1 } else { 2 });
            assert_eq!(
                error.message(),
                "could not persist testing progress (sync denied) or restore the durable journal prefix; restore the journal before resuming"
            );
            assert_eq!(
                error.source().unwrap().to_string(),
                if fail_truncate { "truncate denied" } else { "sync denied" }
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and a filesystem-backed verdict journal")]
    fn resumed_confidence_matches_the_frozen_campaign_without_changing_public_mutant_json() {
        let (_directory, mut state) = fixture();
        let expected = [Confidence::Optimistic, Confidence::Explicit, Confidence::Proven];
        for (mutant, confidence) in state.plan.mutants.iter_mut().zip(expected) {
            mutant.confidence = confidence;
        }
        state.confidences = frozen_confidences(&state.plan);
        save(&state);
        let mut settled = state.plan.mutants[0].clone();
        settled.outcome = Outcome::Killed;
        TestingJournal::open(&state.record.base, None)
            .unwrap()
            .publish(&settled, &state.killers)
            .unwrap();
        let (loaded, _) = TestingState::load(&state.record.base).unwrap();
        assert_eq!(
            loaded.plan.mutants.iter().map(|mutant| mutant.confidence).collect::<Vec<_>>(),
            expected
        );
        assert!(serde_json::to_value(&settled).unwrap().get("confidence").is_none());
        state.confidences.insert(state.plan.mutants[0].id.clone(), "invalid".into());
        save(&state);
        assert_eq!(
            failure(&state.record.base),
            format!(
                "testing state at `{}` contains invalid mutant confidence; start a new campaign",
                state.record.base
            )
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and captured library artifacts")]
    fn captured_target_libraries_require_unchanged_bytes_even_in_legacy_snapshots() {
        for extension in ["dll", "so", "dylib"] {
            let (_directory, mut state) = fixture();
            let directory = state.artifact_roots[0].join("target/debug/deps");
            fs::create_dir_all(&directory).unwrap();
            let library = directory.join(format!("subject.{extension}"));
            fs::write(&library, "prepared library").unwrap();
            state.workspace["libraries"] = serde_json::json!([directory]);
            state.artifacts.push(Artifact {
                generated_output: None,
                path: library.clone(),
                digest: fingerprint(&library).unwrap(),
                metadata: None,
            });
            save(&state);
            let (loaded, _) = TestingState::load(&state.record.base).unwrap();
            loaded.validate_artifacts().unwrap();
            fs::write(&library, "changed library").unwrap();
            assert_eq!(
                loaded.validate_artifacts().unwrap_err().message(),
                format!(
                    "cannot resume testing: prepared artifact `{library}` changed; restore the campaign artifacts or start a new campaign"
                )
            );
            fs::remove_file(&library).unwrap();
            assert_eq!(
                loaded.validate_artifacts().unwrap_err().message(),
                format!("could not inspect prepared artifact `{library}`; restore it or start a new campaign")
            );
        }
    }

    #[test]
    fn metadata_capture_uses_scripted_types_permissions_and_symlink_targets() {
        use std::cell::{Cell, RefCell};
        let path = Utf8Path::new("prepared");
        let regular = ArtifactMetadata::read_with(
            path,
            |follow| {
                assert!(!follow, "regular artifacts must not follow a link");
                Ok((ArtifactKind::File, 0o101))
            },
            || panic!("regular artifacts must not read a link"),
        )
        .unwrap();
        assert_eq!(
            regular,
            ArtifactMetadata {
                kind: ArtifactKind::File,
                executable: 0o101,
                link: None
            }
        );
        for (target, expected, bits) in [
            (Ok((ArtifactKind::File, 0o100)), ArtifactKind::FileSymlink, 0o100),
            (Ok((ArtifactKind::Other, 0o111)), ArtifactKind::Symlink, 0),
            (Err(std::io::Error::from(std::io::ErrorKind::NotFound)), ArtifactKind::Symlink, 0),
        ] {
            let target = RefCell::new(Some(target));
            let calls = Cell::new(0);
            let metadata = ArtifactMetadata::read_with(
                path,
                |follow| {
                    calls.set(calls.get() + 1);
                    if follow {
                        target.borrow_mut().take().expect("the target is inspected only once")
                    } else {
                        Ok((ArtifactKind::Symlink, 0o111))
                    }
                },
                || Ok("..".into()),
            )
            .unwrap();
            assert_eq!(calls.get(), 2);
            assert_eq!(
                metadata,
                ArtifactMetadata {
                    kind: expected,
                    executable: bits,
                    link: Some("..".into())
                }
            );
        }
    }

    #[test]
    fn metadata_capture_surfaces_inspection_and_link_errors() {
        use core::error::Error as _;
        let path = Utf8Path::new("prepared");
        for symlink in [false, true] {
            let error = ArtifactMetadata::read_with(
                path,
                |follow| {
                    if symlink && !follow {
                        Ok((ArtifactKind::Symlink, 0))
                    } else {
                        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected denial"))
                    }
                },
                || panic!("failed inspection must not read the link"),
            )
            .unwrap_err();
            assert_eq!(
                error.source().unwrap().downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                error.message(),
                if symlink {
                    "could not inspect prepared symlink target `prepared`; restore it or start a new campaign"
                } else {
                    "could not inspect prepared artifact `prepared`; restore it or start a new campaign"
                }
            );
        }
        let error = ArtifactMetadata::read_with(
            path,
            |follow| Ok((if follow { ArtifactKind::File } else { ArtifactKind::Symlink }, 0)),
            || Err(error!("injected link failure")),
        )
        .unwrap_err();
        assert_eq!(error.message(), "injected link failure");
    }

    #[test]
    fn metadata_validation_rejects_changed_executable_bits_types_and_symlink_targets() {
        let expected = ArtifactMetadata {
            kind: ArtifactKind::File,
            executable: 0o100,
            link: None,
        };
        let actual = ArtifactMetadata {
            kind: ArtifactKind::File,
            executable: 0,
            link: None,
        };
        assert!(!artifact_metadata_matches(Some(&expected), &actual));
        assert!(artifact_metadata_matches(None, &actual));
        for kind in [ArtifactKind::Directory, ArtifactKind::Symlink, ArtifactKind::Other] {
            let actual = ArtifactMetadata {
                kind,
                executable: 0o100,
                link: None,
            };
            assert!(!artifact_metadata_matches(Some(&expected), &actual));
            assert!(!artifact_metadata_matches(None, &actual));
        }
        let first = ArtifactMetadata {
            kind: ArtifactKind::FileSymlink,
            executable: 0o100,
            link: Some("one".into()),
        };
        let second = ArtifactMetadata {
            kind: ArtifactKind::FileSymlink,
            executable: 0o100,
            link: Some("two".into()),
        };
        assert!(!artifact_metadata_matches(Some(&first), &second));
        assert!(artifact_metadata_matches(Some(&first), &first));
    }

    #[test]
    fn directory_symlinks_are_rejected_before_reading_or_walking_the_target() {
        let failure = ArtifactMetadata::read_with(
            Utf8Path::new("prepared"),
            |follow| Ok((if follow { ArtifactKind::Directory } else { ArtifactKind::Symlink }, 0)),
            || panic!("directory symlinks must be rejected before reading their target spelling"),
        )
        .unwrap_err();
        assert_eq!(
            failure.message(),
            "prepared directory symlink `prepared` cannot be frozen for resumable testing; replace it with a real directory and start a new campaign"
        );
    }

    #[test]
    fn dangling_symlink_digest_does_not_open_its_target() {
        let metadata = ArtifactMetadata {
            kind: ArtifactKind::Symlink,
            executable: 0,
            link: Some("..".into()),
        };
        let digest = artifact_digest(Utf8Path::new("must-not-be-opened"), Some(&metadata)).unwrap();
        assert_eq!(digest, blake3::hash(b"..").to_hex().to_string());
        let directory = ArtifactMetadata {
            kind: ArtifactKind::Directory,
            executable: 0,
            link: None,
        };
        assert_eq!(
            artifact_digest(Utf8Path::new("must-not-be-opened"), Some(&directory))
                .unwrap_err()
                .to_string(),
            "prepared artifact `must-not-be-opened` is not a regular file; restore it or start a new campaign"
        );
    }

    #[test]
    fn legacy_directory_symlinks_are_not_accepted_as_frozen_artifacts() {
        let link = Utf8PathBuf::from("prepared/src/cycle");
        let expected = format!(
            "prepared directory symlink `{link}` cannot be frozen for resumable testing; \
             replace it with a real directory and start a new campaign"
        );
        for metadata in [
            None,
            Some(ArtifactMetadata {
                kind: ArtifactKind::Symlink,
                executable: 0,
                link: Some("..".into()),
            }),
        ] {
            let artifact = Artifact {
                path: link.clone(),
                digest: blake3::hash(b"..").to_hex().to_string(),
                generated_output: Some(false),
                metadata,
            };
            let failure = validate_artifact(
                &artifact,
                |path| {
                    assert_eq!(path, link);
                    ArtifactMetadata::read_with(
                        path,
                        |follow| Ok((if follow { ArtifactKind::Directory } else { ArtifactKind::Symlink }, 0)),
                        || panic!("ancestor links must be rejected before reading or traversing the target"),
                    )
                },
                |_, _| panic!("legacy metadata must not bypass inspection and read directory contents"),
            )
            .unwrap_err();
            assert_eq!(failure.message(), expected);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem artifact validation")]
    fn snapshots_without_artifact_metadata_remain_resumable() {
        let (_directory, mut state) = fixture();
        state.artifacts[0].metadata = None;
        save(&state);
        let (loaded, _) = TestingState::load(&state.record.base).unwrap();
        assert!(loaded.artifacts[0].metadata.is_none());
        loaded.validate_artifacts().unwrap();
    }

    fn scripted_sweep(state: &mut TestingState, checkpoint: impl FnMut(&Mutant, &Killers) -> Result<()>, first: Outcome) -> Result<usize> {
        let mut checkpoint = checkpoint;
        scripted_sweep_with_cost(state, |mutant, killers, _cost| checkpoint(mutant, killers), first)
    }

    fn scripted_sweep_with_cost(
        state: &mut TestingState,
        checkpoint: impl FnMut(&Mutant, &Killers, &super::super::session::SweepCost) -> Result<()>,
        first: Outcome,
    ) -> Result<usize> {
        use super::super::stall::Stall;
        use super::super::sweep::{self, Judgement, Sweep};
        use super::super::test_binary::{Reachability, TestScope};

        let work = Workspace::adopt(state.plan.root.clone(), state.plan.root.join("target"));
        let binaries = [crate::testing::test_binary("scripted-test-binary")];
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&state.plan, &binaries, &scope);
        let mut events = crate::testing::Recorder::default();
        sweep::test_all_with_accounting_hooks(
            &work,
            &mut state.plan,
            &reach,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: state.settings.confirm,
            },
            false,
            &mut state.killers,
            &mut events,
            (
                checkpoint,
                |ordinal| {
                    let outcome = match ordinal {
                        1 => first,
                        2 => Outcome::Survived,
                        _ => panic!("the coordinator must not test excluded or unspecified mutants"),
                    };
                    let killer = (outcome == Outcome::Killed).then(|| crate::discover::Killer {
                        package: "subject".into(),
                        target: "lib:subject".into(),
                        test: "test_one".into(),
                    });
                    Some(Judgement::Reached(outcome, killer, Some("scripted verdict".into())))
                },
                || panic!("single-worker scripted execution must not wait for resource admission"),
            ),
        )?;
        Ok(events.mutants)
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem-backed verdict journals")]
    fn checkpoints_restore_cumulative_accounting_across_repeated_resumes() {
        use super::super::session::{PackageSweepCost, SweepCost};

        let (_directory, state) = fixture();
        let base = &state.record.base;
        let journal = TestingJournal::open(base, None).unwrap();
        let mut first = state.plan.mutants[0].clone();
        first.outcome = Outcome::Killed;
        let prefix = SweepCost {
            elapsed: Duration::from_millis(100),
            launches: 7,
            probes: 3,
            launches_saved: 2,
            packages: vec![PackageSweepCost {
                package: "subject".into(),
                mutants: 1,
                first_started_ms: 5,
                settled_ms: 90,
            }],
            ..SweepCost::default()
        };
        journal.publish_with_cost(&first, &state.killers, Some(prefix.clone())).unwrap();
        let (mut restored, _) = TestingState::load(base).unwrap();
        assert_eq!(
            serde_json::to_value(&restored.session.phases.sweep).unwrap(),
            serde_json::to_value(Some(&prefix)).unwrap(),
        );
        let prior = restored.session.phases.sweep.clone();
        let mut workspace = super::super::workspace::Workspace::adopt(restored.plan.root.clone(), base.join("target"));
        workspace.arm_testing_journal().unwrap();
        workspace.retain_sweep_cost(prior.clone());
        let mut published = None;
        assert_eq!(
            scripted_sweep_with_cost(
                &mut restored,
                |mutant, killers, segment| {
                    assert_eq!(segment.packages.len(), 1);
                    assert_eq!(segment.packages[0].mutants, 1);
                    let total = SweepCost::combine(prior.clone(), Some(segment.clone()));
                    workspace.checkpoint_mutant(mutant, killers, segment)?;
                    published = total;
                    Ok(())
                },
                Outcome::Killed,
            )
            .unwrap(),
            1,
        );
        let (mut completed, _) = TestingState::load(base).unwrap();
        assert_eq!(
            serde_json::to_value(&completed.session.phases.sweep).unwrap(),
            serde_json::to_value(&published).unwrap(),
        );
        let total = completed.session.phases.sweep.as_ref().unwrap();
        assert_eq!(
            (total.launches, total.probes, total.launches_saved, total.packages[0].mutants),
            (7, 3, 2, 2)
        );
        assert_eq!(
            scripted_sweep_with_cost(
                &mut completed,
                |_, _, _| panic!("a fully settled resume must not publish another checkpoint"),
                Outcome::Killed,
            )
            .unwrap(),
            0,
        );
        assert_eq!(
            serde_json::to_value(completed.session.phases.sweep).unwrap(),
            serde_json::to_value(published).unwrap(),
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "writes and reloads prepared snapshot files")]
    fn report_provenance_round_trips_and_legacy_snapshots_omit_it() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        let (loaded, _) = TestingState::load(base).unwrap();
        assert_eq!(
            serde_json::to_value(loaded.record.report_info).unwrap(),
            serde_json::to_value(&state.record.report_info).unwrap(),
        );
        let mut legacy = serde_json::to_value(&state).unwrap();
        legacy["record"].as_object_mut().unwrap().remove("report_info");
        publication::write(&base.join(SNAPSHOT), &serde_json::to_string(&legacy).unwrap()).unwrap();
        set_phase(base, Phase::Testing).unwrap();
        let (loaded, _) = TestingState::load(base).unwrap();
        assert!(loaded.record.report_info.is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem-backed verdict journals")]
    fn scripted_resume_keeps_the_durable_verdict_and_matches_uninterrupted_testing() {
        for first in [Outcome::Survived, Outcome::Killed, Outcome::Timeout] {
            let (_directory, mut interrupted) = fixture();
            let base = interrupted.record.base.clone();
            let frozen = fs::read(base.join(SNAPSHOT)).unwrap();
            let journal = TestingJournal::open(&base, None).unwrap();
            let mut attempted = 0;
            let error = scripted_sweep(
                &mut interrupted,
                |mutant, killers| {
                    attempted += 1;
                    if attempted == 1 {
                        journal.publish(mutant, killers)
                    } else {
                        Err(error!("scripted interruption before second checkpoint"))
                    }
                },
                first,
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "the run cannot be trusted to judge anything further: scripted interruption before second checkpoint.\n\
                 It stops here rather than reach verdicts that would each have to be taken on faith."
            );
            assert_eq!(attempted, 2);
            let prefix = fs::read(base.join(JOURNAL)).unwrap();
            let (mut resumed, length) = TestingState::load(&base).unwrap();
            assert_eq!(resumed.plan.mutants[0].outcome, first);
            assert_eq!(resumed.plan.mutants[1].outcome, Outcome::Pending);
            assert_eq!(resumed.plan.mutants[2].outcome, Outcome::Ignored);
            let partial = partial_testing_record(&base).unwrap().unwrap();
            assert_eq!(
                partial.outcomes().iter().map(|entry| entry.outcome).collect::<Vec<_>>(),
                [first, Outcome::Pending, Outcome::Ignored]
            );
            let recovered_journal = TestingJournal::open(&base, Some(length)).unwrap();
            assert_eq!(
                scripted_sweep(&mut resumed, |mutant, killers| recovered_journal.publish(mutant, killers), first).unwrap(),
                1
            );
            assert_eq!(fs::read(base.join(SNAPSHOT)).unwrap(), frozen);
            let bytes = fs::read(base.join(JOURNAL)).unwrap();
            assert!(bytes.starts_with(&prefix));
            assert_eq!(bytes.last(), Some(&b'\n'));
            assert_eq!(bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).count(), 2);
            let (finished, _) = TestingState::load(&base).unwrap();

            let (_directory, mut uninterrupted) = fixture();
            let complete_journal = TestingJournal::open(&uninterrupted.record.base, None).unwrap();
            assert_eq!(
                scripted_sweep(
                    &mut uninterrupted,
                    |mutant, killers| complete_journal.publish(mutant, killers),
                    first
                )
                .unwrap(),
                2
            );
            let verdicts = |plan: &Plan| {
                plan.mutants
                    .iter()
                    .map(|mutant| (mutant.id.clone(), mutant.outcome, mutant.killer.clone(), mutant.note.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(verdicts(&finished.plan), verdicts(&uninterrupted.plan));
            assert_eq!(finished.killers.hint("m1"), uninterrupted.killers.hint("m1"));
            TestingJournal::complete(&base).unwrap();
            let (recovered, _) = TestingState::load(&base).unwrap();
            assert_eq!(verdicts(&recovered.plan), verdicts(&finished.plan));
            finish_testing_record(&base).unwrap();
            assert!(failure(&base).contains("not interrupted during testing"));
        }
    }
    #[test]
    #[cfg_attr(miri, ignore = "writes and reloads campaign phase files")]
    fn rejects_absent_preparing_and_completed_phases() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        for value in [Phase::Preparing, Phase::Completed] {
            set_phase(base, value).unwrap();
            assert!(failure(base).contains("not interrupted during testing"));
        }
        fs::remove_file(base.join(PHASE)).unwrap();
        assert!(failure(base).contains("not interrupted during testing"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem-backed verdict journals")]
    fn retains_all_completed_verdicts_and_only_retries_pending_work() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        for outcome in [
            Outcome::Killed,
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::OutOfMemory,
            Outcome::Flaky,
            Outcome::NoCoverage,
        ] {
            save(&state);
            let journal = TestingJournal::open(base, None).unwrap();
            let mut mutant = state.plan.mutants[0].clone();
            mutant.outcome = outcome;
            mutant.elapsed_ms = 4321;
            mutant.note = Some("observed".into());
            let mut killers = Killers::default();
            killers.record(
                mutant.id.clone(),
                crate::discover::Killer {
                    package: "subject".into(),
                    target: "lib:subject".into(),
                    test: "test_one".into(),
                },
            );
            journal.publish(&mutant, &killers).unwrap();
            let (loaded, bytes) = TestingState::load(base).unwrap();
            assert!(bytes > 0);
            assert_eq!(loaded.plan.mutants[0].outcome, outcome);
            assert_eq!(loaded.plan.mutants[0].elapsed_ms, 4321);
            assert_eq!(loaded.plan.mutants[0].note.as_deref(), Some("observed"));
            assert_eq!(loaded.plan.mutants[1].outcome, Outcome::Pending);
            assert_eq!(loaded.plan.mutants[2].outcome, Outcome::Ignored);
            assert_eq!(loaded.killers.hint("m1").unwrap().test, "test_one");
            assert_eq!(loaded.session.baseline, state.session.baseline);
            assert_eq!(loaded.settings.confirm, state.settings.confirm);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "repairs filesystem-backed verdict journals")]
    fn discards_only_a_torn_final_append_and_can_resume_repeatedly() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        let journal = TestingJournal::open(base, None).unwrap();
        let mut first = state.plan.mutants[0].clone();
        first.outcome = Outcome::Survived;
        journal.publish(&first, &state.killers).unwrap();
        let durable = fs::metadata(base.join(JOURNAL)).unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(base.join(JOURNAL))
            .unwrap()
            .write_all(b"{\"mutant\":")
            .unwrap();
        let (loaded, valid_bytes) = TestingState::load(base).unwrap();
        assert_eq!(valid_bytes, durable);
        assert_eq!(loaded.plan.mutants[0].outcome, Outcome::Survived);
        assert_eq!(loaded.plan.mutants[1].outcome, Outcome::Pending);
        drop(journal);
        let resumed = TestingJournal::open(base, Some(valid_bytes)).unwrap();
        assert_eq!(fs::metadata(base.join(JOURNAL)).unwrap().len(), durable);
        let mut second = state.plan.mutants[1].clone();
        second.outcome = Outcome::Timeout;
        resumed.publish(&second, &state.killers).unwrap();
        let (loaded, _) = TestingState::load(base).unwrap();
        assert_eq!(loaded.plan.mutants[0].outcome, Outcome::Survived);
        assert_eq!(loaded.plan.mutants[1].outcome, Outcome::Timeout);
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses a filesystem-backed verdict journal")]
    fn unfinished_observations_are_not_committed_as_verdicts() {
        let (_directory, state) = fixture();
        TestingJournal::open(&state.record.base, None)
            .unwrap()
            .publish(&state.plan.mutants[0], &state.killers)
            .unwrap();
        let (loaded, length) = TestingState::load(&state.record.base).unwrap();
        assert_eq!(length, 0);
        assert_eq!(loaded.plan.mutants[0].outcome, Outcome::Pending);
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem-backed verdict journals")]
    fn finished_sweeps_with_unjudged_mutants_remain_resumable_until_ledger_publication() {
        for publication_failed in [false, true] {
            let (_directory, mut state) = fixture();
            let base = state.record.base.clone();
            let journal = TestingJournal::open(&base, None).unwrap();
            assert_eq!(
                scripted_sweep(&mut state, |mutant, killers| journal.publish(mutant, killers), Outcome::Pending).unwrap(),
                2
            );
            assert_eq!(state.plan.mutants[0].outcome, Outcome::Pending);
            assert_eq!(state.plan.mutants[1].outcome, Outcome::Survived);
            TestingJournal::complete(&base).unwrap();
            drop(journal);
            if publication_failed {
                let ledger = base.join("last-gamma-run.json");
                fs::create_dir(&ledger).unwrap();
                let record = partial_testing_record(&base).unwrap().unwrap();
                let failure = record
                    .store_completed(&base)
                    .expect_err("a directory must refuse final ledger replacement");
                assert!(failure.message().contains(ledger.as_str()), "{failure}");
                fs::remove_dir(&ledger).unwrap();
            }

            let (mut recovered, valid_bytes) = TestingState::load(&base).unwrap();
            assert!(!recovered.is_settled());
            assert_eq!(recovered.plan.mutants[0].outcome, Outcome::Pending);
            assert_eq!(recovered.plan.mutants[1].outcome, Outcome::Survived);
            let partial = partial_testing_record(&base).unwrap().unwrap();
            assert_eq!(
                partial.outcomes().iter().filter(|entry| entry.outcome == Outcome::Pending).count(),
                1
            );
            let journal = TestingJournal::open(&base, Some(valid_bytes)).unwrap();
            assert_eq!(
                scripted_sweep(&mut recovered, |mutant, killers| journal.publish(mutant, killers), Outcome::Killed,).unwrap(),
                1,
                "resume must retry only the unjudged mutant"
            );
            assert!(recovered.is_settled());
            assert_eq!(recovered.plan.mutants[1].outcome, Outcome::Survived);
            recovered.complete_testing().unwrap();
            partial_testing_record(&base).unwrap().unwrap().store_completed(&base).unwrap();
            finish_testing_record(&base).unwrap();
            assert_eq!(phase(&base).unwrap(), Some(Phase::Completed));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "publishes a filesystem-backed completed campaign ledger")]
    fn publishing_a_reportable_pending_outcome_still_ends_resumability() {
        let (_directory, mut state) = fixture();
        let base = state.record.base.clone();
        let journal = TestingJournal::open(&base, None).unwrap();
        scripted_sweep(&mut state, |mutant, killers| journal.publish(mutant, killers), Outcome::Pending).unwrap();
        TestingJournal::complete(&base).unwrap();
        let record = partial_testing_record(&base).unwrap().unwrap();
        assert!(record.outcomes().iter().any(|entry| entry.outcome == Outcome::Pending));
        record.store_completed(&base).unwrap();
        finish_testing_record(&base).unwrap();
        assert_eq!(phase(&base).unwrap(), Some(Phase::Completed));
        assert!(failure(&base).contains("not interrupted during testing"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses read-only filesystem-backed verdict journal handles")]
    fn a_checkpoint_write_failure_is_an_error_and_leaves_the_mutant_pending() {
        let (_directory, state) = fixture();
        let journal = TestingJournal {
            file: File::open(state.record.base.join(JOURNAL)).unwrap(),
            previous_sweep: None,
            prior_confirmations: std::collections::BTreeSet::new(),
            initial_hints: None,
            live_refresh_failures: core::sync::atomic::AtomicUsize::new(0),
        };
        let mut mutant = state.plan.mutants[0].clone();
        mutant.outcome = Outcome::Killed;
        assert!(
            journal
                .publish(&mutant, &state.killers)
                .unwrap_err()
                .to_string()
                .contains("could not persist")
        );
        let (loaded, length) = TestingState::load(&state.record.base).unwrap();
        assert_eq!(length, 0);
        assert_eq!(loaded.plan.mutants[0].outcome, Outcome::Pending);
    }

    #[test]
    #[cfg_attr(miri, ignore = "writes and reloads filesystem-backed verdict journals")]
    fn rejects_corrupt_complete_entries_duplicate_verdicts_and_unknown_ids() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        fs::write(base.join(JOURNAL), b"{bad}\n").unwrap();
        assert!(failure(base).contains("corrupt"));
        save(&state);
        let journal = TestingJournal::open(base, None).unwrap();
        let mut mutant = state.plan.mutants[0].clone();
        mutant.outcome = Outcome::Killed;
        journal.publish(&mutant, &state.killers).unwrap();
        journal.publish(&mutant, &state.killers).unwrap();
        assert!(failure(base).contains("conflicting"));
        drop(journal);
        save(&state);
        mutant.id = "foreign".into();
        TestingJournal::open(base, None).unwrap().publish(&mutant, &state.killers).unwrap();
        assert!(failure(base).contains("conflicting"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "writes and reloads prepared snapshot files")]
    fn rejects_incompatible_identity_version_platform_and_missing_snapshot() {
        let (_directory, mut state) = fixture();
        for incompatible in [1, VERSION + 1] {
            state.version = incompatible;
            save(&state);
            assert!(failure(&state.record.base).contains("incompatible"));
        }
        state.version = VERSION;
        state.platform = "foreign".into();
        save(&state);
        assert!(failure(&state.record.base).contains("incompatible"));
        state.platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        state.tool = "old".into();
        save(&state);
        assert!(failure(&state.record.base).contains("incompatible"));
        fs::remove_file(state.record.base.join(SNAPSHOT)).unwrap();
        assert!(failure(&state.record.base).contains("could not read"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and artifact validation")]
    fn rejects_changed_or_missing_prepared_artifacts_but_not_live_checkout_edits() {
        let (_directory, state) = fixture();
        state.validate_artifacts().unwrap();
        fs::write(state.record.base.join("unrelated-live-edit"), "changed").unwrap();
        state.validate_artifacts().unwrap();
        fs::write(&state.artifacts[0].path, "changed").unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("changed"));
        fs::remove_file(&state.artifacts[0].path).unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("restore it"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and generated test files")]
    fn existing_checkpoints_allow_test_created_files_outside_target() {
        let (_directory, state) = fixture();
        let scratch = state.artifact_roots[0].join("crates/cargo-aprz-lib/.tmpkR8ahz");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("empty.toml"), "test-created input").unwrap();
        let (loaded, _) = TestingState::load(&state.record.base).unwrap();
        loaded.validate_artifacts().unwrap();
        fs::write(&loaded.artifacts[0].path, "changed source").unwrap();
        assert!(loaded.validate_artifacts().unwrap_err().to_string().contains("changed"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and generated test files")]
    fn existing_checkpoints_ignore_new_changed_and_removed_test_work() {
        let (_directory, mut state) = fixture();
        let output = state.artifact_roots[0].join("target");
        let old = output.join("test-work/old/src/lib.rs");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, "baseline test scratch").unwrap();
        state.artifacts.push(Artifact {
            generated_output: None,
            path: old.clone(),
            digest: fingerprint(&old).unwrap(),
            metadata: None,
        });
        save(&state);
        fs::write(&old, "mutant test scratch").unwrap();
        let new = output.join("test-work/run-plan-AwBFuu/src/lib.rs");
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&new, "new test scratch").unwrap();
        let (loaded, _) = TestingState::load(&state.record.base).unwrap();
        loaded.validate_artifacts().unwrap();
        fs::remove_file(old).unwrap();
        loaded.validate_artifacts().unwrap();
        assert!(!tree_paths(&loaded.artifact_roots, &output).unwrap().contains(&new));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and generated package output")]
    fn existing_checkpoints_ignore_package_local_target_output() {
        let (_directory, mut state) = fixture();
        let workspace = &state.artifact_roots[0];
        let package = workspace.join("crates/cargo-aprz-lib");
        fs::create_dir_all(&package).unwrap();
        let manifest = package.join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"subject\"\nversion = \"0.1.0\"\n").unwrap();
        let old = package.join("target/baseline-cache");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, "old").unwrap();
        for path in [&manifest, &old] {
            state.artifacts.push(Artifact {
                generated_output: None,
                path: path.clone(),
                digest: fingerprint(path).unwrap(),
                metadata: None,
            });
        }
        save(&state);
        let new = package.join("target/cargo-aprz-lib-hosting-tests/missing-issues-endpoint-17640-4/github.com/owner/repo.bin");
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&new, "test cache").unwrap();
        fs::write(&old, "changed").unwrap();
        let (loaded, _) = TestingState::load(&state.record.base).unwrap();
        loaded.validate_artifacts().unwrap();
        fs::remove_file(old).unwrap();
        loaded.validate_artifacts().unwrap();
        assert!(
            !tree_paths(&loaded.artifact_roots, &workspace.join("target"))
                .unwrap()
                .contains(&new)
        );
        state.artifact_roots.push(new.parent().unwrap().to_owned());
        state.artifacts.push(Artifact {
            generated_output: None,
            path: new.clone(),
            digest: fingerprint(&new).unwrap(),
            metadata: None,
        });
        state.validate_artifacts().unwrap();
        fs::write(&new, "changed explicit input").unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("changed"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and source artifact validation")]
    fn a_source_directory_named_target_is_not_generated_output() {
        let (_directory, mut state) = fixture();
        let source = state.artifact_roots[0].join("module/target/source.rs");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "source").unwrap();
        state.validate_artifacts().unwrap();
        state.artifacts.push(Artifact {
            generated_output: Some(generated_output(
                &source,
                &state.artifact_roots[0].join("target"),
                &state.artifact_roots,
            )),
            path: source.clone(),
            digest: fingerprint(&source).unwrap(),
            metadata: None,
        });
        assert_eq!(state.artifacts.last().unwrap().generated_output, Some(false));
        save(&state);
        fs::write(source.parent().unwrap().parent().unwrap().join("Cargo.toml"), "[workspace]\n").unwrap();
        let (state, _) = TestingState::load(&state.record.base).unwrap();
        assert!(generated_output(
            &source,
            &state.artifact_roots[0].join("target"),
            &state.artifact_roots
        ));
        state.validate_artifacts().unwrap();
        fs::write(&source, "changed").unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("changed"));
        fs::remove_file(&source).unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("could not"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed snapshots and captured build outputs")]
    fn explicit_build_outputs_and_binaries_under_target_still_require_unchanged_bytes() {
        let (_directory, mut state) = fixture();
        let output = state.artifact_roots[0].join("target");
        let build = output.join("build/subject/out");
        fs::create_dir_all(&build).unwrap();
        let asset = build.join("asset");
        fs::write(&asset, "frozen").unwrap();
        state.artifact_roots.push(build.clone());
        state.artifacts.push(Artifact {
            generated_output: None,
            path: asset.clone(),
            digest: fingerprint(&asset).unwrap(),
            metadata: None,
        });
        state.validate_artifacts().unwrap();
        fs::write(&asset, "changed").unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("changed"));
        fs::write(&asset, "frozen").unwrap();
        fs::write(build.join("new"), "new").unwrap();
        state.validate_artifacts().unwrap();
        fs::remove_file(build.join("new")).unwrap();
        let executable = output.join("test-binary");
        fs::write(&executable, "binary").unwrap();
        state.session.binaries.push(crate::testing::test_binary(executable.as_str()));
        state.artifacts.push(Artifact {
            generated_output: None,
            path: executable.clone(),
            digest: fingerprint(&executable).unwrap(),
            metadata: None,
        });
        state.validate_artifacts().unwrap();
        fs::write(executable, "changed binary").unwrap();
        assert!(state.validate_artifacts().unwrap_err().to_string().contains("changed"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses prepared snapshot files and filesystem-backed verdict journals")]
    fn partial_ledger_retains_observed_knowledge_without_inventing_pending_verdicts() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        let mut mutant = state.plan.mutants[0].clone();
        mutant.outcome = Outcome::Timeout;
        TestingJournal::open(base, None).unwrap().publish(&mutant, &state.killers).unwrap();
        let record = partial_testing_record(base).unwrap().unwrap();
        assert_eq!(
            record.outcomes().iter().filter(|entry| entry.outcome == Outcome::Timeout).count(),
            1
        );
        assert_eq!(
            record.outcomes().iter().filter(|entry| entry.outcome == Outcome::Pending).count(),
            1
        );
        let mut remaining = state.plan.mutants[1].clone();
        remaining.outcome = Outcome::Survived;
        TestingJournal::open(base, None)
            .unwrap()
            .publish(&remaining, &state.killers)
            .unwrap();
        TestingJournal::complete(base).unwrap();
        let finished = partial_testing_record(base).unwrap().unwrap();
        assert_eq!(
            finished.outcomes().iter().filter(|entry| entry.outcome == Outcome::Timeout).count(),
            1
        );
        assert_eq!(
            finished
                .outcomes()
                .iter()
                .filter(|entry| entry.outcome == Outcome::Survived)
                .count(),
            1
        );
        assert!(!finished.outcomes().iter().any(|entry| entry.outcome == Outcome::Pending));
        finish_testing_record(base).unwrap();
        assert!(partial_testing_record(base).unwrap().is_none());
        assert!(failure(base).contains("not interrupted"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "writes and reloads campaign phase files")]
    fn a_fresh_campaign_invalidates_the_previous_testing_checkpoint() {
        let (_directory, state) = fixture();
        invalidate_testing(&state.record.base).unwrap();
        assert!(failure(&state.record.base).contains("not interrupted"));
        assert!(partial_testing_record(&state.record.base).unwrap().is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "writes and reloads prepared snapshot and verdict files")]
    fn integrity_checks_reject_parseable_snapshot_and_verdict_changes() {
        let (_directory, state) = fixture();
        let base = &state.record.base;
        let path = base.join(SNAPSHOT);
        let mut snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        snapshot["settings"]["confirm"] = false.into();
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(failure(base).contains("integrity"));
        save(&state);
        let mut mutant = state.plan.mutants[0].clone();
        mutant.outcome = Outcome::Survived;
        TestingJournal::open(base, None).unwrap().publish(&mutant, &state.killers).unwrap();
        let path = base.join(JOURNAL);
        let mut verdict: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        verdict["mutant"]["outcome"] = "Killed".into();
        let mut bytes = serde_json::to_vec(&verdict).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
        assert!(failure(base).contains("integrity"));
    }
}
