// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::error::Error as StdError;
use core::fmt::{self, Display, Formatter};
use std::backtrace::Backtrace;
use std::io;

/// An engine error with the coordinator-facing classification preserved.
///
/// This type intentionally makes no `UnwindSafe` or `RefUnwindSafe` promise. Its source is an
/// unconstrained dynamic error supplied by the failing subsystem, and callers crossing a panic
/// boundary must decide whether that particular operation is safe to resume from.
#[derive(Debug)]
pub struct Error {
    message: String,
    cause: Option<Box<dyn StdError + Send + Sync>>,
    usage: bool,
    skippable: bool,

    /// Captured at construction, unconditionally.
    ///
    /// Every path that produces an `Error` funnels through [`Self::new`], so capturing there
    /// covers both direct construction and every `From` conversion at its true origin, without a
    /// second capture point to keep in sync. Whether frames are actually recorded is controlled
    /// the same way the standard library controls it everywhere else, by
    /// `RUST_BACKTRACE`/`RUST_LIB_BACKTRACE`, so this costs nothing when they are unset.
    backtrace: Backtrace,
}

impl Error {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
            usage: false,
            skippable: false,
            backtrace: Backtrace::capture(),
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

    /// Returns the backtrace captured when this error was constructed.
    #[inline]
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    /// Takes this error apart so that a caller can rebuild it as its own type.
    ///
    /// The backtrace goes with the rest. A conversion that captured a fresh one would record the
    /// conversion rather than the failure, which is the one place a backtrace is no use: every
    /// engine error crossing into the coordinator would point at the same `From` implementation.
    #[must_use]
    pub fn into_parts(self) -> Parts {
        Parts {
            message: self.message,
            cause: self.cause,
            usage: self.usage,
            skippable: self.skippable,
            backtrace: self.backtrace,
        }
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

/// Everything an [`Error`] carries, handed over one field at a time.
///
/// Named fields prevent the flags and optional cause from being confused by position.
///
/// Like [`Error`], this type intentionally makes no unwind-safety promise because it carries the
/// same unconstrained dynamic source.
#[derive(Debug)]
pub struct Parts {
    /// What was being attempted, in the words the user will read.
    pub message: String,

    /// The underlying failure, when there was one.
    pub cause: Option<Box<dyn StdError + Send + Sync>>,

    /// Whether this is something the user typed rather than something that went wrong.
    pub usage: bool,

    /// Whether the caller may step over this and finish the rest of the job.
    pub skippable: bool,

    /// Where the error was constructed, captured there rather than here.
    pub backtrace: Backtrace,
}

macro_rules! error {
    ($($arg:tt)*) => { $crate::error::Error::new(format!($($arg)*)) };
}

pub(crate) use error;

#[cfg(test)]
mod tests {
    use std::backtrace::BacktraceStatus;
    use std::env;
    use std::process::Command;

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

    /// `into_parts` hands back exactly the state built up on the error.
    #[test]
    fn into_parts_returns_the_built_up_state() {
        let error = Error::new("partial write")
            .usage()
            .skippable()
            .caused_by(io::Error::other("truncated"));

        let Parts {
            message,
            cause,
            usage,
            skippable,
            backtrace: _backtrace,
        } = error.into_parts();

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

    /// Every construction path, direct or via a `From` conversion, records real frames when the
    /// process asks for them.
    ///
    /// Run in a child process rather than in this one, because whether a backtrace is captured is
    /// decided by the environment the process started with and no test may change that for its
    /// neighbours.
    /// The parent re-executes the test binary with `RUST_BACKTRACE` and `RUST_LIB_BACKTRACE` set
    /// and a marker variable that tells the child it is the child; the child then does the
    /// asserting. The parent checks both its exit status and that the requested test actually ran.
    ///
    /// The child demands [`BacktraceStatus::Captured`], which is the whole point of the isolation:
    /// asserting only "not `Unsupported`" passed just as happily against a `Backtrace::disabled()`,
    /// because the default environment reports `Disabled` either way. `Unsupported` is still
    /// accepted, because a platform with no backtrace support is a fact about the host rather than
    /// a regression in this file — and `Backtrace::disabled()` never reports it, so the mutant this
    /// test exists to catch is still caught there.
    #[test]
    fn every_construction_path_captures_a_backtrace() {
        const CHILD: &str = "CARGO_GAMMA_BACKTRACE_CHILD";
        const TEST: &str = "error::tests::every_construction_path_captures_a_backtrace";

        if env::var_os(CHILD).is_some() {
            let direct = Error::new("something went wrong");
            let converted = Error::from(io::Error::other("disk exploded"));

            for error in [&direct, &converted] {
                let status = error.backtrace().status();

                assert!(
                    matches!(status, BacktraceStatus::Captured | BacktraceStatus::Unsupported),
                    "a backtrace was requested but not taken: {status:?}"
                );
            }

            return;
        }

        let executable = env::current_exe().expect("the test executable is known");
        let output = Command::new(executable)
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_LIB_BACKTRACE", "1")
            .output()
            .expect("re-run this test with backtraces enabled");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "the isolated run failed: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert!(stdout.contains(TEST), "the exact filter did not run `{TEST}`\n{stdout}\n{stderr}");
    }
}
