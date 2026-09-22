// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The verdict-only view of a mutation-testing report.

use std::borrow::Cow;

use serde::Deserialize;

use super::report::{
    FLAKY_PREFIX, FRAMEWORK_NAME, NOT_BUILT_PREFIX, OUT_OF_MEMORY_PREFIX, Report, SUPPORTED_SCHEMA_MAJOR, TIMEOUT_PREFIX,
    supported_schema_version,
};
use crate::model::{MUTANT_ID_VERSION, MutantId, Outcome};
use crate::{HashMap, HashSet};

/// The slice of a report a cross-run reader needs.
///
/// Serde skips embedded source fields without allocating them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Digest<'text> {
    #[serde(borrow)]
    pub schema_version: Cow<'text, str>,
    #[serde(borrow)]
    pub framework: FrameworkDigest<'text>,
    #[serde(borrow)]
    pub files: HashMap<Cow<'text, str>, FileDigest<'text>>,
}

/// The report producer.
#[derive(Debug, Deserialize)]
pub struct FrameworkDigest<'text> {
    #[serde(borrow)]
    pub name: Cow<'text, str>,
}

impl Digest<'_> {
    /// Checks that this tool can safely reuse the document's verdicts.
    ///
    /// # Errors
    ///
    /// Returns an error for another producer or an unsupported schema version.
    pub fn ensure_ours(&self) -> Result<(), String> {
        if self.framework.name != FRAMEWORK_NAME {
            return Err(format!(
                "the report was written by `{}` rather than by {FRAMEWORK_NAME}, so its verdicts are not this tool's to carry forward",
                self.framework.name
            ));
        }

        if supported_schema_version(&self.schema_version) {
            Ok(())
        } else {
            Err(format!(
                "the report claims schema version `{}`, which this build does not understand; \
                 versions 1 to {SUPPORTED_SCHEMA_MAJOR} are supported",
                self.schema_version
            ))
        }
    }
}

/// The mutants recorded for one file.
#[derive(Debug, Deserialize)]
pub struct FileDigest<'text> {
    #[serde(borrow)]
    pub mutants: Vec<MutantDigest<'text>>,
}

/// One recorded verdict.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutantDigest<'text> {
    #[serde(borrow)]
    pub id: Cow<'text, str>,
    #[serde(borrow)]
    pub status: Cow<'text, str>,
    #[serde(borrow, default)]
    pub status_reason: Option<Cow<'text, str>>,
}

impl MutantDigest<'_> {
    /// Maps a verdict onto the outcome it settles.
    #[must_use]
    pub fn settled_outcome(&self) -> Option<Outcome> {
        settled_verdict(&self.status, self.status_reason.as_deref())
    }
}

/// Reads the IDs of mutants an earlier report settled.
///
/// # Errors
///
/// Returns an error if the text is not a compatible report from this tool.
#[cfg_attr(
    not(feature = "internals"),
    allow(
        dead_code,
        reason = "kept to verify that writer and incremental reader agree about settled verdicts"
    )
)]
pub fn settled_mutants(text: &str) -> Result<HashSet<String>, String> {
    let report: Digest<'_> = serde_json::from_str(text).map_err(|cause| cause.to_string())?;

    report.ensure_ours()?;

    Ok(report
        .files
        .values()
        .flat_map(|file| file.mutants.iter())
        .filter(|mutant| mutant.settled_outcome().is_some())
        .map(|mutant| mutant.id.clone().into_owned())
        .collect())
}

/// Reads the genuine survivor IDs from a validated cargo-gamma report.
///
/// Resource outcomes export as `Survived` because the interchange schema counts them as
/// undetected. Their reason distinguishes them from mutants for which every test passed.
///
/// # Errors
///
/// Returns an error for another producer or mutant-identity scheme.
pub(crate) fn surviving_mutants(report: &Report) -> Result<HashSet<MutantId>, String> {
    if report.framework.name != FRAMEWORK_NAME {
        return Err(format!(
            "the report was written by `{}` rather than by {FRAMEWORK_NAME}",
            report.framework.name
        ));
    }

    let identity_version = report
        .config
        .as_ref()
        .and_then(|config| config.mutant_id_version)
        .unwrap_or(MUTANT_ID_VERSION);

    if identity_version != MUTANT_ID_VERSION {
        return Err(format!(
            "the report uses mutant-ID scheme {identity_version}, but this build uses scheme {MUTANT_ID_VERSION}"
        ));
    }

    Ok(report
        .files
        .values()
        .flat_map(|file| file.mutants.iter())
        .filter(|mutant| mutant.status == "Survived" && mutant.status_reason.is_none())
        .map(|mutant| MutantId::new(&mutant.id))
        .collect())
}

pub(super) fn settled_verdict(status: &str, reason: Option<&str>) -> Option<Outcome> {
    let reason_is = |prefix: &str| reason.is_some_and(|reason| reason.starts_with(prefix));

    match status {
        "Survived" if reason_is(OUT_OF_MEMORY_PREFIX) => None,
        "Survived" if reason_is(TIMEOUT_PREFIX) => Some(Outcome::Timeout),
        "Timeout" if reason.is_none() || reason_is(OUT_OF_MEMORY_PREFIX) => None,
        "Ignored" if reason.is_none() || reason_is(NOT_BUILT_PREFIX) || reason_is(FLAKY_PREFIX) => None,
        "Killed" => Some(Outcome::Killed),
        "Timeout" => Some(Outcome::Timeout),
        "CompileError" => Some(Outcome::CompileError),
        "Ignored" => Some(Outcome::Ignored),
        _other => None,
    }
}
