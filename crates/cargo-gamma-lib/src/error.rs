// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The error type used throughout the tool.

use core::error::Error as StdError;
use core::fmt::{self, Display, Formatter};
use std::backtrace::Backtrace;
use std::io;

use cargo_gamma_engine::{Error as EngineError, Parts as EngineParts};
use serde_json::Value;

/// A machine-readable diagnostic that should be persisted before an error unwinds its run state.
#[derive(Debug)]
pub(crate) struct Artifact {
    pub(crate) file_name: &'static str,
    pub(crate) value: Value,
}

/// An error carrying a human-readable message and an optional cause.
///
/// Messages are written for the person who ran the command, not for a log aggregator: they say
/// what was being attempted and what to do about it.
#[derive(Debug)]
pub struct Error {
    message: String,
    cause: Option<Box<dyn StdError + Send + Sync>>,
    usage: bool,
    skippable: bool,
    artifact: Option<Box<Artifact>>,

    /// Captured at construction, unconditionally.
    ///
    /// Every path that produces an `Error` funnels through [`Self::new`] or through a conversion
    /// that carries the origin's own capture across, so the frames recorded are the ones where the
    /// failure happened rather than where it was rewrapped. Whether frames are recorded at all is
    /// controlled the way the standard library controls it everywhere else, by
    /// `RUST_BACKTRACE`/`RUST_LIB_BACKTRACE`, so this costs nothing when they are unset.
    backtrace: Backtrace,
}

impl Error {
    /// Creates an error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
            usage: false,
            skippable: false,
            artifact: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Marks this as a usage error: something the user typed or configured, not something that
    /// went wrong while running.
    ///
    /// The distinction is the whole point of the exit-code scheme. A CI script needs to tell "you
    /// invoked me wrongly" from "I ran and could not proceed", and collapsing the two forces it to
    /// parse the message text to find out which happened.
    #[must_use]
    pub const fn usage(mut self) -> Self {
        self.usage = true;
        self
    }

    /// Returns whether this is a usage error.
    #[must_use]
    pub const fn is_usage(&self) -> bool {
        self.usage
    }

    /// Marks this as an error the caller may step over: one file could not be handled, and the
    /// work the caller is doing is still worth finishing without it.
    ///
    /// Only the *producer* of an error knows whether its subject is the whole job or one item of
    /// it, and only the *consumer* knows whether stepping over an item is acceptable there. This
    /// flag is how the first tells the second, instead of the second matching on message text. It
    /// is never permission to be quiet: a caller that skips must say what it skipped, because a
    /// file dropped from a mutation population silently raises the score.
    #[must_use]
    pub const fn skippable(mut self) -> Self {
        self.skippable = true;
        self
    }

    /// Returns whether the caller may step over this error and carry on with the rest of the job.
    #[must_use]
    pub const fn is_skippable(&self) -> bool {
        self.skippable
    }

    /// Attaches a machine-readable diagnostic for the command layer to persist.
    #[must_use]
    pub(crate) fn with_artifact(mut self, file_name: &'static str, value: Value) -> Self {
        self.artifact = Some(Box::new(Artifact { file_name, value }));
        self
    }

    /// Returns the diagnostic attached to this failure, when it has one.
    pub(crate) fn artifact(&self) -> Option<&Artifact> {
        self.artifact.as_deref()
    }

    /// Appends command-layer context that was not available where the error originated.
    pub(crate) fn append_message(&mut self, suffix: &str) {
        self.message.push_str(suffix);
    }

    /// Attaches an underlying cause.
    #[must_use]
    pub fn caused_by(mut self, cause: impl StdError + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// Returns the message, without the cause chain.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the backtrace captured where this error was first constructed.
    ///
    /// Not printed to the user: a mutation run reports what it could not do and what to do about
    /// it, and a stack of this tool's own frames answers neither question. It is here for the case
    /// the message cannot cover, a failure that should have been impossible, where the only useful
    /// next question is which code path produced it.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl Display for Error {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)?;

        if let Some(cause) = &self.cause {
            write!(formatter, ": {cause}")?;
        }

        Ok(())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.cause.as_ref().map(|cause| &**cause as &(dyn StdError + 'static))
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::new("I/O error").caused_by(value)
    }
}

impl From<EngineError> for Error {
    fn from(value: EngineError) -> Self {
        // The engine's capture is carried across rather than replaced. Capturing here would record
        // this conversion, which every engine error passes through and which therefore identifies
        // nothing.
        let EngineParts {
            message,
            cause,
            usage,
            skippable,
            backtrace,
        } = value.into_parts();

        Self {
            message,
            cause,
            usage,
            skippable,
            artifact: None,
            backtrace,
        }
    }
}

/// Creates an [`Error`] from a format string.
macro_rules! error {
    ($($arg:tt)*) => { $crate::error::Error::new(format!($($arg)*)) };
}

pub(crate) use error;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_preserved() {
        let error = Error::new("could not read the manifest");

        assert_eq!(error.message(), "could not read the manifest");
        assert_eq!(error.to_string(), "could not read the manifest");
    }

    #[test]
    fn cause_is_appended_to_the_display_form() {
        let cause = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let error = Error::new("could not read the manifest").caused_by(cause);

        assert_eq!(error.to_string(), "could not read the manifest: no such file");
    }

    #[test]
    fn source_is_exposed_for_the_error_trait() {
        use core::error::Error as _;

        let cause = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let error = Error::new("outer").caused_by(cause);

        assert!(error.source().is_some());
        assert!(Error::new("outer").source().is_none());
    }

    #[test]
    fn errors_are_not_usage_errors_by_default() {
        assert!(!Error::new("something went wrong").is_usage());
    }

    #[test]
    fn usage_errors_are_marked() {
        // The exit-code scheme depends on this: a caller must be able to tell "you invoked me
        // wrongly" from "I ran and could not proceed" without parsing the message.
        assert!(Error::new("bad selector").usage().is_usage());
    }

    #[test]
    fn marking_a_usage_error_preserves_the_message_and_cause() {
        let cause = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let error = Error::new("outer").caused_by(cause).usage();

        assert_eq!(error.to_string(), "outer: no such file");
        assert!(error.is_usage());
    }

    #[test]
    fn io_errors_convert() {
        let error: Error = io::Error::new(io::ErrorKind::PermissionDenied, "denied").into();

        assert!(error.to_string().contains("denied"));
    }

    #[test]
    fn engine_errors_preserve_classification_and_causes() {
        let engine = cargo_gamma_engine::Error::new("could not read source")
            .caused_by(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            .usage()
            .skippable();
        let error = Error::from(engine);

        assert_eq!(error.to_string(), "could not read source: denied");
        assert!(error.is_usage());
        assert!(error.is_skippable());
        assert!(error.source().is_some());
    }
}
