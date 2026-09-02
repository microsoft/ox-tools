// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Preparing, spawning, adopting, observing, and terminating one process tree.

use core::fmt;
use core::time::Duration;
use std::io;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::thread::{self, JoinHandle};

#[cfg(target_os = "linux")]
use cargo_gamma_unsafe::cgroup::Cgroup;
#[cfg(unix)]
use cargo_gamma_unsafe::group;
#[cfg(unix)]
use cargo_gamma_unsafe::interrupt;
#[cfg(windows)]
use cargo_gamma_unsafe::job::{self, Job};
use cargo_gamma_unsafe::{PlatformError, Situation};

#[cfg(any(test, feature = "fault-injection"))]
use crate::faults;
use crate::{MemoryRequest, MemoryUsage};

/// How many concurrent child subtrees can be watched for terminal interruption.
#[cfg(unix)]
#[must_use]
pub const fn capacity() -> usize {
    interrupt::capacity()
}

/// Platforms without a shared interrupt registry have no process-wide watch limit.
#[cfg(not(unix))]
#[must_use]
pub const fn capacity() -> usize {
    usize::MAX
}

/// Reports whether this host can hold a test subtree in a boundary it cannot leave, or why not.
///
/// A process group is not such a boundary: a descendant that calls `setsid` or `setpgid` leaves it,
/// and every later signal to the group misses it. A Linux cgroup leaf and a Windows job object both
/// are, because membership is inherited and cannot be renounced by the member.
///
/// Settled once and cached, since on Linux the answer involves creating a cgroup and possibly
/// moving this process. A run is expected to ask once, before it copies, builds or executes
/// anything the repository controls, and to say plainly that its containment is best-effort when
/// the answer is an error — the absence is invisible until a test leaves its group and outlives the
/// run holding scratch-tree locks and inherited pipes.
///
/// # Errors
///
/// Returns [`Situation::Unsupported`] carrying the reason this host cannot seal a test subtree. On
/// Linux that is the same standing fact [`support`](crate::support) reports: no cgroup v2 unified
/// hierarchy, no delegated cgroup, no memory controller to hand to children, or a kernel missing
/// the interface files a leaf needs. On every other Unix there is no unprivileged process-tree
/// boundary at all.
#[must_use = "a host that cannot seal a subtree has to be reported before repository code runs"]
#[cfg_attr(
    windows,
    expect(
        clippy::unnecessary_wraps,
        reason = "the cross-platform API reports containment errors, although Windows job objects need no fallible setup"
    )
)]
pub fn containment() -> Result<(), PlatformError> {
    #[cfg(target_os = "linux")]
    {
        crate::support()
    }

    #[cfg(windows)]
    {
        // A job object needs no delegation and no privilege, and one is created for every child.
        // #[gamma::skip(result.ok_to_err, reason = "this compile-time branch is observable only in a Windows build; Linux mutation runs cannot execute it")]
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        // #[gamma::skip(result.err_to_ok, literal.str_to_empty, literal.str_to_xyzzy, reason = "this compile-time branch exists only on non-Linux, non-Windows targets and cannot be executed by the Linux mutation run")]
        Err(PlatformError::new_static(
            Situation::Unsupported,
            "this platform offers no unprivileged boundary a test subtree cannot leave, so \
             containment reduces to a process group: a descendant that calls `setsid` escapes it \
             and survives timeout, cancellation and interrupt cleanup",
        ))
    }
}

/// The resource control installed for one spawn, carried through its launch states.
///
/// Created before the child exists because every part of it has to be: a cgroup's `cgroup.procs`
/// must be open before the fork for the child to move itself, a job object's limits must be
/// configured before a process is assigned to it, and on Unix the interrupt handler has to have
/// been told that a group is about to exist before there is one. A job also implies a second thing
/// on Windows — the command is marked to start suspended, so that [`ProcessTree::adopt`] can assign the
/// child to the job and only then let it run.
///
/// Moved into a [`SpawnedCommand`] when the spawn succeeds, then surrendered to
/// [`ProcessTree::adopt`] with that child. This bounds the Unix spawn window: the window closes
/// when the guard is dropped, and the child cannot be separated from the guard while the caller
/// still owes the adoption.
///
/// Not public, and not obtainable on its own. The only way to install containment is [`prepare`],
/// which consumes the command it prepares; see [`PreparedCommand`] for why that matters.
#[cfg_attr(not(unix), derive(Default))]
#[derive(Debug)]
pub(crate) struct SpawnGuard {
    /// The interrupt handler's promise not to finish this process until the child is watched.
    ///
    /// Held across the spawn. A signal arriving in that window would otherwise kill this process and
    /// leave the child running in a group nothing had been told about; see
    /// [`cargo_gamma_unsafe::interrupt`] for the protocol that closes it.
    #[cfg(unix)]
    spawning: interrupt::Spawning,

    /// The cgroup leaf the child will place itself in, on the platform that has them.
    ///
    /// Present for every launch the host can seal, whether or not accounting was asked for: the
    /// process group beside it is escapable and the cgroup is not. Absent only where
    /// [`containment`] already reported that this host cannot seal a subtree at all.
    #[cfg(target_os = "linux")]
    cgroup: Option<Cgroup>,

    /// Whether the caller asked for the leaf's memory accounting to be read back.
    ///
    /// A leaf used for containment alone must not have its readings reported as a
    /// measurement to a run that never asked to be measured.
    #[cfg(target_os = "linux")]
    metered: bool,

    /// The job the child will be assigned to, on the platform that has them.
    ///
    /// Its presence is also what says the child was asked to start suspended, and therefore has to
    /// be resumed once it is inside.
    #[cfg(windows)]
    job: Option<Job>,

    /// Whether memory accounting was requested, rather than containment alone.
    #[cfg(windows)]
    metered: bool,
}

impl SpawnGuard {
    /// Whether the child this guard covers will enter a boundary its descendants cannot leave.
    ///
    /// False means containment reduces to a Unix process group, which a descendant leaves with
    /// `setsid`. [`containment`] reports this per run, before anything the repository controls has
    /// been executed; this reports the same fact for one launch.
    #[must_use]
    #[cfg_attr(
        not(any(target_os = "linux", windows)),
        expect(
            clippy::unused_self,
            reason = "only the cgroup and job platforms have a boundary to report, and the signature is shared"
        )
    )]
    pub(crate) const fn sealed(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.cgroup.is_some()
        }

        #[cfg(windows)]
        {
            self.job.is_some()
        }

        #[cfg(not(any(target_os = "linux", windows)))]
        {
            false
        }
    }

    /// Closes the interrupt window while a transient spawn refusal backs off, then reopens it.
    ///
    /// # Errors
    ///
    /// Returns [`Situation::Interrupted`] when a terminal signal began taking the run apart during
    /// the wait. The window cannot be reopened then, and the caller must abandon the retry rather
    /// than create a child this process may not outlive.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "the shared API is fallible on Unix, where reopening the interrupt window can fail"
        )
    )]
    pub(crate) fn backoff(self, duration: Duration) -> Result<Self, PlatformError> {
        #[cfg(target_os = "linux")]
        {
            let Self { spawning, cgroup, metered } = self;

            drop(spawning);
            std::thread::sleep(duration);

            Ok(Self {
                spawning: window()?,
                cgroup,
                metered,
            })
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let Self { spawning } = self;

            drop(spawning);
            std::thread::sleep(duration);

            Ok(Self { spawning: window()? })
        }

        #[cfg(not(unix))]
        {
            std::thread::sleep(duration);

            Ok(self)
        }
    }
}

/// A command that has been prepared for launch, together with the containment prepared for it.
///
/// This type is how "prepared exactly once" is stated in the type system rather than checked at
/// run time. [`prepare`] takes the [`Command`] by value and never gives it back, so from that
/// moment there is no [`Command`] left for anybody to prepare a second time, and no combination of
/// safe calls can produce one: this type hands out no `&mut Command`, and a `Command` cannot be
/// cloned or replaced out of a shared or unique borrow it never grants.
///
/// A second preparation is worth this much trouble because it cannot be made to work. Preparation
/// on Linux appends a pre-exec step that moves the child into one specific cgroup leaf, and a
/// `Command` keeps every step it is given. Two preparations put two steps on one command: the child
/// walks through both leaves while only the second is reported as its boundary, and once the first
/// boundary has been dropped its leaf is gone, so the step fails and takes every later spawn from
/// that command with it. Refusing at run time was the alternative, and any run-time mark on the command
/// — an environment entry, a reserved argument — is one the caller can erase with `env_clear` or
/// rebuild around, which makes it a warning rather than a guarantee.
///
/// A spawn attempt consumes this state. Failure returns it inside [`SpawnFailure`] so a transient
/// refusal can be retried with the same preparation; success advances to [`SpawnedCommand`], which
/// has no spawning operation and can only be adopted. That transition makes it impossible to
/// leave one successful child without adoption and start another from the same boundary.
///
/// `PreparedCommand` intentionally does not implement `UnwindSafe` or `RefUnwindSafe`: on Unix,
/// `std::process::Command` retains `pre_exec` closures whose unwind safety the standard library
/// does not promise.
#[derive(Debug)]
pub struct PreparedCommand {
    /// The prepared command, reachable only by consuming this value through [`Self::spawn`].
    command: Command,

    /// The containment installed for it, moved to [`SpawnedCommand`] on success.
    guard: SpawnGuard,
}

impl PreparedCommand {
    /// Starts the child, with the containment already arranged for it.
    ///
    /// This consumes the prepared state. Success returns the distinct [`SpawnedCommand`] state,
    /// which couples the child to the containment [`ProcessTree::adopt`] must consume. Failure
    /// returns a [`SpawnFailure`] carrying this same preparation so callers can retry without
    /// preparing the command again.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Command::spawn`] returns: the child does not exist, so there is nothing
    /// here to clean up and the returned preparation remains valid for another attempt.
    pub fn spawn(mut self) -> Result<SpawnedCommand, SpawnFailure> {
        match self.command.spawn() {
            Ok(child) => {
                let Self { command: _spawned, guard } = self;

                Ok(SpawnedCommand {
                    child: Some(child),
                    guard: Some(guard),
                })
            }
            Err(cause) => Err(SpawnFailure {
                cause,
                prepared: Box::new(self),
            }),
        }
    }

    /// Whether the child this launch produces will enter a boundary its descendants cannot leave.
    ///
    /// False means containment reduces to a Unix process group, which a descendant leaves with
    /// `setsid`. [`containment`] says so once per run, before anything the repository controls has
    /// been executed; this reports the same fact for one launch.
    #[must_use]
    pub const fn sealed(&self) -> bool {
        self.guard.sealed()
    }

    /// Closes the interrupt window while a transient resource-related spawn failure backs off,
    /// then reopens it.
    ///
    /// Taken and returned by value so that the wait cannot happen with the window still open: the
    /// caller has nothing to spawn from until the window is back. The caller must classify
    /// [`SpawnFailure::cause`] before choosing this path; permanent launch failures must be
    /// propagated rather than retried.
    ///
    /// # Errors
    ///
    /// Returns [`Situation::Interrupted`] when a terminal signal began taking the run apart during
    /// the wait. The window cannot be reopened then, and the caller must abandon the retry rather
    /// than create a child this process may not outlive.
    pub fn backoff(self, duration: Duration) -> Result<Self, PlatformError> {
        let Self { command, guard } = self;

        Ok(Self {
            command,
            guard: guard.backoff(duration)?,
        })
    }
}

/// A successful spawn waiting to be adopted into a [`ProcessTree`].
///
/// This is the post-spawn state of [`PreparedCommand`]. It owns both the live child and the
/// containment prepared for that exact launch, so safe code cannot reuse the preparation or pair
/// the child with another boundary. [`ProcessTree::adopt`] consumes the bundle; dropping it before
/// adoption terminates the contained subtree and reaps its leader.
#[must_use = "a spawned child must be adopted so its process tree remains contained"]
#[derive(Debug)]
pub struct SpawnedCommand {
    child: Option<Child>,
    guard: Option<SpawnGuard>,
}

impl SpawnedCommand {
    /// Returns the operating-system identifier of the child awaiting adoption.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child
            .as_ref()
            .expect("a spawned command retains its child until adoption consumes it")
            .id()
    }

    /// Takes the child and its guard while leaving the cleanup-on-drop state empty.
    fn into_parts(mut self) -> (Child, SpawnGuard) {
        let child = self
            .child
            .take()
            .expect("a spawned command retains its child until adoption consumes it");
        let guard = self
            .guard
            .take()
            .expect("a spawned command retains its containment guard until adoption consumes it");

        (child, guard)
    }
}

impl Drop for SpawnedCommand {
    fn drop(&mut self) {
        if let (Some(mut child), Some(guard)) = (self.child.take(), self.guard.take()) {
            let _abandoned = abandon(&mut child, &guard);
        }
    }
}

/// A failed spawn together with the preparation that can be retried.
///
/// The child does not exist when this is returned. [`Self::into_parts`] recovers both the kernel's
/// reason and the unchanged [`PreparedCommand`]. Callers classify the reason and may back off and
/// try the same prepared launch again only for transient resource-related failures.
///
/// No unwind-safety auto-trait is promised because the operating-system error representation is
/// outside this crate's control.
#[derive(Debug)]
pub struct SpawnFailure {
    cause: io::Error,
    prepared: Box<PreparedCommand>,
}

impl SpawnFailure {
    /// Borrows the operating-system error that prevented the spawn.
    #[must_use]
    pub const fn cause(&self) -> &io::Error {
        &self.cause
    }

    /// Recovers the failure and the preparation that can be retried when the error is transient.
    #[must_use]
    pub fn into_parts(self) -> (io::Error, PreparedCommand) {
        (self.cause, *self.prepared)
    }
}

impl fmt::Display for SpawnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cause.fmt(f)
    }
}

impl std::error::Error for SpawnFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Why a command could not be run to completion inside a process-tree boundary.
///
/// No unwind-safety auto-trait is promised because both variants retain platform or
/// operating-system errors whose representations are outside this crate's control.
#[derive(Debug)]
pub enum OutputError {
    /// The boundary could not be prepared or could not adopt the spawned child.
    Containment(PlatformError),

    /// Spawning, waiting for, or reading from the child failed.
    Io(io::Error),
}

impl fmt::Display for OutputError {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Containment(cause) => cause.fmt(formatter),
            Self::Io(cause) => cause.fmt(formatter),
        }
    }
}

impl std::error::Error for OutputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Containment(cause) => Some(cause),
            Self::Io(cause) => Some(cause),
        }
    }
}

impl From<PlatformError> for OutputError {
    fn from(cause: PlatformError) -> Self {
        Self::Containment(cause)
    }
}

impl From<io::Error> for OutputError {
    fn from(cause: io::Error) -> Self {
        Self::Io(cause)
    }
}

/// Runs a command inside a process-tree boundary and captures its complete output.
///
/// This is the contained equivalent of [`Command::output`]. Standard input is disconnected and
/// both output streams are captured. The streams are drained concurrently while the child runs,
/// so either one may exceed the operating system's pipe capacity without deadlocking the other.
/// Once the leader exits, its descendants are terminated before their inherited pipe handles are
/// drained to end of file.
///
/// # Errors
///
/// Returns [`OutputError::Containment`] when the boundary cannot be prepared or cannot adopt the
/// child, and [`OutputError::Io`] when spawning, waiting, reading a pipe, or creating or joining a
/// reader thread fails.
pub fn output(mut command: Command, request: MemoryRequest) -> Result<Output, OutputError> {
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let prepared = prepare(command, request)?;
    let spawned = prepared.spawn().map_err(|failure| {
        let (cause, prepared) = failure.into_parts();
        drop(prepared);

        OutputError::Io(cause)
    })?;
    let subtree = ProcessTree::adopt(spawned)?;

    subtree.wait_with_output().map_err(OutputError::Io)
}

/// Arranges for a child's descendants to be killable along with it, and accounted for.
///
/// Called before spawning, because on Unix the containment has to be requested as part of the
/// spawn itself, and because the interrupt window this opens is only worth anything if it is open
/// before the child exists.
///
/// Containment is not conditional on `request`. A process group alone is escapable — a descendant
/// that calls `setsid` leaves it and survives every later signal to it — so every Windows launch
/// enters a job and every Linux launch attempts to enter a cgroup leaf whether or not anything is
/// being measured. An unmetered Linux launch may fall back to best-effort process-group containment
/// only when the host has no supported cgroup facility; [`containment`] reports that host-wide
/// limitation before repository-controlled code runs. What `request` decides is whether accounting
/// is required and whether the boundary's readings are reported as a measurement.
///
/// The command is taken by value and returned only as a [`PreparedCommand`], which is what makes a
/// second preparation of it impossible rather than merely detected: preparation appends a pre-exec
/// step naming one particular boundary, `Command` accumulates every step it is given, and a command
/// carrying two of them cannot be launched correctly. Retrying a spawn does not require preparing
/// again — [`PreparedCommand::backoff`] carries the whole launch across the wait.
///
/// # Errors
///
/// Returns [`Situation::Unsupported`] when `request` asked for measurement or a ceiling and this
/// host has no facility that can provide one. A run that asked to be protected and silently was not
/// is the failure this reports rather than swallows: the user would believe the machine was
/// bounded, and find out otherwise only when it was not.
///
/// Returns [`Situation::Refused`] when this host does have a boundary to give and this launch could
/// not be given one — a leaf that could not be created or armed, a job that could not be created,
/// terminal-signal protection that could not be installed. The launch is refused rather than
/// degraded, because a caller told that containment succeeded must not be handed an escapable
/// boundary; one refused launch costs one mutant, and a silent degradation costs the guarantee.
///
/// Returns [`Situation::Interrupted`] when an interrupt has already begun taking the run apart,
/// since a process free to die at the next instruction must not create a child that would outlive
/// it.
///
/// A host that can seal nothing at all — every Unix that is not Linux — is not an error here.
/// Containment degrades to the process group, and [`containment`] is what reports that, once,
/// before any repository-controlled code has run.
///
/// # Examples
///
/// A command is built, prepared once, spawned, and surrendered along with its child:
///
/// ```no_run
/// use std::process::Command;
///
/// use cargo_gamma_process::{MemoryRequest, ProcessTree, prepare};
///
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let prepared = prepare(Command::new("true"), MemoryRequest::default())?;
/// let spawned = prepared.spawn()?;
/// let subtree = ProcessTree::adopt(spawned)?;
/// # drop(subtree);
/// # Ok(())
/// # }
/// ```
///
/// Preparing one command twice does not compile, which is what taking it by value buys: after the
/// first preparation there is no command left to hand over again. This example is the one above
/// with the second preparation added, so it fails to build for that reason and no other:
///
/// ```compile_fail
/// use std::process::Command;
///
/// use cargo_gamma_process::{MemoryRequest, prepare};
///
/// let command = Command::new("true");
/// let first = prepare(command, MemoryRequest::default()).expect("containment");
/// let second = prepare(command, MemoryRequest::default());
/// ```
///
/// A successful spawn also consumes its preparation, so another child cannot be started before the
/// first one has been adopted:
///
/// ```compile_fail
/// use std::process::Command;
///
/// use cargo_gamma_process::{MemoryRequest, prepare};
///
/// let prepared = prepare(Command::new("true"), MemoryRequest::default()).expect("containment");
/// let spawned = prepared.spawn().expect("spawn");
/// let another = prepared.spawn();
/// ```
pub fn prepare(command: Command, request: MemoryRequest) -> Result<PreparedCommand, PlatformError> {
    // Rebound as a unique borrow for the platform arms below, which reach into the command to add
    // the pre-exec step or the suspended-start flag. The value itself never leaves this function
    // except inside a `PreparedCommand`, which is what the one-preparation rule rests on.
    #[cfg_attr(
        not(any(unix, windows)),
        expect(unused_mut, reason = "only the platforms with a boundary reach into the command")
    )]
    let mut command = command;

    #[cfg(any(test, feature = "fault-injection"))]
    if faults::fired(faults::Fault::Prepare) {
        return Err(PlatformError::new_static(
            Situation::Refused,
            "the containment a test asked to fail could not be installed",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        interrupt::arm().map_err(|cause| {
            PlatformError::because_static(Situation::Refused, "terminal-signal protection could not be installed", cause)
        })?;

        // The child leads its own process group, so a later signal to the negated group id reaches
        // every descendant that has not deliberately left the group. Without this the child shares
        // this process's group, and signalling the group would kill the run itself.
        let _ = command.process_group(0);
    }

    #[cfg(target_os = "linux")]
    {
        #[cfg(any(test, feature = "fault-injection"))]
        if faults::fired(faults::Fault::Boundary) {
            return Err(PlatformError::new_static(
                Situation::Refused,
                "the accounting boundary a test asked to fail could not be created",
            ));
        }

        // Attempted for every launch, not only for a metered one. Test listing and census asked
        // for no accounting and still run repository-controlled code, and a process group is not a
        // boundary that code cannot leave.
        let cgroup = if request.wanted() {
            Some(Cgroup::create(request.limit)?)
        } else {
            // Unsupported containment is the expected unmetered fallback, so inspect the cached
            // capability without constructing and discarding a backtrace-bearing error per child.
            Cgroup::create_unmetered_if_supported()?
        };

        if let Some(cgroup) = cgroup.as_ref() {
            cgroup.arm(&mut command)?;
        }

        // Opened last, once every fallible piece of setup is behind us. An open window defers the
        // whole run's response to `Ctrl-C`, so it is worth no more than it has to be: creating a
        // cgroup is several filesystem writes and, on the first contained spawn, can be a process
        // migration, none of which can produce a child. Nothing exists to protect until `spawn`.
        let spawning = window()?;

        Ok(PreparedCommand {
            command,
            guard: SpawnGuard {
                spawning,
                cgroup,
                metered: request.wanted(),
            },
        })
    }

    #[cfg(windows)]
    {
        // A job is the whole of Windows containment, so a launch that cannot have one is refused
        // rather than run with its descendants unreachable. Nesting has been permitted since
        // Windows 8, so this is a genuine host failure rather than the ordinary case of a run
        // started inside somebody else's job.
        let Some(job) = Job::create(request.limit) else {
            return Err(PlatformError::new_static(
                Situation::Refused,
                "a Windows job object could not be created, so this test binary's descendants could not be contained",
            ));
        };

        // The child starts suspended so that it is inside the job before it executes an
        // instruction. Assigning an already-running process would leave it a window in which it is
        // bounded by nothing, and a test that allocates immediately — the one shape a ceiling
        // exists for — would spend that window doing exactly that.
        job::start_suspended(&mut command);

        Ok(PreparedCommand {
            command,
            guard: SpawnGuard {
                job: Some(job),
                metered: request.wanted(),
            },
        })
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        if request.wanted() {
            // `support` cannot say yes on this platform, but its signature is shared with the two
            // that can, so the success case still needs an answer. Saying it plainly beats
            // `unwrap_or_else`, which reads as if it handled the error and in fact discards the
            // reason and yields `()` — this line did not compile at all until a macOS build was
            // first attempted.
            let Err(reason) = crate::support() else {
                return Err(PlatformError::new_static(
                    Situation::Unsupported,
                    "this platform cannot bound a test subtree's memory",
                ));
            };

            return Err(reason);
        }

        #[cfg(unix)]
        {
            Ok(PreparedCommand {
                command,
                guard: SpawnGuard { spawning: window()? },
            })
        }

        #[cfg(not(unix))]
        {
            Ok(PreparedCommand {
                command,
                guard: SpawnGuard::default(),
            })
        }
    }
}

/// Opens the window in which a child may exist that no interrupt can yet reach.
///
/// Taken as late as possible — after every fallible piece of containment setup and immediately
/// before the caller spawns — because a handler that finds it open kills what it can see and then
/// declines to die, so the run absorbs `Ctrl-C` for as long as any window is held. The check is the
/// spawner's half of the handshake: a caller told the run is already being interrupted must create
/// nothing, since the process is free to die at the next instruction.
#[cfg(unix)]
fn window() -> Result<interrupt::Spawning, PlatformError> {
    #[cfg(any(test, feature = "fault-injection"))]
    if faults::fired(faults::Fault::Window) {
        return Err(interrupted());
    }

    let spawning = interrupt::spawning();

    if spawning.interrupted() {
        return Err(interrupted());
    }

    Ok(spawning)
}

/// The one refusal that is neither the host's fault nor this launch's: the run is already dying.
#[cfg(unix)]
fn interrupted() -> PlatformError {
    PlatformError::new_static(
        Situation::Interrupted,
        "the run is being interrupted, so nothing further was started",
    )
}

/// A handle on a spawned child's whole subtree.
///
/// Created from an already-spawned child, and used once when the run decides the mutant has hung.
#[derive(Debug)]
pub struct ProcessTree {
    /// The leader retained until this subtree is observed or terminated.
    child: Option<Child>,

    /// The process group the child leads, on the platform that has them.
    ///
    /// Absent when the child's id does not fit a signal's idea of one, which cannot happen on any
    /// real system but is not worth an unchecked conversion to assume.
    #[cfg(unix)]
    group: Option<i32>,

    /// Where this child's group sits in the list an interrupt walks, released when it is killed.
    #[cfg(unix)]
    slot: Option<usize>,

    /// The cgroup leaf holding and, when asked, accounting for this subtree.
    ///
    /// Present for every launch on a host that can seal one, whether or not accounting was
    /// requested: it is the only Linux boundary a descendant cannot leave.
    #[cfg(target_os = "linux")]
    cgroup: Option<Cgroup>,

    /// Whether the caller asked for the leaf's memory accounting.
    #[cfg(target_os = "linux")]
    metered: bool,

    /// The dedicated job the child was placed in, on the platform that has them.
    ///
    /// Absent only in tests; production launches are refused when a job cannot be created.
    #[cfg(windows)]
    job: Option<Job>,

    /// Whether the caller asked for the job's memory accounting.
    #[cfg(windows)]
    metered: bool,
}

impl ProcessTree {
    /// Takes hold of a freshly spawned child, together with whatever [`prepare`] set up for it.
    ///
    /// The whole post-spawn state is surrendered, not just the child. [`PreparedCommand::spawn`]
    /// already consumed the preparation to create this bundle, so there is no prepared command
    /// left that could launch a sibling before this child is contained.
    ///
    /// On Windows this is also where the child is let go: it was created suspended so that it
    /// could be placed inside its job before running, and it stays that way until this puts it
    /// there.
    ///
    /// # Errors
    ///
    /// Returns [`Situation::Refused`] when the containment could not be applied to the child that
    /// was just spawned: a process id that does not fit a signal's idea of a group, an interrupt
    /// registry with no free slot, a job that would not take the child or would not let it out of
    /// suspension. This method ends and reaps that child before returning: it is not safe to hand a
    /// caller a group leader it cannot watch, because a later cleanup would no longer be able to
    /// prove that its numeric group id still belongs to this run.
    #[cfg_attr(
        not(target_os = "linux"),
        expect(
            unused_mut,
            reason = "only the cgroup platform records the interrupt slot inside the leaf it \
                      watches, which is what needs the guard's cgroup uniquely; the signature is \
                      shared"
        )
    )]
    pub fn adopt(spawned: SpawnedCommand) -> Result<Self, PlatformError> {
        let (mut child, mut guard) = spawned.into_parts();

        #[cfg(any(test, feature = "fault-injection"))]
        if faults::fired(faults::Fault::Adopt) {
            return Err(abandoning(
                PlatformError::new_static(
                    Situation::Refused,
                    "the accounting boundary a test asked to fail would not take the child",
                ),
                &mut child,
                &guard,
            ));
        }

        #[cfg(unix)]
        {
            let group = match group_id(child.id()) {
                Ok(group) => group,
                Err(reason) => {
                    return Err(abandoning(reason, &mut child, &guard));
                }
            };

            // Registered so that an interrupt can reach it. A run cut off at the terminal takes its
            // children with it only while they share its group; since they lead their own, the
            // only thing that still knows about them is this list.
            //
            // The window `prepare` opened is still open here, which is what makes the registration
            // safe rather than merely quick: a handler that ran between the spawn and this line
            // could not have finished the process, and `watch` kills the group itself if one did
            // begin. The window closes when the guard is dropped at the end of this function.
            //
            // A registration that does not take is a refusal rather than a detail to store: the
            // child would run in a group of its own that no interrupt could reach, which is exactly
            // the leak this containment exists to close. `adopt` ends the child on this error,
            // so refusing here costs one mutant and leaks nothing.
            #[cfg(target_os = "linux")]
            let watched = guard.spawning.watch_cgroup(group, guard.cgroup.as_mut());
            #[cfg(not(target_os = "linux"))]
            let watched = guard.spawning.watch(group);

            let Some(slot) = watched else {
                return Err(abandoning(
                    PlatformError::new(
                        Situation::Refused,
                        format!(
                            "process group {group} could not be watched for interrupts, so the child would have outlived a cancelled run"
                        ),
                    ),
                    &mut child,
                    &guard,
                ));
            };

            #[cfg(target_os = "linux")]
            {
                Ok(Self {
                    child: Some(child),
                    group: Some(group),
                    slot: Some(slot),
                    cgroup: guard.cgroup,
                    metered: guard.metered,
                })
            }

            #[cfg(not(target_os = "linux"))]
            {
                Ok(Self {
                    child: Some(child),
                    group: Some(group),
                    slot: Some(slot),
                })
            }
        }

        #[cfg(windows)]
        {
            if let Some(job) = guard.job.as_ref() {
                if !job.assign(&child) {
                    return Err(abandoning(
                        PlatformError::new_static(
                            Situation::Refused,
                            "a Windows job object could not be given the test binary it was created for, so its descendants could not be terminated safely",
                        ),
                        &mut child,
                        &guard,
                    ));
                }

                // The child has been waiting since it was created. It is now inside the new job,
                // where termination can reach every descendant it creates.
                if !job::release(child.id()) {
                    return Err(abandoning(
                        PlatformError::new_static(
                            Situation::Refused,
                            "a Windows test binary could not be resumed inside the containment boundary holding it",
                        ),
                        &mut child,
                        &guard,
                    ));
                }
            }

            Ok(Self {
                child: Some(child),
                job: guard.job,
                metered: guard.metered,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let SpawnGuard {} = guard;

            Ok(Self { child: Some(child) })
        }
    }

    /// Takes the child's stdout pipe before observation starts.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    /// Takes the child's stderr pipe before observation starts.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    /// Waits for the subtree to finish and captures both output streams without pipe deadlock.
    ///
    /// Each present pipe is drained on its own thread while the leader runs. After the leader exits,
    /// [`Self::observe`] terminates descendants before reaping it, which closes pipe handles those
    /// descendants inherited before this waits for the readers to reach end of file.
    ///
    /// This method accepts missing pipes and reports them as empty output, matching
    /// [`Child::wait_with_output`]. [`output`] arranges for both pipes to be present.
    ///
    /// # Errors
    ///
    /// Returns the operating system's reason when a reader thread could not be created, an output
    /// stream could not be read, descendants could not be terminated, or the child could not be
    /// observed or reaped. A reader-thread panic is reported as [`io::ErrorKind::Other`].
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        let stdout = output_reader(self.take_stdout(), "cargo-gamma-process-stdout")?;
        let stderr = match output_reader(self.take_stderr(), "cargo-gamma-process-stderr") {
            Ok(stderr) => stderr,
            Err(cause) => {
                if self.terminate().is_ok() {
                    let _drained = join_output_reader(stdout, "stdout");
                }

                return Err(cause);
            }
        };

        let status = match wait_for_output(&mut self) {
            Ok(status) => Ok(status),
            Err(cause) => {
                if self.child.is_none() || self.terminate().is_err() {
                    // Cleanup could not prove that descendants released their pipe handles.
                    // Dropping a JoinHandle detaches the reader rather than blocking this error.
                    return Err(cause);
                }

                Err(cause)
            }
        };

        // Both joins are attempted before either result is returned. If one reader failed, the
        // other must still be allowed to finish rather than being detached from this lifecycle.
        let stdout = join_output_reader(stdout, "stdout");
        let stderr = join_output_reader(stderr, "stderr");

        Ok(Output {
            status: status?,
            stdout: stdout?,
            stderr: stderr?,
        })
    }

    /// Whether this subtree is held in a boundary its descendants cannot leave.
    ///
    /// False means containment reduced to the Unix process group for this launch, which a
    /// descendant leaves with `setsid`. [`containment`] reports the same fact for the host once,
    /// before anything the repository controls has run.
    #[must_use]
    #[cfg_attr(
        not(any(target_os = "linux", windows)),
        expect(
            clippy::unused_self,
            reason = "only the cgroup and job platforms have a boundary to report, and the signature is shared"
        )
    )]
    pub const fn sealed(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.cgroup.is_some()
        }

        #[cfg(windows)]
        {
            self.job.is_some()
        }

        #[cfg(not(any(target_os = "linux", windows)))]
        {
            false
        }
    }

    /// What the platform accounted for, read once the subtree has finished.
    ///
    /// A leaf or a job now exists for containment alone, so a run that asked for no accounting is
    /// told nothing rather than handed a reading it never requested and would read as a
    /// measurement.
    #[must_use]
    #[cfg_attr(
        not(any(target_os = "linux", windows)),
        expect(
            clippy::unused_self,
            reason = "only the cgroup and job paths have anything to read, and the signature is shared"
        )
    )]
    pub fn usage(&self) -> MemoryUsage {
        #[cfg(target_os = "linux")]
        {
            usage_from_reading(self.cgroup.as_ref().filter(|_leaf| self.metered).map(Cgroup::usage))
        }

        #[cfg(windows)]
        {
            self.job.as_ref().filter(|_| self.metered).map_or_else(MemoryUsage::default, |job| {
                let (peak, exhausted) = job.usage();

                MemoryUsage { peak, exhausted }
            })
        }

        #[cfg(not(any(target_os = "linux", windows)))]
        {
            MemoryUsage::default()
        }
    }

    /// Stops an interrupt from reaching this subtree.
    ///
    /// This is always called before reaping or draining output. A watch slot holds a numeric
    /// process-group id, and leaving it active after the leader is reaped could make a later
    /// interrupt signal a reused id.
    ///
    /// On Linux the cgroup's own kill descriptor was published into the same slot, and handing it
    /// back safely — after waiting for any signal-handler sweep still using it — is the cgroup's
    /// business rather than this one's: it happens when the leaf is dropped, whether or not this
    /// ever ran.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_self,
            clippy::needless_pass_by_ref_mut,
            clippy::missing_const_for_fn,
            reason = "only the Unix watch list has a slot to hand back, and the signature is shared"
        )
    )]
    fn release(&mut self) {
        #[cfg(unix)]
        if let Some((slot, group)) = self.slot.take().zip(self.group) {
            // Named as well as numbered, so that a slot an interrupt already emptied and some
            // later spawn has since claimed is left to its new owner rather than cleared.
            interrupt::forget(slot, group);
        }
    }

    /// Revokes every capability that names this subtree by a number another process can be given.
    ///
    /// Called when an observation proved the leader was already reaped by somebody else. From that
    /// instant its pid and its process-group id are free, so a later `killpg` or `Child::kill`
    /// naming either of them could reach an unrelated replacement.
    ///
    /// The boundary that names a directory rather than a number is swept first, because it is the
    /// only capability that can still reach a descendant of the subtree that really was this run's
    /// — and this is the last moment at which that remains true.
    #[cfg(unix)]
    fn revoke_group(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = self.cgroup.as_ref() {
            cgroup.kill();
        }

        self.release();
        self.group = None;
    }

    /// Observes a completed child, kills survivors, and only then reaps its leader.
    ///
    /// Unix uses `waitid(WNOWAIT)` to observe the exit without freeing the leader's pid or group
    /// id. That keeps `killpg` tied to this subtree even if this thread is preempted. The actual
    /// reap is deliberately internal to this lifecycle operation, so callers cannot restore a
    /// post-reap cleanup window by accident.
    ///
    /// # Errors
    ///
    /// Returns the operating system's reason when the leader could not be observed, and
    /// [`io::ErrorKind::Other`] when this subtree's leader has already been reaped through an
    /// earlier call.
    ///
    /// A Unix `ECHILD` is the one error that is also a fact about the run: another waiter consumed
    /// the leader, so its pid and process-group id may already belong to somebody else. Every
    /// capability naming either of them is revoked before this returns, and every later lifecycle
    /// call on this subtree then reports an already-reaped leader rather than signalling a stranger.
    pub fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| io::Error::other("the subtree leader was already reaped"))?;

        #[cfg(unix)]
        {
            let observed = match group::exited(child.id()) {
                Ok(observed) => observed,
                Err(cause) => {
                    if group::is_no_child_to_wait_for(&cause) {
                        self.revoke_group();

                        // Dropped rather than restored. `Child` holds nothing but a pid the kernel
                        // has already released, and every later lifecycle step — `terminate`,
                        // `kill`, the drop — would signal it. Letting it go is what makes those
                        // steps report an already-reaped leader instead of reaching a stranger.
                        drop(child);

                        return Err(cause);
                    }

                    self.child = Some(child);

                    return Err(cause);
                }
            };

            match cleanup_after_observation(
                observed,
                || {
                    let swept = self.sweep();
                    self.release();

                    swept
                },
                || child.wait(),
            ) {
                Observation::Pending => {
                    self.child = Some(child);
                    Ok(None)
                }
                Observation::Reaped(status) => Ok(Some(status)),
                Observation::CleanupFailed(cause) => Err(cause),
                Observation::ReapFailed(cause) => {
                    if group::is_no_child_to_wait_for(&cause) {
                        self.revoke_group();
                        drop(child);
                    } else {
                        self.child = Some(child);
                    }

                    Err(cause)
                }
            }
        }

        #[cfg(not(unix))]
        {
            let status = match child.try_wait() {
                Ok(status) => status,
                Err(cause) => {
                    self.child = Some(child);

                    return Err(cause);
                }
            };

            if status.is_some() {
                // Windows jobs retain object handles rather than numeric process identifiers, and
                // platforms without groups have no identifier that a sweep could reuse.
                self.sweep()?;
                self.release();
            } else {
                self.child = Some(child);
            }

            Ok(status)
        }
    }

    /// Whether an interrupt can no longer reach this subtree through the watch list.
    ///
    /// Lets callers assert that release precedes output draining. The ordering is the one thing
    /// both reap paths guarantee, and it is the kind of ordering a later edit reverses without
    /// noticing.
    #[must_use]
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_self,
            reason = "only the Unix watch list has a registration to give up, and the signature is shared"
        )
    )]
    pub const fn released(&self) -> bool {
        #[cfg(unix)]
        {
            self.slot.is_none()
        }

        #[cfg(not(unix))]
        {
            true
        }
    }

    /// Kills the child and every process descended from it.
    ///
    /// Falls back to killing the child alone whenever the subtree cannot be reached, because a run
    /// that cut off one process is still better than one that cut off none.
    fn kill(&self, child: &mut Child) -> io::Result<()> {
        let swept = self.sweep();

        // The group or job may not have covered the child — the id could not be converted, the job
        // could not be created — and in any case this is what makes `wait` return.
        let killed = child.kill();

        swept.and(killed)
    }

    /// Ends the subtree and reaps its leader without exposing its group id to reuse.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Other`] when this subtree's leader has already been reaped —
    /// through an earlier [`Self::terminate`], or because [`Self::observe`] found it consumed
    /// elsewhere — and the operating system's reason when the reap itself fails.
    pub fn terminate(&mut self) -> io::Result<ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| io::Error::other("the subtree leader was already reaped"))?;

        let killed = self.kill(&mut child);
        self.release();

        let reaped = child.wait();

        killed?;
        reaped
    }

    /// Ends descendants while their leader's process-group id is still reserved.
    ///
    /// An exited leader can leave servers and inherited pipe handles behind. This private
    /// primitive is reachable only from [`Self::observe`] and [`Self::terminate`], which signal
    /// before reaping that leader, so `killpg` cannot name a replacement group.
    fn sweep(&self) -> io::Result<()> {
        // The cgroup reaches further than the process group does: a descendant that called
        // `setsid` has left the group but cannot leave the cgroup, so this goes first where it
        // exists.
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = self.cgroup.as_ref() {
            cgroup.kill();
        }

        // Signalling the group has to come first: killing the leader on its own leaves the group
        // without one, and the descendants are then reparented and unreachable.
        #[cfg(unix)]
        let killed = self.group.map_or(Ok(()), group::kill);

        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }

        #[cfg(unix)]
        return killed;

        #[cfg(not(unix))]
        Ok(())
    }

    #[cfg(all(test, unix))]
    fn id(&self) -> u32 {
        self.child.as_ref().expect("the subtree leader is live").id()
    }

    #[cfg(all(test, windows))]
    const fn child(&self) -> &Child {
        self.child.as_ref().expect("the subtree leader is live")
    }
}

fn output_reader<R>(pipe: Option<R>, name: &'static str) -> io::Result<Option<JoinHandle<io::Result<Vec<u8>>>>>
where
    R: io::Read + Send + 'static,
{
    pipe.map(|mut pipe| {
        thread::Builder::new().name(name.to_owned()).spawn(move || {
            let mut bytes = Vec::new();

            pipe.read_to_end(&mut bytes)?;

            Ok(bytes)
        })
    })
    .transpose()
}

fn join_output_reader(reader: Option<JoinHandle<io::Result<Vec<u8>>>>, stream: &str) -> io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_panic| io::Error::other(format!("the {stream} reader thread panicked")))?,
        None => Ok(Vec::new()),
    }
}

fn wait_for_output(subtree: &mut ProcessTree) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = subtree.observe()? {
            return Ok(status);
        }

        thread::sleep(Duration::from_millis(1));
    }
}

/// Maps one accounting reading into the shape callers see, or reports no accounting at all.
///
/// Split out of `usage` so the mapping — trivial once a reading exists — has a decision to test
/// against an injected reading, since a `Cgroup` that has actually measured something can only be
/// produced by a real, kernel-delegated cgroup this host does not always have.
#[cfg(target_os = "linux")]
fn usage_from_reading(reading: Option<(Option<u64>, bool)>) -> MemoryUsage {
    reading.map_or_else(MemoryUsage::default, |(peak, exhausted)| MemoryUsage { peak, exhausted })
}

/// Converts a freshly spawned child's process id into the group id `adopt` watches it under.
///
/// Pulled out of `adopt` so the failure — which cannot happen on a real Linux host, since no pid
/// ever approaches `i32::MAX` — has a decision to test on its own, without needing a process whose
/// id actually overflows a signal's idea of one.
#[cfg(unix)]
fn group_id(child_id: u32) -> Result<i32, PlatformError> {
    i32::try_from(child_id).map_err(|_out_of_range| {
        PlatformError::new_static(
            Situation::Refused,
            "the child's process id does not fit a process group id, so it could not be watched for interrupts",
        )
    })
}

/// Preserves a containment refusal while reporting failure to clean up its child.
fn abandoning(refusal: PlatformError, child: &mut Child, guard: &SpawnGuard) -> PlatformError {
    match abandon(child, guard) {
        Ok(()) => refusal,
        Err(cause) => PlatformError::because(
            Situation::Refused,
            format!("{refusal}; cleanup of the refused child also failed"),
            cause,
        ),
    }
}

/// Ends a child that failed containment before it can escape its unregistered subtree.
#[cfg_attr(
    not(any(target_os = "linux", windows)),
    expect(
        unused_variables,
        reason = "only Linux cgroups and Windows jobs carry containment state needed during abandonment"
    )
)]
fn abandon(child: &mut Child, guard: &SpawnGuard) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(cgroup) = guard.cgroup.as_ref() {
        cgroup.kill();
    }

    #[cfg(unix)]
    let group_killed = i32::try_from(child.id()).map_or(Ok(()), group::kill);

    #[cfg(windows)]
    if let Some(job) = guard.job.as_ref() {
        job.terminate();
    }

    let child_killed = child.kill();
    let reaped = child.wait().map(|_status| ());

    #[cfg(unix)]
    {
        group_killed.and(child_killed).and(reaped)
    }

    #[cfg(not(unix))]
    {
        child_killed.and(reaped)
    }
}

/// Performs the only safe order after an exit observation.
///
/// Kept separate so the regression can run the exact order against a fake process-group backend
/// that reuses the group's numeric identifier as soon as its leader is reaped.
#[cfg(any(unix, test))]
enum Observation<T> {
    Pending,
    Reaped(T),
    CleanupFailed(io::Error),
    ReapFailed(io::Error),
}

#[cfg(any(unix, test))]
fn cleanup_after_observation<T>(
    observed: bool,
    cleanup: impl FnOnce() -> io::Result<()>,
    reap: impl FnOnce() -> io::Result<T>,
) -> Observation<T> {
    if !observed {
        return Observation::Pending;
    }

    let cleaned = cleanup();
    let reaped = reap();

    match (cleaned, reaped) {
        (Ok(()), Ok(status)) => Observation::Reaped(status),
        (Err(cause), Ok(_status)) => Observation::CleanupFailed(cause),
        (Ok(()), Err(cause)) => Observation::ReapFailed(cause),
        (Err(_cleanup), Err(reap)) => Observation::ReapFailed(reap),
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _killed = self.kill(&mut child);
            self.release();
            let _reaped = child.wait();
        } else {
            self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    #[cfg(unix)]
    use core::mem;
    #[cfg(unix)]
    use std::env;
    use std::error::Error as _;
    use std::fs;
    #[cfg(unix)]
    use std::io::{BufRead as _, Write as _};
    use std::time::Instant;

    use camino::Utf8Path;

    use super::*;
    use crate::testing;

    /// Why the containment tests below are ignored by default, and how to run them.
    ///
    /// A boundary a member cannot leave is not something every host has to offer. On Linux it needs
    /// a delegated cgroup v2 subtree with the memory controller handed down, which containers, CI
    /// runners and ordinary unprivileged systemd sessions all answer differently; on every other
    /// Unix there is none at all. What the tests that need one must not do is *skip* invisibly.
    /// Returning early leaves them reported as passes, and these are the only tests standing behind
    /// measurement, sealing and the escape a process group has no answer to — so a green run on an
    /// unsupported host silently asserts nothing about any of it, and a regression stays invisible
    /// until somebody happens to run the suite somewhere that can seal. `#[ignore]` says so in the
    /// harness's own vocabulary, in every runner, without a convention anybody has to know to read.
    ///
    /// Not applied on Windows, where a job object is always available: there the capability is not
    /// conditional, so there is nothing for a reader to be told and no reason to make the coverage
    /// opt-in.
    #[cfg(not(windows))]
    const NEEDS_CONTAINMENT: &str =
        "needs a host that can seal a subtree: run with --ignored (on Linux, under `systemd-run --user --scope -p Delegate=yes`)";

    /// Fails an explicitly requested run on a host that cannot seal a subtree, saying what is
    /// missing.
    ///
    /// Reached only when somebody asked for these by name, so the answer is a failure rather than a
    /// skip: they asked for coverage of containment and did not get it, and the reason is the same
    /// one the tool itself would give a user who asked to be protected here.
    fn demand_containment() {
        #[cfg(not(windows))]
        assert!(containment().is_ok(), "{NEEDS_CONTAINMENT}: {:?}", containment().err());

        #[cfg(windows)]
        assert!(
            containment().is_ok(),
            "a Windows host always has a job object: {:?}",
            containment().err()
        );
    }

    fn wait_for(subtree: &mut ProcessTree) -> ExitStatus {
        loop {
            match subtree.observe().expect("observe") {
                Some(status) => return status,
                None => thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// A command that does nothing and exits successfully, built the same way on every platform
    /// this crate supports, so one test body can assert the same thing on all of them without a
    /// runtime branch that is always dead on whichever platform actually compiled it.
    #[cfg(windows)]
    fn no_op_command() -> Command {
        let mut command = Command::new("cmd");
        let _ = command.args(["/c", "exit 0"]);
        command
    }

    /// A command that does nothing and exits successfully, built the same way on every platform
    /// this crate supports, so one test body can assert the same thing on all of them without a
    /// runtime branch that is always dead on whichever platform actually compiled it.
    #[cfg(not(windows))]
    fn no_op_command() -> Command {
        Command::new("true")
    }

    /// Reaping this fake leader instantly reuses its numeric group id for an unrelated process.
    ///
    /// It makes the old race deterministic: a cleanup that reaped before signalling would set
    /// `replacement_signalled`, rather than requiring PID-space wraparound at just the wrong
    /// scheduler instant to expose the fault.
    #[test]
    fn an_observed_exit_sweeps_before_a_reused_group_can_be_signalled() {
        #[derive(Default)]
        struct FakeGroup {
            reaped: bool,
            original_signalled: bool,
            replacement_signalled: bool,
        }

        impl FakeGroup {
            fn sweep(&mut self) {
                if self.reaped {
                    self.replacement_signalled = true;
                } else {
                    self.original_signalled = true;
                }
            }

            fn reap(&mut self) {
                self.reaped = true;
            }
        }

        let group = RefCell::new(FakeGroup::default());
        let result = cleanup_after_observation(
            true,
            || {
                group.borrow_mut().sweep();
                Ok(())
            },
            || {
                group.borrow_mut().reap();
                Ok(())
            },
        );

        assert!(matches!(result, Observation::Reaped(())));

        let group = group.into_inner();

        assert!(group.original_signalled, "the original subtree was not swept");
        assert!(
            !group.replacement_signalled,
            "cleanup signalled the replacement group after the leader's id was reused"
        );
    }

    #[test]
    fn a_cleanup_failure_is_reported_after_the_leader_is_reaped() {
        let reaped = RefCell::new(false);
        let result = cleanup_after_observation(
            true,
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "group kill failed")),
            || {
                *reaped.borrow_mut() = true;
                Ok(())
            },
        );

        assert!(matches!(
            result,
            Observation::CleanupFailed(ref cause) if cause.kind() == io::ErrorKind::PermissionDenied
        ));
        assert!(*reaped.borrow(), "cleanup failure must not leave the child unreaped");
    }

    #[test]
    fn a_reap_failure_is_distinguished_from_a_cleanup_failure() {
        let result = cleanup_after_observation(
            true,
            || Ok(()),
            || Err::<(), _>(io::Error::new(io::ErrorKind::Interrupted, "reap failed")),
        );

        assert!(matches!(
            result,
            Observation::ReapFailed(ref cause) if cause.kind() == io::ErrorKind::Interrupted
        ));
    }

    /// A request that asks for nothing gets containment without an accounting boundary.
    #[test]
    fn a_run_that_asks_for_no_accounting_reports_no_usage() {
        // Metering is opt-in, and a run that did not ask for it must not be told a peak of zero as
        // though it were a measurement.
        let command = no_op_command();

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        let _status = wait_for(&mut subtree);

        assert_eq!(subtree.usage(), MemoryUsage::default());
    }

    #[test]
    fn a_request_that_asks_for_nothing_is_recognized_as_such() {
        assert!(!MemoryRequest::default().wanted());
        assert!(MemoryRequest { meter: true, limit: None }.wanted());
        assert!(
            MemoryRequest {
                meter: false,
                limit: Some(1)
            }
            .wanted()
        );
    }

    /// Asking for accounting on a host that cannot provide it fails rather than running anyway.
    #[test]
    fn asking_for_accounting_a_host_cannot_provide_fails_the_spawn() {
        let command = no_op_command();

        // Either the host can meter and this succeeds, or it cannot and the caller is told why.
        // What must never happen is the third thing: a run that believes it is bounded, is not,
        // and finds out when the machine runs out of memory.
        match prepare(command, MemoryRequest { meter: true, limit: None }) {
            Ok(_guard) => crate::support().expect("metering succeeded, so it is supported"),
            Err(reason) => assert!(!reason.to_string().is_empty(), "a refusal has to say why"),
        }
    }

    #[test]
    fn a_contained_child_can_still_be_run_and_waited_for() {
        // Containment must not change what a normal run does, only what a kill reaches. On Windows
        // this is also what says the child was let out of the suspension it is created in: one
        // that was never resumed would hang here rather than exit.
        let command = no_op_command();

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        assert!(wait_for(&mut subtree).success());
    }

    /// Fails an explicitly requested run on a host that cannot measure a subtree's memory.
    ///
    /// A separate question from sealing, even where the two are answered by the same facility: a
    /// host could grow a boundary it cannot meter, and a test about the reading would then fail for
    /// a reason that has nothing to do with the reading.
    fn demand_measurement() {
        #[cfg(not(windows))]
        assert!(crate::support().is_ok(), "{NEEDS_CONTAINMENT}: {:?}", crate::support().err());

        #[cfg(windows)]
        assert!(
            crate::support().is_ok(),
            "a Windows host always accounts for a job object: {:?}",
            crate::support().err()
        );
    }

    /// A metered child's peak is reported through the subtree, on a host that can measure one.
    #[test]
    #[cfg_attr(all(coverage_nightly, not(windows)), coverage(off))]
    #[cfg_attr(
        not(windows),
        ignore = "needs a host that can seal a subtree: run with --ignored (on Linux, under `systemd-run --user --scope -p Delegate=yes`)"
    )]
    fn a_metered_subtree_reports_what_it_used() {
        demand_measurement();

        // The compiled helper rather than a shell pipeline, so that this runs on every platform
        // the tool builds for. Containment and accounting are promises about somebody else's
        // machine, and a fixture that only exists on Unix leaves the whole of the job-object
        // implementation unproven.
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("eat:32"), testing::directive("exit:0")]);

        let prepared = prepare(command, MemoryRequest { meter: true, limit: None }).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        let _status = wait_for(&mut subtree);
        let usage = subtree.usage();

        // The measurement is what every derived ceiling is built from, so a boundary that
        // installed itself and then measured nothing would be worse than none at all.
        assert!(usage.peak.is_some(), "{usage:?}");
    }

    /// A subtree that never got a process group still kills the child directly, rather than
    /// silently doing nothing.
    ///
    /// The conversion from a process id to a group only fails when the id does not fit a signal's
    /// idea of one, which cannot happen on a real Linux host; but `kill` cannot assume the
    /// conversion always succeeded, and this is the fallback that keeps a wrong assumption from
    /// turning into a run that believed it had cut a mutant off while the process kept running.
    #[cfg(unix)]
    #[test]
    fn a_subtree_with_no_group_still_kills_the_child_directly() {
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "sleep 30"]);
        let mut child = command.spawn().expect("spawn");

        let subtree = ProcessTree {
            child: None,
            group: None,
            slot: None,
            #[cfg(target_os = "linux")]
            cgroup: None,
            #[cfg(target_os = "linux")]
            metered: false,
        };

        subtree.kill(&mut child).expect("kill subtree");

        let status = child.wait().expect("wait");

        // With no group to signal, the fallback still has to reach the child itself, or a subtree
        // this run could not fully identify would simply be left running.
        assert!(!status.success(), "{status:?}");
    }

    /// A subtree with no job still kills the child directly.
    ///
    /// The Windows counterpart of the fallback above. A job object can fail to be created — an
    /// existing job on the process that forbids nesting, a policy that refuses one — and
    /// containment tolerates that, because killing one process is better than killing none. What
    /// it must not do is decide it has nothing to kill.
    #[cfg(windows)]
    #[test]
    fn a_subtree_with_no_job_still_kills_the_child_directly() {
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("sleep:30000"), testing::directive("exit:0")]);

        let mut child = command.spawn().expect("spawn");
        let subtree = ProcessTree {
            child: None,
            job: None,
            metered: false,
        };

        subtree.kill(&mut child).expect("kill subtree");

        let status = child.wait().expect("wait");

        assert!(!status.success(), "{status:?}");
    }

    /// A contained child is inside a job object, which is what a later kill reaches through.
    ///
    /// The Windows counterpart of `a_contained_child_leads_its_own_process_group`. Both say the
    /// same thing in the platform's own terms: the descendants can be reached without reaching
    /// this run. It also says the child was let out of the suspension it is created in, since a
    /// process still suspended would never reach the assignment being asserted.
    #[cfg(windows)]
    #[test]
    fn a_contained_child_is_placed_inside_a_job() {
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("sleep:30000"), testing::directive("exit:0")]);

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");
        let inside = job::in_any_job(subtree.child());

        let _reaped = subtree.terminate();

        assert_eq!(inside, Some(true), "the contained child was left outside every job");
    }

    /// A subtree that never claimed a slot in the interrupt registry drops without touching it.
    ///
    /// `forget` releases whatever an interrupt would otherwise signal, and a slot of `None` means
    /// this subtree was never registered in the first place — calling it anyway would release
    /// whatever unrelated child had since taken slot zero, and an interrupt would then miss the
    /// group it was meant to kill and take a stranger's instead.
    #[cfg(unix)]
    #[test]
    fn a_subtree_with_no_watched_slot_drops_without_touching_the_registry() {
        let subtree = ProcessTree {
            child: None,
            group: None,
            slot: None,
            #[cfg(target_os = "linux")]
            cgroup: None,
            #[cfg(target_os = "linux")]
            metered: false,
        };

        drop(subtree);
    }

    /// Releasing a subtree frees its slot there and then, rather than at the drop.
    ///
    /// Every path that reaps a child releases before it drains that child's output, and the drain
    /// is the long part: a slot still naming a reaped pid is a slot naming whatever took the id
    /// next, and the interrupt handler kills what the slot names.
    #[cfg(unix)]
    #[test]
    fn releasing_a_subtree_frees_its_slot_immediately_rather_than_at_drop() {
        let group = 0x0060_0d1e;
        let spawning = interrupt::spawning();
        let slot = spawning.watch(group).expect("a free slot");

        let mut subtree = ProcessTree {
            child: None,
            group: Some(group),
            slot: Some(slot),
            #[cfg(target_os = "linux")]
            cgroup: None,
            #[cfg(target_os = "linux")]
            metered: false,
        };

        assert_eq!(interrupt::watched(slot), group, "the slot names this subtree's group");
        assert!(!subtree.released(), "a subtree still holding a slot is not released");

        subtree.release();

        assert_eq!(
            interrupt::watched(slot),
            0,
            "releasing must free the slot without waiting for the drop"
        );
        assert!(subtree.released(), "and must say so, since the drain ordering is asserted on it");

        // Whatever claims the slot next belongs to a different child, so neither a second release
        // nor the drop that follows may take it away from them.
        let taken = spawning.watch(0x0060_0d1f).expect("a free slot");

        subtree.release();
        drop(subtree);

        assert_eq!(interrupt::watched(taken), 0x0060_0d1f, "a later child's slot must survive");

        interrupt::forget(taken, 0x0060_0d1f);
    }

    #[cfg(unix)]
    #[test]
    fn a_subtree_reaped_elsewhere_revokes_its_numeric_group_capabilities() {
        let group = 0x0060_0d1e;
        let spawning = interrupt::spawning();
        let slot = spawning.watch(group).expect("a free slot");
        let mut subtree = ProcessTree {
            child: None,
            group: Some(group),
            slot: Some(slot),
            #[cfg(target_os = "linux")]
            cgroup: None,
            #[cfg(target_os = "linux")]
            metered: false,
        };

        subtree.revoke_group();

        assert!(subtree.released(), "the stale interrupt slot remains active");
        assert_eq!(subtree.group, None, "drop could still signal a reused process-group id");
    }

    /// Forgetting a slot past the end of the registry does nothing, rather than panicking.
    ///
    /// The slot a `ProcessTree` carries always came from `watch`, which never hands out an
    /// out-of-range index; but `forget` runs from a `Drop` implementation, where a panic would abort
    /// whatever mutant run was unwinding through it instead of merely leaving one interrupt
    /// registration stale.
    #[cfg(unix)]
    #[test]
    fn forgetting_an_out_of_range_slot_does_nothing() {
        interrupt::forget(usize::MAX, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_contained_child_leads_its_own_process_group() {
        // Which is what makes the group signal reach the descendants without reaching this run.
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "sleep 30"]);

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let spawned = prepared.spawn().expect("spawn");
        let id = i32::try_from(spawned.id()).expect("pid");

        assert_eq!(group::group_of(id), Some(id));

        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");
        let _reaped = subtree.terminate();
    }

    /// Killing a subtree stops a grandchild that outlived its parent, on every platform.
    ///
    /// The whole point of containment: a test that spawned something is not cut off by killing the
    /// harness. The grandchild deliberately outlives its parent, which is the shape that leaves an
    /// orphan holding the scratch tree and the caller's pipe.
    ///
    /// Written against files rather than against pids, so that it says the same thing on Unix,
    /// where a survivor would be found with signal zero, and on Windows, where the equivalent is a
    /// handle query. It is also the stronger claim of the two: a killed process that had already
    /// done its work is not contained in any sense that matters, and a liveness check would not
    /// notice.
    #[test]
    fn killing_the_subtree_reaches_a_grandchild() {
        let work = testing::workdir("gamma-grandchild");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, finished) = (base.join("started"), base.join("finished"));

        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.arg(testing::directive(format_args!(
            "spawn:touch:{started}|sleep:2000|touch:{finished}"
        )));
        let _ = command.arg(testing::directive("sleep:30000"));

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        // Waited for rather than assumed: killing before the grandchild exists would pass without
        // containment reaching anything at all.
        let mut running = false;

        for _attempt in 0..600 {
            if started.exists() {
                running = true;
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(running, "the grandchild never started, so the kill proves nothing");

        let _reaped = subtree.terminate();

        // Past the grandchild's own sleep with margin, so an unreached one has had every chance to
        // finish its work and say so.
        thread::sleep(Duration::from_secs(4));

        assert!(!finished.exists(), "the grandchild kept working after the subtree was killed");
    }

    /// Killing a subtree reaches a grandchild that left the process group.
    ///
    /// This is the escape a process group has no answer to. `setsid` and `setpgid` cost one
    /// unprivileged call, and afterwards every signal to the group misses the escapee: it outlives
    /// the timeout, the cancellation and the interrupt cleanup, holding the scratch tree and the
    /// pipe the run is still reading. Build scripts, test harnesses and daemons started by tests do
    /// this deliberately.
    ///
    /// A cgroup leaf and a job object are boundaries a member cannot renounce, which is why one is
    /// created for every launch rather than only for a launch that asked to be measured. The
    /// request here asks for neither measurement nor a ceiling, so a containment tied to accounting
    /// would leave this grandchild running.
    #[test]
    #[cfg_attr(all(coverage_nightly, not(windows)), coverage(off))]
    #[cfg_attr(
        not(windows),
        ignore = "needs a host that can seal a subtree: run with --ignored (on Linux, under `systemd-run --user --scope -p Delegate=yes`)"
    )]
    fn killing_the_subtree_reaches_a_grandchild_that_left_the_process_group() {
        demand_containment();

        let work = testing::workdir("gamma-escapee");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, finished) = (base.join("started"), base.join("finished"));

        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.arg(testing::directive(format_args!("flee:touch:{started}|sleep:2000|touch:{finished}")));
        let _ = command.arg(testing::directive("sleep:30000"));

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        assert!(
            prepared.sealed(),
            "a host that reports it can seal a subtree must seal one that asked for no accounting"
        );

        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        assert!(subtree.sealed(), "the adopted subtree gave up the boundary its guard carried");

        let mut running = false;

        for _attempt in 0..600 {
            if started.exists() {
                running = true;
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(running, "the escapee never started, so the kill proves nothing");

        let _reaped = subtree.terminate();

        thread::sleep(Duration::from_secs(4));

        assert!(
            !finished.exists(),
            "a grandchild that left the process group survived the subtree being killed"
        );
    }

    /// An unmetered launch is still sealed, and still reports no measurement.
    ///
    /// The two halves of the same decision. Containment stopped being conditional on the request,
    /// so the request now decides one thing only: whether the boundary's readings are reported. A
    /// leaf created for containment alone must not start answering a question nobody asked.
    #[test]
    #[cfg_attr(all(coverage_nightly, not(windows)), coverage(off))]
    #[cfg_attr(
        not(windows),
        ignore = "needs a host that can seal a subtree: run with --ignored (on Linux, under `systemd-run --user --scope -p Delegate=yes`)"
    )]
    fn an_unmetered_launch_is_sealed_but_reports_no_usage() {
        demand_containment();

        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.arg(testing::directive("exit:0"));

        let request = MemoryRequest::default();

        assert!(!request.wanted(), "the default request asks for neither measurement nor a ceiling");

        let prepared = prepare(command, request).expect("containment");

        assert!(prepared.sealed(), "an unmetered launch was left in an escapable boundary");

        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        let _reaped = subtree.terminate();

        assert_eq!(
            subtree.usage(),
            MemoryUsage::default(),
            "a boundary created for containment alone reported a measurement nobody asked for"
        );
    }

    /// What the host says about sealing is what every launch on it then does.
    ///
    /// A run announces best-effort containment once, before it executes anything the repository
    /// controls, and that announcement is worth nothing if the per-launch answer can differ from it.
    #[test]
    fn the_host_answer_about_sealing_matches_what_a_launch_gets() {
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.arg(testing::directive("exit:0"));

        let Ok(prepared) = prepare(command, MemoryRequest::default()) else {
            // A refusal is the other permitted answer, and never a silent degradation.
            return;
        };

        assert_eq!(
            prepared.sealed(),
            containment().is_ok(),
            "a launch was sealed differently from what the host reported before the run started"
        );

        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");
        let _reaped = subtree.terminate();
    }

    #[test]
    fn dropping_an_unreleased_subtree_reaps_its_leader_and_grandchild() {
        let work = testing::workdir("gamma-drop-grandchild");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, finished) = (base.join("started"), base.join("finished"));

        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command
            .arg(testing::directive(format_args!(
                "spawn:touch:{started}|sleep:2000|touch:{finished}"
            )))
            .arg(testing::directive("sleep:30000"));

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(spawned).expect("adoption");

        for _attempt in 0..600 {
            if started.exists() {
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(started.exists(), "the grandchild never started");
        drop(subtree);
        thread::sleep(Duration::from_secs(3));
        assert!(!finished.exists(), "the dropped subtree left its grandchild alive");
    }

    /// The fixture the test above rests on really does outlive its parent and really does finish.
    ///
    /// Without this, a helper whose `spawn` quietly did nothing would make that test pass by
    /// producing no grandchild to reach — the exact shape of a test that cannot fail.
    #[test]
    fn an_unkilled_grandchild_outlives_its_parent_and_finishes() {
        let work = testing::workdir("gamma-grandchild-control");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, release, finished) = (base.join("started"), base.join("release"), base.join("finished"));

        let mut child = Command::new(testing::helper_binary_path().as_std_path())
            .arg(testing::directive(format_args!(
                "spawn:touch:{started}|wait:{release}|touch:{finished}"
            )))
            .arg(testing::directive("exit:0"))
            .spawn()
            .expect("spawn");

        assert!(child.wait().expect("wait").success());

        let mut grandchild_started = false;
        for _attempt in 0..600 {
            if started.exists() {
                grandchild_started = true;
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(!finished.exists(), "the grandchild ignored the release gate");
        fs::write(release.as_std_path(), "").expect("release the orphaned grandchild");
        assert!(grandchild_started, "the grandchild never started after its parent exited");

        for _attempt in 0..600 {
            if finished.exists() {
                return;
            }

            thread::sleep(Duration::from_millis(10));
        }

        panic!("the orphaned grandchild never finished its work");
    }

    /// Runs one of the self-interrupting inner tests below, and reports the pid it spawned.
    ///
    /// In a subprocess, because the thing under test ends by re-raising a terminal signal and
    /// killing whoever is running it.
    #[cfg(unix)]
    fn interrupted_run(inner: &str) -> i32 {
        let program = env::args().next().expect("this test binary");
        let mut child = Command::new(&program)
            .args(["--exact", "--nocapture", &format!("process_tree::tests::{inner}")])
            .env("GAMMA_INTERRUPT_CHILD", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        // Read line by line and only as far as the marker, rather than to end of file: the child
        // the inner test spawns inherits this pipe, so reading it to the end would wait out that
        // child — and this test is about to ask whether the interrupt cut it short, which is a
        // question already answered yes by anything that waited for it first.
        //
        // The marker is looked for anywhere in a line rather than as a line of its own, because the
        // harness running the inner test writes its own progress on the same line when it is
        // running single-threaded, which is what an inherited `RUST_TEST_THREADS` makes it do.
        let reported = {
            let said = child.stdout.take().expect("pipe");

            io::BufReader::new(said)
                .lines()
                .find_map(|line| {
                    let text = line.ok()?;
                    let (_before, rest) = text.split_once("grandchild ")?;

                    rest.split_whitespace().next()?.parse().ok()
                })
                .expect("the inner test reports the pid it spawned")
        };

        // The inner process dies of the signal it raised, one way or another; this is only here so
        // it is reaped rather than left as a zombie for the rest of the suite.
        let _status = child.wait();

        reported
    }

    /// Marks a re-run of this test binary as the inner half of an isolated test.
    ///
    /// The name is on the child's environment rather than on its command line because the command
    /// line belongs to the test harness, which would reject an argument it does not know.
    #[cfg(unix)]
    const ISOLATED_CHILD: &str = "GAMMA_ISOLATED_CHILD";

    /// Runs one of the inner tests below in a process of its own, and pins what it reported.
    ///
    /// For the tests whose subject is process-global state that cannot be put back: the interrupt
    /// registry's write-once "a run is being interrupted" flag, and the fixed set of watch slots
    /// every thread in the process shares. Run in this process, each would sabotage whatever the
    /// harness happened to schedule beside or after it — a refusal every later `prepare` inherits,
    /// or a registry with no free slot — and, being a matter of scheduling, would do so on some
    /// machines and not others. A subprocess gives each one a registry it is free to ruin.
    ///
    /// The marker is required rather than the exit status alone, because a filter that matched
    /// nothing — a renamed inner test, say — is also a successful run of zero tests.
    #[cfg(unix)]
    fn isolated_run(inner: &str, marker: &str) {
        let program = env::args().next().expect("this test binary");
        let outcome = Command::new(&program)
            .args(["--exact", "--nocapture", &format!("process_tree::tests::{inner}")])
            .env(ISOLATED_CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("this test binary can be re-run");

        let said = String::from_utf8_lossy(&outcome.stdout);
        let complained = String::from_utf8_lossy(&outcome.stderr);

        assert!(
            outcome.status.success(),
            "the isolated `{inner}` failed: {:?}\n{said}\n{complained}",
            outcome.status
        );
        assert!(
            said.contains(marker),
            "the isolated `{inner}` did not report `{marker}`\n{said}\n{complained}"
        );
    }

    /// Whether a process has gone within a couple of seconds, which is what a kill looks like.
    #[cfg(unix)]
    fn gone_soon(pid: i32) -> bool {
        for _attempt in 0..200 {
            if !group::exists(pid) {
                return true;
            }

            thread::sleep(Duration::from_millis(10));
        }

        false
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_takes_the_children_with_it() {
        // A child in its own process group no longer dies with the terminal's `Ctrl-C`, so the run
        // has to take it along deliberately.
        testing::within(testing::WATCHDOG, "an interrupt with a child watched", || {
            let reported = interrupted_run("spawns_a_child_then_interrupts_itself");

            assert!(gone_soon(reported), "the child outlived the interrupt");
        });
    }

    /// A signal arriving between the spawn and the registration still takes the child.
    ///
    /// The window that would lose it: the child exists, leads a process group of its own, and
    /// nothing has been told about that group, so a handler scanning the registry finds nothing to
    /// kill and re-raises — the run dies of the signal and the child, which the terminal's signal
    /// never reached either, goes on running. Delivered on the spawning thread itself, which is
    /// both the deterministic way to land in the window and the case a handler cannot wait its way
    /// out of.
    #[cfg(unix)]
    #[test]
    fn an_interrupt_inside_the_spawn_window_still_takes_the_child() {
        testing::within(testing::WATCHDOG, "an interrupt inside the spawn window", || {
            let reported = interrupted_run("interrupts_itself_between_the_spawn_and_the_registration");

            assert!(gone_soon(reported), "the child created inside the spawn window survived");
        });
    }

    /// The inner half of the test above: contains a child, then interrupts itself.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs in.
    #[cfg(unix)]
    #[test]
    fn spawns_a_child_then_interrupts_itself() {
        if env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        // Deliberately never waited on: this process is about to be killed by the signal it is
        // here to raise, and the point of the test is what happens to the child when it is.
        let spawned = prepared.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(spawned).expect("adoption");

        println!("grandchild {}", subtree.id());

        // Kept alive so the slot stays claimed, which is exactly the state a run is in when the
        // interrupt arrives.
        mem::forget(subtree);

        group::raise_interrupt();

        thread::sleep(Duration::from_secs(30));
    }

    /// The inner half of the window test: interrupts itself with the child spawned and unwatched.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs
    /// in. The signal is raised on this thread, so the handler runs here — the one case a handler
    /// could not have waited its way out of, because the thread it would be waiting for is the one
    /// it is running on.
    #[cfg(unix)]
    #[test]
    fn interrupts_itself_between_the_spawn_and_the_registration() {
        if env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let spawned = prepared.spawn().expect("spawn");

        // Said before the signal, because nothing after the adoption below runs: the guard's window
        // closes inside it, and closing the last window of an interrupted run ends this process.
        println!("grandchild {}", spawned.id());

        group::raise_interrupt();

        // Reached only because the handler deferred rather than re-raising, which is the protocol
        // this test exists for. Registering the group is also what kills it.
        let subtree = ProcessTree::adopt(spawned).expect("adoption");

        mem::forget(subtree);

        thread::sleep(Duration::from_secs(30));
    }

    /// The terminal's *other* stop keystroke takes the children too.
    ///
    /// `Ctrl-\\` sends `SIGQUIT` to the whole foreground process group — but containment has
    /// already moved every child out of that group precisely so the terminal cannot reach it, so a
    /// signal this run does not handle kills the run alone and leaves the entire subtree alive
    /// holding its scratch trees. Handling `SIGINT` and not `SIGQUIT` is therefore not a smaller
    /// version of the same guarantee, it is the absence of it on one keystroke.
    #[cfg(unix)]
    #[test]
    fn a_quit_takes_the_children_with_it_too() {
        testing::within(testing::WATCHDOG, "a quit with a child watched", || {
            let reported = interrupted_run("spawns_a_child_then_quits_itself");

            assert!(gone_soon(reported), "the child outlived the quit");
        });
    }

    /// A second signal arriving during a deferred interrupt still ends with the child dead.
    ///
    /// The disposition has to survive its own delivery for this to hold. Where it does not — the
    /// System V reading of `signal()`, which POSIX permits — the first signal resets the handler to
    /// the default, and since that first handler *deferred* rather than dying, the second signal
    /// terminates the run outright: nothing sweeps the registry, and every contained child survives
    /// in a group nothing signalled. Inert on glibc, musl and the BSDs, which all give the other
    /// reading; the explicit `sigaction` flags are what make it true everywhere, and
    /// `every_terminal_signal_is_armed_to_survive_its_own_delivery` is what checks them.
    #[cfg(unix)]
    #[test]
    fn a_second_interrupt_during_a_deferred_one_still_takes_the_child() {
        testing::within(testing::WATCHDOG, "a second interrupt while deferred", || {
            let reported = interrupted_run("interrupts_itself_twice_with_a_window_open");

            assert!(gone_soon(reported), "the child survived the second interrupt");
        });
    }

    /// The inner half of the quit test: contains a child, then quits on itself.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs in.
    #[cfg(unix)]
    #[test]
    fn spawns_a_child_then_quits_itself() {
        if env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let spawned = prepared.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(spawned).expect("adoption");

        println!("grandchild {}", subtree.id());

        // Kept alive so the slot stays claimed, which is the state a run is in when the signal
        // arrives.
        mem::forget(subtree);

        group::raise_quit();

        thread::sleep(Duration::from_secs(30));
    }

    /// The inner half of the two-signal test: defers one interrupt, then takes another.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs
    /// in. The child is watched before either signal, and a second window is then held open so the
    /// handler defers instead of dying — which is the only state in which a reset disposition can
    /// be observed, because it is the only one where the run is still alive to receive a second
    /// signal. Closing that window is what finally performs the death, and the sweep with it.
    #[cfg(unix)]
    #[test]
    fn interrupts_itself_twice_with_a_window_open() {
        if env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let spawned = prepared.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(spawned).expect("adoption");

        // A second window, for a spawn that never happens: it is what makes the handler defer.
        let mut pending = Command::new("sleep");
        let _ = pending.arg("60");

        let held = prepare(pending, MemoryRequest::default()).expect("containment");

        // Said before either signal, because the run ends inside the drop below.
        println!("grandchild {}", subtree.id());

        mem::forget(subtree);

        group::raise_interrupt();

        // Reached only because the handler deferred. Under a disposition that reset on delivery,
        // this second raise is the default action and the child below is never swept.
        group::raise_interrupt();

        // Closing the last window is what kills the watched group and then this process.
        drop(held);

        thread::sleep(Duration::from_secs(30));
    }

    /// The accounting boundary is built before the spawn window opens, not inside it.
    ///
    /// An open window is the run's whole response to `Ctrl-C` held back: `Registry::interrupt` sees
    /// a spawn in flight, declines to die, and defers to whoever closes the last window. That is
    /// exactly right for the interval it exists to cover — a child that exists but is not yet
    /// watched — and exactly wrong for anything else, because nothing bounds how long the other
    /// work takes. Creating a cgroup is a `mkdir` and several interface-file writes, and on the
    /// first contained spawn of a run it can be a whole controller discovery and a process
    /// migration; on a filesystem that stalls, holding the window across it turns `SIGINT`,
    /// `SIGTERM`, `SIGHUP` and `SIGQUIT` into signals the run absorbs until somebody reaches for
    /// `SIGKILL`. Nothing exists to protect until `spawn`, so the window is worth opening only
    /// after every fallible step before it has succeeded.
    ///
    /// Read from which failure comes back when both steps are asked to fail: the first one to run
    /// is the one that reports, so this pins the order rather than the timing, which is the part a
    /// future edit could quietly reverse.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_accounting_boundary_is_built_before_the_spawn_window_opens() {
        let boundary = faults::arm(faults::Fault::Boundary);
        let window = faults::arm(faults::Fault::Window);

        let command = Command::new("true");
        let reason = prepare(command, MemoryRequest { meter: true, limit: None })
            .expect_err("both steps were asked to fail, so containment cannot have succeeded");

        assert!(
            reason.to_string().contains("accounting boundary"),
            "the spawn window was opened before the accounting boundary was built: {reason}"
        );

        drop((boundary, window));
    }

    /// This crate's own watch-limit wrapper reports the shared registry's real capacity.
    #[cfg(unix)]
    #[test]
    fn capacity_matches_the_shared_interrupt_registry() {
        assert_eq!(capacity(), interrupt::capacity());
    }

    /// Backing off closes the spawn window, waits, and reopens a fresh one for the retry.
    #[test]
    fn backing_off_reopens_the_window_after_the_wait() {
        let command = no_op_command();
        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let prepared = prepared
            .backoff(Duration::from_millis(1))
            .expect("the window reopens after the wait");

        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        assert!(wait_for(&mut subtree).success());
    }

    /// A window fault refuses to open a spawn window, rather than reaching the real registry.
    #[cfg(unix)]
    #[test]
    fn a_window_fault_refuses_to_open() {
        let armed = faults::arm(faults::Fault::Window);

        let reason = window().expect_err("the fault should refuse the window");

        assert!(reason.to_string().contains("being interrupted"), "{reason}");

        drop(armed);
    }

    /// Once a real interrupt has begun, a fresh window refuses to open — proven with a real signal
    /// rather than a fault, since the two checks inside `window` are otherwise indistinguishable.
    ///
    /// Run in a subprocess of its own. The registry's "a run is being interrupted" flag is
    /// process-wide and write-once, and the window this test has to hold open to survive the signal
    /// is a slot that is never given back, so an in-process run would leave every later `prepare`
    /// in the suite refusing to open a window and one slot short forever. Which tests that hit is
    /// a matter of harness scheduling, so the damage would show up on some machines and not others.
    #[cfg(unix)]
    #[test]
    fn window_refuses_to_open_once_a_real_interrupt_has_begun() {
        testing::within(testing::WATCHDOG, "a window refused after a real interrupt", || {
            isolated_run("a_real_interrupt_closes_the_registry_to_new_windows", "the window was refused");
        });
    }

    /// The inner half of the test above: interrupts its own process and asks for another window.
    ///
    /// Inert unless the outer test asked for it, since it ruins the registry of whatever process it
    /// runs in. The interrupt is delivered while a spawn window is already held open, so the
    /// production handler defers instead of dying: this is real `SIGINT` delivery through the real
    /// handler, not a substitute. The held window is never dropped for real afterward — only
    /// forgotten — so that `Spawning::drop`'s own real reaction to the now-interrupted registry
    /// (dying of the signal) cannot run and turn the clean exit of this subprocess into a signal death
    /// the outer test would read as a failure.
    #[cfg(unix)]
    #[test]
    fn a_real_interrupt_closes_the_registry_to_new_windows() {
        if env::var_os(ISOLATED_CHILD).is_none() {
            return;
        }

        interrupt::arm().expect("the interrupt handlers install on this host");

        let holding = interrupt::spawning();

        group::raise_interrupt();

        let reason = window().expect_err("a window must not open once a real interrupt has begun");

        assert!(reason.to_string().contains("being interrupted"), "{reason}");

        mem::forget(holding);

        println!("the window was refused");
    }

    /// A prepare fault refuses to install containment at all, before anything is spawned.
    #[test]
    fn a_prepare_fault_refuses_to_install_containment() {
        let armed = faults::arm(faults::Fault::Prepare);

        let command = no_op_command();
        let reason = prepare(command, MemoryRequest::default()).expect_err("the fault should refuse containment");

        assert!(reason.to_string().contains("could not be installed"), "{reason}");

        drop(armed);
    }

    /// A failed spawn returns the one preparation so the same launch can be attempted again.
    ///
    /// The Linux path appends a pre-exec step that moves the child into one specific leaf, and a
    /// `Command` keeps every step it is handed. A command prepared twice would walk its child
    /// through two leaves while reporting only one as its boundary, or — once the first boundary
    /// had been dropped and its leaf removed — through a directory that no longer exists, failing
    /// the spawn outright. Neither is reachable, because [`prepare`] consumes the command and
    /// [`PreparedCommand`] never hands its command back. Instead, [`SpawnFailure`] returns the
    /// prepared state only when no child was created; a success advances to [`SpawnedCommand`] and
    /// cannot spawn again. This pins that retry belongs only to the failed-spawn transition.
    #[test]
    fn a_failed_spawn_returns_the_preparation_for_retry() {
        let sandbox = tempfile::tempdir().expect("temporary directory");
        let working_directory = sandbox.path().join("created-after-first-attempt");
        let mut command = no_op_command();
        let _ = command.current_dir(&working_directory);
        let prepared = prepare(command, MemoryRequest::default()).expect("containment");

        let failed = prepared
            .spawn()
            .expect_err("the absent working directory makes the first spawn fail");
        assert!(
            matches!(failed.cause().kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory),
            "the missing working directory should prevent the spawn: {failed}"
        );
        assert!(!failed.to_string().is_empty());
        let source = failed
            .source()
            .and_then(|cause| cause.downcast_ref::<io::Error>())
            .expect("the source is the spawn error");
        assert_eq!(source.kind(), failed.cause().kind());
        let (_reason, prepared) = failed.into_parts();

        fs::create_dir(&working_directory).expect("working directory");

        let spawned = prepared.spawn().expect("the returned preparation spawns");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        assert!(wait_for(&mut subtree).success());
    }

    /// An adopt fault abandons the freshly spawned child and refuses to hand back a subtree.
    #[cfg(unix)]
    #[test]
    fn an_adopt_fault_abandons_the_child_and_refuses() {
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "sleep 30"]);

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let pid = i32::try_from(spawned.id()).expect("pid fits");

        let armed = faults::arm(faults::Fault::Adopt);
        let reason = ProcessTree::adopt(spawned).expect_err("the fault should refuse adoption");

        assert!(reason.to_string().contains("would not take the child"), "{reason}");
        assert!(gone_soon(pid), "an adopt fault must still kill the child it refused");

        drop(armed);
    }

    /// Once every watch slot is taken, adoption is refused rather than left unwatched.
    ///
    /// Run in a subprocess of its own, because the registry it has to exhaust is process-wide and
    /// there is exactly one of it: while the slots are held, every other test in the binary that
    /// prepares or adopts anything would be refused, and every test that had already taken a slot
    /// would leave this one unable to fill the registry it is here to fill. Both directions of that
    /// interference depend on what the harness happens to be running alongside it.
    #[cfg(unix)]
    #[test]
    fn adoption_refuses_a_child_once_every_slot_is_taken() {
        testing::within(testing::WATCHDOG, "adoption refused with every slot taken", || {
            isolated_run("a_saturated_registry_refuses_an_adoption", "the adoption was refused");
        });
    }

    /// The inner half of the test above: fills every watch slot, then tries to adopt a child.
    ///
    /// Inert unless the outer test asked for it, since it takes the whole registry away from
    /// whatever else is running. Filled with fake group ids on the guard's own interrupt registry,
    /// so that the real spawn below finds every slot already claimed without needing anywhere near
    /// a thousand real processes.
    #[cfg(unix)]
    #[test]
    fn a_saturated_registry_refuses_an_adoption() {
        if env::var_os(ISOLATED_CHILD).is_none() {
            return;
        }

        let command = no_op_command();

        // Prepared before the registry is filled, since preparing opens a window of its own and
        // there would be no slot left for it afterward. The refusal under test is adoption's, not
        // preparation's.
        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let window = interrupt::spawning();
        let mut watched = Vec::new();

        for offset in 0..interrupt::capacity() {
            let group = i32::try_from(offset + 1).expect("small positive group id");

            if let Some(slot) = window.watch(group) {
                watched.push((slot, group));
            }
        }

        let spawned = prepared.spawn().expect("spawn");
        let pid = i32::try_from(spawned.id()).expect("pid fits");

        let reason = ProcessTree::adopt(spawned).expect_err("every slot is already taken");

        assert!(reason.to_string().contains("could not be watched"), "{reason}");
        assert!(gone_soon(pid), "a refused adoption must still kill the child");

        for (slot, group) in watched {
            interrupt::forget(slot, group);
        }

        println!("the adoption was refused");
    }

    /// This process's own registry is still usable, which is what the isolation above is worth.
    ///
    /// A window that opens and a slot that can be claimed are exactly the two things the two
    /// isolated tests would have taken away had they run here. Asserted from the shared process so
    /// that a future test which ruins the registry in place is caught by name rather than by
    /// whichever unrelated test the harness happened to schedule after it.
    #[cfg(unix)]
    #[test]
    fn the_shared_registry_is_left_usable_by_the_isolated_tests() {
        let holding = window().expect("no test may leave this process's registry interrupted");
        let group = 0x0060_0d7e;
        let slot = holding.watch(group).expect("no test may leave this process's registry full");

        interrupt::forget(slot, group);
        drop(holding);
    }

    /// A child id is converted into the group id `adopt` watches it under, or a reason is given
    /// when it does not fit — which cannot happen on a real Linux host, since no pid ever
    /// approaches `i32::MAX`, but the decision is still worth testing on its own.
    #[cfg(unix)]
    #[test]
    fn a_child_id_that_does_not_fit_a_process_group_id_is_refused() {
        let reason = group_id(u32::MAX).expect_err("u32::MAX never fits an i32");

        assert!(reason.to_string().contains("does not fit a process group id"), "{reason}");
    }

    #[cfg(unix)]
    #[test]
    fn a_child_id_that_fits_becomes_its_own_group_id() {
        assert_eq!(group_id(1234).expect("1234 fits an i32"), 1234);
    }

    /// Once a reading exists, its peak and exhaustion are carried through unchanged.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_present_reading_is_reported_as_the_subtrees_usage() {
        let usage = usage_from_reading(Some((Some(4096), true)));

        assert_eq!(
            usage,
            MemoryUsage {
                peak: Some(4096),
                exhausted: true
            }
        );
    }

    /// No reading at all — no cgroup was ever created — reports the default, unmeasured usage.
    #[cfg(target_os = "linux")]
    #[test]
    fn no_reading_at_all_reports_the_default_usage() {
        assert_eq!(usage_from_reading(None), MemoryUsage::default());
    }

    /// Capturing a child drains both pipes while it runs rather than waiting on one before the other.
    ///
    /// The child fills stdout first and then stderr. A sequential implementation blocks reading
    /// stdout to end of file while the child blocks on a full stderr pipe, so the watchdog makes
    /// that deadlock a named failure rather than a hung suite.
    #[cfg(unix)]
    #[test]
    fn contained_output_drains_both_full_pipes_concurrently() {
        const INNER: &str = "GAMMA_PROCESS_OUTPUT_CHILD";
        const BYTES: usize = 256 * 1024;

        if env::var_os(INNER).is_some() {
            std::io::stdout().write_all(&vec![b'o'; BYTES]).expect("stdout");
            std::io::stderr().write_all(&vec![b'e'; BYTES]).expect("stderr");

            return;
        }

        let program = env::current_exe().expect("the test binary has a path");
        let mut command = Command::new(program);
        let _ = command
            .args([
                "--exact",
                "--nocapture",
                "process_tree::tests::contained_output_drains_both_full_pipes_concurrently",
            ])
            .env(INNER, "1");

        let captured = testing::within(testing::WATCHDOG, "contained output draining both full pipes", move || {
            output(command, MemoryRequest::default()).expect("capture")
        });

        assert!(captured.status.success(), "{:?}", captured.status);
        assert!(captured.stdout.len() >= BYTES);
        assert!(captured.stdout.windows(1024).any(|window| window == [b'o'; 1024]));
        assert!(captured.stderr.len() >= BYTES);
        assert!(captured.stderr.windows(1024).any(|window| window == [b'e'; 1024]));
    }

    /// Descendants are swept before capture waits for inherited output handles to close.
    #[test]
    fn contained_output_does_not_wait_for_a_descendant_holding_its_pipes() {
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("spawn:sleep:10000"), testing::directive("exit:0")]);
        let started = Instant::now();

        let captured = output(command, MemoryRequest::default()).expect("capture");

        assert!(captured.status.success(), "{:?}", captured.status);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "capture waited for a descendant that should have been swept: {:?}",
            started.elapsed()
        );
    }

    /// A spawn failure retains the operating system's original error kind.
    #[test]
    fn contained_output_preserves_spawn_errors() {
        let sandbox = testing::workdir("gamma-missing-output-program");
        let command = Command::new(sandbox.path().join("not-there"));

        let reason = output(command, MemoryRequest::default()).expect_err("the missing program cannot start");

        match reason {
            OutputError::Io(cause) => assert_eq!(cause.kind(), io::ErrorKind::NotFound),
            OutputError::Containment(other) => panic!("expected a spawn error, got {other}"),
        }
    }

    /// Both contained-output error variants preserve their source and display text.
    #[test]
    fn contained_output_errors_preserve_their_causes() {
        let containment = OutputError::from(PlatformError::new_static(Situation::Refused, "containment failed"));
        assert_eq!(containment.to_string(), "containment failed");
        assert!(containment.source().is_some());

        let io = OutputError::from(io::Error::other("stream failed"));
        assert_eq!(io.to_string(), "stream failed");
        assert!(io.source().is_some());
    }

    /// Each pipe is handed over once, and never handed over again once taken.
    #[test]
    fn stdout_and_stderr_pipes_are_taken_once_and_only_once() {
        let mut command = no_op_command();
        let _ = command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        assert!(subtree.take_stdout().is_some(), "a piped stdout must be handed over");
        assert!(
            subtree.take_stdout().is_none(),
            "a pipe already taken must not be handed over twice"
        );
        assert!(subtree.take_stderr().is_some(), "a piped stderr must be handed over");
        assert!(
            subtree.take_stderr().is_none(),
            "a pipe already taken must not be handed over twice"
        );

        let _reaped = subtree.terminate();
    }

    /// The test-only pid accessor reports the leader's real process id.
    #[cfg(unix)]
    #[test]
    fn id_reports_the_leaders_process_id() {
        let command = no_op_command();
        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let expected = spawned.id();

        let mut subtree = ProcessTree::adopt(spawned).expect("adoption");

        assert_eq!(subtree.id(), expected);

        let _reaped = subtree.terminate();
    }

    /// Dropping the post-spawn state cannot leave its live child outside the lifecycle.
    #[cfg(unix)]
    #[test]
    fn an_unadopted_spawn_is_terminated_and_reaped_on_drop() {
        let mut command = Command::new("sleep");
        let _ = command.arg("30");
        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let pid = i32::try_from(spawned.id()).expect("the child id fits a Unix pid");

        drop(spawned);

        assert!(gone_soon(pid), "the unadopted child survived its spawned-state owner");
    }

    /// A survivor is reported as not gone, rather than assumed gone once the poll loop runs out.
    #[cfg(unix)]
    #[test]
    fn gone_soon_reports_false_for_a_survivor() {
        // `sleep` directly, rather than a shell wrapping it, so there is exactly one process to
        // kill: a shell that does not exec-replace itself for its last simple command would leave
        // an orphaned survivor behind once only the shell's own pid is signalled.
        let mut command = Command::new("sleep");
        let _ = command.arg("30");

        let mut child = command.spawn().expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid fits");

        assert!(!gone_soon(pid), "a process kept alive for the whole poll window was reported gone");

        let _killed = child.kill();
        let _reaped = child.wait();
    }

    /// `observe` reports the reap race it cannot recover from, and revokes what it must.
    ///
    /// The leader is reaped through the standard library's own path before `observe` is asked
    /// about it, which is exactly the race a concurrent reaper could win: the OS no longer
    /// considers the pid this process's child at all, so the real `waitid` call inside `observe`
    /// reports `ECHILD` for real. From that instant the pid and the group id it named are free, so
    /// what this asserts is not only the error but that nothing numeric survives it — a retained
    /// group or `Child` would let the drop below signal whatever took the id next.
    #[cfg(unix)]
    #[test]
    fn observe_reports_a_reap_race_rather_than_losing_the_leader() {
        let command = Command::new("true");
        let prepared = prepare(command, MemoryRequest::default()).expect("containment");
        let spawned = prepared.spawn().expect("spawn");
        let (mut child, guard) = spawned.into_parts();
        #[cfg(target_os = "linux")]
        let cgroup = guard.cgroup;
        #[cfg(not(target_os = "linux"))]
        drop(guard);
        let group = i32::try_from(child.id()).expect("pid fits");

        let _status = child.wait().expect("the child exits almost immediately");

        let spawning = interrupt::spawning();
        let slot = spawning.watch(group).expect("a free slot");
        let mut subtree = ProcessTree {
            child: Some(child),
            group: Some(group),
            slot: Some(slot),
            #[cfg(target_os = "linux")]
            cgroup,
            #[cfg(target_os = "linux")]
            metered: false,
        };

        let outcome = subtree.observe();

        assert!(outcome.is_err(), "{outcome:?}");
        assert!(subtree.released(), "a reaped-elsewhere subtree kept its interrupt registration");
        assert_eq!(
            interrupt::watched(slot),
            0,
            "an interrupt could still signal whatever took the reaped group id"
        );
        assert_eq!(subtree.group, None, "the stale numeric group capability was retained");

        // Immediate reuse, made deterministic: a different group takes the registration the
        // revocation just handed back. Neither the interrupt path nor the drop below may reach it.
        let replacement = 0x0060_0d3e;
        let taken = spawning.watch(replacement).expect("a free slot");

        // The leader is gone, so every later lifecycle call has to say so rather than reach the
        // pid — which by now may belong to anyone.
        let again = subtree.terminate();

        assert!(again.is_err(), "{again:?}");

        drop(subtree);

        assert_eq!(
            interrupt::watched(taken),
            replacement,
            "cleanup after an external reap took the replacement group out of the interrupt registry"
        );

        interrupt::forget(taken, replacement);
    }
}
