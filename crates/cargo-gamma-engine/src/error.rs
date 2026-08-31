// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::error::Error as StdError;
use core::fmt::{self, Display, Formatter};
use std::io;

/// An engine error with the coordinator-facing classification preserved.
#[derive(Debug)]
pub struct Error {
    message: String,
    cause: Option<Box<dyn StdError + Send + Sync>>,
    usage: bool,
    skippable: bool,
}

impl Error {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
            usage: false,
            skippable: false,
        }
    }

    #[must_use]
    pub const fn usage(mut self) -> Self {
        self.usage = true;
        self
    }

    #[must_use]
    pub const fn is_usage(&self) -> bool {
        self.usage
    }

    #[must_use]
    pub const fn skippable(mut self) -> Self {
        self.skippable = true;
        self
    }

    #[must_use]
    pub const fn is_skippable(&self) -> bool {
        self.skippable
    }

    #[must_use]
    pub fn caused_by(mut self, cause: impl StdError + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    #[must_use]
    pub fn into_parts(self) -> (String, Option<Box<dyn StdError + Send + Sync>>, bool, bool) {
        (self.message, self.cause, self.usage, self.skippable)
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

macro_rules! error {
    ($($arg:tt)*) => { $crate::error::Error::new(format!($($arg)*)) };
}

pub(crate) use error;

#[cfg(test)]
mod tests {
    use super::*;

    /// A freshly built error carries the message given to it and starts out neither a usage error
    /// nor skippable, with no cause attached.
    #[test]
    fn a_new_error_starts_plain() {
        let error = Error::new("something went wrong");

        assert!(!error.is_usage());
        assert!(!error.is_skippable());
        assert_eq!(error.to_string(), "something went wrong");
        assert!(error.source().is_none());
    }

    /// `usage` and `skippable` each flip their own flag and leave the other alone, in either
    /// application order.
    #[test]
    fn usage_and_skippable_are_independent_flags() {
        let usage_only = Error::new("bad flags").usage();

        assert!(usage_only.is_usage());
        assert!(!usage_only.is_skippable());

        let skippable_only = Error::new("missing file").skippable();

        assert!(!skippable_only.is_usage());
        assert!(skippable_only.is_skippable());

        let both = Error::new("both").usage().skippable();

        assert!(both.is_usage());
        assert!(both.is_skippable());
    }

    /// A cause attached with `caused_by` is appended to the display text and surfaces as the
    /// `source` the standard error trait exposes.
    #[test]
    fn a_cause_is_displayed_and_surfaced_as_the_source() {
        let cause = io::Error::other("disk exploded");
        let error = Error::new("could not read file").caused_by(cause);

        assert_eq!(error.to_string(), "could not read file: disk exploded");
        assert_eq!(error.source().expect("a cause was attached").to_string(), "disk exploded");
    }

    /// `into_parts` hands back exactly the state built up on the error, in the documented order.
    #[test]
    fn into_parts_returns_the_built_up_state() {
        let error = Error::new("partial write")
            .usage()
            .skippable()
            .caused_by(io::Error::other("truncated"));

        let (message, cause, usage, skippable) = error.into_parts();

        assert_eq!(message, "partial write");
        assert_eq!(cause.expect("a cause was attached").to_string(), "truncated");
        assert!(usage);
        assert!(skippable);
    }

    /// An I/O error converts into an engine error labeled generically, with the original error
    /// preserved as the cause so its detail is not lost.
    #[test]
    fn an_io_error_converts_with_its_detail_preserved_as_the_cause() {
        let io_error = io::Error::other("permission denied");
        let error = Error::from(io_error);

        assert_eq!(error.to_string(), "I/O error: permission denied");
        assert_eq!(error.source().expect("a cause was attached").to_string(), "permission denied");
    }

    /// The `error!` macro formats its arguments the way `format!` would, producing a plain error
    /// with that text as its message.
    #[test]
    fn the_error_macro_formats_its_arguments() {
        let built = error!("{} of {}", 2, 3);

        assert_eq!(built.to_string(), "2 of 3");
        assert!(!built.is_usage());
        assert!(!built.is_skippable());
    }
}
