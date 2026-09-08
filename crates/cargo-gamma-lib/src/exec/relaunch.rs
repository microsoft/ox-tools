// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Re-running this process inside a cgroup it is allowed to subdivide.
//!
//! Bounding a test subtree needs a cgroup cargo-gamma may create children under, and on a host that
//! never handed it one there is nothing to create them in. That is not an exotic situation: it is
//! the default everywhere a process is not started by a systemd user session — containers, CI
//! runners, `docker exec`, IDE-spawned terminals — and it is exactly where an unattended run is
//! most likely and a machine-wide out-of-memory kill is most expensive, because the kernel picks
//! its victim by heuristic and may well pick the user's editor rather than the run.
//!
//! The systemd user manager will delegate a cgroup on request, which is what
//! `systemd-run --user --scope -p Delegate=yes` does. A process cannot move *itself* into such a
//! scope after the fact — migrating between cgroups needs write access to the common ancestor of
//! the two, and the ancestor here belongs to root — so the scope has to exist before the process
//! does. Hence a relaunch rather than an adoption: cargo-gamma runs itself again inside the scope
//! and waits for it, forwarding the exit code.
//!
//! This is deliberately narrow. It happens only when memory control was wanted, only when the host
//! genuinely refused to delegate, and never more than once. Everything else — no systemd, no user
//! manager, an explicit refusal — falls through to the behaviour of not relaunching at all, which
//! is to say the diagnostic and the degradation that were there before.

use std::env;
use std::ffi::OsString;
use std::process::Command;

/// Marks a process as already being the relaunched one.
///
/// The guard has to survive into the child, so it is an environment variable rather than a flag:
/// a flag would have to be appended to a command line that is otherwise forwarded verbatim, and
/// anything appended can land after a `--` and be read as a value rather than as an option.
const MARKER: &str = "CARGO_GAMMA_SCOPE";

/// The command that asks the systemd user manager for a scope.
const SYSTEMD_RUN: &str = "systemd-run";

/// Why a relaunch was not attempted, for a caller that has to explain itself.
///
/// A relaunch that cannot happen is not an error — the run continues under the same rules as
/// before — but the reason is worth saying, because the user is about to be told that memory
/// control is unavailable and "why not just do the thing that would fix it" is the obvious next
/// question.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// This process is already the relaunched one.
    ///
    /// Reaching here means the scope was obtained and delegation still failed, so relaunching
    /// again would loop without ever changing the answer.
    AlreadyInScope,

    /// There is no working `systemd-run` to ask, or asking it did not produce a scope.
    ///
    /// Deliberately one variant rather than several. A missing `systemd-run`, an unreachable user
    /// manager and a scope that failed to start are different causes with identical consequences,
    /// and the caller's next step — explain that memory control is unavailable, then error or
    /// degrade according to whether it was asked for — is the same for all of them.
    Unavailable,
}

/// Whether this process is the result of a relaunch.
pub(crate) fn relaunched() -> bool {
    env::var_os(MARKER).is_some()
}

/// Builds the command that would re-run this process inside a delegated scope.
///
/// Split from running it so that the shape of the command is testable without a systemd on the
/// other end of it.
fn scope_command(exe: OsString, args: Vec<OsString>) -> Command {
    let mut command = Command::new(SYSTEMD_RUN);

    let _built = command
        .arg("--user")
        .arg("--scope")
        // Without this the scope is created but cargo-gamma still may not subdivide it, which is
        // the failure this exists to prevent and one that would otherwise look identical.
        .arg("--property=Delegate=yes")
        // The controller has to be enabled on the unit for a delegated tree to inherit it.
        .arg("--property=MemoryAccounting=yes")
        // Deliberately no `MemoryMax` on the scope itself. cargo-gamma bounds each test binary
        // from inside, and an outer ceiling low enough to matter would also be low enough to kill
        // an ordinary `rustc` link step — which would surface as mutants "caught" by a build
        // failure and a run that looks greener than it is.
        .arg("--quiet")
        // The unit is transient and nobody will ever look at it again; leaving spent scopes behind
        // for the user to garbage-collect would be a slow leak in their session.
        .arg("--collect")
        .arg("--same-dir")
        .arg("--")
        .arg(exe)
        .args(args)
        .env(MARKER, "1");

    command
}

/// Re-runs this process inside a delegated scope, returning its exit code.
///
/// `Ok(None)` means no relaunch was attempted and the caller should carry on as it would have.
/// An `Err` means the relaunch was attempted and failed, which is worth reporting rather than
/// swallowing: the user asked for a bound, something was supposed to provide it, and it did not.
pub(crate) fn relaunch() -> Result<Option<i32>, Refusal> {
    relaunch_unless(relaunched())
}

/// Relaunches unless this process is already the relaunched one.
///
/// The guard is a parameter rather than a call so that the recursion case can be tested without
/// setting an environment variable, which in a threaded test binary is a change every other test
/// sees.
fn relaunch_unless(marked: bool) -> Result<Option<i32>, Refusal> {
    if marked {
        return Err(Refusal::AlreadyInScope);
    }

    let Ok(exe) = env::current_exe() else {
        return Err(Refusal::Unavailable);
    };

    if which(SYSTEMD_RUN).is_none() {
        return Err(Refusal::Unavailable);
    }

    // The first argument is this program's own name, which is replaced by the resolved path to the
    // executable. `cargo gamma` reaches here as `cargo-gamma gamma ...`, so the subcommand word is
    // part of the arguments and has to be forwarded with the rest.
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    match scope_command(exe.into_os_string(), args).status() {
        // A scope that could not be started leaves the run exactly where it was, so this is a
        // refusal rather than a failure: the caller degrades or errors on its own terms.
        Err(_cause) => Err(Refusal::Unavailable),

        Ok(status) => Ok(Some(status.code().unwrap_or(crate::commands::EXIT_CANNOT_PROCEED))),
    }
}

/// Finds an executable on `PATH`.
///
/// Hand-rolled rather than taken from a crate, because the question is small and asking it wrongly
/// is cheap: a false negative means no relaunch and the old behaviour, and a false positive means
/// a spawn that fails and is handled.
fn which(program: &str) -> Option<std::path::PathBuf> {
    let paths = env::var_os("PATH")?;

    env::split_paths(&paths)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every property the relaunch depends on lives in this command line, and getting any of them
    /// wrong fails in a way that looks like the host being at fault rather than the command.
    #[test]
    fn the_scope_command_asks_for_a_delegated_cgroup_and_forwards_the_invocation() {
        let command = scope_command(
            OsString::from("/opt/cargo-gamma"),
            vec![OsString::from("gamma"), OsString::from("run")],
        );

        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(command.get_program(), "systemd-run");
        assert!(args.contains(&"--user".to_owned()), "{args:?}");
        assert!(args.contains(&"--scope".to_owned()), "{args:?}");
        assert!(args.contains(&"--property=Delegate=yes".to_owned()), "{args:?}");

        // The executable and its arguments must follow the separator, or a leading `-` in the
        // forwarded command line would be parsed as an option to `systemd-run`.
        let separator = args.iter().position(|arg| arg == "--").expect("a separator");
        assert_eq!(args[separator + 1], "/opt/cargo-gamma");
        assert_eq!(args[separator + 2..], ["gamma", "run"]);
    }

    /// The child must be able to tell that it is the child, or it relaunches forever.
    #[test]
    fn the_relaunched_process_is_marked_as_one() {
        let command = scope_command(OsString::from("/opt/cargo-gamma"), Vec::new());

        let marked = command
            .get_envs()
            .any(|(name, value)| name == MARKER && value.is_some_and(|value| !value.is_empty()));

        assert!(marked, "the marker must be set in the child's environment");
    }

    /// Relaunching from within the scope would loop, and would do so while looking like progress:
    /// each generation would print the same note and start the same run.
    #[test]
    fn a_process_already_inside_a_scope_refuses_to_relaunch_again() {
        assert_eq!(relaunch_unless(true), Err(Refusal::AlreadyInScope));
    }

    /// A program that is not on `PATH` must not be reported as found, or the relaunch spawns
    /// something that cannot exist and blames the host for the failure.
    #[test]
    #[cfg(not(miri))]
    fn an_absent_program_is_not_found_on_the_path() {
        assert!(which("cargo-gamma-nonexistent-probe").is_none());
    }
}
