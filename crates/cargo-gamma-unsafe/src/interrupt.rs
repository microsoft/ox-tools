// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Taking the children along when this process is interrupted.
//!
//! Everything here runs, or may run, inside a signal handler, so it is written to the rules
//! that implies: no allocation, no locks, and no calls that are not async-signal-safe. The
//! registry is therefore a fixed array of atomics rather than the obvious `Vec` behind a
//! `Mutex`, which a handler could deadlock against the very thread it interrupted.
//!
//! # The window a child spends unwatched
//!
//! A child is placed in a process group of its own by the spawn itself, and the id of that group
//! is the child's pid — which the parent only learns once the spawn has returned. There is
//! therefore an unavoidable window in which a group exists and nothing has been told about it. A
//! handler that ran in that window with no protocol to consult would scan the registry, find
//! nothing, restore the default disposition and re-raise: the parent dies of the signal and the
//! child, in a group of its own that no signal had reached, goes on running. Registering afterwards
//! is no answer either, since the handler scans each slot once and may have already passed the slot
//! the registration lands in.
//!
//! [`spawning`] closes both. A spawner announces itself before it creates anything and reads the
//! interrupt state afterwards; a handler announces itself before it scans and reads the spawner
//! count afterwards. Both sides use sequentially consistent operations, so at least one of them
//! sees the other — this is Dekker's argument, and it is the whole of the guarantee:
//!
//! * The spawner sees the interrupt. It then either has not created its child yet, and does not,
//!   or has, and [`Spawning::watch`] kills the new group as it registers it.
//! * The handler sees the spawner. It kills every group it can see and returns **without** dying,
//!   leaving the fatal half of the interrupt to whichever spawner is last out of the window.
//!
//! There is no interleaving in which the spawner believes the run is calm and the handler believes
//! nothing is being spawned, so no thread ever re-raises a terminal signal while a child exists
//! that nothing has been told about.
//!
//! The cost is that an interrupt arriving mid-spawn is delivered when that spawn finishes rather
//! than at once, and the alternative is the leak above. The width is whatever a caller holds the
//! window open for, not a property this module can enforce, so it places an obligation on callers
//! rather than making a promise: **hold a [`Spawning`] across as little as possible, and never
//! across work that cannot itself produce a child.** Held tightly — from immediately before the
//! spawn to immediately after the group is watched — it is a `fork` and an `exec` wide. Held
//! across containment setup, a stalled filesystem in that setup becomes a run that absorbs
//! `SIGINT`, `SIGTERM`, `SIGHUP` and `SIGQUIT` for as long as the stall lasts.

use core::fmt;
#[cfg(not(loom))]
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::io;
use std::sync::OnceLock;

#[cfg(loom)]
use loom::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

#[cfg(target_os = "linux")]
use crate::cgroup::Cgroup;

/// How many children can be watched at once.
///
/// A run has one live child per worker, and workers are capped at the machine's parallelism.
/// A slot that cannot be claimed costs nothing but the guarantee this module adds, so the size
/// is generous rather than exact.
#[cfg(not(loom))]
const SLOTS: usize = 1024;
#[cfg(loom)]
const SLOTS: usize = 2;

/// How many children this module can watch at once.
///
/// Published because the guarantee is only as good as the supply: a caller running more children
/// than there are slots gets no diagnostic from [`Spawning::watch`] beyond a `None` that is easy to
/// discard, and an unwatched child is the leak this whole module exists to prevent. A caller that
/// chooses its own width is expected to bound it by this.
#[must_use]
pub const fn capacity() -> usize {
    SLOTS
}

/// The empty slot marker. No process group is ever zero from this process's point of view.
pub const EMPTY: i32 = 0;

/// What [`Registry::signal`] reads back while no terminal signal has arrived.
///
/// Zero is not a signal number, so it cannot be confused with one that has.
const CALM: i32 = 0;

/// A slot with neither a process group nor a cgroup kill descriptor.
const EMPTY_ENTRY: u64 = 0;

/// The groups an interrupt takes with it, and the protocol that keeps a child from slipping past.
///
/// Separated from the process-wide [`REGISTRY`] it is normally reached through, and given the kill
/// it performs as a parameter, so that the protocol can be driven through its races by a test in a
/// chosen order. Signal timing is not something a test can choose; this is.
struct Registry {
    /// Process groups and their optional cgroup kill switches, atomically paired per slot.
    watched: [AtomicU64; SLOTS],

    /// Sweeps currently using descriptors, so release cannot close one under a handler.
    handling: AtomicUsize,

    /// How many children exist, or are about to, without their group being watched yet.
    spawning: AtomicUsize,

    /// The signal a handler has begun acting on, or [`CALM`].
    ///
    /// Written once and never cleared: a run that has begun dying of a signal does not stop.
    interrupted: AtomicI32,
}

impl Registry {
    /// A registry watching nothing, on a run nothing has interrupted.
    #[cfg(not(loom))]
    const fn new() -> Self {
        Self {
            watched: [const { AtomicU64::new(EMPTY_ENTRY) }; SLOTS],
            handling: AtomicUsize::new(0),
            spawning: AtomicUsize::new(0),
            interrupted: AtomicI32::new(CALM),
        }
    }

    /// A registry watching nothing, on a loom execution.
    #[cfg(loom)]
    fn new() -> Self {
        Self {
            watched: core::array::from_fn(|_| AtomicU64::new(EMPTY_ENTRY)),
            handling: AtomicUsize::new(0),
            spawning: AtomicUsize::new(0),
            interrupted: AtomicI32::new(CALM),
        }
    }

    /// Announces a spawn about to create a group nothing has been told about yet.
    ///
    /// Sequentially consistent, and paired with the [`Registry::signal`] the caller performs next:
    /// together they are one half of the handshake described at the top of this module.
    fn open(&self) {
        let _previous = self.spawning.fetch_add(1, Ordering::SeqCst);
    }

    /// The signal a handler has begun acting on, or [`CALM`].
    fn signal(&self) -> i32 {
        self.interrupted.load(Ordering::SeqCst)
    }

    /// Closes a spawn window, and says what the caller must now die of.
    ///
    /// `Some` means a handler found this window open, killed what it could see and left the fatal
    /// half of the interrupt here. The last window out performs it, having first swept the groups
    /// registered since that handler ran; an earlier one must not, because a window still open is
    /// a child that may still be unwatched.
    #[cfg(all(test, not(miri)))]
    fn close<K: Fn(i32)>(&self, kill: &K) -> Option<i32> {
        self.close_with(kill, &|_descriptor| {})
    }

    fn close_with<K: Fn(i32), C: Fn(i32)>(&self, kill: &K, kill_cgroup: &C) -> Option<i32> {
        let last = self.spawning.fetch_sub(1, Ordering::SeqCst) == 1;
        let signal = self.signal();

        if !last || signal == CALM {
            return None;
        }

        self.sweep_with(kill, kill_cgroup);

        Some(signal)
    }

    /// Starts watching a freshly created group, returning the slot it took.
    ///
    /// Kills the group there and then if a handler has already begun. That is what covers the
    /// second race: the handler scans each slot once, so a registration landing in a slot it has
    /// already passed would otherwise be seen by nobody. The group stays in its slot afterwards, so
    /// the sweep in [`Registry::close`] reaches it a second time — a group whose only member has
    /// just been killed and not yet reaped still pins its id, so the repeat cannot land elsewhere.
    fn claim<K: Fn(i32)>(&self, group: i32, kill: &K) -> Option<usize> {
        if group <= EMPTY {
            return None;
        }

        let slot = self.watched.iter().position(|slot| {
            slot.compare_exchange(EMPTY_ENTRY, entry(group, None), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        });

        if self.signal() != CALM {
            kill(group);
        }

        slot
    }

    /// Stops watching a group, leaving a slot some other spawn has since claimed alone.
    ///
    /// Conditional on the group rather than unconditional, because a handler empties every slot as
    /// it kills, and a run that deferred its death goes on spawning: the slot a caller was given
    /// may already belong to somebody else by the time it is handed back, and clearing it would
    /// take that child out of the next sweep.
    fn release(&self, slot: usize, group: i32, wait_for_cgroup: bool) {
        if let Some(entry) = self.watched.get(slot) {
            let mut held = entry.load(Ordering::SeqCst);

            while entry_group(held) == group {
                match entry.compare_exchange(held, EMPTY_ENTRY, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_released) => break,
                    Err(current) => held = current,
                }
            }
        }

        if wait_for_cgroup {
            while self.handling.load(Ordering::SeqCst) != 0 {
                std::thread::yield_now();
            }
        }
    }

    /// Pairs an already-open cgroup kill switch with a claimed process-group slot.
    #[cfg(target_os = "linux")]
    fn attach_cgroup<C: Fn(i32)>(&self, slot: usize, group: i32, cgroup: i32, kill: &C) {
        if let Some(slot) = self.watched.get(slot) {
            let _attached = slot.compare_exchange(entry(group, None), entry(group, Some(cgroup)), Ordering::SeqCst, Ordering::SeqCst);
        }

        if self.signal() != CALM {
            kill(cgroup);
        }
    }

    /// One pass of the signal handler, and whether it may go on to die of the signal.
    ///
    /// `None` means it must not: a spawn was in flight, so a child may exist that this pass could
    /// not see, and dying now would leave it running. The spawner finishes the job in
    /// [`Registry::close`].
    #[cfg(any(all(test, not(miri)), loom))]
    fn interrupt<K: Fn(i32)>(&self, signal: i32, kill: &K) -> Option<i32> {
        self.interrupt_with(signal, kill, &|_descriptor| {})
    }

    fn interrupt_with<K: Fn(i32), C: Fn(i32)>(&self, signal: i32, kill: &K, kill_cgroup: &C) -> Option<i32> {
        let _previous = self.interrupted.swap(signal, Ordering::SeqCst);

        self.sweep_with(kill, kill_cgroup);

        (self.spawning.load(Ordering::SeqCst) == 0).then_some(signal)
    }

    fn sweep_with<K: Fn(i32), C: Fn(i32)>(&self, kill: &K, kill_cgroup: &C) {
        let _active = self.handling.fetch_add(1, Ordering::SeqCst);

        for entry in &self.watched {
            let (group, cgroup) = split_entry(entry.swap(EMPTY_ENTRY, Ordering::SeqCst));

            if group > EMPTY {
                if let Some(descriptor) = cgroup {
                    kill_cgroup(descriptor);
                }
                kill(group);
            }
        }

        let _active = self.handling.fetch_sub(1, Ordering::SeqCst);
    }

    /// What a slot currently holds, or [`EMPTY`] for one that is free.
    fn holding(&self, slot: usize) -> i32 {
        self.watched
            .get(slot)
            .map_or(EMPTY, |entry| entry_group(entry.load(Ordering::SeqCst)))
    }
}

fn entry(group: i32, cgroup: Option<i32>) -> u64 {
    let group = u64::from(u32::try_from(group).expect("claim accepts only positive process-group identifiers"));
    let descriptor = cgroup.map_or(0, |fd| {
        u64::from(u32::try_from(fd).expect("cgroup kill handles contain open, nonnegative descriptors")) + 1
    });

    (group << 32) | descriptor
}

fn split_entry(entry: u64) -> (i32, Option<i32>) {
    let group = i32::try_from(entry >> 32).unwrap_or(EMPTY);
    let descriptor = u32::try_from(entry & u64::from(u32::MAX))
        .ok()
        .and_then(|encoded| encoded.checked_sub(1))
        .and_then(|fd| i32::try_from(fd).ok());

    (group, descriptor)
}

fn entry_group(entry: u64) -> i32 {
    split_entry(entry).0
}

/// The run's registry, which every signal handler and every spawn shares.
#[cfg(not(loom))]
static REGISTRY: Registry = Registry::new();

#[cfg(loom)]
loom::lazy_static! {
    static ref REGISTRY: Registry = Registry::new();
}

fn registry() -> &'static Registry {
    &REGISTRY
}

/// Guards installing the handlers, which must happen exactly once.
static ARMED: OnceLock<Result<(), i32>> = OnceLock::new();

/// The signals that end a run and must take its children with them.
///
/// The three a terminal or a supervisor sends to stop something, plus `SIGQUIT`, which a terminal
/// sends from a keystroke of its own and which would otherwise walk straight past the registry:
/// containment puts every child in a group the terminal cannot reach, so a signal this module does
/// not handle kills the run and leaves the whole subtree alive. `SIGKILL` and `SIGSTOP` cannot be
/// handled at all, and `SIGPIPE` need not be, because it ends no run — a closed pipe surfaces as a
/// write error on the thread that owns the stream.
const TERMINAL: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// Installs every terminal handler with the supplied registration operation.
fn install_with(mut install: impl FnMut(i32, &libc::sigaction) -> Result<(), i32>) -> Result<(), i32> {
    // C spells a signal handler as an integer, which is what `sa_sigaction` holds, so the
    // pointer has to be widened into one. There is no other way to say this.
    #[expect(clippy::fn_to_numeric_cast_any, reason = "the C signal API takes a handler as an integer")]
    let target: libc::sighandler_t = handler as extern "C" fn(i32) as usize;

    for signal in TERMINAL {
        // SAFETY: `sigaction` is being asked only to read a fully initialised `sigaction`
        // struct and install it for a valid signal number, which is its whole contract. The
        // struct is zeroed first because it carries platform-specific padding and, on Linux, a
        // restorer field that must not be given a stale value; every field this code depends
        // on is written explicitly afterwards.
        //
        // `sigaction` rather than `signal` because POSIX leaves two properties of `signal`
        // unspecified that this protocol depends on. The disposition must survive delivery: a
        // handler that deferred its death because a spawn window was open has to still be
        // installed when the next signal arrives, or the second `Ctrl-C` kills the run by the
        // default action and leaves every contained child alive — the exact leak this module
        // exists to prevent. And the signal must be blocked for the duration of the handler,
        // which is what makes the re-raise in `die` land after the default disposition is
        // restored rather than re-entering the handler. Both are guaranteed here rather than
        // inherited from whichever semantics the platform's `signal` happens to have.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };

        action.sa_sigaction = target;
        action.sa_flags = libc::SA_RESTART;

        // SAFETY: `sigemptyset` initialises the signal set through the pointer it is given,
        // which addresses the fully owned local above. The zeroing before it is not enough on
        // its own: the representation of an empty set is the platform's business, not this
        // module's.
        let _emptied = unsafe { libc::sigemptyset(&raw mut action.sa_mask) };

        for blocked in TERMINAL {
            // SAFETY: the set was initialised immediately above and is addressed through the
            // same fully owned local, and every signal named is a valid number. The mask ends
            // up blocking the whole terminal set for the duration of the handler, so a second
            // interrupt cannot re-enter it and race the sweep against itself; it is delivered
            // when the handler returns, by which point `die` has either run or deliberately
            // deferred.
            let _added = unsafe { libc::sigaddset(&raw mut action.sa_mask, blocked) };
        }

        install(signal, &action)?;
    }

    Ok(())
}

/// Installs the handlers, the first time anything is contained.
///
/// # Errors
///
/// Returns the operating system error from the first handler that could not be installed. Callers
/// must refuse to spawn: continuing would leave a subtree that an interrupt cannot reap.
pub fn arm() -> io::Result<()> {
    let installed = ARMED.get_or_init(|| {
        install_with(|signal, action| {
            // SAFETY: `action` is fully initialised and the null third argument discards the old
            // disposition. The installed handler calls only async-signal-safe functions.
            let result = unsafe { libc::sigaction(signal, core::ptr::from_ref(action), core::ptr::null_mut()) };

            map_sigaction_result(result, &io::Error::last_os_error())
        })
    });

    arm_result(*installed)
}

/// The decision behind one `sigaction` call in [`arm`], taking the return code and the error
/// `errno` left behind rather than making the real call, so both outcomes — including the
/// `errno`-less case a synthetic `io::Error` can produce but a real `sigaction` failure on this
/// host cannot, since every signal `install_with` registers is a valid, catchable one — can be
/// driven directly.
fn map_sigaction_result(result: i32, last_os_error: &io::Error) -> Result<(), i32> {
    if result == 0 {
        Ok(())
    } else {
        Err(last_os_error.raw_os_error().unwrap_or(libc::EIO))
    }
}

/// The decision behind [`arm`], taking the cached installation result rather than reading it from
/// [`ARMED`], so both the success and failure the `OnceLock` can only ever cache once per process
/// can each be driven directly.
fn arm_result(cached: Result<(), i32>) -> io::Result<()> {
    match cached {
        Ok(()) => Ok(()),
        Err(errno) => Err(io::Error::from_raw_os_error(errno)),
    }
}

/// Opens the window in which a child may exist without its group being watched.
///
/// Taken before the child is created and surrendered once its group has been registered, which is
/// what stops an interrupt arriving in between from killing this process and leaving the child
/// behind. [`Spawning::interrupted`] must be consulted before anything is created: it is the
/// spawner's half of the handshake, and a caller that sees it set has been told that this process
/// may die at any moment and must create nothing.
#[must_use = "the window closes as soon as this is dropped, which reopens the race it exists to close"]
pub fn spawning() -> Spawning {
    let registry = registry();
    registry.open();

    Spawning(registry)
}

/// A spawn in flight, and the promise that no interrupt will finish this process before it is done.
///
/// Held from before the child is created until its group is watched. Dropping it closes the
/// window, and, when a handler found the window open and left the fatal half of the interrupt
/// behind, ends the process the way that handler would have.
pub struct Spawning(&'static Registry);

impl Spawning {
    /// Whether a terminal signal has already begun taking this run apart.
    ///
    /// A spawner that sees this must not create anything: the process is free to die at the next
    /// instruction, since nothing it is protecting exists yet.
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.0.signal() != CALM
    }

    /// Starts watching a freshly created group, returning the slot it took.
    ///
    /// Kills the group at once when the run is already being interrupted, because the handler that
    /// began it may have scanned this slot before it was claimed.
    ///
    /// `None` means the group is **not** being watched — either it is not a valid group id, or
    /// every one of [`capacity`]'s slots is already taken. A caller that goes on to run a child it
    /// was given `None` for has a child no interrupt can reach, which is the leak this module
    /// exists to prevent, so the answer has to be acted on rather than stored.
    #[must_use]
    pub fn watch(&self, group: i32) -> Option<usize> {
        self.0.claim(group, &kill_group)
    }

    /// Watches a Linux cgroup alongside its process group for terminal interruption.
    ///
    /// The cgroup must remain alive until [`forget_cgroup`] releases this slot. That release waits
    /// for an in-flight handler sweep before the owning `File` may close its descriptor.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn watch_cgroup(&self, group: i32, cgroup: Option<&Cgroup>) -> Option<usize> {
        let slot = self.watch(group)?;

        if let Some(cgroup) = cgroup.and_then(Cgroup::kill_handle) {
            self.0.attach_cgroup(slot, group, cgroup.raw(), &kill_cgroup);
        }

        Some(slot)
    }
}

/// Says whether this window has already been overtaken by an interrupt, and nothing else.
///
/// Written out rather than derived because the registry behind it is a thousand atomic slots, and
/// nothing reading a diagnostic wants to be shown them.
impl fmt::Debug for Spawning {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Spawning")
            .field("interrupted", &self.interrupted())
            .finish()
    }
}

impl Drop for Spawning {
    fn drop(&mut self) {
        if let Some(signal) = self.0.close_with(&kill_group, &kill_cgroup) {
            die(signal);
        }
    }
}

/// What a slot currently holds, or [`EMPTY`] for one that is free.
///
/// Test support for the process-tree tests in `cargo-gamma-process`, which is a different crate and so
/// cannot reach a `#[cfg(test)]` item here.
#[must_use]
pub fn watched(slot: usize) -> i32 {
    registry().holding(slot)
}

/// Stops watching a group, leaving a slot some other spawn has since claimed alone.
///
/// The group is named as well as the slot because a slot is only ever this caller's while the
/// group it was claimed for is still in it: a handler empties every slot it kills, and a run that
/// deferred its own death goes on spawning into the slots that frees.
pub fn forget(slot: usize, group: i32) {
    registry().release(slot, group, false);
}

/// Stops watching a Linux group and waits for any handler using its cgroup descriptor.
#[cfg(target_os = "linux")]
pub fn forget_cgroup(slot: usize, group: i32) {
    registry().release(slot, group, true);
}

/// Kills a process group, which is the only thing the registry does to the world outside itself.
fn kill_group(group: i32) {
    // SAFETY: `kill` is POSIX async-signal-safe, takes two integers and touches no caller memory.
    // `Registry::claim` admits only positive group ids, so negation is defined and addresses that
    // process group rather than an individual process.
    let _sent = unsafe { libc::kill(-group, libc::SIGKILL) };
}

/// Kills every process in a cgroup through an already-open `cgroup.kill` descriptor.
fn kill_cgroup(descriptor: i32) {
    // SAFETY: descriptors admitted here come from a live `Cgroup` retained until `forget` waits
    // for every active sweep. `write` is POSIX async-signal-safe and reads one static byte.
    let _sent = unsafe { libc::write(descriptor, b"1".as_ptr().cast(), 1) };
}

/// Dies of the signal that arrived, once every group this run started has been killed.
///
/// Re-raising rather than exiting is what makes the wait status right: a shell reports an
/// interrupted process by the signal that killed it, and a process that quietly exits instead
/// looks to every script above it like one that decided to stop.
///
/// Reached from the handler itself, and from the last spawner out of a window the handler found
/// open. Both are async-signal-safe, because the handler is the harder of the two cases and this
/// is written to it.
fn die(signal: i32) {
    // SAFETY: `signal` is async-signal-safe and is being asked only to restore the default
    // disposition for a valid signal number. `signal` rather than `sigaction` here because the
    // ambiguity that made `arm` use the latter does not arise: `SIG_DFL` means the same thing
    // under both sets of semantics, and there is no handler left whose persistence could matter.
    // Doing it first is what keeps the re-raise below from re-entering the handler forever.
    let _previous = unsafe { libc::signal(signal, libc::SIG_DFL) };

    // SAFETY: `raise` is async-signal-safe and touches no memory. The disposition is now the
    // default, so this ends the process with the status the signal implies. Reached from the
    // handler, the signal is blocked by the mask `arm` installed, so this stays pending until the
    // handler returns and is then taken by the default action; reached from the last spawner out
    // of a window, nothing is blocked and it is taken at once. Both end the process the same way.
    let _raised = unsafe { libc::raise(signal) };
}

/// Kills every watched group and then dies of the signal that arrived, unless a spawn is in flight.
///
/// A spawn in flight is a child this pass may not have been able to see, so the death is deferred
/// to the spawner rather than taken here — see the module documentation for why that is safe, and
/// why waiting for the spawn instead would deadlock whenever this handler is running on the very
/// thread performing it.
extern "C" fn handler(signal: i32) {
    if let Some(fatal) = registry().interrupt_with(signal, &kill_group, &kill_cgroup) {
        die(fatal);
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use core::cell::RefCell;
    use core::sync::atomic::AtomicBool;
    use core::time::Duration;
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::thread;
    use std::time::Instant;

    use super::*;

    /// A registry and a record of every group it asked to have killed.
    ///
    /// The kill is a parameter of the protocol precisely so that it can be recorded here: what
    /// these tests assert is which groups were reached and when, which a real `SIGKILL` answers
    /// only by leaving a process behind or not.
    fn recorded<T>(body: impl FnOnce(&Registry, &dyn Fn(i32), &RefCell<Vec<i32>>) -> T) -> T {
        let registry = Registry::new();
        let killed = RefCell::new(Vec::new());
        let kill = |group: i32| killed.borrow_mut().push(group);

        body(&registry, &kill, &killed)
    }

    fn run_process_case(case: &str) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "interrupt::tests::process_case", "--nocapture"])
            .env("CARGO_GAMMA_INTERRUPT_CASE", case)
            .status()
            .expect("run process case")
    }

    #[test]
    fn process_case() {
        let Ok(case) = std::env::var("CARGO_GAMMA_INTERRUPT_CASE") else {
            return;
        };

        match case.as_str() {
            "drop" => {
                let registry = Box::leak(Box::new(Registry::new()));
                registry.open();
                assert_eq!(registry.interrupt(libc::SIGTERM, &|_| {}), None);
                drop(Spawning(registry));
            }
            "die" => {
                // SAFETY: installing SIG_IGN for a valid signal is process-local test setup.
                let _previous = unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
                die(libc::SIGTERM);
            }
            "handler" => handler(libc::SIGTERM),
            other => panic!("unknown process case {other}"),
        }
    }

    #[test]
    fn a_spawn_guard_reports_that_its_registry_was_interrupted() {
        let registry = Box::leak(Box::new(Registry::new()));
        registry.open();
        let spawning = Spawning(registry);

        assert_eq!(registry.interrupt(libc::SIGINT, &|_| {}), None);
        assert!(spawning.interrupted());

        core::mem::forget(spawning);
    }

    #[test]
    fn a_spawn_guard_debug_value_names_its_interrupted_state() {
        let registry = Box::leak(Box::new(Registry::new()));
        let spawning = Spawning(registry);

        assert_eq!(format!("{spawning:?}"), "Spawning { interrupted: false }");

        core::mem::forget(spawning);
    }

    #[test]
    fn dropping_the_last_interrupted_spawn_window_dies_of_that_signal() {
        assert_eq!(run_process_case("drop").signal(), Some(libc::SIGTERM));
    }

    /// `die` really performs both of its two operations — [`libc::signal`] then [`libc::raise`] —
    /// which the `"die"` and `"drop"` process cases above prove by dying for real, at the cost of
    /// their own coverage: a process that ends by an uncaught signal never flushes the profile data
    /// that would show these lines as reached.
    ///
    /// Driven directly here with `SIGCHLD` — a signal whose default disposition is to be ignored
    /// rather than to terminate — so the very same production function runs to completion and this
    /// process survives to report its result rather than dying of the signal under test. The
    /// assertion is that a handler installed beforehand is *not* the one that runs: `die` resets
    /// the disposition to the default before re-raising, so the caller's own handler must not still
    /// be installed by the time the raise lands.
    #[test]
    fn dying_resets_the_disposition_to_the_default_before_re_raising() {
        static DELIVERED: AtomicBool = AtomicBool::new(false);

        extern "C" fn record(_signal: i32) {
            DELIVERED.store(true, Ordering::SeqCst);
        }

        // C spells a signal handler as an integer, so the function has to be widened into one,
        // exactly as `install_with` does for the production handler.
        #[expect(clippy::fn_to_numeric_cast_any, reason = "the C signal API takes a handler as an integer")]
        let record_handler: libc::sighandler_t = record as extern "C" fn(i32) as usize;

        // SAFETY: installs a handler for a signal this test both owns for its whole process and
        // chooses because it cannot terminate the process by default.
        let _previous = unsafe { libc::signal(libc::SIGCHLD, record_handler) };

        die(libc::SIGCHLD);

        assert!(
            !DELIVERED.load(Ordering::SeqCst),
            "die() must reset the disposition to the default before re-raising, not leave the \
             caller's handler installed"
        );
    }

    /// The production handler, called directly with a survivable signal rather than through a real
    /// delivery, dies at once when nothing is spawning — the same real [`handler`] and [`die`] the
    /// `"handler"` process case exercises, but surviving to flush its own coverage because `SIGCHLD`
    /// is not fatal by default.
    #[test]
    fn the_production_handler_dies_of_a_survivable_signal_when_nothing_is_spawning() {
        handler(libc::SIGCHLD);

        assert_eq!(registry().signal(), libc::SIGCHLD);
    }

    /// The production handler defers to the spawner when a window is open, and the spawner —
    /// dropping the last one, exactly as [`Spawning::drop`] does in production — then dies of the
    /// deferred signal itself.
    ///
    /// This is the real module-level [`spawning`], [`handler`], and [`Spawning`] drop glue, with
    /// `SIGCHLD` standing in for the terminal signal a real interrupt would use, so that both halves
    /// of the handoff run for real and this process survives to report it.
    #[test]
    fn the_production_handler_defers_to_the_spawner_which_then_dies_itself() {
        let spawning = spawning();

        handler(libc::SIGCHLD);

        assert_eq!(registry().signal(), libc::SIGCHLD, "the handler still records the interrupt");

        // `Spawning::drop` finds this the last window and this run already interrupted, so it dies
        // of `SIGCHLD` here — surviving, since that signal is not fatal by default.
        drop(spawning);
    }

    #[test]
    fn dying_uses_exactly_the_requested_signal_even_when_it_was_ignored() {
        assert_eq!(run_process_case("die").signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn the_handler_dies_of_exactly_the_signal_it_received() {
        assert_eq!(run_process_case("handler").signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn killing_a_group_reaches_every_member_with_sigkill() {
        let mut child = Command::new("sh");
        let _grouped = child
            .args(["-c", "sleep 30 & echo $!; wait"])
            .process_group(0)
            .stdout(Stdio::piped());
        let mut child = child.spawn().expect("spawn group");
        let group = i32::try_from(child.id()).expect("pid fits");
        let mut output = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();

        let _read = output.read_line(&mut line).expect("descendant pid");
        let descendant = line.trim().parse::<i32>().expect("numeric pid");

        kill_group(group);

        let status = child.wait().expect("group leader");
        assert_eq!(status.signal(), Some(libc::SIGKILL));

        let deadline = Instant::now() + Duration::from_secs(2);
        // SAFETY: signal 0 performs an existence check on the pid and dereferences no pointers.
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        // SAFETY: signal 0 performs an existence check on the pid and dereferences no pointers.
        if unsafe { libc::kill(descendant, 0) } == 0 {
            // SAFETY: cleanup of the exact child pid printed by the shell above.
            let _cleaned = unsafe { libc::kill(descendant, libc::SIGKILL) };
            panic!("the descendant survived the group kill");
        }
    }

    /// Only a real process group can be watched, since the kill negates whatever it is given.
    #[test]
    fn only_positive_process_groups_can_be_watched() {
        recorded(|registry, kill, killed| {
            assert_eq!(registry.claim(EMPTY, &kill), None);
            assert_eq!(registry.claim(-1, &kill), None);
            assert_eq!(registry.claim(i32::MIN, &kill), None);

            assert!(killed.borrow().is_empty(), "nothing that was refused may be signalled");
        });
    }

    /// A handler with nothing being spawned kills what it sees and dies of the signal itself.
    #[test]
    fn a_handler_that_finds_no_spawn_in_flight_dies_of_the_signal_itself() {
        recorded(|registry, kill, killed| {
            let _slot = registry.claim(4242, &kill).expect("a free slot");

            assert_eq!(registry.interrupt(libc::SIGINT, &kill), Some(libc::SIGINT));
            assert_eq!(*killed.borrow(), [4242], "the watched group had to go with it");
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_interrupt_kills_the_cgroup_paired_with_the_process_group() {
        let registry = Registry::new();
        let groups = RefCell::new(Vec::new());
        let cgroups = RefCell::new(Vec::new());
        let group_kill = |group| groups.borrow_mut().push(group);
        let cgroup_kill = |descriptor| cgroups.borrow_mut().push(descriptor);
        let slot = registry.claim(4242, &group_kill).expect("a free slot");

        registry.attach_cgroup(slot, 4242, 17, &cgroup_kill);

        assert_eq!(registry.interrupt_with(libc::SIGINT, &group_kill, &cgroup_kill), Some(libc::SIGINT));
        assert_eq!(*groups.borrow(), vec![4242]);
        assert_eq!(*cgroups.borrow(), vec![17]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_old_owner_cannot_clear_a_reused_slots_cgroup() {
        let registry = Registry::new();
        let groups = RefCell::new(Vec::new());
        let cgroups = RefCell::new(Vec::new());
        let group_kill = |group| groups.borrow_mut().push(group);
        let cgroup_kill = |descriptor| cgroups.borrow_mut().push(descriptor);
        let old = registry.claim(11, &group_kill).expect("a free slot");

        registry.attach_cgroup(old, 11, 17, &cgroup_kill);
        assert_eq!(registry.interrupt_with(libc::SIGINT, &group_kill, &cgroup_kill), Some(libc::SIGINT));

        let new = registry.claim(22, &group_kill).expect("the handler returned the slot");
        registry.attach_cgroup(new, 22, 23, &cgroup_kill);
        registry.release(old, 11, true);

        assert_eq!(registry.holding(new), 22);
        registry.sweep_with(&group_kill, &cgroup_kill);
        assert!(cgroups.borrow().contains(&23), "the new owner's cgroup descriptor must survive");
    }

    /// A child created in the spawn window is killed even though no handler ever saw its group.
    ///
    /// The first of the two races, driven in the order that loses the child: the signal lands after
    /// the child exists and before anything has been told about it. The handler must not die there,
    /// because dying is what leaves the child behind.
    #[test]
    fn a_group_created_inside_the_spawn_window_is_still_killed() {
        recorded(|registry, kill, killed| {
            registry.open();

            assert_eq!(registry.signal(), CALM, "the spawner saw a calm run, so it created its child");

            assert_eq!(
                registry.interrupt(libc::SIGINT, &kill),
                None,
                "a handler must not die while a child may exist that it cannot see"
            );
            assert!(killed.borrow().is_empty(), "the new group was not in the registry to be found");

            let slot = registry.claim(4242, &kill).expect("a free slot");

            assert_eq!(registry.holding(slot), 4242);
            assert_eq!(*killed.borrow(), [4242], "the spawner has to kill what the handler could not see");

            assert_eq!(
                registry.close(&kill),
                Some(libc::SIGINT),
                "and then finish the death the handler deferred"
            );
        });
    }

    /// A registration landing in a slot the handler has already scanned is still killed.
    ///
    /// The second race. The handler walks the registry once, so a group claimed into a slot behind
    /// it is a group nothing in that pass will ever look at again.
    #[test]
    fn a_group_claimed_in_a_slot_the_handler_already_scanned_is_still_killed() {
        recorded(|registry, kill, killed| {
            registry.open();

            let scanned = registry.claim(11, &kill).expect("a free slot");

            assert_eq!(registry.interrupt(libc::SIGTERM, &kill), None, "a spawn was in flight");
            assert_eq!(*killed.borrow(), [11], "the handler kills what it can see, and empties the slot");

            // The very slot the pass above has already been through, which is what makes this the
            // race rather than a restatement of the test before it.
            assert_eq!(registry.claim(22, &kill), Some(scanned));
            assert_eq!(killed.borrow()[1], 22, "a group behind the scan still has to be reached");

            assert_eq!(registry.close(&kill), Some(libc::SIGTERM));
        });
    }

    /// Only the last spawner out of the window finishes the death, and it sweeps before it does.
    #[test]
    fn the_last_spawner_out_finishes_the_death_and_no_earlier_one_does() {
        recorded(|registry, kill, killed| {
            registry.open();
            registry.open();

            assert_eq!(registry.interrupt(libc::SIGHUP, &kill), None);

            let late = registry.claim(77, &kill).expect("a free slot");

            assert_eq!(
                registry.close(&kill),
                None,
                "a window still open is a child that may still be unwatched"
            );

            assert_eq!(registry.close(&kill), Some(libc::SIGHUP));
            assert_eq!(registry.holding(late), EMPTY, "the last one out sweeps what was registered since");
            assert!(killed.borrow().contains(&77));
        });
    }

    /// A spawn window closed on a run nothing interrupted dies of nothing.
    #[test]
    fn a_window_closed_on_a_calm_run_kills_nothing_and_dies_of_nothing() {
        recorded(|registry, kill, killed| {
            registry.open();

            let _slot = registry.claim(9, &kill).expect("a free slot");

            assert_eq!(registry.close(&kill), None);
            assert!(killed.borrow().is_empty(), "an ordinary spawn must survive its own window");
        });
    }

    /// A spawner that sees the interrupt before it creates anything is told so.
    ///
    /// The other side of the handshake: the handler got there first, saw nothing in flight and is
    /// free to end the process at any instruction, so this spawner must not create a child at all.
    #[test]
    fn a_spawner_that_arrives_after_the_handler_is_told_not_to_create_anything() {
        recorded(|registry, kill, _killed| {
            assert_eq!(registry.interrupt(libc::SIGINT, &kill), Some(libc::SIGINT));

            registry.open();

            assert_eq!(
                registry.signal(),
                libc::SIGINT,
                "the run is already dying, so nothing may be spawned"
            );
            assert_eq!(registry.close(&kill), Some(libc::SIGINT));
        });
    }

    /// A slot handed back after another spawn has claimed it leaves that spawn registered.
    ///
    /// A handler empties every slot it kills, and a deferred death means the run goes on spawning
    /// into those slots. An unconditional release would take the new child out of the next sweep.
    #[test]
    fn releasing_a_slot_another_spawn_has_since_claimed_leaves_it_alone() {
        recorded(|registry, kill, _killed| {
            let mine = registry.claim(11, &kill).expect("a free slot");

            registry.release(mine, 11, false);

            let theirs = registry.claim(22, &kill).expect("the same slot, now free");

            assert_eq!(theirs, mine);

            registry.release(mine, 11, false);

            assert_eq!(registry.holding(theirs), 22, "somebody else's registration must survive");
        });
    }

    /// Releasing a slot past the end of the registry does nothing, rather than panicking.
    #[test]
    fn releasing_an_out_of_range_slot_does_nothing() {
        recorded(|registry, _kill, _killed| {
            registry.release(usize::MAX, 1, false);

            assert_eq!(registry.holding(usize::MAX), EMPTY);
        });
    }

    /// The invariant the protocol exists to keep, under threads rather than a chosen order.
    ///
    /// One thread spawns and one interrupts, with no synchronization between them beyond the
    /// protocol itself. Two things must hold however the two are scheduled: the process still dies
    /// of the signal, and any group the spawner created was killed.
    ///
    /// What this cannot show is the handshake's memory ordering. Both announcements are
    /// read-modify-writes, which on this workstation's architecture are full barriers whatever
    /// ordering they are written with, so a weakened `Ordering` would still pass here and fail on a
    /// machine that reorders a store past a later load. The sequential consistency is required by
    /// the model rather than by the schedule; the deterministic tests above are what has teeth.
    #[test]
    fn parallel_shared_state_interrupt_claims_and_sweeps_leave_no_group_orphaned() {
        for round in 0..500 {
            let registry = Registry::new();
            let killed = Mutex::new(Vec::new());
            let kill = |group: i32| killed.lock().expect("the recorder").push(group);
            let died = AtomicI32::new(CALM);
            let group = 1000 + round;

            thread::scope(|threads| {
                let spawner = threads.spawn(|| {
                    registry.open();

                    // Exactly the shape of a real spawn: nothing is created once the run is known
                    // to be dying, and what is created is registered before the window closes.
                    let created = (registry.signal() == CALM).then(|| {
                        let _slot = registry.claim(group, &kill);

                        group
                    });

                    if let Some(signal) = registry.close(&kill) {
                        died.store(signal, Ordering::Relaxed);
                    }

                    created
                });

                let interrupter = threads.spawn(|| {
                    if let Some(signal) = registry.interrupt(libc::SIGINT, &kill) {
                        died.store(signal, Ordering::Relaxed);
                    }
                });

                interrupter.join().expect("the interrupting thread");

                let created = spawner.join().expect("the spawning thread");

                assert_eq!(died.load(Ordering::Relaxed), libc::SIGINT, "the signal was never acted on");

                if let Some(group) = created {
                    assert!(
                        killed.lock().expect("the recorder").contains(&group),
                        "a group created beside the interrupt outlived it"
                    );
                }
            });
        }
    }

    /// Exhaustion is reported, not swallowed, so a caller can refuse the child it cannot watch.
    ///
    /// The leak this guards: `claim` finds a free slot with `position`, which answers `None` when
    /// there is none. A caller that stores that `None` and runs the child anyway has a child no
    /// interrupt reaches. The signal has to exist for the caller to act on it, and it has to say
    /// exhaustion rather than, say, silently reusing the last slot and dropping the group already
    /// in it — so this asserts both that the answer is `None` and that nothing was killed to
    /// produce it.
    #[test]
    fn a_full_registry_refuses_the_group_it_cannot_watch() {
        recorded(|registry, kill, killed| {
            for group in 0..capacity() {
                let slot = registry.claim(i32::try_from(group).expect("a slot index fits a group id") + 1, &kill);

                assert!(slot.is_some(), "slot {group} of {} was refused", capacity());
            }

            assert_eq!(registry.claim(9999, &kill), None, "a full registry accepted one more group");
            assert!(killed.borrow().is_empty(), "a refusal killed a group that was already watched");
        });
    }

    /// Every published slot is really usable, and releasing one really returns it to the supply.
    ///
    /// `capacity` is what bounds the run's width, so it understating or overstating the array both
    /// matter: understating wastes parallelism, overstating brings back the unwatched child.
    #[test]
    fn a_released_slot_returns_to_the_supply() {
        recorded(|registry, kill, _killed| {
            let mut taken = Vec::with_capacity(capacity());

            for group in 0..capacity() {
                let group = i32::try_from(group).expect("a slot index fits a group id") + 1;

                taken.push((registry.claim(group, &kill).expect("a free slot"), group));
            }

            let (slot, group) = taken.pop().expect("the last claim");

            registry.release(slot, group, false);

            assert_eq!(registry.claim(9999, &kill), Some(slot), "the released slot was not reused");
        });
    }

    #[test]
    fn a_handler_registration_failure_is_returned() {
        let attempted = core::cell::Cell::new(0);
        let failure = install_with(|_signal, _action| {
            attempted.set(attempted.get() + 1);
            Err(libc::EPERM)
        })
        .expect_err("registration failure must not be discarded");

        assert_eq!(failure, libc::EPERM);
        assert_eq!(attempted.get(), 1, "installation must stop at the first unprotected signal");
    }

    /// `sigaction` returning zero is success, whatever `errno` happens to hold from an earlier,
    /// unrelated call — a successful return never touches it.
    #[test]
    fn a_successful_sigaction_result_is_ok_regardless_of_stale_errno() {
        assert_eq!(map_sigaction_result(0, &io::Error::from_raw_os_error(libc::EPERM)), Ok(()));
    }

    /// A non-zero return reports the operating-system error it left behind.
    #[test]
    fn a_failed_sigaction_result_reports_its_raw_os_error() {
        assert_eq!(
            map_sigaction_result(-1, &io::Error::from_raw_os_error(libc::EINVAL)),
            Err(libc::EINVAL)
        );
    }

    /// A non-zero return without a raw OS error — which a real `sigaction` failure never produces,
    /// since every signal this module installs is valid and catchable, but which the mapping still
    /// has to answer for — falls back to `EIO`.
    #[test]
    fn a_failed_sigaction_result_without_a_raw_os_error_falls_back_to_eio() {
        assert_eq!(map_sigaction_result(-1, &io::Error::other("no raw code")), Err(libc::EIO));
    }

    /// [`arm`]'s cached success is reported as success.
    #[test]
    fn a_cached_arm_success_is_reported_as_ok() {
        arm_result(Ok(())).expect("a cached success must be reported");
    }

    /// [`arm`]'s cached failure is reported with the exact `errno` it was cached with — the one
    /// outcome a live process cannot reliably reproduce a second time, since `ARMED` caches the
    /// very first result for the rest of the process and every real terminal signal installs
    /// successfully on this host.
    #[test]
    fn a_cached_arm_failure_is_reported_with_its_errno() {
        let error = arm_result(Err(libc::EPERM)).expect_err("a cached failure must be reported");

        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    /// Every terminal signal really is armed, with the two properties the protocol depends on.
    ///
    /// Read back from the kernel rather than from the array, because the array is the intent and
    /// this is the outcome. Three things are asserted and each is a leak if it is false:
    ///
    /// * the handler is installed for all four signals — one left unhandled kills the run by the
    ///   default action while every contained child, in a process group the terminal cannot reach,
    ///   goes on running;
    /// * the disposition is not reset on delivery — a handler that deferred because a spawn window
    ///   was open must still be installed when the second `Ctrl-C` arrives, or that second signal
    ///   is the one that kills the run, and it kills nothing else;
    /// * the whole terminal set is blocked for the duration of the handler — which is what makes
    ///   the re-raise in `die` land after `SIG_DFL` is restored, and what stops a second signal
    ///   re-entering the handler and racing the sweep against itself.
    ///
    /// `SA_RESTART` is asserted too: the `EINTR` reasoning elsewhere in the run depends on it, and
    /// nothing else in the tree would notice its absence until a wait somewhere returned early.
    #[test]
    fn every_terminal_signal_is_armed_to_survive_its_own_delivery() {
        arm().expect("terminal handlers can be installed");

        #[expect(clippy::fn_to_numeric_cast_any, reason = "the C signal API takes a handler as an integer")]
        let ours: libc::sighandler_t = handler as extern "C" fn(i32) as usize;

        for signal in TERMINAL {
            let mut installed = core::mem::MaybeUninit::<libc::sigaction>::uninit();

            // SAFETY: `sigaction` is being asked to report the current disposition of a valid
            // signal number without changing it, which is what a null new-action pointer means.
            // The out pointer addresses a fully owned local of exactly the type it writes, so the
            // write is in bounds and correctly aligned, and it initialises the struct on success.
            let read = unsafe { libc::sigaction(signal, core::ptr::null(), installed.as_mut_ptr()) };

            assert_eq!(read, 0, "the disposition of signal {signal} could not be read back");

            // SAFETY: the call above returned success, which is its guarantee that it wrote a
            // complete `sigaction` through the pointer, so the value is initialised.
            let installed = unsafe { installed.assume_init() };

            assert_eq!(installed.sa_sigaction, ours, "signal {signal} is not handled by this module");
            assert_ne!(
                installed.sa_flags & libc::SA_RESTART,
                0,
                "signal {signal} was armed without SA_RESTART"
            );
            assert_eq!(
                installed.sa_flags & libc::SA_RESETHAND,
                0,
                "signal {signal} was armed to reset its disposition on delivery"
            );

            for blocked in TERMINAL {
                // SAFETY: `sigismember` reads the set through the pointer it is given, which
                // addresses the initialised local above, and every signal named is a valid number.
                let member = unsafe { libc::sigismember(&raw const installed.sa_mask, blocked) };

                assert_eq!(member, 1, "signal {blocked} is not blocked while signal {signal} is handled");
            }
        }
    }
}

#[cfg(loom)]
mod loom_models {
    use loom::sync::{Arc, Mutex};

    use super::Registry;

    pub(super) fn claim_versus_sweep_never_orphans_the_claimed_group() {
        loom::model(|| {
            const GROUP: i32 = 41;

            let registry = Arc::new(Registry::new());
            let killed = Arc::new(Mutex::new(Vec::new()));

            let claiming = {
                let registry = Arc::clone(&registry);
                let killed = Arc::clone(&killed);

                loom::thread::spawn(move || {
                    let kill = |group| killed.lock().expect("kill recorder").push(group);

                    assert!(registry.claim(GROUP, &kill).is_some());
                })
            };
            let sweeping = {
                let registry = Arc::clone(&registry);
                let killed = Arc::clone(&killed);

                loom::thread::spawn(move || {
                    let kill = |group| killed.lock().expect("kill recorder").push(group);

                    assert_eq!(registry.interrupt(libc::SIGINT, &kill), Some(libc::SIGINT));
                })
            };

            claiming.join().expect("claim thread");
            sweeping.join().expect("sweep thread");

            assert!(
                killed.lock().expect("kill recorder").contains(&GROUP),
                "the claimed group escaped both sides of the protocol"
            );
        });
    }
}

#[cfg(loom)]
pub(crate) fn run_loom_models() {
    loom_models::claim_versus_sweep_never_orphans_the_claimed_group();
}
