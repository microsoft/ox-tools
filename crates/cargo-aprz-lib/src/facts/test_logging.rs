// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only support for exercising code that lives inside `log!` macro arguments.
//!
//! The `log` macros evaluate their arguments only when a logger is installed and the
//! level is enabled. Without one, every formatting argument of a `log::debug!` call is
//! dead code, which makes those lines both untested and uncovered. Installing a logger
//! that discards everything makes the arguments run without producing any output.

use core::sync::atomic::{AtomicBool, Ordering};

/// A logger that evaluates the message and throws it away.
#[derive(Debug)]
struct DiscardingLogger;

impl log::Log for DiscardingLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        // Force the argument closure to run, which is the whole point of installing
        // this logger, then drop the result.
        let _ = record.args().to_string();
    }

    fn flush(&self) {}
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install a process-wide logger that evaluates every record and discards it.
///
/// Idempotent: later calls are no-ops. Call this at the start of a test that needs the
/// arguments of a `log::debug!` (or any other level) to be evaluated.
pub(crate) fn enable_log_argument_evaluation() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    log::set_max_level(log::LevelFilter::Trace);
    log::set_logger(&DiscardingLogger).expect("no other logger is installed by the test binaries");
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

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
