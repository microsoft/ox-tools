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
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_JOB_MEMORY_LIMIT;
use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

#[cfg(test)]
use crate::native_faults::{self, NativeCall};

/// The fallible Win32 operations used by the release and job algorithms.
///
/// Production and tests run the same algorithms. Tests replace only this dependency with a backend
/// that can return one requested native failure before delegating every other call to Windows.
trait NativeCalls {
    fn thread_snapshot(&self, process: u32) -> HANDLE;
    fn first_thread(&self, snapshot: HANDLE, entry: &mut THREADENTRY32) -> bool;
    fn next_thread(&self, snapshot: HANDLE, entry: &mut THREADENTRY32) -> bool;
    fn open_thread(&self, thread: u32) -> HANDLE;
    fn resume_thread(&self, thread: HANDLE) -> u32;
    fn create_job(&self) -> HANDLE;
    fn create_completion_port(&self) -> HANDLE;
    fn associate_completion_port(&self, job: HANDLE, association: &mut JOBOBJECT_ASSOCIATE_COMPLETION_PORT) -> bool;
    fn configure_job(&self, job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> bool;
    fn assign_process(&self, job: HANDLE, process: HANDLE) -> bool;
    fn query_accounting(&self, job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION, returned: &mut u32) -> bool;
    fn completion_status(&self, completion: HANDLE, message: &mut u32, key: &mut usize, overlapped: &mut *mut OVERLAPPED) -> bool;
    fn terminate_job(&self, job: HANDLE) -> bool;
}

struct SystemCalls;

impl NativeCalls for SystemCalls {
    fn thread_snapshot(&self, process: u32) -> HANDLE {
        // SAFETY: the arguments are a flag word and a process id, and no caller memory is touched.
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, process) }
    }

    fn first_thread(&self, snapshot: HANDLE, entry: &mut THREADENTRY32) -> bool {
        // SAFETY: `snapshot` is live and `entry` declares its initialized structure size.
        unsafe { Thread32First(snapshot, core::ptr::from_mut(entry)) != 0 }
    }

    fn next_thread(&self, snapshot: HANDLE, entry: &mut THREADENTRY32) -> bool {
        // SAFETY: the arguments retain the validity established for `first_thread`.
        unsafe { Thread32Next(snapshot, core::ptr::from_mut(entry)) != 0 }
    }

    fn open_thread(&self, thread: u32) -> HANDLE {
        // SAFETY: the arguments are an access mask, an inheritance flag and a thread identifier.
        unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread) }
    }

    fn resume_thread(&self, thread: HANDLE) -> u32 {
        // SAFETY: the handle was opened with `THREAD_SUSPEND_RESUME`.
        unsafe { ResumeThread(thread) }
    }

    fn create_job(&self) -> HANDLE {
        // SAFETY: null arguments request an unnamed job with default security.
        unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) }
    }

    fn create_completion_port(&self) -> HANDLE {
        // SAFETY: these documented sentinel arguments request a new completion port.
        unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 0, 1) }
    }

    fn associate_completion_port(&self, job: HANDLE, association: &mut JOBOBJECT_ASSOCIATE_COMPLETION_PORT) -> bool {
        // SAFETY: the information class matches the initialized structure and its exact size.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                core::ptr::from_mut(association).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>()).unwrap_or(0),
            ) != 0
        }
    }

    fn configure_job(&self, job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> bool {
        // SAFETY: the information class matches the initialized structure and its exact size.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                core::ptr::from_mut(limits).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
            ) != 0
        }
    }

    fn assign_process(&self, job: HANDLE, process: HANDLE) -> bool {
        // SAFETY: both handles are live kernel object references for the duration of the call.
        unsafe { AssignProcessToJobObject(job, process.cast::<c_void>()) != 0 }
    }

    fn query_accounting(&self, job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION, returned: &mut u32) -> bool {
        // SAFETY: the information class matches the writable structure and its exact size.
        unsafe {
            QueryInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                core::ptr::from_mut(limits).cast::<c_void>(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                core::ptr::from_mut(returned),
            ) != 0
        }
    }

    fn completion_status(&self, completion: HANDLE, message: &mut u32, key: &mut usize, overlapped: &mut *mut OVERLAPPED) -> bool {
        // SAFETY: the completion port is live, output references are writable, and the zero timeout
        // makes the operation non-blocking.
        unsafe { GetQueuedCompletionStatus(completion, message, key, overlapped, 0) != 0 }
    }

    fn terminate_job(&self, job: HANDLE) -> bool {
        // SAFETY: `job` is a live job handle and the exit code is an arbitrary payload.
        unsafe { TerminateJobObject(job, 1) != 0 }
    }
}

#[cfg(test)]
struct FaultInjectingCalls;

#[cfg(test)]
impl NativeCalls for FaultInjectingCalls {
    fn thread_snapshot(&self, process: u32) -> HANDLE {
        if native_faults::fired(NativeCall::Snapshot) {
            INVALID_HANDLE_VALUE
        } else {
            SystemCalls.thread_snapshot(process)
        }
    }

    fn first_thread(&self, snapshot: HANDLE, entry: &mut THREADENTRY32) -> bool {
        !native_faults::fired(NativeCall::ThreadEnumeration) && SystemCalls.first_thread(snapshot, entry)
    }

    fn next_thread(&self, snapshot: HANDLE, entry: &mut THREADENTRY32) -> bool {
        SystemCalls.next_thread(snapshot, entry)
    }

    fn open_thread(&self, thread: u32) -> HANDLE {
        if native_faults::fired(NativeCall::OpenThread) {
            core::ptr::null_mut()
        } else {
            SystemCalls.open_thread(thread)
        }
    }

    fn resume_thread(&self, thread: HANDLE) -> u32 {
        if native_faults::fired(NativeCall::ResumeThread) {
            u32::MAX
        } else {
            SystemCalls.resume_thread(thread)
        }
    }

    fn create_job(&self) -> HANDLE {
        if native_faults::fired(NativeCall::CreateJob) {
            core::ptr::null_mut()
        } else {
            SystemCalls.create_job()
        }
    }

    fn create_completion_port(&self) -> HANDLE {
        if native_faults::fired(NativeCall::CompletionPort) {
            core::ptr::null_mut()
        } else {
            SystemCalls.create_completion_port()
        }
    }

    fn associate_completion_port(&self, job: HANDLE, association: &mut JOBOBJECT_ASSOCIATE_COMPLETION_PORT) -> bool {
        !native_faults::fired(NativeCall::AssociatePort) && SystemCalls.associate_completion_port(job, association)
    }

    fn configure_job(&self, job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> bool {
        !native_faults::fired(NativeCall::ConfigureJob) && SystemCalls.configure_job(job, limits)
    }

    fn assign_process(&self, job: HANDLE, process: HANDLE) -> bool {
        !native_faults::fired(NativeCall::AssignProcess) && SystemCalls.assign_process(job, process)
    }

    fn query_accounting(&self, job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION, returned: &mut u32) -> bool {
        !native_faults::fired(NativeCall::QueryAccounting) && SystemCalls.query_accounting(job, limits, returned)
    }

    fn completion_status(&self, completion: HANDLE, message: &mut u32, key: &mut usize, overlapped: &mut *mut OVERLAPPED) -> bool {
        !native_faults::fired(NativeCall::CompletionStatus) && SystemCalls.completion_status(completion, message, key, overlapped)
    }

    fn terminate_job(&self, job: HANDLE) -> bool {
        !native_faults::fired(NativeCall::TerminateJob) && SystemCalls.terminate_job(job)
    }
}

#[cfg(not(test))]
static NATIVE_CALLS: SystemCalls = SystemCalls;
#[cfg(test)]
static NATIVE_CALLS: FaultInjectingCalls = FaultInjectingCalls;

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
    let snapshot = thread_snapshot(process);

    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return false;
    }

    // SAFETY: the handle was just returned by `CreateToolhelp32Snapshot` and is neither null
    // nor the invalid sentinel, and ownership of it is not held anywhere else. Wrapping it
    // here is what closes it once, on every path out of this function.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };

    main_thread(&snapshot, process).is_some_and(resume)
}

/// Takes the thread snapshot [`release`] scans.
fn thread_snapshot(process: u32) -> HANDLE {
    NATIVE_CALLS.thread_snapshot(process)
}

/// The id of the one thread a suspended child has, if the snapshot still lists it.
fn main_thread(snapshot: &OwnedHandle, process: u32) -> Option<u32> {
    let mut entry = THREADENTRY32 {
        // The API rejects an entry whose declared size is not its own, which is how it tells
        // the versions of this structure apart.
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0),
        ..Default::default()
    };

    let mut found = NATIVE_CALLS.first_thread(snapshot.as_raw_handle(), &mut entry);

    while found {
        if entry.th32OwnerProcessID == process {
            return Some(entry.th32ThreadID);
        }

        found = NATIVE_CALLS.next_thread(snapshot.as_raw_handle(), &mut entry);
    }

    None
}

/// Resumes one thread, reporting whether it actually came out of suspension.
fn resume(thread: u32) -> bool {
    let handle = open_thread(thread);

    if handle.is_null() {
        return false;
    }

    // SAFETY: the handle was just returned by `OpenThread` and is not null, and nothing else
    // owns it, so this is what closes it exactly once when the function returns.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };

    previous_suspension_count(&handle) != u32::MAX
}

/// Opens the child's one thread for resumption.
fn open_thread(thread: u32) -> HANDLE {
    NATIVE_CALLS.open_thread(thread)
}

/// Brings one thread out of suspension, reporting `u32::MAX` when it stayed suspended.
///
/// The handle is still owned by the caller, so the failure a test injects here leaves it to be
/// closed on the way out exactly as a real failure would.
fn previous_suspension_count(handle: &OwnedHandle) -> u32 {
    NATIVE_CALLS.resume_thread(handle.as_raw_handle())
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
        let handle = create_job_object();

        if handle.is_null() {
            return None;
        }

        let completion = if limit.is_some() {
            let completion = create_completion_port();

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

            if !associate_completion_port(job.handle, &mut association) {
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

        if !configure_job(job.handle, &mut limits) {
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
        NATIVE_CALLS.assign_process(self.handle, child.as_raw_handle())
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

        let queried = NATIVE_CALLS.query_accounting(self.handle, &mut limits, &mut returned);

        let peak = queried.then(|| u64::try_from(limits.PeakJobMemoryUsed).ok()).flatten();
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

            let received = NATIVE_CALLS.completion_status(completion.as_raw_handle(), &mut message, &mut key, &mut overlapped);

            if !received {
                return *hit;
            }

            if message == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
                *hit = true;
            }
        }
    }

    /// Kills everything in the job.
    pub fn terminate(&self) {
        let _terminated = NATIVE_CALLS.terminate_job(self.handle);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `CreateJobObjectW`, is closed exactly once because
        // this type is not `Clone`, and is not used afterwards.
        let _closed = unsafe { CloseHandle(self.handle) };
    }
}

/// Creates the job object itself.
///
/// Each native step of creation is a function of its own so that the arm taken when it fails can be
/// executed deliberately. Those arms are where a handle is leaked or a caller is handed a job that
/// was never configured, and on a healthy machine none of them ever runs.
fn create_job_object() -> HANDLE {
    NATIVE_CALLS.create_job()
}

/// Creates the completion port a memory ceiling reports violations through.
fn create_completion_port() -> HANDLE {
    NATIVE_CALLS.create_completion_port()
}

/// Points the job's notifications at the completion port created for it.
fn associate_completion_port(job: HANDLE, association: &mut JOBOBJECT_ASSOCIATE_COMPLETION_PORT) -> bool {
    NATIVE_CALLS.associate_completion_port(job, association)
}

/// Installs the job's policy flags and, when there is one, its memory ceiling.
fn configure_job(job: HANDLE, limits: &mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> bool {
    NATIVE_CALLS.configure_job(job, limits)
}

/// Combines the independently queried peak and limit-violation evidence.
///
/// The peak remains useful for sizing later limits, but it is never proof that one stopped this
/// job. Only the kernel's job-memory-limit message can make `exhausted` true.
const fn usage_from(limit: Option<u64>, peak: Option<u64>, memory_limit_hit: bool) -> (Option<u64>, bool) {
    (peak, limit.is_some() && memory_limit_hit)
}

/// Whether a process is inside any job object at all.
///
/// `None` means the question could not be put to the operating system, which is not the same
/// answer as no. Exposed for containment diagnostics and tests; assignment itself is performed
/// through [`Job`].
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

#[cfg(all(test, not(miri)))]
mod tests {
    use std::process::Stdio;
    use std::sync::Arc;
    use std::{env, thread};

    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    use super::*;

    /// Allocation increment used by the job-memory-limit regression child.
    const JOB_LIMIT_REGRESSION_BLOCK: usize = 16 * 1024 * 1024;

    /// Shared calibration for handle-leak fault loops.
    ///
    /// One leaked handle per repetition exceeds the allowed process-wide drift by more than an
    /// order of magnitude. Warm-ups absorb lazy runtime and Win32 initialization before the
    /// baseline, while the small drift allowance tolerates unrelated harness handle activity. The
    /// limit is non-zero, fits a 32-bit `usize`, and enters every limited-job path without asking
    /// these tests to allocate memory.
    const HANDLE_LEAK_REPEATS: u32 = 200;
    const HANDLE_LEAK_WARMUPS: u32 = 8;
    const ALLOWED_HANDLE_DRIFT: u32 = 8;
    const FAULT_TEST_LIMIT: u64 = 1024 * 1024 * 1024;

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

    /// Waits for the job transition under test to make the child exit.
    fn wait_for_end(child: &mut Child) {
        let _status = child.wait().expect("the child exits after its job is terminated");
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

        wait_for_end(&mut child);
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
        let executable = env::current_exe().expect("the test binary knows its own path");
        let mut command = Command::new(executable);

        let _ = command
            .args([
                "--exact",
                "job::tests::the_child_attempts_to_pass_its_job_memory_limit",
                "--nocapture",
            ])
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
        if env::var_os("CARGO_GAMMA_JOB_LIMIT_REGRESSION_CHILD").is_none() {
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

        wait_for_end(&mut child);
    }

    /// The number of kernel handles this process currently holds.
    ///
    /// A handle leaked in an error arm is invisible in that arm's own result — the call reports
    /// failure either way — so it is measured instead, across enough repetitions that one leak per
    /// repetition cannot be mistaken for ordinary drift.
    fn handle_count() -> u32 {
        let mut count = 0_u32;

        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: the pseudo-handle remains valid for the duration of the call, and the output
        // argument addresses a live, initialized `u32` this call writes through.
        let queried = unsafe { GetProcessHandleCount(process, &raw mut count) };

        assert_ne!(queried, 0, "the process handle count could not be read");

        count
    }

    /// Runs a process-global handle-count assertion without concurrent tests changing the count.
    fn delegate_handle_count_test(test: &str) -> bool {
        const CHILD: &str = "CARGO_GAMMA_HANDLE_COUNT_CHILD";

        if env::var_os(CHILD).is_some() {
            return false;
        }

        let output = Command::new(env::current_exe().expect("the test binary knows its own path"))
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("the isolated handle-count test starts");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "the isolated handle-count test failed: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert!(stdout.contains(test), "the exact filter did not run `{test}`\n{stdout}\n{stderr}");

        true
    }

    /// A job that cannot be created is refused, rather than reported as containment.
    ///
    /// The one answer that must never be given here is a `Job` that holds nothing: the caller uses
    /// its existence as the proof that a subtree can be terminated, and `prepare` refuses a launch
    /// on the strength of this `None`.
    #[test]
    fn a_job_that_cannot_be_created_is_refused() {
        let _armed = native_faults::arm(NativeCall::CreateJob);

        assert!(Job::create(None).is_none(), "a job that was never created was handed out");
    }

    /// Every later step of creation is equally fatal to the job, and none of them degrades it.
    ///
    /// A limited job whose completion port is missing or unassociated cannot report a violation,
    /// and a job whose limits never went in has neither ceiling nor kill-on-close. Each of those
    /// would be a boundary the caller believes in and does not have.
    #[test]
    fn a_job_that_cannot_be_completely_configured_is_refused() {
        for (call, limit) in [
            (NativeCall::CompletionPort, Some(FAULT_TEST_LIMIT)),
            (NativeCall::AssociatePort, Some(FAULT_TEST_LIMIT)),
            (NativeCall::ConfigureJob, Some(FAULT_TEST_LIMIT)),
            (NativeCall::ConfigureJob, None),
        ] {
            let _armed = native_faults::arm(call);

            assert!(
                Job::create(limit).is_none(),
                "a job whose {call:?} failed was handed out as a containment boundary"
            );
        }
    }

    /// A refused creation closes everything it had already opened.
    ///
    /// The arms above run only on a machine that has run out of something, which is exactly when a
    /// leaked handle compounds. Measured rather than reasoned about, because the ownership here is
    /// split between a raw `HANDLE` closed by `Drop` and an `OwnedHandle` closed by scope.
    #[test]
    fn a_refused_job_creation_closes_what_it_opened() {
        if delegate_handle_count_test("job::tests::a_refused_job_creation_closes_what_it_opened") {
            return;
        }

        for _warmup in 0..HANDLE_LEAK_WARMUPS {
            let _armed = native_faults::arm(NativeCall::ConfigureJob);
            let _refused = Job::create(Some(FAULT_TEST_LIMIT));
        }

        let before = handle_count();

        for _repeat in 0..HANDLE_LEAK_REPEATS {
            for call in [NativeCall::CompletionPort, NativeCall::AssociatePort, NativeCall::ConfigureJob] {
                let _armed = native_faults::arm(call);

                assert!(Job::create(Some(FAULT_TEST_LIMIT)).is_none());
            }
        }

        let after = handle_count();

        assert!(
            after <= before + ALLOWED_HANDLE_DRIFT,
            "{HANDLE_LEAK_REPEATS} refused job creations grew the handle count from {before} to {after}"
        );
    }

    /// A job that will not take its child says so, so the caller can end the child instead.
    #[test]
    fn a_job_that_refuses_its_child_reports_it_rather_than_claiming_containment() {
        let job = Job::create(None).expect("a job is created");
        let mut command = sleeper();
        let mut child = command.spawn().expect("the child starts");

        let _armed = native_faults::arm(NativeCall::AssignProcess);

        assert!(!job.assign(&child), "a refused assignment was reported as containment");

        // What the caller does with that answer, and what this test must not leave behind: the
        // child is still suspended and is in no job, so nothing but this ends it.
        let _killed = child.kill();
        let _reaped = child.wait().expect("the unassigned child is reaped");
    }

    /// Every way the resume can fail is reported, and leaves the child suspended rather than lost.
    ///
    /// A `true` here from a child that never came out of suspension is the worst answer available:
    /// the run would wait out the whole timeout on a process that has executed nothing, once per
    /// mutant. The child must also still be killable afterwards, which is what the caller does.
    #[test]
    fn a_resume_that_fails_is_reported_and_leaves_the_child_killable() {
        for call in [
            NativeCall::Snapshot,
            NativeCall::ThreadEnumeration,
            NativeCall::OpenThread,
            NativeCall::ResumeThread,
        ] {
            let job = Job::create(None).expect("a job is created");
            let mut command = sleeper();
            let mut child = command.spawn().expect("the child starts");

            assert!(job.assign(&child), "the job refused the child");

            let _armed = native_faults::arm(call);

            assert!(!release(child.id()), "a failed {call:?} was reported as a successful resume");
            assert!(
                child.try_wait().expect("the child's status can be read").is_none(),
                "a failed {call:?} did not leave the child suspended, so nothing was tested"
            );

            job.terminate();

            wait_for_end(&mut child);

            let _reaped = child.wait().expect("the abandoned child is reaped");
        }
    }

    /// A failed resume closes the snapshot and thread handles it opened on the way to failing.
    #[test]
    fn a_failed_resume_closes_what_it_opened() {
        if delegate_handle_count_test("job::tests::a_failed_resume_closes_what_it_opened") {
            return;
        }

        let mut command = sleeper();
        let mut child = command.spawn().expect("the child starts");
        let process = child.id();

        for _warmup in 0..HANDLE_LEAK_WARMUPS {
            let _armed = native_faults::arm(NativeCall::ResumeThread);
            let _refused = release(process);
        }

        let before = handle_count();

        for _repeat in 0..HANDLE_LEAK_REPEATS {
            for call in [NativeCall::ThreadEnumeration, NativeCall::OpenThread, NativeCall::ResumeThread] {
                let _armed = native_faults::arm(call);

                assert!(!release(process), "a failed {call:?} was reported as a successful resume");
            }
        }

        let after = handle_count();

        assert!(
            after <= before + ALLOWED_HANDLE_DRIFT,
            "{HANDLE_LEAK_REPEATS} failed resumes grew the handle count from {before} to {after}"
        );

        let _killed = child.kill();
        let _reaped = child.wait().expect("the suspended child is reaped");
    }

    /// Accounting failures are supplied by the backend used by the production usage algorithm.
    #[test]
    fn accounting_call_failures_do_not_invent_peak_or_limit_evidence() {
        let job = Job::create(Some(FAULT_TEST_LIMIT)).expect("a limited job is created");

        let query_failure = native_faults::arm(NativeCall::QueryAccounting);
        let (peak, exhausted) = job.usage();

        assert!(peak.is_none(), "a failed accounting query invented a peak");
        assert!(!exhausted, "a failed accounting query invented a limit violation");

        drop(query_failure);

        let _status_failure = native_faults::arm(NativeCall::CompletionStatus);
        let (_peak, exhausted) = job.usage();

        assert!(!exhausted, "a failed completion-port read invented a limit violation");
    }

    /// A termination that did not happen is not mistaken for one that did.
    ///
    /// `terminate` returns nothing, so the only way to see that it worked is the subtree. This
    /// asserts the injected failure really does keep the child alive — otherwise the test above it
    /// proves nothing — and that a real termination afterwards still reaches it.
    #[test]
    fn a_termination_that_fails_leaves_the_subtree_reachable() {
        let job = Job::create(None).expect("a job is created");
        let mut command = sleeper();
        let mut child = running_in(&job, &mut command);

        let _armed = native_faults::arm(NativeCall::TerminateJob);

        job.terminate();

        assert!(
            child.try_wait().expect("the child's status can be read").is_none(),
            "the injected failure did not prevent the termination, so nothing was tested"
        );

        job.terminate();

        wait_for_end(&mut child);
    }
}
