// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Holding a child and everything it spawns inside a Windows job object.
//!
//! A job object is how Windows says "this process and everything descended from it", and it is
//! also where the memory accounting lives: the same object that can kill the subtree can carry a
//! limit for it and report what it reached. Neither is reachable from `std`, so both are made here
//! through Win32 and handed out as safe methods.

use core::ffi::c_void;
use core::mem;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::process::{Child, Command};
use std::sync::{Mutex, Once};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    GetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_JOB_MEMORY_LIMIT;
use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

/// Prevents this process and the children that inherit its error mode from opening system dialogs.
///
/// A command-line supervisor cannot answer modal UI. Native faults and loader failures must become
/// exit statuses the waiting thread can observe; otherwise one missing DLL holds the whole run
/// behind a window that may not even be visible in CI. Windows recommends setting
/// `SEM_FAILCRITICALERRORS` at process startup for this reason. The other two flags cover fault and
/// file-open dialogs from test executables, while preserving every mode bit the caller already set.
pub fn suppress_error_dialogs() {
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        // SAFETY: this reads the process-wide integer flag word without accessing Rust memory.
        let inherited = unsafe { GetErrorMode() };

        // SAFETY: existing bits are preserved, and `Once` makes this process-wide mutation
        // race-free.
        let _previous = unsafe { SetErrorMode(inherited | SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX) };
    });
}

/// Marks a command as one whose child starts suspended.
///
/// The flag is added to whatever the standard library sets for its own reasons rather than
/// replacing it, so nothing about how the child is otherwise started changes.
pub fn start_suspended(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    let _ = command.creation_flags(CREATE_SUSPENDED);
}

/// Lets a child created suspended start running.
///
/// A process created suspended has exactly one thread and has executed nothing, so the single
/// thread the snapshot reports for it is the one to resume. The snapshot covers every thread
/// on the machine, which is why the owning process is checked; the child cannot be confused
/// with anything else, since its id cannot be reused while this run still holds it open.
#[must_use]
pub fn release(process: u32) -> bool {
    // SAFETY: the arguments are a flag word and a process id, and no memory is touched. A
    // thread snapshot covers every process, so the process id is ignored. Failure is
    // reported as `INVALID_HANDLE_VALUE`, which is checked below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, process) };

    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return false;
    }

    // SAFETY: the handle was just returned by `CreateToolhelp32Snapshot` and is neither null
    // nor the invalid sentinel, and ownership of it is not held anywhere else. Wrapping it
    // here is what closes it once, on every path out of this function.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };

    main_thread(&snapshot, process).is_some_and(resume)
}

/// The id of the one thread a suspended child has, if the snapshot still lists it.
fn main_thread(snapshot: &OwnedHandle, process: u32) -> Option<u32> {
    let mut entry = THREADENTRY32 {
        // The API rejects an entry whose declared size is not its own, which is how it tells
        // the versions of this structure apart.
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0),
        ..Default::default()
    };

    // SAFETY: the handle is a live thread snapshot, and the entry is a live, fully initialised
    // `THREADENTRY32` whose `dwSize` is its own size, which is what the API requires of it.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), core::ptr::from_mut(&mut entry)) };

    while found != 0 {
        if entry.th32OwnerProcessID == process {
            return Some(entry.th32ThreadID);
        }

        // SAFETY: as above, and the entry is the one the previous call filled in, which is
        // what this continues from.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), core::ptr::from_mut(&mut entry)) };
    }

    None
}

/// Resumes one thread, reporting whether it actually came out of suspension.
fn resume(thread: u32) -> bool {
    // SAFETY: the arguments are an access mask, an inheritance flag and a thread id, and no
    // memory is touched. Failure is reported as a null handle.
    let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread) };

    if handle.is_null() {
        return false;
    }

    // SAFETY: the handle was just returned by `OpenThread` and is not null, and nothing else
    // owns it, so this is what closes it exactly once when the function returns.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };

    // SAFETY: the handle is a live thread opened for exactly this right. The count it returns
    // is the suspension count before the call, and `u32::MAX` is how failure is spelled.
    let previous = unsafe { ResumeThread(handle.as_raw_handle()) };

    previous != u32::MAX
}

/// A Windows job object holding one child and everything it goes on to spawn.
#[derive(Debug)]
pub struct Job {
    /// The job handle, closed when this is dropped.
    handle: HANDLE,

    /// The completion port that reports a rejected job-memory commit, when a limit needs one.
    completion: Option<OwnedHandle>,

    /// The aggregate committed memory the job was limited to, when it was limited at all.
    limit: Option<u64>,

    /// Whether the completion port has reported the job-memory limit for this job.
    ///
    /// Reading a completion port consumes its messages, so retaining this answer makes repeated
    /// [`Self::usage`] calls agree. The mutex also keeps concurrent readers from stealing the
    /// one message that proves the limit was hit from each other.
    memory_limit_hit: Mutex<bool>,
}

// SAFETY: a job handle is a kernel object reference with no thread affinity, and every use of
// it here goes through a Win32 call that is itself thread-safe.
unsafe impl Send for Job {}

// SAFETY: `HANDLE` is `!Sync` because of what a raw pointer may point at, not because of how the
// field holding it is used — `&Cell<u8>` is never mutated through the reference either and is
// emphatically not `Sync`. The premise that carries this impl is the one above: the pointee is a
// kernel object whose operations are internally synchronized, and shared use of a `Job` is confined
// to `&self` methods — `assign`, `usage` and `terminate` — each of which passes the handle to such
// a call and writes only to locals, except for `memory_limit_hit`, which is protected by its
// mutex. Two threads calling them at once therefore race inside the kernel, which is where that
// race is defined and resolved, or through that mutex.
unsafe impl Sync for Job {}

impl Job {
    /// Creates a job, configured before any process is put in it.
    ///
    /// The limit is installed here rather than after [`Self::assign`] because a job whose limit
    /// arrives second is a job the child spent its first instants outside of, and the test this
    /// exists to bound is exactly the one that allocates immediately.
    #[must_use]
    pub fn create(limit: Option<u64>) -> Option<Self> {
        // SAFETY: both arguments are null, which the API documents as "unnamed job with
        // default security". A failure is reported as a null handle rather than by any other
        // means.
        let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };

        if handle.is_null() {
            return None;
        }

        let completion = if limit.is_some() {
            // SAFETY: `INVALID_HANDLE_VALUE` asks for a new completion port; the null existing-port
            // handle, zero key and one concurrent thread are valid creation arguments. Failure is
            // a null handle.
            let completion = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 0, 1) };

            if completion.is_null() {
                // SAFETY: `handle` was just created above and has no other owner.
                let _closed = unsafe { CloseHandle(handle) };

                return None;
            }

            // SAFETY: `completion` was just returned by `CreateIoCompletionPort` and is not null,
            // so this wrapper owns and closes it exactly once.
            Some(unsafe { OwnedHandle::from_raw_handle(completion) })
        } else {
            None
        };
        let job = Self {
            handle,
            completion,
            limit,
            memory_limit_hit: Mutex::new(false),
        };
        if let Some(completion) = job.completion.as_ref() {
            let mut association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: core::ptr::null_mut(),
                CompletionPort: completion.as_raw_handle(),
            };

            // SAFETY: `job.handle` is live, the information class matches `association`, and the
            // structure and its size describe initialized memory for the duration of the call.
            let associated = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectAssociateCompletionPortInformation,
                    core::ptr::from_mut(&mut association).cast::<c_void>(),
                    u32::try_from(size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>()).unwrap_or(0),
                )
            };

            if associated == 0 {
                return None;
            }
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            // SAFETY: the structure is plain old data whose every field is an integer, and it
            // is fully overwritten below except for the reserved fields, which the API expects
            // to be zero.
            unsafe { mem::zeroed() };

        // Everything in the job dies when the last handle to it closes, which covers the run
        // being killed itself: the handle goes with the process, and the subtree with it.
        //
        // Native faults must terminate rather than open Windows Error Reporting UI. Mutants are
        // expected to reach invalid states, especially in unsafe code; a modal crash dialog would
        // hold the child open indefinitely and make the mutation run look like the process that
        // crashed. Windows implements this job flag by setting SEM_NOGPFAULTERRORBOX on each member.
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;

        if let Some(limit) = limit {
            // The job-wide limit is the one worth having: a per-process limit would let a test
            // that spawns helpers reach any total it liked, which is the shape of runaway
            // allocation this is here to stop.
            let Ok(bytes) = usize::try_from(limit) else {
                return None;
            };

            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.JobMemoryLimit = bytes;
        }

        // SAFETY: the handle is a live job, the class matches the structure being passed, and
        // the length is that structure's own size.
        let set = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                core::ptr::from_mut(&mut limits).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
            )
        };

        if set == 0 {
            return None;
        }

        Some(job)
    }

    /// Puts a child in the job, before it has run.
    ///
    /// The child was created suspended by [`start_suspended`], so this happens between its
    /// creation and its first instruction: nothing it or anything it spawns allocates can have
    /// happened outside the job, and the ceiling the job carries is therefore in force for the
    /// whole of its life rather than for all of it but the beginning.
    #[must_use]
    pub fn assign(&self, child: &Child) -> bool {
        // SAFETY: the handle is a live job and the second argument is the child's own process
        // handle, which `Child` keeps open for as long as it lives.
        let assigned = unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle().cast::<c_void>()) };

        assigned != 0
    }

    /// What the job accounted for, read once its processes have finished.
    ///
    /// A job whose limit fires refuses allocations rather than necessarily killing anything at
    /// once, so neither a non-zero exit status nor a high-water mark identifies the cause. The
    /// completion port reports `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT` only when a process actually
    /// tries to pass the job's hard limit. Windows does not guarantee delivery of this particular
    /// notification, so a missing message is conservatively not classified as exhaustion.
    #[must_use]
    pub fn usage(&self) -> (Option<u64>, bool) {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            // SAFETY: the structure is plain old data whose every field is an integer, and the
            // call below either overwrites it entirely or is reported as having failed.
            unsafe { mem::zeroed() };
        let mut returned: u32 = 0;

        // SAFETY: the handle is a live job, the class matches the structure being written into,
        // and the length is that structure's own size. The final argument is a live `u32`.
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                core::ptr::from_mut(&mut limits).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                core::ptr::from_mut(&mut returned),
            )
        };

        let peak = (queried != 0).then(|| u64::try_from(limits.PeakJobMemoryUsed).ok()).flatten();
        let memory_limit_hit = self.limit.is_some_and(|_| self.memory_limit_was_hit());

        usage_from(self.limit, peak, memory_limit_hit)
    }

    /// Drains job messages until none is immediately available, retaining a memory-limit hit.
    fn memory_limit_was_hit(&self) -> bool {
        let Some(completion) = self.completion.as_ref() else {
            return false;
        };
        let mut hit = self.memory_limit_hit.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        if *hit {
            return true;
        }

        loop {
            let (mut message, mut key, mut overlapped) = (0_u32, 0_usize, core::ptr::null_mut());

            // SAFETY: `completion` is a live I/O completion port; the three output pointers name
            // initialized locals for this call, and a zero timeout makes the drain non-blocking.
            let received =
                unsafe { GetQueuedCompletionStatus(completion.as_raw_handle(), &raw mut message, &raw mut key, &raw mut overlapped, 0) };

            if received == 0 {
                return *hit;
            }

            if message == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
                *hit = true;
            }
        }
    }

    /// Kills everything in the job.
    pub fn terminate(&self) {
        // SAFETY: the handle is a live job. The exit code is arbitrary and is never read, since
        // a killed mutant's status is decided by the run rather than by the process.
        let _terminated = unsafe { TerminateJobObject(self.handle, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `CreateJobObjectW`, is closed exactly once because
        // this type is not `Clone`, and is not used afterwards.
        let _closed = unsafe { CloseHandle(self.handle) };
    }
}

/// Combines the independently queried peak and limit-violation evidence.
///
/// The peak remains useful for sizing later limits, but it is never proof that one stopped this
/// job. Only the kernel's job-memory-limit message can make `exhausted` true.
const fn usage_from(limit: Option<u64>, peak: Option<u64>, memory_limit_hit: bool) -> (Option<u64>, bool) {
    (peak, limit.is_some() && memory_limit_hit)
}

/// Whether a process is inside any job object at all, or `None` if the question could not be put.
///
/// Exposed for containment diagnostics and tests; assignment itself is performed through [`Job`].
#[must_use]
pub fn in_any_job(child: &Child) -> Option<bool> {
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;

    let handle = child.as_raw_handle().cast::<c_void>();
    let mut inside = 0;

    // SAFETY: the handle is the child's own process handle, which `Child` keeps open for as long
    // as it lives. A null job argument asks about any job at all rather than about a particular
    // one, and the final argument is a live `i32` the call writes through.
    let queried = unsafe { IsProcessInJob(handle, core::ptr::null_mut(), &raw mut inside) };

    (queried != 0).then_some(inside != 0)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use super::*;

    /// How long a killed child is given to actually go, which is a scheduling question.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// Allocation increment used by the job-memory-limit regression child.
    const JOB_LIMIT_REGRESSION_BLOCK: usize = 16 * 1024 * 1024;

    /// A child that outlives any test unless something kills it.
    ///
    /// `ping` against the loopback address is the one sleep every Windows install has; `timeout`
    /// refuses to run without a console, which is exactly the state a captured test harness leaves
    /// its children in.
    fn sleeper() -> Command {
        let mut command = Command::new("cmd");

        let _ = command
            .args(["/C", "ping", "-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        start_suspended(&mut command);

        command
    }

    /// Starts a child inside `job` and lets it run.
    ///
    /// The order is the one the design depends on and is worth repeating in the tests: the child is
    /// created suspended, assigned while it has still executed nothing, and only then released — so
    /// a test that observed the job's accounting is observing all of the child's life rather than
    /// all of it but the beginning.
    fn running_in(job: &Job, command: &mut Command) -> Child {
        let child = command.spawn().expect("the child starts");

        assert!(job.assign(&child), "the job refused the child");
        assert!(release(child.id()), "the child never came out of suspension");

        child
    }

    /// Whether a process has ended within a few seconds, which is what being killed looks like.
    fn ended_soon(child: &mut Child) -> bool {
        let deadline = Instant::now() + PATIENCE;

        while Instant::now() < deadline {
            if child.try_wait().expect("the child's status can be read").is_some() {
                return true;
            }

            thread::sleep(Duration::from_millis(20));
        }

        false
    }

    /// Reads back the policy installed on a job.
    fn flags(job: &Job) -> u32 {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            // SAFETY: the structure is plain old data and the call below overwrites it.
            unsafe { mem::zeroed() };

        // SAFETY: the handle is a live job, the information class matches the output structure,
        // and the output length is that structure's size.
        let queried = unsafe {
            QueryInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                core::ptr::from_mut(&mut limits).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                core::ptr::null_mut(),
            )
        };

        assert_ne!(queried, 0, "the job's limits could not be read back");

        limits.BasicLimitInformation.LimitFlags
    }

    /// A native fault is a test result, not an invitation to put up modal UI.
    ///
    /// Mutation routinely drives unsafe code into fail-fast paths. Without this policy Windows
    /// Error Reporting holds the faulting test open behind a dialog, so the run appears to have
    /// crashed and cannot collect the child's exit status until somebody clicks it.
    #[test]
    fn a_job_suppresses_unhandled_exception_dialogs() {
        let job = Job::create(None).expect("a job is created");

        assert_ne!(
            flags(&job) & JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
            0,
            "the job allows native faults to open Windows Error Reporting UI"
        );
    }

    /// Loader failures happen before a child can report anything through its own stderr.
    #[test]
    fn the_process_and_its_children_suppress_system_error_dialogs() {
        suppress_error_dialogs();

        // SAFETY: this reads the process's integer error-mode flags and touches no memory.
        let mode = unsafe { GetErrorMode() };
        let required = SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX;

        assert_eq!(mode & required, required, "the process still allows a child error dialog");
    }

    /// Closing the last handle to a job takes everything in it.
    ///
    /// This is the whole reason Windows containment is simpler than the Unix side: the guarantee
    /// belongs to the kernel and holds however the parent dies, including by a signal it never got
    /// to handle. Every claim `cargo-gamma-process` makes about Windows rests on this one behaviour, so
    /// it is asserted rather than assumed.
    #[test]
    fn a_child_dies_when_the_last_handle_to_its_job_closes() {
        let job = Job::create(None).expect("a job is created");
        let mut command = sleeper();
        let mut child = running_in(&job, &mut command);

        assert!(
            child.try_wait().expect("the child's status can be read").is_none(),
            "the child was not running to begin with, so its death proves nothing"
        );

        drop(job);

        assert!(ended_soon(&mut child), "the child outlived the job that held it");
    }

    /// The job accounts for what ran in it, which is what the memory ceiling is read from.
    ///
    /// Asserted as growth from a known zero rather than as a bare non-zero peak, because a query
    /// that quietly failed also reports zero and `usage` turns failure into a default — so only the
    /// difference between the two readings distinguishes accounting that works from accounting that
    /// is not happening at all.
    #[test]
    fn a_job_reports_a_peak_that_grew_once_something_ran_in_it() {
        let job = Job::create(None).expect("a job is created");

        assert_eq!(job.usage().0, Some(0), "an empty job had already accounted for something");

        let mut command = Command::new("cmd");

        let _ = command
            .args(["/C", "ping", "-n", "2", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        start_suspended(&mut command);

        let mut child = running_in(&job, &mut command);
        let _status = child.wait().expect("the child finishes");

        let usage = job.usage();

        assert!(usage.0.is_some_and(|peak| peak > 0), "the job accounted for nothing: {usage:?}");
        assert!(!usage.1, "a job with no limit reported exhaustion: {usage:?}");
    }

    /// A high-water mark is measurement, not evidence that the next commit was refused.
    #[test]
    fn a_peak_equal_to_the_limit_without_a_violation_is_not_exhaustion() {
        let usage = usage_from(Some(4096), Some(4096), false);

        assert_eq!(usage.0, Some(4096));
        assert!(!usage.1, "{usage:?}");
    }

    /// The completion-port event, rather than the peak, is sufficient evidence of exhaustion.
    #[test]
    fn a_job_memory_limit_event_marks_the_workload_exhausted() {
        let usage = usage_from(Some(4096), Some(4095), true);

        assert!(usage.1, "{usage:?}");
    }

    #[test]
    fn parallel_shared_state_job_usage_queries_agree() {
        let job = Arc::new(Job::create(Some(1024 * 1024 * 1024)).expect("a job is created"));

        thread::scope(|scope| {
            for _reader in 0..8 {
                let job = Arc::clone(&job);

                let _reader = scope.spawn(move || {
                    for _query in 0..100 {
                        let usage = job.usage();

                        assert!(usage.0.is_some());
                        assert!(!usage.1);
                    }
                });
            }
        });
    }

    /// An ordinary test failure must not be relabelled as memory exhaustion merely because it ran
    /// in a memory-limited job.
    #[test]
    fn an_unrelated_failing_child_is_not_memory_exhaustion() {
        let job = Job::create(Some(1024 * 1024 * 1024)).expect("a job is created");
        let mut command = Command::new("cmd");

        let _ = command.args(["/C", "exit /B 1"]).stdout(Stdio::null()).stderr(Stdio::null());

        start_suspended(&mut command);

        let mut child = running_in(&job, &mut command);
        let status = child.wait().expect("the child finishes");
        let usage = job.usage();

        assert!(!status.success(), "the child must fail for its own reason");
        assert!(!usage.1, "an ordinary failure was called exhaustion: {usage:?}");
    }

    /// A process that actually tries to pass the hard ceiling produces the event `usage` trusts.
    #[test]
    #[ignore = "requires a Windows job-memory limit violation; run explicitly"]
    fn a_job_memory_limit_violation_is_reported() {
        const LIMIT: u64 = 256 * 1024 * 1024;

        let job = Job::create(Some(LIMIT)).expect("a limited job is created");
        let executable = std::env::current_exe().expect("the test binary knows its own path");
        let mut command = Command::new(executable);

        let _ = command
            .args(["--exact", "tests::the_child_attempts_to_pass_its_job_memory_limit", "--nocapture"])
            .env("CARGO_GAMMA_JOB_LIMIT_REGRESSION_CHILD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        start_suspended(&mut command);

        let mut child = running_in(&job, &mut command);
        let status = child.wait().expect("the child finishes");
        let usage = job.usage();

        assert!(status.success(), "the allocation child did not complete: {status}");
        assert!(usage.1, "the kernel reported no job-memory violation: {usage:?}");
    }

    #[test]
    fn the_child_attempts_to_pass_its_job_memory_limit() {
        if std::env::var_os("CARGO_GAMMA_JOB_LIMIT_REGRESSION_CHILD").is_none() {
            return;
        }

        let mut blocks: Vec<Vec<u8>> = Vec::new();

        for _ in 0..64 {
            let mut block = Vec::new();

            if block.try_reserve_exact(JOB_LIMIT_REGRESSION_BLOCK).is_err() {
                return;
            }

            block.resize(JOB_LIMIT_REGRESSION_BLOCK, 0);
            blocks.push(block);
        }

        assert_eq!(core::hint::black_box(blocks.len()), 64);
        panic!("the job allowed more than one GiB to be committed");
    }

    /// Terminating a job kills what is in it, which is how a timed-out or cancelled mutant ends.
    ///
    /// Distinct from the drop above: a run that has decided a mutant is finished still holds the
    /// job, because it is about to read the accounting off it.
    #[test]
    fn terminating_a_job_kills_the_child_in_it() {
        let job = Job::create(None).expect("a job is created");
        let mut command = sleeper();
        let mut child = running_in(&job, &mut command);

        assert!(
            child.try_wait().expect("the child's status can be read").is_none(),
            "the child was not running to begin with, so its death proves nothing"
        );

        job.terminate();

        assert!(ended_soon(&mut child), "the child outlived the termination of its job");
    }
}
