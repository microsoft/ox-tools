// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::File;
use std::io::Write as _;

use camino::{Utf8Path, Utf8PathBuf};

use super::console_events::mutant_detail;
use crate::error::error;
use crate::model::Mutant;
use crate::report::Styler;

pub(super) const TESTING_PROGRESS_LOG: &str = "gamma-progress.log";

#[derive(Debug, Default)]
pub(super) enum VerdictLog {
    #[default]
    Disabled,
    Writing {
        file: File,
        path: Utf8PathBuf,
    },
    Failed {
        path: Utf8PathBuf,
        cause: std::io::Error,
    },
}

impl VerdictLog {
    pub(super) fn start(&mut self, scratch: &Utf8Path) -> crate::Result<()> {
        let path = scratch.join(TESTING_PROGRESS_LOG);
        let file = File::create(path.as_std_path())
            .map_err(|cause| error!("could not create the testing progress log at `{path}`").caused_by(cause))?;

        *self = Self::Writing { file, path };

        Ok(())
    }

    /// Writes and flushes one verdict, retaining the first failure for [`Self::finish`].
    pub(super) fn record(&mut self, mutant: &Mutant) {
        let failed = match self {
            Self::Writing { file, .. } => {
                let label = Styler::new(false).outcome(mutant.outcome);
                let detail = mutant_detail(mutant);

                writeln!(file, "{label} {detail}").and_then(|()| file.flush()).err()
            }
            Self::Disabled | Self::Failed { .. } => None,
        };

        let Some(cause) = failed else { return };
        let path = match self {
            Self::Writing { path, .. } => path.clone(),
            Self::Disabled | Self::Failed { .. } => return,
        };

        *self = Self::Failed { path, cause };
    }

    pub(super) fn finish(&mut self) -> crate::Result<()> {
        let previous = core::mem::take(self);

        match previous {
            Self::Failed { path, cause } => Err(error!("could not write the testing progress log at `{path}`").caused_by(cause)),
            Self::Disabled | Self::Writing { .. } => Ok(()),
        }
    }
}
