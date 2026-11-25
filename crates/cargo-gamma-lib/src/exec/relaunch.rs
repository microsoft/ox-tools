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
use std::io::{Read as _, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::time::Duration;

use tick::SimpleClock;

use crate::error::error;

/// Marks a process as already being the relaunched one.
///
/// The guard has to survive into the child, so it is an environment variable rather than a flag:
/// a flag would have to be appended to a command line that is otherwise forwarded verbatim, and
/// anything appended can land after a `--` and be read as a value rather than as an option.
const MARKER: &str = "CARGO_GAMMA_SCOPE";
const STARTED: &str = "CARGO_GAMMA_SCOPE_STARTED";
const ACKNOWLEDGEMENT_LEN: usize = blake3::OUT_LEN * 2;

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
#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, reason = "Reads the inherited recursion marker; relaunch_unless tests recursion prevention without changing process-wide environment.", tag = "native-boundary")]
pub(crate) fn relaunched() -> bool {
    env::var_os(MARKER).is_some()
}

/// Acknowledges a scoped child's startup before any command work can run.
#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, reason = "Reads inherited startup markers and connects over loopback; acknowledge_start_with covers validation and delivery failures without native I/O.", tag = "native-boundary")]
pub(crate) fn acknowledge_start() -> crate::Result<()> {
    if !relaunched() {
        return Ok(());
    }

    let started = env::var_os(STARTED)
        .map(|value| {
            value
                .into_string()
                .map_err(|_value| error!("the scoped startup acknowledgement is not UTF-8"))
        })
        .transpose()?;
    acknowledge_start_with(started.as_deref(), |address, timeout| {
        let stream = TcpStream::connect_timeout(&address, timeout)?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(stream)
    })
    .map_err(|error| error!("cannot acknowledge the scoped startup before running commands: {error}"))
}

/// Keeps the inherited recursion guard without asking descendants to acknowledge our startup.
pub(crate) fn for_child(command: &mut Command) {
    _ = command.env_remove(STARTED);
}

fn acknowledge_start_with<W: Write>(
    started: Option<&str>,
    connect: impl FnOnce(std::net::SocketAddr, Duration) -> std::io::Result<W>,
) -> std::io::Result<()> {
    let Some(started) = started else {
        return Ok(());
    };
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid scoped startup acknowledgement");
    let (address, token) = started.split_once('/').ok_or_else(invalid)?;
    let address = address.parse::<std::net::SocketAddr>().map_err(|_invalid_address| invalid())?;
    if !address.ip().is_loopback() || token.len() != ACKNOWLEDGEMENT_LEN || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    connect(address, Duration::from_secs(1))?.write_all(token.as_bytes())
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
        .env(
            MARKER,
            // #[gamma::skip(literal.str_to_xyzzy, reason = "The recursion guard observes only marker presence; substituting another nonempty opaque value preserves relaunch prevention.", tag = "equivalent")]
            "1",
        );

    command
}

/// Re-runs this process inside a delegated scope, returning its exit code.
///
/// `Ok(None)` means no relaunch was attempted and the caller should carry on as it would have.
/// An `Err` means the relaunch was attempted and failed, which is worth reporting rather than
/// swallowing: the user asked for a bound, something was supposed to provide it, and it did not.
#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, reason = "This native wrapper can re-execute the harness; the shared relaunch_unless guard and scope_command arguments remain mutation-tested without spawning.", tag = "native-boundary")]
pub(crate) fn relaunch() -> Result<Option<i32>, Refusal> {
    relaunch_unless(relaunched(), relaunch_in_scope)
}

/// Relaunches unless this process is already the relaunched one.
///
/// The guard is a parameter rather than a call so that the recursion case can be tested without
/// setting an environment variable, which in a threaded test binary is a change every other test
/// sees.
fn relaunch_unless(marked: bool, attempt: impl FnOnce() -> Result<Option<i32>, Refusal>) -> Result<Option<i32>, Refusal> {
    if marked {
        return Err(Refusal::AlreadyInScope);
    }

    attempt()
}

#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, reason = "Real executable discovery, argv and systemd-run status require environment/process injection; executing this adapter under a harness can recursively relaunch tests.", tag = "native-boundary")]
fn relaunch_in_scope() -> Result<Option<i32>, Refusal> {
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

    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_unavailable| Refusal::Unavailable)?;
    listener.set_nonblocking(true).map_err(|_unavailable| Refusal::Unavailable)?;
    let address = listener.local_addr().map_err(|_unavailable| Refusal::Unavailable)?;
    let token = blake3::hash(format!("{}-{:?}", std::process::id(), SimpleClock::new_system().system_time()).as_bytes()).to_hex();
    let mut command = scope_command(exe.into_os_string(), args);
    let _command = command.env(STARTED, format!("{address}/{token}"));
    scope_status(
        &mut command,
        |command| command.status().map(|status| status.code()),
        || {
            while let Ok((mut stream, _)) = listener.accept() {
                let _timeout = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let mut received = [0; ACKNOWLEDGEMENT_LEN];
                if stream.read_exact(&mut received).is_ok() && received == token.as_bytes() {
                    return true;
                }
            }
            false
        },
    )
}

/// Attributes launcher status to Gamma only after the child independently acknowledges startup.
fn scope_status(
    command: &mut Command,
    run: impl FnOnce(&mut Command) -> std::io::Result<Option<i32>>,
    started: impl FnOnce() -> bool,
) -> Result<Option<i32>, Refusal> {
    let status = run(command).map_err(|_unavailable| Refusal::Unavailable)?;
    if !started() {
        return Err(Refusal::Unavailable);
    }
    Ok(Some(status.unwrap_or(crate::commands::EXIT_CANNOT_PROCEED)))
}

/// Finds an executable on `PATH`.
///
/// Hand-rolled rather than taken from a crate, because the question is small and asking it wrongly
/// is cheap: a false negative means no relaunch and the old behaviour, and a false positive means
/// a spawn that fails and is handled.
// #[gamma::skip(all, reason = "This adapter combines the process PATH with real filesystem metadata; deterministic coverage requires the deferred environment/filesystem seam.", tag = "native-boundary")]
fn which(program: &str) -> Option<std::path::PathBuf> {
    let paths = env::var_os("PATH")?;

    env::split_paths(&paths)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_scope_start_failure_is_not_the_campaigns_exit_status() {
        let mut command = scope_command("never-launched".into(), Vec::new());
        for code in [Some(0), Some(1), None] {
            assert_eq!(scope_status(&mut command, |_| Ok(code), || false), Err(Refusal::Unavailable));
            assert_eq!(
                scope_status(&mut command, |_| Ok(code), || true),
                Ok(Some(code.unwrap_or(crate::commands::EXIT_CANNOT_PROCEED)))
            );
        }
        assert_eq!(
            scope_status(
                &mut command,
                |_| Err(std::io::Error::other("spawn refused")),
                || panic!("no child started")
            ),
            Err(Refusal::Unavailable)
        );
    }

    #[test]
    fn startup_acknowledgement_uses_only_the_private_loopback_token() {
        let token = blake3::hash(b"scope-start-regression").to_hex();
        let address = "127.0.0.1:42".parse().unwrap();
        let mut received = Vec::new();
        acknowledge_start_with(Some(&format!("{address}/{token}")), |actual, timeout| {
            assert_eq!(actual, address);
            assert_eq!(timeout, Duration::from_secs(1));
            Ok(&mut received)
        })
        .unwrap();
        assert_eq!(received, token.as_bytes());
        for invalid in [
            format!("not-an-address/{token}"),
            format!("192.0.2.1:42/{token}"),
            "127.0.0.1:1/short".into(),
            format!("127.0.0.1:1/{}", &token.as_str()[..63]),
            format!("127.0.0.1:1/{token}0"),
            format!("127.0.0.1:1/z{}", &token.as_str()[1..]),
            format!("127.0.0.1:1/{}", "z".repeat(64)),
            "no-separator".into(),
        ] {
            let error = acknowledge_start_with(Some(&invalid), |_, _| -> std::io::Result<Vec<u8>> {
                panic!("invalid startup acknowledgement must not connect")
            })
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("startup acknowledgement"), "{error}");
        }
    }

    #[test]
    fn startup_acknowledgement_propagates_connection_and_write_failures() {
        let started = format!("127.0.0.1:42/{}", blake3::hash(b"scope-start-regression").to_hex());
        let error = acknowledge_start_with(Some(&started), |_, _| -> std::io::Result<std::io::Sink> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);

        let mut closed: &mut [u8] = &mut [];
        let error = acknowledge_start_with(Some(&started), |_, _| Ok(&mut closed)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
    }

    #[test]
    fn descendant_commands_preserve_the_guard_without_repeating_startup_delivery() {
        let mut command = Command::new("not-launched");
        _ = command.env(MARKER, "1").env(STARTED, "parent-only-address-and-token");
        for_child(&mut command);
        assert_eq!(
            command.get_envs().find(|(name, _)| *name == MARKER).unwrap().1,
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(command.get_envs().find(|(name, _)| *name == STARTED).unwrap().1, None);
        acknowledge_start_with::<std::io::Sink>(None, |_, _| panic!("descendants do not repeat startup delivery")).unwrap();
    }

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
        // The executable and its arguments must follow the separator, or a leading `-` in the
        // forwarded command line would be parsed as an option to `systemd-run`.
        let separator = args.iter().position(|arg| arg == "--").expect("a separator");
        for required in [
            "--user",
            "--scope",
            "--property=Delegate=yes",
            "--property=MemoryAccounting=yes",
            "--quiet",
            "--collect",
            "--same-dir",
        ] {
            assert!(args[..separator].iter().any(|arg| arg == required), "missing {required}: {args:?}");
        }
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
        let attempted = core::cell::Cell::new(false);
        let result = relaunch_unless(true, || {
            attempted.set(true);
            Err(Refusal::Unavailable)
        });

        assert_eq!(result, Err(Refusal::AlreadyInScope));
        assert!(!attempted.get(), "an already-marked process must not attempt a relaunch");
    }

    #[test]
    fn an_unmarked_process_attempts_relaunch_once_and_preserves_its_result() {
        let attempts = core::cell::Cell::new(0);
        let result = relaunch_unless(false, || {
            attempts.set(attempts.get() + 1);
            Ok(Some(23))
        });

        assert_eq!(result, Ok(Some(23)));
        assert_eq!(attempts.get(), 1);
        assert_eq!(relaunch_unless(false, || Ok(None)), Ok(None));
        assert_eq!(relaunch_unless(false, || Err(Refusal::Unavailable)), Err(Refusal::Unavailable));
    }

    /// A program that is not on `PATH` must not be reported as found, or the relaunch spawns
    /// something that cannot exist and blames the host for the failure.
    #[test]
    #[cfg(not(miri))]
    fn an_absent_program_is_not_found_on_the_path() {
        assert!(which("cargo-gamma-nonexistent-probe").is_none());
    }
}
