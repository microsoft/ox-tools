// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! One platform operation that could not be performed, and everything known about why.

use core::fmt;
use std::backtrace::Backtrace;
use std::error::Error;
use std::io;

use crate::Situation;

/// A platform facility that could not be installed, created, or applied.
///
/// The canonical error of the two platform crates. It carries a [`Situation`] so callers branch on
/// a value rather than on message text, the operating system's own error where there was one, and
/// a backtrace captured at construction so a refusal that surfaces several layers above its cause
/// can still be traced back to it.
///
/// The backtrace is captured with [`Backtrace::capture`], which does nothing unless the process was
/// started with backtraces enabled, so an error on a hot path costs an allocation for its message
/// and nothing else.
pub struct PlatformError {
    situation: Situation,
    message: String,
    source: Option<io::Error>,
    backtrace: Backtrace,
}

impl PlatformError {
    /// Reports a refusal that has no operating-system error behind it.
    #[must_use]
    pub fn new(situation: Situation, message: impl Into<String>) -> Self {
        Self {
            situation,
            message: message.into(),
            source: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Reports a refusal the operating system gave a reason for.
    #[must_use]
    pub fn because(situation: Situation, message: impl Into<String>, cause: io::Error) -> Self {
        Self {
            situation,
            message: message.into(),
            source: Some(cause),
            backtrace: Backtrace::capture(),
        }
    }

    /// What kind of refusal this is, for callers that must act rather than report.
    #[must_use]
    pub const fn situation(&self) -> Situation {
        self.situation
    }

    /// Where this refusal was constructed, when the process was started with backtraces enabled.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

/// The message alone, because it is written as the sentence a user is shown.
impl fmt::Display for PlatformError {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Written out rather than derived so the captured backtrace is shown as text rather than as the
/// opaque structure `Backtrace`'s own `Debug` produces for a disabled capture.
impl fmt::Debug for PlatformError {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlatformError")
            .field("situation", &self.situation)
            .field("message", &self.message)
            .field("source", &self.source)
            .field("backtrace", &format_args!("{}", self.backtrace))
            .finish()
    }
}

impl Error for PlatformError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|cause| cause as &(dyn Error + 'static))
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_displays_only_its_message() {
        let error = PlatformError::new(Situation::Unsupported, "this host has no cgroup delegation");

        assert_eq!(error.to_string(), "this host has no cgroup delegation");
        assert_eq!(error.situation(), Situation::Unsupported);
        assert!(error.source().is_none(), "a refusal with no cause must not invent one");
    }

    #[test]
    fn an_operating_system_cause_is_retained_as_the_error_source() {
        let error = PlatformError::because(
            Situation::Refused,
            "`cgroup.procs` could not be opened",
            io::Error::from_raw_os_error(13),
        );

        assert_eq!(error.situation(), Situation::Refused);

        let source = error.source().expect("the operating system's reason is retained");

        assert!(source.to_string().len() > 1, "{source}");
    }

    /// The debug rendering names every field a diagnostic needs, including the situation a caller
    /// branches on, so a logged error is not reduced to its sentence.
    #[test]
    fn the_debug_rendering_names_the_situation_and_the_message() {
        let rendered = format!("{:?}", PlatformError::new(Situation::Interrupted, "the run is being interrupted"));

        assert!(rendered.contains("Interrupted"), "{rendered}");
        assert!(rendered.contains("the run is being interrupted"), "{rendered}");
        assert!(rendered.contains("backtrace"), "{rendered}");
    }

    /// A captured backtrace is reachable, whether or not the host enabled capture.
    #[test]
    fn a_backtrace_is_captured_at_construction() {
        let error = PlatformError::new(Situation::Refused, "refused");

        assert!(!format!("{}", error.backtrace()).is_empty());
    }
}
