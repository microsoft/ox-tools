// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The host that talks to the real terminal and the real process.

use std::io::{IsTerminal, Write, stderr, stdout};

use cargo_gamma_lib::Host;

/// Host that talks to the real terminal and the real process.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealHost;

#[cfg_attr(coverage_nightly, coverage(off))]
impl Host for RealHost {
    fn output(&mut self) -> impl Write {
        stdout()
    }

    fn error(&mut self) -> impl Write {
        stderr()
    }

    // #[gamma::skip(fn_value.bool_false, fn_value.bool_true, reason = "the answer belongs to the invoking terminal and tests validly run both attached and detached")]
    fn is_terminal(&self) -> bool {
        stderr().is_terminal()
    }

    // #[gamma::skip(fn_value.none, reason = "terminal dimensions are optional and None is already the valid fallback when the host cannot report them")]
    fn terminal_width(&self) -> Option<u16> {
        terminal_size::terminal_size().map(|(width, _)| width.0)
    }

    /// This host is the real process, so `current_exe` is cargo-gamma and replacing it is safe.
    fn may_replace_process(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::Command;

    use super::*;

    /// A variable the parent gives the child, whose exact value `RealHost` has to return.
    const PRESENT_VAR: &str = "GAMMA_REAL_HOST_PRESENT";

    /// What the parent sets it to, chosen so that no ambient environment could supply it.
    const PRESENT_VALUE: &str = "a value only this test chose";

    /// A variable the parent removes from the child, whatever the launching environment held.
    const ABSENT_VAR: &str = "GAMMA_REAL_HOST_ABSENT";

    /// The real host is driven through the trait, because every other test in the workspace is not.
    ///
    /// The fakes in `cargo_gamma_lib::testing` are only worth as much as their agreement with this
    /// implementation, and that agreement is otherwise assumed: `RealHost` is excluded from coverage
    /// and is constructed in exactly one place, `main`, which no test calls. Nothing here asserts a
    /// particular terminal — the suite runs both on a tty and in CI — but it does assert the parts
    /// of the contract that hold either way, and it makes the type reachable so that a signature
    /// that stopped satisfying `Host` fails a test rather than only the release build.
    #[test]
    fn the_real_host_answers_the_same_contract_the_fakes_stand_in_for() {
        let mut host = RealHost;

        // Both streams accept a write. An empty one is used because a test that printed would be
        // indistinguishable from a test that failed noisily, and the write path is what is under
        // test, not the bytes.
        host.output().write_all(b"").expect("stdout accepts a write");
        host.error().write_all(b"").expect("stderr accepts a write");

        // A width is a terminal's property, so claiming one without being a terminal would have the
        // console wrap its progress to a width nothing has. The converse is allowed: a terminal
        // whose size cannot be interrogated is an ordinary state, and the console falls back.
        assert!(
            host.is_terminal() || host.terminal_width().is_none(),
            "a host that is not a terminal must not report a terminal width"
        );

        // This is the one answer that differs from every fake, and it is the answer that decides
        // whether the tool may replace its own process. It is true here precisely because this host
        // is the real process; a fake saying so would relaunch the test harness.
        assert!(host.may_replace_process());
    }

    /// The real host reads the real environment rather than overriding `env`.
    ///
    /// Driven through a child process this test launches itself, because the two states the
    /// host has to tell apart — set, and not set — are properties of a process environment, and
    /// this process's own is whatever the developer or the CI runner happened to export. Asserting
    /// against it reads correct code as broken in a sanitized environment, and reads broken code as
    /// correct whenever the ambient process happens to agree. Writing this process's environment to
    /// arrange the two states is not an option either: `setenv` races every other thread's read,
    /// which is why nothing in this workspace does it.
    ///
    /// So the states are arranged on the child's `Command` — one variable given a value only this
    /// test chose, one removed outright — and the child reports what the host returned for each.
    #[test]
    fn the_real_host_reads_the_process_environment() {
        let executable = env::current_exe().expect("the test binary knows its own path");

        // `--exact` confines the child to the reporting helper, and `--nocapture` exposes its
        // printed protocol to `Command::output()`.
        let output = Command::new(&executable)
            .args(["--exact", "real_host::tests::the_child_reports_what_the_host_read", "--nocapture"])
            .env(PRESENT_VAR, PRESENT_VALUE)
            .env_remove(ABSENT_VAR)
            .output()
            .expect("the child runs");

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

        let text = String::from_utf8_lossy(&output.stdout);

        // The value is compared, not merely its presence: a host that answered `Some` with the
        // name, the empty string, or another variable's value would satisfy a presence check.
        assert!(text.contains(&format!("present={:?}", Some(PRESENT_VALUE))), "{text}");
        assert!(text.contains("absent=None"), "{text}");
    }

    /// Reports what [`RealHost`] read for each variable the test above arranged.
    ///
    /// Only meaningful when launched by that test; harmless on its own, where it prints two
    /// absences and asserts nothing.
    #[test]
    fn the_child_reports_what_the_host_read() {
        let host = RealHost;

        println!("present={:?} absent={:?}", host.env(PRESENT_VAR), host.env(ABSENT_VAR));
    }
}
