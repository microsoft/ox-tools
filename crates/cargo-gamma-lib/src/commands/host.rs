// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::io::{self, Write};

/// Everything the library needs from the outside world.
///
/// Routing all output and all terminal interrogation through one trait is what makes the console
/// UI testable. A fake host captures both streams and reports a fixed width, so the progress
/// rendering, the color decisions and the exit codes are all ordinary assertions in an
/// integration test rather than things verified by eye.
pub trait Host {
    /// The stream for results the user might pipe into another program.
    fn output(&mut self) -> impl Write;

    /// The result stream with normal early pipe closure treated as successful consumption.
    fn results(&mut self) -> impl Write {
        Results(self.output())
    }

    /// The stream for progress and diagnostics.
    fn error(&mut self) -> impl Write;

    /// Whether the diagnostic stream is a terminal.
    fn is_terminal(&self) -> bool;

    /// The width of the terminal in columns, if there is one.
    fn terminal_width(&self) -> Option<u16>;

    /// The value of an environment variable.
    ///
    /// Reading the real environment is right for every caller but a test, and a test that wants to
    /// pretend it is running inside a CI runner should not have to mutate the process it shares
    /// with every other test to do it.
    fn env(&self, name: &str) -> Option<String> {
        env::var(name).ok()
    }

    /// Whether this host stands for the real process, and so may replace it with another.
    ///
    /// Relaunching re-runs `current_exe`, which is cargo-gamma only when cargo-gamma is what the
    /// operating system actually started. Under a test harness `current_exe` is the harness, so
    /// relaunching there would spawn a second copy of the test suite rather than a second copy of
    /// the tool — which is why this is false unless a host says otherwise.
    fn may_replace_process(&self) -> bool {
        false
    }
}

struct Results<W>(W);

impl<W: Write> Write for Results<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.write(buf) {
            Err(cause) if cause.kind() == io::ErrorKind::BrokenPipe => Ok(buf.len()),
            outcome => outcome,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.flush() {
            Err(cause) if cause.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            outcome => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that only says it is a terminal is not thereby the real process.
    ///
    /// Regression: relaunching under a test harness re-runs the harness, so the suite spawns a
    /// second copy of itself, and every test that inspects output sees two runs interleaved. The
    /// default has to be the safe answer, because a host that forgets to say is a host that
    /// cannot survive being replaced.
    #[test]
    fn a_host_does_not_permit_being_replaced_unless_it_says_so() {
        assert!(!PlainHost.may_replace_process());
    }

    struct PlainHost;

    impl Host for PlainHost {
        fn output(&mut self) -> impl Write {
            Vec::new()
        }

        fn error(&mut self) -> impl Write {
            Vec::new()
        }

        fn is_terminal(&self) -> bool {
            false
        }

        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }

    /// A host that overrides nothing gets the real environment, which is right for the real binary.
    #[test]
    #[cfg(not(miri))]
    fn default_env_reads_the_process_environment() {
        let host = PlainHost;
        let path = host.env("PATH").expect("PATH should be set for cargo test");

        assert!(!path.is_empty());
        assert_eq!(host.env("GAMMA_DEFINITELY_NOT_SET_IN_THE_ENVIRONMENT"), None);
    }

    /// The rest of the contract is answered too, so the default double stays honest.
    #[test]
    fn a_minimal_host_still_answers_the_whole_contract() {
        let mut host = PlainHost;

        host.output().write_all(b"out").expect("output is a sink");
        host.error().write_all(b"err").expect("error is a sink");

        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
    }
}
