// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, Write as _};

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_engine::ops::collect::Confidence;
use serde::{Deserialize, Serialize};

use super::census::Census;
use super::killers::Killers;
use super::measure::{Built, Oracle};
use super::session::Session;
use crate::discover::{ContextDigest, Plan, RunRecord, WorkspaceSnapshot};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::{Result, elements as publication};

const VERSION: u32 = 1;
const SNAPSHOT: &str = "testing-session.json";
const JOURNAL: &str = "testing-verdicts.jsonl";
const PHASE: &str = "testing-phase.json";
const MAX_STATE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RecordPreparation {
    #[serde(default)]
    pub(crate) report_info: Option<crate::elements::RunInfo>,
    pub(crate) base: Utf8PathBuf,
    pub(crate) context: ContextDigest,
    pub(crate) inputs: WorkspaceSnapshot,
    pub(crate) killers: crate::discover::Killers,
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
    pub(super) census: Census,
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
}

#[derive(Serialize, Deserialize)]
struct Settled {
    mutant: Mutant,
    probe: Option<crate::discover::Killer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sweep: Option<super::session::SweepCost>,
}

#[derive(Serialize, Deserialize)]
struct Committed {
    #[serde(flatten)]
    verdict: Settled,
    checksum: String,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Utf8Path) -> Result<T> {
    let file = File::open(path).map_err(|cause| error!("could not read testing state `{path}`; start a new campaign").caused_by(cause))?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| error!("could not read testing state `{path}`").caused_by(cause))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(error!("testing state `{path}` exceeds the supported size; start a new campaign"));
    }
    serde_json::from_slice(&bytes).map_err(|cause| error!("testing state `{path}` is invalid; start a new campaign").caused_by(cause))
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
    pub(super) fn start(
        plan: &Plan,
        built: &Built,
        census: Census,
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
            census,
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
        restore_confidences(&mut state.plan, &state.confidences, &ids, base)?;
        let path = base.join(JOURNAL);
        let file = File::open(&path).map_err(|cause| error!("could not read testing verdicts `{path}`").caused_by(cause))?;
        let mut reader = BufReader::new(file);
        let mut valid_bytes = 0_u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            let length = reader
                .by_ref()
                .take(MAX_STATE_BYTES + 1)
                .read_until(b'\n', &mut line)
                .map_err(|cause| error!("could not read testing verdicts `{path}`").caused_by(cause))?;
            if length == 0 {
                break;
            }
            if length as u64 > MAX_STATE_BYTES {
                return Err(error!("testing verdict in `{path}` exceeds the supported size"));
            }
            // An interrupted append has no committed newline. Only that final fragment is ignored.
            if line.last() != Some(&b'\n') {
                break;
            }
            let committed: Committed = serde_json::from_slice(&line)
                .map_err(|cause| error!("testing verdicts `{path}` are corrupt; start a new campaign").caused_by(cause))?;
            let encoded = serde_json::to_vec(&committed.verdict)
                .map_err(|cause| error!("could not validate testing verdicts `{path}`").caused_by(cause))?;
            if blake3::hash(&encoded).to_hex().as_str() != committed.checksum {
                return Err(error!(
                    "testing verdicts `{path}` failed their integrity check; start a new campaign"
                ));
            }
            let settled = committed.verdict;
            if let Some(cost) = settled.sweep {
                state.session.phases.sweep = Some(cost);
            }
            let index = ordinals
                .get(&settled.mutant.ordinal)
                .ok_or_else(|| error!("testing verdicts `{path}` name an unknown mutant"))?;
            let mutant = &mut state.plan.mutants[*index];
            if mutant.id != settled.mutant.id || mutant.outcome != Outcome::Pending || settled.mutant.outcome == Outcome::Pending {
                return Err(error!("testing verdicts `{path}` contain conflicting or unfinished outcomes"));
            }
            mutant.outcome = settled.mutant.outcome;
            mutant.elapsed_ms = settled.mutant.elapsed_ms;
            mutant.killed_by = settled.mutant.killed_by;
            mutant.killer = settled.mutant.killer;
            mutant.note = settled.mutant.note;
            match settled.probe {
                Some(probe) => state.killers.record(mutant.id.clone(), probe),
                None => state.killers.forget(&mutant.id),
            }
            valid_bytes += length as u64;
        }
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

impl TestingJournal {
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
        let verdict = Settled {
            mutant: mutant.clone(),
            probe: killers.hint(&mutant.id).cloned(),
            sweep,
        };
        let encoded = serde_json::to_vec(&verdict).map_err(|cause| error!("could not encode a testing verdict").caused_by(cause))?;
        let mut bytes = serde_json::to_vec(&Committed {
            verdict,
            checksum: blake3::hash(&encoded).to_hex().to_string(),
        })
        .map_err(|cause| error!("could not encode a testing checkpoint").caused_by(cause))?;
        bytes.push(b'\n');
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
        for entry in fs::read_dir(directory).map_err(|cause| error!("could not inspect test libraries `{directory}`").caused_by(cause))? {
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
            let entry = entry.map_err(|cause| error!("could not inspect prepared testing tree `{root}`").caused_by(cause))?;
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
    let mut file = File::open(path)
        .map_err(|cause| error!("could not read prepared artifact `{path}`; restore it or start a new campaign").caused_by(cause))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let length = file
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
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::discover::TargetFile;
    use crate::exec::{CensusStatus, OrderingHints, Phases};
    use crate::fixtures;

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
    #[expect(
        clippy::too_many_lines,
        reason = "the in-memory campaign keeps its three learned tiers and conflicting verdict evidence together"
    )]
    fn partial_records_promote_interrupted_generalized_learning_without_replacing_journal_evidence() {
        use crate::discover::{
            BinaryHint, FileBinaryHints, GeneralizedHints, Hints, ItemHints, Killer, RankedHint, ReachCluster, SiteIdentity,
        };

        let root = Utf8PathBuf::from("workspace");
        let plan = Plan {
            root: root.clone(),
            files: vec![TargetFile {
                path: "src/lib.rs".into(),
                absolute: root.join("src/lib.rs"),
                package: "subject".into(),
                source: Some("a".into()),
            }],
            mutants: vec![
                fixtures::mutant(),
                fixtures::mutant_at("pending", "src/lib.rs", 1, "relational.lt_to_le", Outcome::Pending),
            ],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: crate::HashMap::default(),
            skipped: Vec::new(),
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let preparation = RecordPreparation {
            report_info: None,
            context: ContextDigest::default(),
            inputs: serde_json::from_value(serde_json::json!({
                "complete": true,
                "files": [{"path": "src/lib.rs", "digest": crate::discover::digest(b"a"), "size": 1}]
            }))
            .unwrap(),
            killers: crate::discover::Killers::default(),
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
            phases: Phases {
                census_status: CensusStatus::Disabled,
                ..Phases::default()
            },
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
            census: Census::default(),
            killers: Killers::default(),
            record: RecordPreparation {
                report_info: fixtures::report_with(Some((1, 3)), 123, Vec::new()).config,
                context: ContextDigest::default(),
                inputs: RunRecord::snapshot_with_external(&root, &base, &[], false),
                killers: crate::discover::Killers::default(),
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
        use super::super::workspace::Workspace;

        fn waiters(receiver: &mpsc::Receiver<std::thread::ThreadId>, active: std::thread::ThreadId) {
            let mut waiting = crate::HashSet::default();
            while waiting.len() < 3 {
                let worker = receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("three workers reach resource admission");
                if worker != active {
                    let _inserted = waiting.insert(worker);
                }
            }
            while receiver.try_recv().is_ok() {}
        }

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
        };
        let mut work = Workspace::adopt(state.plan.root.clone(), state.plan.root.join("target"));
        let binary = crate::testing::test_binary(state.plan.root.join("fake-binary").as_str());
        work.set_resources(super::super::resources::Resources::fake_binary(&binary));
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
            census: &state.census,
        };
        let mut events = crate::testing::Recorder::default();
        let mut calls = 0;
        let claimed = AtomicUsize::new(0);
        let release = (Mutex::new(0), Condvar::new());
        let (running, active) = mpsc::channel();
        let (waiting, blocked) = mpsc::channel();
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
                    let (phase, worker) = active
                        .recv_timeout(Duration::from_secs(10))
                        .expect("the next resource user becomes active");
                    assert_eq!(phase, calls + 1);
                    waiters(&blocked, worker);
                    let persisted = if calls == 1 {
                        journal.publish(mutant, killers)
                    } else {
                        failing.publish(mutant, killers)
                    };
                    *release.0.lock().unwrap() = phase;
                    release.1.notify_all();
                    persisted
                },
                |_| {
                    let phase = claimed.fetch_add(1, Ordering::SeqCst) + 1;
                    if matches!(phase, 2 | 3) {
                        running.send((phase, std::thread::current().id())).unwrap();
                        let (_guard, deadline) = release
                            .1
                            .wait_timeout_while(release.0.lock().unwrap(), Duration::from_secs(10), |released| *released < phase)
                            .unwrap();
                        assert!(!deadline.timed_out(), "the coordinator releases each active worker");
                    }
                    Some(Judgement::Reached(Outcome::NoCoverage, None, None))
                },
                || {
                    waiting.send(std::thread::current().id()).unwrap();
                },
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
        use super::super::workspace::Workspace;

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
                census: &state.census,
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
    #[cfg_attr(miri, ignore = "uses read-only filesystem-backed verdict journal handles")]
    fn a_checkpoint_write_failure_is_an_error_and_leaves_the_mutant_pending() {
        let (_directory, state) = fixture();
        let journal = TestingJournal {
            file: File::open(state.record.base.join(JOURNAL)).unwrap(),
            previous_sweep: None,
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
        state.version += 1;
        save(&state);
        assert!(failure(&state.record.base).contains("incompatible"));
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
        TestingJournal::complete(base).unwrap();
        assert!(partial_testing_record(base).unwrap().is_some());
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

    #[test]
    fn census_round_trip_preserves_positive_negative_and_incomplete_evidence() {
        let binary = crate::testing::test_binary("test-binary");
        for census in [Census::examined(&binary.path, 7, 1, 4), Census::partial(&binary.path, 7, 1, 4)] {
            let restored: Census = serde_json::from_slice(&serde_json::to_vec(&census).unwrap()).unwrap();
            assert_eq!(census.selection(&binary, 7), restored.selection(&binary, 7));
            assert_eq!(census.selection(&binary, 99), restored.selection(&binary, 99));
            assert_eq!(census.complete_binaries(), restored.complete_binaries());
            assert_eq!(census.incomplete_binaries(), restored.incomplete_binaries());
        }
    }
}
