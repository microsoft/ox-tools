// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only support for exercising code that lives inside `log!` macro arguments.
//!
//! The `log` macros evaluate their arguments only when a logger is installed and the
//! level is enabled. Without one, every formatting argument of a `log::debug!` call is
//! dead code, which makes those lines both untested and uncovered. Installing a logger
//! that observes records only inside a thread-local capture scope makes the arguments
//! run without producing any output.

use std::cell::RefCell;
use std::sync::Once;

use log::Level;

type CapturedRecord = (Level, String, String);

thread_local! {
    static CAPTURED: RefCell<Option<Vec<CapturedRecord>>> = const { RefCell::new(None) };
}

/// A logger that evaluates messages and records them only inside a capture scope.
#[derive(Debug)]
struct DiscardingLogger;

impl log::Log for DiscardingLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let message = record.args().to_string();
        CAPTURED.with_borrow_mut(|records| {
            if let Some(records) = records {
                records.push((record.level(), record.target().to_owned(), message));
            }
        });
    }

    fn flush(&self) {}
}

static INSTALLED: Once = Once::new();

/// Observe actual log records emitted synchronously by this test thread.
pub(crate) fn capture_logs(action: impl FnOnce()) -> Vec<CapturedRecord> {
    struct Restore(Option<Vec<CapturedRecord>>);

    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURED.with_borrow_mut(|records| *records = self.0.take());
        }
    }

    enable_log_argument_evaluation();
    let _restore = Restore(CAPTURED.replace(Some(Vec::new())));
    action();
    CAPTURED
        .take()
        .expect("the capture scope installs a record buffer until the action returns")
}

/// Install a process-wide logger that evaluates every record without printing it.
///
/// Idempotent: later calls are no-ops. Call this at the start of a test that needs the
/// arguments of a `log::debug!` (or any other level) to be evaluated.
pub(crate) fn enable_log_argument_evaluation() {
    INSTALLED.call_once(|| {
        log::set_logger(&DiscardingLogger).expect("no other logger is installed by the test binaries");
        log::set_max_level(log::LevelFilter::Trace);
    });
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn nested_capture_scopes_restore_the_outer_observer() {
        let outer = capture_logs(|| {
            log::warn!(target: "outer", "before");
            let inner = capture_logs(|| log::info!(target: "inner", "during"));
            assert_eq!(inner, [(log::Level::Info, "inner".to_owned(), "during".to_owned())]);
            log::error!(target: "outer", "after");
        });
        assert_eq!(
            outer,
            [
                (log::Level::Warn, "outer".to_owned(), "before".to_owned()),
                (log::Level::Error, "outer".to_owned(), "after".to_owned()),
            ]
        );
        assert!(capture_logs(|| {}).is_empty());
    }

    #[test]
    fn the_logger_answers_and_evaluates_every_record() {
        enable_log_argument_evaluation();

        // A second call is a no-op rather than a panic from `set_logger`.
        enable_log_argument_evaluation();

        assert!(log::log_enabled!(log::Level::Trace));
        log::trace!("evaluated {}", 1 + 1);
        log::logger().flush();
    }
}
