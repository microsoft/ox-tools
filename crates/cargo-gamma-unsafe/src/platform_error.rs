// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! One platform operation that could not be performed, and everything known about why.

use core::any::type_name;
use core::fmt;
use std::backtrace::Backtrace;
use std::borrow::Cow;
use std::error::Error;
use std::io;

use crate::Situation;

/// A platform facility that could not be installed, created, or applied.
///
/// The shared platform error for `cargo-gamma-unsafe` and `cargo-gamma-process`. It carries a
/// [`Situation`] so callers branch on a value rather than on message text, the operating system's
/// own error where there was one, and a construction backtrace whose availability depends on the
/// process's backtrace configuration.
///
/// This type intentionally makes no `UnwindSafe` or `RefUnwindSafe` promise. It retains the
/// operating system's error representation, so a caller crossing a panic boundary must decide
/// whether the surrounding operation is safe to resume.
pub struct PlatformError {
    situation: Situation,
    message: Cow<'static, str>,
    source: Option<io::Error>,
    backtrace: Backtrace,
}

impl PlatformError {
    /// Reports a platform failure that has no operating-system error behind it.
    #[must_use]
    pub fn new(situation: Situation, message: impl Into<String>) -> Self {
        Self {
            situation,
            message: Cow::Owned(message.into()),
            source: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Reports a platform failure with a message embedded in the program.
    #[must_use]
    pub fn new_static(situation: Situation, message: &'static str) -> Self {
        Self {
            situation,
            message: Cow::Borrowed(message),
            source: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Reports a platform failure for which the operating system supplied a reason.
    #[must_use]
    pub fn because(situation: Situation, message: impl Into<String>, cause: io::Error) -> Self {
        Self {
            situation,
            message: Cow::Owned(message.into()),
            source: Some(cause),
            backtrace: Backtrace::capture(),
        }
    }

    /// Reports a platform failure with an embedded message and an operating-system reason.
    #[must_use]
    pub fn because_static(situation: Situation, message: &'static str, cause: io::Error) -> Self {
        Self {
            situation,
            message: Cow::Borrowed(message),
            source: Some(cause),
            backtrace: Backtrace::capture(),
        }
    }

    /// The classification of this platform failure.
    #[inline]
    #[must_use]
    pub const fn situation(&self) -> Situation {
        self.situation
    }

    /// Where this platform failure was constructed, when backtrace capture is enabled.
    #[inline]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

/// The message alone, because it is written as the sentence a user is shown.
impl fmt::Display for PlatformError {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message.as_ref())
    }
}

/// Written out rather than derived so the captured backtrace is shown as text rather than as the
/// opaque structure `Backtrace`'s own `Debug` produces for a disabled capture.
impl fmt::Debug for PlatformError {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct(type_name::<Self>().rsplit("::").next().unwrap_or_else(type_name::<Self>))
            .field("situation", &self.situation)
            .field("message", &self.message.as_ref())
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
    fn a_platform_error_displays_only_its_message() {
        let error = PlatformError::new_static(Situation::Unsupported, "this host has no cgroup delegation");

        assert_eq!(error.to_string(), "this host has no cgroup delegation");
        assert_eq!(error.situation(), Situation::Unsupported);
        assert!(error.source().is_none(), "an error with no cause must not invent one");
    }

    #[test]
    fn an_operating_system_cause_is_retained_as_the_error_source() {
        let error = PlatformError::because_static(
            Situation::Refused,
            "`cgroup.procs` could not be opened",
            io::Error::new(io::ErrorKind::PermissionDenied, "controlled operating-system cause"),
        );

        assert_eq!(error.situation(), Situation::Refused);

        let source = error.source().expect("the operating system's reason is retained");

        assert_eq!(source.to_string(), "controlled operating-system cause");
        assert_eq!(
            source.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
    }

    /// The debug rendering names every field a diagnostic needs, including the situation a caller
    /// branches on, so a logged error is not reduced to its sentence.
    #[test]
    fn the_debug_rendering_names_the_situation_and_the_message() {
        let rendered = format!(
            "{:?}",
            PlatformError::new_static(Situation::Interrupted, "the run is being interrupted")
        );

        assert!(rendered.contains("Interrupted"), "{rendered}");
        assert!(rendered.contains("message: \"the run is being interrupted\""), "{rendered}");
        assert!(!rendered.contains("Borrowed("), "{rendered}");
        assert!(!rendered.contains("Owned("), "{rendered}");
        assert!(rendered.contains("backtrace"), "{rendered}");
    }

    /// A captured backtrace is reachable, whether or not the host enabled capture.
    #[test]
    fn a_backtrace_is_captured_at_construction() {
        let error = PlatformError::new_static(Situation::Refused, "refused");

        assert!(!format!("{}", error.backtrace()).is_empty());
    }

    #[test]
    fn a_borrowed_non_static_message_remains_accepted() {
        let message = String::from("borrowed for construction");
        let error = PlatformError::new(Situation::Refused, message.as_str());

        assert_eq!(error.to_string(), message);
    }
}
