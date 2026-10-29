// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The command-line surface and the orchestration behind it.

use core::fmt::{self, Display, Formatter};

mod clean;
mod cli;
mod completions;
mod console_events;
mod dashboard;
mod dispatch;
mod explain;
mod hints;
mod host;
mod list;
mod merge;
mod run;
mod suppress;
mod unsuppress;
mod verdict_log;
mod when;

#[doc(inline)]
pub use cli::{
    BuildLimitArgs, CleanArgs, Cli, Command, CompletionsArgs, ConfigArgs, ExplainArgs, FeatureArgs, HintsArgs, ListArgs, ListCommand,
    ListFilesArgs, ListKind, ListMutantsArgs, ListRegistryArgs, MeasureArgs, MergeArgs, RunArgs, SelectArgs, SuppressArgs, UnsuppressArgs,
};
#[doc(inline)]
pub use dispatch::{EXIT_CANNOT_PROCEED, EXIT_GATE_FAILED, EXIT_INTERNAL, EXIT_OK, EXIT_USAGE, run};
#[doc(inline)]
pub use host::Host;
#[doc(inline)]
pub use when::When;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CanonicalMutantId(crate::model::MutantId);

impl CanonicalMutantId {
    fn parse(value: &str) -> Result<Self, InvalidMutantId> {
        // #[gamma::skip(cond.always_true, tag = "outofmemory", reason = "accepting every subject as a canonical mutant id exhausted memory during campaign discovery")]
        if value.len() == crate::model::MUTANT_ID_HEX_LEN
            && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            Ok(Self(crate::model::MutantId::new(value)))
        } else {
            Err(InvalidMutantId(value.to_owned()))
        }
    }

    fn into_inner(self) -> crate::model::MutantId {
        self.0
    }

    fn as_inner(&self) -> &crate::model::MutantId {
        &self.0
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug)]
struct InvalidMutantId(String);

impl Display for InvalidMutantId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` has an invalid format; expected {} lowercase hexadecimal characters",
            crate::report::encode_controls(&self.0),
            crate::model::MUTANT_ID_HEX_LEN
        )
    }
}

impl core::error::Error for InvalidMutantId {}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

#[cfg(feature = "internals")]
#[doc(hidden)]
pub use run::recovery_test_util;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub use suppress::native_test_util as suppression_test_util;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub use verdict_log::native_test_util as journal_test_util;
