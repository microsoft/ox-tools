// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Metering and bounding a test subtree's memory with a cgroup v2 leaf.
//!
//! cgroup v2 is the only unprivileged Linux facility that accounts for a whole process tree as one
//! quantity, which is exactly what a test binary is: a harness plus whatever servers, databases and
//! nested builds it starts. Every invocation gets a freshly created leaf cgroup, so the accounting
//! starts at zero without relying on a kernel that supports resetting `memory.peak`, and so one
//! mutant's peak can never be attributed to the next.
//!
//! The child is moved into that leaf by the child itself, from a
//! [`std::os::unix::process::CommandExt::pre_exec`] hook, and
//! not by this process after the spawn. Moving it afterwards leaves a window in which a test that
//! allocates immediately — which is precisely the test this exists to bound — has already escaped
//! the limit. The hook runs between `fork` and `exec` in a process that may hold locks belonging to
//! threads that no longer exist, so it does exactly one thing: a `write` of two bytes to a file
//! descriptor that was opened before the fork. No allocation, no formatting, no locking. Writing
//! `0` rather than a pid is what makes the formatting unnecessary; the kernel reads it as "the
//! process doing the writing".
//!
//! Availability is the real limitation. The host must use the unified hierarchy, and cargo-gamma's
//! own cgroup must both permit creating children and be allowed to hand the memory controller to
//! them. cgroup v2 refuses to delegate a controller out of a cgroup that still holds processes of
//! its own, so when the controller is not already delegated this process moves itself into a
//! subgroup first — but only after proving the source cgroup contains no other process. That is
//! the arrangement `systemd-run --user --scope -p Delegate=yes` expects of anyone it delegates to.
//! Where none of that is possible, `support` says so with the reason, and the run reports that
//! rather than claiming a limit it never installed.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Mutex, OnceLock};
use std::{io, thread};

#[cfg(loom)]
use loom::lazy_static;
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{PlatformError, Situation, interrupt};

/// Where the unified hierarchy is mounted, on every distribution that uses it.
const MOUNT: &str = "/sys/fs/cgroup";

/// The file naming the cgroup a process belongs to.
const SELF_CGROUP: &str = "/proc/self/cgroup";

/// The subgroup this process moves itself into when the controller has to be delegated.
const SUPERVISOR: &str = "gamma.supervisor";

/// What is written to `cgroup.procs` to move the writing process into that cgroup.
///
/// Two bytes rather than a formatted pid, so that the `pre_exec` hook needs neither a buffer nor a
/// conversion; a hook that formatted anything would be allocating between `fork` and `exec`.
const MOVE_SELF: &[u8] = b"0\n";

/// How many times removing a spent cgroup is retried before the background reaper takes over.
///
/// Removal fails while the cgroup still holds a process, which after a normal run means something
/// the test spawned outlived it. Waiting briefly collects the ordinary case; waiting indefinitely
/// would hand a run's pace to whatever a test forgot to kill.
const REMOVAL_ATTEMPTS: u32 = 20;

/// How long to wait between attempts at removing a spent cgroup.
const REMOVAL_PAUSE: Duration = Duration::from_millis(10);

/// How long the one background reaper waits between attempts at an abandoned leaf.
const REAPER_PAUSE: Duration = Duration::from_millis(100);

/// Where per-invocation cgroups are created, or the reason there is nowhere to create them.
static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// Distinguishes concurrently created cgroups, since workers create them from several threads.
#[cfg(not(loom))]
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(loom)]
lazy_static! {
    /// Distinguishes concurrently created cgroups, under a scheduler that owns its own statics.
    static ref SEQUENCE: AtomicU64 = AtomicU64::new(0);
}

/// State shared by leaf creation, destruction, and the background reaper.
///
/// A newly created leaf is registered while this mutex is held, so the reaper cannot mistake the
/// brief interval before its child joins for an abandoned empty leaf.
#[derive(Default)]
struct Reaper {
    root: Option<PathBuf>,
    active: Vec<PathBuf>,
    pending: bool,
    generation: u64,
}

/// The one reaper is enough for every leaf beneath this process's delegated root.
static REAPER: OnceLock<Mutex<Reaper>> = OnceLock::new();

/// Prevents starting one reaper thread per abandoned leaf.
#[cfg(not(loom))]
static REAPER_RUNNING: AtomicBool = AtomicBool::new(false);

#[cfg(loom)]
lazy_static! {
    /// Prevents starting one reaper thread per abandoned leaf, under the Loom scheduler.
    static ref REAPER_RUNNING: AtomicBool = AtomicBool::new(false);
}

/// How many distinct names a cgroup creation attempts before refusing to reuse a stale leaf.
const NAME_ATTEMPTS: u64 = 64;

/// Creates a cgroup child, retrying names that a crashed invocation left behind.
///
/// The sequence distinguishes concurrent calls. A collision still remains possible after a crash:
/// the sequence restarts when a replacement process receives the same pid. `EEXIST` is therefore
/// retried with another sequence value; exhausting the finite retry budget is safer than sharing
/// an old cgroup's accounting.
fn create_child_with(root: &Path, kind: &str, pid: u32, generation: u64, mut next: impl FnMut() -> u64) -> io::Result<PathBuf> {
    for _attempt in 0..NAME_ATTEMPTS {
        let path = root.join(format!("{kind}.{pid}.{generation}.{}", next()));

        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(cause),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{NAME_ATTEMPTS} cgroup names under `{}` already exist", root.display()),
    ))
}

/// Locks the state that distinguishes live leaves from ones safe to reap.
fn reaper() -> std::sync::MutexGuard<'static, Reaper> {
    REAPER
        .get_or_init(|| Mutex::new(Reaper::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn leaf_owner(name: &std::ffi::OsStr) -> Option<LeafOwner> {
    let name = name.to_str()?;
    let mut pieces = name.split('.');
    let (Some("gamma"), Some(pid), Some(third)) = (pieces.next(), pieces.next(), pieces.next()) else {
        return None;
    };

    let pid = pid.parse().ok()?;
    let generation = match (pieces.next(), pieces.next()) {
        (None, None) => {
            let _sequence: u64 = third.parse().ok()?;

            None
        }
        (Some(sequence), None) => {
            let generation = third.parse().ok()?;
            let _sequence: u64 = sequence.parse().ok()?;

            Some(generation)
        }
        _ => return None,
    };

    (pid != 0).then_some(LeafOwner { pid, generation })
}

/// Extracts Linux's process start-time generation from a `/proc/<pid>/stat` body.
fn start_time_from(stat: &str) -> Option<u64> {
    let (_identity, fields) = stat.rsplit_once(") ")?;

    // `fields` starts at field 3 (`state`); start time is field 22.
    fields.split_ascii_whitespace().nth(19)?.parse().ok()
}

/// The kernel generation of one process identifier.
fn process_generation(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;

    start_time_from(&stat).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process stat has no start time"))
}

/// Whether the exact process generation encoded by a leaf may still own it.
///
/// An error other than `NotFound` is deliberately treated as live: permission and namespace
/// boundaries must not turn a leaf belonging to another running invocation into one we remove.
fn owner_is_live(owner: LeafOwner) -> bool {
    match process_generation(owner.pid) {
        Ok(generation) => owner.generation.is_none_or(|owned| generation == owned),
        Err(cause) => cause.kind() != io::ErrorKind::NotFound,
    }
}

/// Removes every empty stale leaf this process can prove it owns.
///
/// Leaves belonging to a live foreign creator are left alone. A live leaf of this process is
/// registered before it can be observed here, so only one dropped after foreground removal gave up
/// can be removed. A failed `rmdir` normally means an orphan still occupies the cgroup; returning
/// `true` asks the background reaper to try again after it exits.
fn reap_owned_leaves(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return true;
    };
    let current_pid = process::id();
    let Ok(current_generation) = process_generation(current_pid) else {
        return true;
    };
    let current = LeafOwner {
        pid: current_pid,
        generation: Some(current_generation),
    };
    let mut pending = false;

    for entry in entries {
        let Ok(entry) = entry else {
            pending = true;
            continue;
        };

        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }

        let Some(owner) = leaf_owner(&entry.file_name()) else {
            continue;
        };

        if owner != current && owner_is_live(owner) {
            continue;
        }

        let path = entry.path();
        let active = reaper().active.iter().any(|active| active == &path);

        if active {
            continue;
        }

        if fs::remove_dir(path).is_err() {
            pending = true;
        }
    }

    pending
}

/// Hands out the value that distinguishes one concurrently created leaf name from another.
///
/// Taken through a function of its own, rather than read from the static at the call site, so that
/// the uniqueness every leaf name rests on can be modelled: a read-modify-write is indivisible and
/// a load-then-store is not, and the call site alone does not say which of the two this is. Two
/// leaves sharing a name means two invocations sharing an accounting, which is silently wrong
/// rather than loud.
///
/// Relaxed is enough. Nothing is published through this value; it is only required to be different
/// from every other value handed out, and that is a property of the read-modify-write itself.
fn next_name_ticket(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed)
}

/// Claims the sole right to run the background reaper, or declines because somebody holds it.
///
/// Taken through a function of its own for the same reason as [`next_name_ticket`]: exactly one
/// caller may be told to start a thread, and a compare-and-swap is what makes that true where a
/// load followed by a store would let two callers both see `false`. One reaper thread per
/// abandoned leaf is the failure this prevents.
fn claim_reaper(running: &AtomicBool) -> bool {
    running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Hands the reaper claim back after a start that did not happen.
///
/// Without this, a single failed thread spawn would leave the claim held forever and no later
/// abandoned leaf would ever be reaped, on a run that is still perfectly able to spawn threads.
fn release_reaper_claim(running: &AtomicBool) {
    running.store(false, Ordering::Release);
}

/// Starts the bounded, shared background reaper after foreground cleanup gives up.
fn ensure_reaper() {
    if !claim_reaper(&REAPER_RUNNING) {
        return;
    }

    match thread::Builder::new().name("gamma-cgroup-reaper".to_owned()).spawn(reaper_loop) {
        Ok(worker) => drop(worker),
        Err(_cause) => release_reaper_claim(&REAPER_RUNNING),
    }
}

/// Reaps leaves that become empty after their owning run has moved on.
fn reaper_loop() -> ! {
    loop {
        let task = {
            let state = reaper();

            if state.pending {
                state.root.clone().map(|root| (root, state.generation))
            } else {
                None
            }
        };

        if let Some((root, generation)) = task {
            let pending = reap_owned_leaves(&root);
            let mut state = reaper();

            // A drop can schedule new work while this scan is in progress. Do not clear that
            // newer request merely because the older scan happened to finish cleanly.
            if state.generation == generation {
                state.pending = pending;
            }
        }

        thread::sleep(REAPER_PAUSE);
    }
}

/// Returns the directory per-invocation cgroups are created under, or why there is none.
///
/// Settled once. The work behind it includes creating a cgroup and possibly moving this process
/// into a subgroup of its own, neither of which is worth repeating, and a run that cannot have a
/// memory ceiling deserves one clear explanation rather than one per mutant.
pub(crate) fn root() -> Result<&'static Path, &'static str> {
    settled(&ROOT, discover)
}

fn settled(
    cache: &'static OnceLock<Result<PathBuf, String>>,
    discover: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<&'static Path, &'static str> {
    match cache.get_or_init(discover) {
        Ok(path) => Ok(path.as_path()),
        Err(reason) => Err(reason.as_str()),
    }
}

/// Works out whether this process can create memory-controlled cgroups, and where.
fn discover() -> Result<PathBuf, String> {
    let root = discover_with(own, delegate, probe)?;
    let pending = reap_owned_leaves(&root);

    {
        let mut state = reaper();
        state.root = Some(root.clone());
        state.pending = pending;
        if pending {
            state.generation = state.generation.wrapping_add(1);
        }
    }

    if pending {
        ensure_reaper();
    }

    Ok(root)
}

fn discover_with(
    own: impl FnOnce() -> Result<PathBuf, String>,
    delegate: impl FnOnce(&Path) -> Result<PathBuf, String>,
    probe: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let own = own()?;
    let root = delegate(&own)?;

    probe(&root)?;

    Ok(root)
}

/// The cgroup this process is currently in.
fn own() -> Result<PathBuf, String> {
    let listed = fs::read_to_string(SELF_CGROUP).map_err(|cause| format!("`{SELF_CGROUP}` could not be read: {cause}"))?;

    resolve(Path::new(MOUNT), &listed)
}

/// Where a `/proc/<pid>/cgroup` body places this process beneath a unified-hierarchy mount.
///
/// The path in that file is relative to the mount even though it is written with a leading slash,
/// so joining it unchanged would produce the absolute path it looks like and land outside the
/// hierarchy entirely.
///
/// Split out from the reading so both the joining and the two refusals can be exercised against a
/// mount a test controls. Neither can be reached through `own`, which reads the host's real
/// `/proc` and its real hierarchy, and so answers the same way on every run of a given machine.
fn resolve(mount: &Path, listed: &str) -> Result<PathBuf, String> {
    let relative = unified_entry(listed).ok_or_else(|| "this host is not using the cgroup v2 unified hierarchy".to_owned())?;
    let path = mount.join(relative.trim().trim_start_matches('/'));

    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "cargo-gamma's own cgroup is not visible at `{}`, which happens when the unified \
             hierarchy is mounted elsewhere or when this process is in a container that hides it",
            path.display()
        ))
    }
}

/// The unified-hierarchy path in the body of a `/proc/<pid>/cgroup` file.
///
/// The unified hierarchy is always the entry with an empty controller list and hierarchy id zero.
/// Anything else in this file belongs to a v1 hierarchy, which has no aggregate memory accounting
/// worth using.
///
/// Split out from the reading so the parsing can be tested against the file bodies real hosts
/// produce. The contents differ enough between a systemd desktop, a container and a v1-only host
/// that guessing at them is not good enough, and none of those shapes can be arranged by a test
/// that has to read the real `/proc`.
fn unified_entry(listed: &str) -> Option<&str> {
    listed.lines().find_map(|line| line.strip_prefix("0::"))
}

/// Ensures children of `own` will have the memory controller, moving this process if it must.
fn delegate(own: &Path) -> Result<PathBuf, String> {
    if lists(own, "cgroup.subtree_control", "memory") {
        return Ok(own.to_owned());
    }

    if !lists(own, "cgroup.controllers", "memory") {
        return Err(format!(
            "the memory controller is not available to cargo-gamma's cgroup `{}`; a delegated \
             cgroup with the memory controller is needed, as `systemd-run --user --scope -p \
             Delegate=yes` provides",
            own.display()
        ));
    }

    if enable(own).is_ok() {
        return Ok(own.to_owned());
    }

    // cgroup v2 will not hand a controller to the children of a cgroup that still holds processes,
    // and this process is one of them. Moving into a subgroup is what a delegated unit is expected
    // to do, and it leaves the delegation boundary itself free to distribute controllers.
    let supervisor = own.join(SUPERVISOR);

    fs::create_dir_all(&supervisor).map_err(|cause| {
        format!(
            "`{}` could not be created: {cause}. Creating a cgroup there needs one delegated to this \
             process, as `systemd-run --user --scope -p Delegate=yes` provides",
            supervisor.display()
        )
    })?;

    vacate(own, &supervisor)?;

    enable(own).map_err(|cause| {
        format!(
            "the memory controller could not be delegated to the children of `{}`: {cause}. \
             This usually means the cgroup is shared with processes that are not part of this run",
            own.display()
        )
    })?;

    // #[gamma::skip(result.ok_to_err, reason = "this recovery succeeds only after the live cgroup kernel refuses enabling a controller, processes are migrated, and the same write then succeeds; an ordinary directory cannot reproduce that state transition")]
    Ok(own.to_owned())
}

/// Moves this process out of `own` and into `supervisor` if it is the sole occupant.
///
/// A delegated unit normally contains only this process. A shared cgroup can also contain jobs
/// unrelated to this invocation, so moving any other listed PID would silently alter another
/// process's cgroup membership. A process that joins after the check is not moved; the subsequent
/// controller-enablement write will fail while it remains in `own`.
///
/// # Errors
///
/// Returns the reason when the cgroup could not be emptied, which leaves the caller to report that
/// no ceiling was installed rather than to proceed as though one had been.
fn vacate(own: &Path, supervisor: &Path) -> Result<(), String> {
    let own_procs = own.join("cgroup.procs");
    let listed = fs::read_to_string(&own_procs).map_err(|cause| format!("`{}` could not be read: {cause}", own_procs.display()))?;
    let own_pid = process::id().to_string();
    let mut occupants = listed.split_ascii_whitespace();

    if occupants.next() != Some(own_pid.as_str()) || occupants.next().is_some() {
        return Err(format!(
            "`{}` is not occupied solely by cargo-gamma; cargo-gamma will not change any other process's cgroup membership",
            own.display()
        ));
    }

    let procs = supervisor.join("cgroup.procs");

    fs::write(&procs, MOVE_SELF).map_err(|cause| format!("`{}` could not be written: {cause}", procs.display()))
}

/// Asks a cgroup to hand the memory controller to its children.
fn enable(own: &Path) -> io::Result<()> {
    fs::write(own.join("cgroup.subtree_control"), "+memory")
}

/// Whether one of a cgroup's space-separated interface files names `wanted`.
fn lists(path: &Path, file: &str, wanted: &str) -> bool {
    fs::read_to_string(path.join(file)).is_ok_and(|text| text.split_ascii_whitespace().any(|name| name == wanted))
}

/// Confirms that a child cgroup can actually be created here and offers what the run needs.
///
/// Both files are checked because they answer different questions and fail on different hosts:
/// `memory.max` is what bounds a mutant, and `memory.peak` — which arrived in Linux 5.19 — is what
/// the baseline measures in order to choose that bound.
fn probe(root: &Path) -> Result<(), String> {
    probe_with(root, process::id(), || next_name_ticket(&SEQUENCE))
}

fn probe_with(root: &Path, pid: u32, next: impl FnMut() -> u64) -> Result<(), String> {
    let generation = process_generation(pid).map_err(|cause| format!("process generation could not be read: {cause}"))?;
    let path = create_child_with(root, "gamma.probe", pid, generation, next).map_err(|cause| {
        format!(
            "a unique child cgroup could not be created under `{}`: {cause}. Creating one needs a \
             delegated cgroup, as `systemd-run --user --scope -p Delegate=yes` provides",
            root.display()
        )
    })?;

    let result = probe_result(root, &path);
    let _removed = fs::remove_dir(&path);

    result
}

fn probe_result(root: &Path, path: &Path) -> Result<(), String> {
    unoffered(path).map_or(Ok(()), |name| {
        Err(format!(
            "a child cgroup created under `{}` has no `{name}`, so this kernel cannot \
             {} a process tree's memory",
            root.display(),
            if name == "memory.peak" { "measure" } else { "bound" }
        ))
    })
}

/// Which of the interface files the run depends on a cgroup does not offer, if any.
///
/// Split out from `probe` because `probe` can only ask about a cgroup it has just created, whose
/// contents are the kernel's to decide. A host either offers both files or neither, so the case
/// that matters most — a pre-5.19 kernel with `memory.max` but no `memory.peak` — cannot be
/// reached on any one machine.
fn unoffered(path: &Path) -> Option<&'static str> {
    ["memory.max", "memory.peak"].into_iter().find(|name| !path.join(name).exists())
}

/// Reports the failure of one leaf, rather than of the host that would have held it.
///
/// Separated from [`Situation::Unsupported`] because the two demand opposite responses: a host that
/// can never hold a leaf lets an unmetered run degrade once, while a host that can and did not is
/// one launch that must be refused rather than run outside its boundary.
fn refused(reason: String) -> PlatformError {
    PlatformError::new(Situation::Refused, reason)
}

/// One invocation's accounting boundary: a cgroup leaf holding a test binary and its descendants.
#[derive(Debug)]
pub struct Cgroup {
    /// The leaf's directory, removed when this is dropped.
    path: PathBuf,

    /// An already-open kill switch that the interrupt handler can write without resolving a path.
    kill: Option<File>,

    /// Whether this leaf was registered with the shared reaper.
    tracked: bool,

    /// The interrupt-registry slot this leaf's kill descriptor was published to, if any.
    ///
    /// Recorded by [`interrupt::Spawning::watch_cgroup`] as it publishes the descriptor, and
    /// discharged by this type's own `Drop` — which is what makes the registration impossible for
    /// a safe caller to outlive. Nothing outside this crate can create, observe, or clear it.
    watch: Option<CgroupWatch>,
}

/// Where a live cgroup's kill descriptor is published in the process-wide interrupt registry.
///
/// The descriptor has a registry slot independent of the leader's process-group slot. Retaining
/// the descriptor lets `Drop` clear the slot only while it still holds this cgroup's registration.
#[derive(Debug)]
pub(crate) struct CgroupWatch {
    slot: usize,
    descriptor: RawFd,
}

/// An open cgroup kill switch, valid while its owning [`Cgroup`] remains alive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KillHandle(RawFd);

/// The creator identity encoded in one of this process's per-invocation leaf names.
///
/// Probe and supervisor cgroups deliberately do not match: only leaves named by
/// [`Cgroup::create_under_with`] are candidates for deferred removal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LeafOwner {
    pid: u32,
    generation: Option<u64>,
}

impl KillHandle {
    pub(crate) const fn raw(self) -> RawFd {
        self.0
    }
}

impl Cgroup {
    /// Creates a leaf, optionally bounded, ready for a child to move itself into.
    ///
    /// # Errors
    ///
    /// Reports [`Situation::Unsupported`] when this host offers no cgroup root to create leaves
    /// under at all — no cgroup v2 unified hierarchy, no delegated cgroup, no memory controller to
    /// hand to children, or a kernel without the interface files a run depends on. This is a
    /// host-wide capability classification rather than one failed leaf.
    ///
    /// Reports [`Situation::Refused`] when a root exists but this leaf could not be made: the
    /// creator's own generation could not be read, every candidate name under the root was taken,
    /// a ceiling could not be written, or the leaf's `cgroup.kill` switch could not be opened. That
    /// concerns one launch rather than the host, and the caller must not launch that command.
    pub fn create(limit: Option<u64>) -> Result<Self, PlatformError> {
        let root = root().map_err(|reason| PlatformError::new(Situation::Unsupported, reason))?;
        Self::create_at(root, limit)
    }

    /// Creates an unmetered leaf when cgroup containment is supported.
    ///
    /// Returns `Ok(None)` for the cached host-wide unsupported condition without constructing a
    /// backtrace-bearing [`PlatformError`]. Per-launch failures remain [`Situation::Refused`]
    /// errors because a supported host failed to create this particular boundary.
    pub fn create_unmetered_if_supported() -> Result<Option<Self>, PlatformError> {
        let Ok(root) = root() else {
            return Ok(None);
        };

        Self::create_at(root, None).map(Some)
    }

    fn create_at(root: &Path, limit: Option<u64>) -> Result<Self, PlatformError> {
        let (group, start_reaper) = {
            // Creation and registration are one critical section. A reaper never sees a
            // successfully created leaf as inactive between these two steps.
            let mut reaper = reaper();
            let mut group = Self::create_under(root, limit).map_err(refused)?;
            let kill_path = group.path.join("cgroup.kill");
            group.kill = Some(OpenOptions::new().write(true).open(&kill_path).map_err(|cause| {
                PlatformError::because(Situation::Refused, format!("`{}` could not be opened", kill_path.display()), cause)
            })?);

            reaper.root = Some(root.to_path_buf());
            reaper.active.push(group.path.clone());

            group.tracked = true;

            (group, reaper.pending)
        };

        if start_reaper {
            ensure_reaper();
        }

        Ok(group)
    }

    /// Creates and configures a leaf beneath an already selected cgroup root.
    fn create_under(root: &Path, limit: Option<u64>) -> Result<Self, String> {
        Self::create_under_with(root, limit, process::id(), || next_name_ticket(&SEQUENCE))
    }

    fn create_under_with(root: &Path, limit: Option<u64>, pid: u32, next: impl FnMut() -> u64) -> Result<Self, String> {
        let generation = process_generation(pid).map_err(|cause| format!("process generation could not be read: {cause}"))?;
        let path = create_child_with(root, "gamma", pid, generation, next)
            .map_err(|cause| format!("a unique child cgroup could not be created under `{}`: {cause}", root.display()))?;

        let group = Self {
            path,
            kill: None,
            tracked: false,
            watch: None,
        };

        group.configure(limit)?;

        Ok(group)
    }

    /// Installs the ceiling and the settings that make reaching it fail cleanly.
    ///
    /// Separate from `create` so the interface files it writes can be pinned against a directory a
    /// test controls; `create` itself can only run where the kernel has delegated a cgroup.
    fn configure(&self, limit: Option<u64>) -> Result<(), String> {
        // An OOM that killed one process of the tree and left the rest running would turn a mutant
        // that exhausted memory into a suite failing for an unrelated-looking reason, with the
        // survivors still holding locks in the scratch tree. Best effort, because a kernel without
        // it still enforces the ceiling; it merely enforces it less tidily.
        let _grouped = self.set("memory.oom.group", "1");

        if let Some(limit) = limit {
            self.set("memory.max", &limit.to_string())?;

            // Capping resident memory alone turns a crash into swap thrashing: the workload stays
            // under `memory.max` by pushing pages to disk and the machine becomes unusable while
            // the mutant is technically within its budget. Best effort, since a host without swap
            // accounting has nothing to disable.
            let _unswapped = self.set("memory.swap.max", "0");
        }

        Ok(())
    }

    /// Arranges for the child to place itself in this cgroup before it executes.
    ///
    /// # Errors
    ///
    /// Reports [`Situation::Refused`] when the leaf's `cgroup.procs` could not be opened for
    /// writing before the fork. The descriptor has to exist before the child does, because the
    /// child moves itself in between `fork` and `exec`; the caller must not spawn after this
    /// refusal.
    pub fn arm(&self, command: &mut Command) -> Result<(), PlatformError> {
        let procs = self.path.join("cgroup.procs");
        let file = OpenOptions::new()
            .write(true)
            .open(&procs)
            .map_err(|cause| PlatformError::because(Situation::Refused, format!("`{}` could not be opened", procs.display()), cause))?;

        let join = move || {
            let fd = file.as_raw_fd();

            // SAFETY: the descriptor is open for writing and owned by the `File` this closure
            // holds, so it cannot have been closed or reused between the open above and this
            // write. The buffer is a `'static` constant and its length is passed exactly, so
            // `write` reads only initialized memory that outlives the call.
            let written = unsafe { libc::write(fd, MOVE_SELF.as_ptr().cast(), MOVE_SELF.len()) };

            // A child that could not join the cgroup must not go on to exec: it would run
            // unaccounted and unbounded, which is the one outcome this whole mechanism exists to
            // prevent. Failing the spawn instead is reported to the caller as a setup failure
            // rather than as a verdict about any mutant.
            if usize::try_from(written).is_ok_and(|count| count == MOVE_SELF.len()) {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };

        // SAFETY: `pre_exec` requires that the closure be safe to run between `fork` and `exec` in
        // a child that has only the forking thread and may hold locks the other threads left
        // behind. `join` allocates nothing, takes no lock, and calls exactly one function —
        // `write`, which POSIX lists as async-signal-safe. The descriptor it writes to was opened
        // before the fork, so no path is resolved and no file is opened in the child, and
        // `as_raw_fd` reads a field rather than performing a call at all. `io::Error` from a raw
        // errno allocates nothing either.
        let _armed = unsafe { command.pre_exec(join) };

        Ok(())
    }

    /// What this cgroup accounted for, once the subtree has finished.
    #[must_use]
    pub fn usage(&self) -> (Option<u64>, bool) {
        (self.get("memory.peak").and_then(|text| text.trim().parse().ok()), self.oom_killed())
    }

    /// Kills everything in the cgroup, including anything that left the process group.
    pub fn kill(&self) {
        if let Some(kill) = self.kill.as_ref() {
            let fd = kill.as_raw_fd();
            // SAFETY: the descriptor is owned by `self`, remains open for this call, and the byte
            // is static initialized storage whose exact length is supplied.
            let _killed = unsafe { libc::write(fd, b"1".as_ptr().cast(), 1) };
        } else {
            let _killed = self.set("cgroup.kill", "1");
        }
    }

    /// The already-open kill switch the signal registry may use while this cgroup is retained.
    ///
    /// The registry stores the raw descriptor after this borrow ends, which is safe only because
    /// [`Self::watched_at`] records where it went and this type's `Drop` hands it back — after
    /// waiting for any sweep still using it — before the owning `File` closes it. Nothing outside
    /// this crate can reach either half, so the pairing cannot be got wrong by a safe caller.
    #[must_use]
    pub(crate) fn kill_handle(&self) -> Option<KillHandle> {
        self.kill.as_ref().map(|kill| KillHandle(kill.as_raw_fd()))
    }

    /// Records where this leaf's kill descriptor was published, so its drop can take it back.
    ///
    /// Called by [`interrupt::Spawning::watch_cgroup`] as the descriptor is published. The unique
    /// cgroup borrow keeps the owning file live until this reminder is stored.
    pub(crate) const fn is_watched(&self) -> bool {
        self.watch.is_some()
    }

    /// Records the owned registry lifetime of this leaf's kill descriptor.
    pub(crate) const fn watched_at(&mut self, slot: usize, descriptor: RawFd) {
        self.watch = Some(CgroupWatch { slot, descriptor });
    }

    /// Whether the kernel reported killing this workload for reaching its ceiling.
    ///
    /// Only `oom` and `oom_kill` count. A `max` event says an allocation reached the ceiling, which
    /// happens whenever reclaim is doing its job, and a suite that allocated hard and then passed
    /// is not a mutant the tests caught. The local file is preferred because it counts this cgroup
    /// alone; the aggregate one is the fallback for kernels that do not offer it.
    fn oom_killed(&self) -> bool {
        let events = self
            .get("memory.events.local")
            .or_else(|| self.get("memory.events"))
            .unwrap_or_default();

        events.lines().any(|line| {
            let mut fields = line.split_ascii_whitespace();
            let name = fields.next();
            let count = fields.next().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);

            matches!(name, Some("oom" | "oom_kill")) && count > 0
        })
    }

    /// Writes one of the cgroup's interface files.
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let path = self.path.join(name);

        fs::write(&path, value).map_err(|cause| format!("`{}` could not be written: {cause}", path.display()))
    }

    /// Reads one of the cgroup's interface files, if the kernel offers it.
    fn get(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.path.join(name)).ok()
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // Discharged first, and unconditionally once a registration was ever made. The registry
        // holds a raw descriptor this leaf owns, and the `kill` field below closes it as this
        // value is destroyed; a handler sweeping while that happened would write `"1"` into
        // whatever the kernel handed the number to next. The release waits for every sweep in
        // flight, so by the time it returns no handler holds this descriptor and none can take it
        // up again. Doing it here rather than asking the caller to is what makes the pairing
        // impossible to get wrong: there is no safe way to drop a watched cgroup without it.
        if let Some(watch) = self.watch.take() {
            interrupt::release_watched_cgroup(watch.slot, watch.descriptor);
        }

        // A cgroup that still holds a process cannot be removed. Foreground cleanup stays bounded
        // so one orphan cannot set the run's pace; the shared reaper keeps trying after this
        // method returns and removes the leaf once the orphan exits.
        let removed = remove_with_retry(|| fs::remove_dir(&self.path).is_ok(), || thread::sleep(REMOVAL_PAUSE));

        if !self.tracked {
            return;
        }

        let start_reaper = {
            let mut reaper = reaper();

            reaper.active.retain(|active| active != &self.path);

            if !removed {
                reaper.pending = true;
                reaper.generation = reaper.generation.wrapping_add(1);
            }

            !removed
        };

        if start_reaper {
            ensure_reaper();
        }
    }
}

fn remove_with_retry(mut remove: impl FnMut() -> bool, mut pause: impl FnMut()) -> bool {
    for _attempt in 0..REMOVAL_ATTEMPTS {
        if remove() {
            return true;
        }

        pause();
    }

    false
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::error::Error as _;

    use super::*;

    /// Why the tests below are `#[ignore]`d, and how to run them.
    ///
    /// Delegation is not universal: containers, CI runners and ordinary unprivileged systemd
    /// sessions all differ, so these cannot run everywhere. What they must not do is *skip*
    /// invisibly. Returning early leaves them reported as passes, which is the worst of the
    /// options — these are the only tests standing behind memory accounting, ceiling enforcement
    /// and leaf cleanup, so a green run on an undelegated host silently asserts nothing about the
    /// feature and a regression in any of them stays invisible until somebody happens to
    /// run the suite somewhere delegated. `#[ignore]` says so in the harness's own vocabulary, in
    /// every runner, without a convention anybody has to know to read.
    const NEEDS_DELEGATION: &str = "needs a delegated cgroup: run with --ignored under `systemd-run --user --scope -p Delegate=yes`";

    /// Arbitrary valid process-group identifiers; replacement must differ from watched.
    const WATCHED_PROCESS_GROUP: i32 = 41;
    const REPLACEMENT_PROCESS_GROUP: i32 = 42;

    /// Fails an explicitly requested run on a host that cannot support it, saying what is missing.
    ///
    /// Reached only when somebody asked for these by name, so the answer is a failure rather than a
    /// skip: they asked for coverage of the memory feature and did not get it, and the reason is
    /// the same one the tool itself would give a user who asked for a ceiling here.
    fn demand_delegation() {
        assert!(root().is_ok(), "{NEEDS_DELEGATION}: {:?}", root().err());
    }

    /// A cgroup standing over a plain directory whose kill switch a signal handler can be given.
    ///
    /// The switch is unlinked as soon as it is opened, so the leaf's directory is still removable
    /// when it is dropped while the descriptor itself stays open and valid — which is the property
    /// the registration these tests are about depends on.
    fn watchable(path: &Path) -> Cgroup {
        let switch = path.join("cgroup.kill");
        let kill = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&switch)
            .expect("the stand-in kill switch is creatable");

        fs::remove_file(&switch).expect("the stand-in kill switch is unlinkable");

        Cgroup {
            path: path.to_path_buf(),
            kill: Some(kill),
            tracked: false,
            watch: None,
        }
    }

    /// A dropped cgroup takes its kill descriptor out of the interrupt registry itself.
    ///
    /// The registry holds a raw descriptor this leaf owns, and dropping the leaf closes it. A
    /// registration left behind is a number the kernel is free to hand to the next `open` on any
    /// thread, and the next terminal signal would then write `"1"` into an unrelated file or an
    /// unrelated cgroup. Nothing but this drop is asked to prevent that: there is no release for a
    /// caller to skip.
    #[test]
    fn dropping_a_watched_cgroup_takes_its_kill_descriptor_out_of_the_registry() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("leaf");

        fs::create_dir(&path).expect("created");

        let mut group = watchable(&path);
        let descriptor = group.kill.as_ref().expect("the stand-in leaf has a kill switch").as_raw_fd();
        let watched = WATCHED_PROCESS_GROUP;
        let spawning = interrupt::spawning();
        let slot = spawning
            .watch_cgroup(watched, Some(&mut group))
            .expect("a free slot, since a fresh registry has a thousand");
        let cgroup_slot = group.watch.as_ref().expect("the cgroup owns its registry watch").slot;

        assert_eq!(interrupt::watched(slot), watched, "the process group was never published");
        assert_eq!(
            interrupt::watched_cgroup(cgroup_slot),
            Some(descriptor),
            "the kill descriptor was never published"
        );

        drop(group);

        assert_eq!(
            interrupt::watched_cgroup(cgroup_slot),
            None,
            "a dropped cgroup left its now-closed kill descriptor registered"
        );

        interrupt::forget(slot, watched);
        drop(spawning);
    }

    /// A dropped cgroup leaves a slot some later spawn has claimed alone.
    ///
    /// The subtree that owned this leaf hands its process-group slot back as soon as its leader is
    /// reaped, well before the leaf itself is dropped, and a run goes on spawning into the slots
    /// that frees. A drop that cleared its remembered slot unconditionally would take a live
    /// child out of the next sweep.
    #[test]
    fn dropping_a_watched_cgroup_leaves_a_slot_its_group_no_longer_holds() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("leaf");

        fs::create_dir(&path).expect("created");

        let mut group = watchable(&path);
        let watched = WATCHED_PROCESS_GROUP;
        let spawning = interrupt::spawning();
        let slot = spawning.watch_cgroup(watched, Some(&mut group)).expect("a free slot");
        let cgroup_slot = group.watch.as_ref().expect("the cgroup owns its registry watch").slot;
        let descriptor = group.kill.as_ref().expect("the stand-in leaf has a kill switch").as_raw_fd();

        // What the owning subtree does the moment its leader is reaped.
        interrupt::forget(slot, watched);
        assert_eq!(
            interrupt::watched_cgroup(cgroup_slot),
            Some(descriptor),
            "releasing the leader's process group retracted descendant interruption coverage"
        );

        let replacement = REPLACEMENT_PROCESS_GROUP;
        let taken = spawning.watch(replacement).expect("the freed slot, or another");

        drop(group);

        assert_eq!(
            interrupt::watched(taken),
            replacement,
            "a dropped cgroup took a later child's registration with it"
        );

        interrupt::forget(taken, replacement);
        drop(spawning);
    }

    /// A cgroup can publish its kill descriptor only once.
    #[test]
    fn a_cgroup_cannot_be_registered_twice() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("leaf");

        fs::create_dir(&path).expect("created");

        let mut group = watchable(&path);
        let spawning = interrupt::spawning();
        let first = spawning
            .watch_cgroup(WATCHED_PROCESS_GROUP, Some(&mut group))
            .expect("the first registration succeeds");
        let cgroup_slot = group.watch.as_ref().expect("the cgroup owns its registry watch").slot;

        assert_eq!(
            spawning.watch_cgroup(REPLACEMENT_PROCESS_GROUP, Some(&mut group)),
            None,
            "a second registration must be refused"
        );
        assert_eq!(
            interrupt::watched_cgroup(cgroup_slot),
            group.kill.as_ref().map(std::os::fd::AsRawFd::as_raw_fd),
            "the original descriptor registration must remain owned"
        );

        interrupt::forget(first, WATCHED_PROCESS_GROUP);
        drop(group);
        drop(spawning);
    }

    /// A cgroup standing over a plain directory, for testing the interface-file handling.
    ///
    /// Everything a `Cgroup` does to a live leaf it does by reading and writing named files in one
    /// directory, so an ordinary directory stands in for the kernel's faithfully enough to pin
    /// which names are used and how their contents are read.
    fn over(path: &Path) -> Cgroup {
        Cgroup {
            path: path.to_path_buf(),
            kill: None,
            tracked: false,
            watch: None,
        }
    }

    #[test]
    fn creating_a_leaf_returns_the_configured_group() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let group = Cgroup::create_under(root.path(), Some(4096)).expect("a configured leaf");

        assert!(group.path.parent().is_some_and(|parent| parent == root.path()));
        assert_eq!(fs::read_to_string(group.path.join("memory.max")).expect("written"), "4096");
        assert_eq!(fs::read_to_string(group.path.join("memory.oom.group")).expect("written"), "1");
    }

    /// A crashed process can leave the probe directory matching a reused pid and reset sequence.
    #[test]
    fn a_stale_probe_directory_is_skipped_for_a_distinct_name() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let pid = process::id();
        let generation = process_generation(pid).expect("this process has a generation");
        let stale = root.path().join(format!("gamma.probe.{pid}.{generation}.0"));

        fs::create_dir(&stale).expect("the stale probe directory exists");

        let mut sequence = 0;
        let refusal = probe_with(root.path(), pid, || {
            let candidate = sequence;
            sequence += 1;
            candidate
        })
        .expect_err("the fresh probe lacks cgroup interface files");

        assert!(refusal.contains("memory.max"), "{refusal}");
        assert!(stale.is_dir(), "the stale probe was not reused or removed");
        assert!(
            !root.path().join(format!("gamma.probe.{pid}.{generation}.1")).exists(),
            "the temporary retry probe is cleaned up after checking it"
        );
    }

    /// A crashed leaf cannot be reused as a later run's accounting boundary.
    #[test]
    fn a_stale_leaf_directory_is_skipped_for_a_distinct_name() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let pid = process::id();
        let generation = process_generation(pid).expect("this process has a generation");
        let stale = root.path().join(format!("gamma.{pid}.{generation}.0"));

        fs::create_dir(&stale).expect("the stale leaf directory exists");

        let mut sequence = 0;
        let group = Cgroup::create_under_with(root.path(), None, pid, || {
            let candidate = sequence;
            sequence += 1;
            candidate
        })
        .expect("the collision is retried");

        assert_eq!(group.path, root.path().join(format!("gamma.{pid}.{generation}.1")));
        assert!(stale.is_dir(), "the stale leaf was not reused or removed");

        fs::remove_file(group.path.join("memory.oom.group")).expect("test configuration is removed");
        drop(group);
    }

    #[test]
    fn a_successful_discovery_is_returned_from_the_cache() {
        let cache = Box::leak(Box::new(OnceLock::new()));
        let expected = PathBuf::from("delegated");

        assert_eq!(settled(cache, || Ok(expected.clone())), Ok(expected.as_path()));
    }

    #[test]
    fn successful_discovery_returns_the_delegated_root() {
        let own = tempfile::tempdir().expect("a temporary directory");
        let path = own.path().to_path_buf();

        assert_eq!(
            discover_with(|| Ok(path.clone()), |found| Ok(found.to_path_buf()), |_delegated| Ok(())),
            Ok(path)
        );
    }

    #[test]
    fn the_live_process_cgroup_is_visible_under_the_unified_mount() {
        match own() {
            Ok(path) => {
                assert!(path.is_dir(), "{}", path.display());
                assert_ne!(path, Path::new(""));
            }
            Err(reason) => assert!(!reason.is_empty(), "an unsupported host explains why"),
        }
    }

    /// A cgroup occupied only by this process is vacated.
    #[test]
    fn a_cgroup_containing_only_this_process_is_vacated() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let own = directory.path().join("own");
        let supervisor = directory.path().join("supervisor");

        fs::create_dir_all(&own).expect("created");
        fs::create_dir_all(&supervisor).expect("created");
        fs::write(own.join("cgroup.procs"), process::id().to_string()).expect("written");
        fs::write(supervisor.join("cgroup.procs"), "").expect("written");

        assert_eq!(vacate(&own, &supervisor), Ok(()));
        assert_eq!(
            fs::read(supervisor.join("cgroup.procs")).expect("readable"),
            MOVE_SELF,
            "this process should have moved itself to the supervisor"
        );
    }

    #[test]
    fn removal_uses_exactly_the_configured_number_of_attempts() {
        let attempts = core::cell::Cell::new(0_u32);
        let pauses = core::cell::Cell::new(0_u32);

        assert!(!remove_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                false
            },
            || pauses.set(pauses.get() + 1),
        ));

        assert_eq!(attempts.get(), REMOVAL_ATTEMPTS);
        assert_eq!(pauses.get(), REMOVAL_ATTEMPTS);
    }

    /// A leaf that outlived foreground retries is retried after its last process has left.
    #[test]
    fn an_abandoned_owned_leaf_is_reaped_once_it_empties() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let pid = process::id();
        let generation = process_generation(pid).expect("this process has a generation");
        let path = root.path().join(format!("gamma.{pid}.{generation}.0"));

        fs::create_dir(&path).expect("the leaf exists");
        fs::write(path.join("occupied"), "").expect("the simulated orphan occupies it");

        assert!(reap_owned_leaves(root.path()), "the occupied leaf needs another pass");
        assert!(path.exists(), "an occupied leaf cannot be removed");

        fs::remove_file(path.join("occupied")).expect("the simulated orphan exits");

        assert!(!reap_owned_leaves(root.path()), "the now-empty leaf was not removed");
        assert!(!path.exists(), "the empty abandoned leaf remains");
    }

    #[test]
    fn only_per_invocation_leaf_names_are_owned_by_the_reaper() {
        assert_eq!(
            leaf_owner(std::ffi::OsStr::new("gamma.42.7.9")),
            Some(LeafOwner {
                pid: 42,
                generation: Some(7),
            })
        );
        assert_eq!(
            leaf_owner(std::ffi::OsStr::new("gamma.42.9")),
            Some(LeafOwner { pid: 42, generation: None }),
            "legacy leaves remain recognizable for cleanup after their pid exits"
        );
        assert_eq!(leaf_owner(std::ffi::OsStr::new("gamma.probe.42.7.9")), None);
        assert_eq!(leaf_owner(std::ffi::OsStr::new("gamma.supervisor")), None);
        assert_eq!(leaf_owner(std::ffi::OsStr::new("gamma.0.7.9")), None);
        assert_eq!(leaf_owner(std::ffi::OsStr::new("gamma.42.not-a-generation.9")), None);
        assert_eq!(leaf_owner(std::ffi::OsStr::new("gamma.42.7.not-a-sequence")), None);
    }

    #[test]
    fn a_leaf_from_a_recycled_live_pid_generation_is_reaped() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let pid = process::id();
        let generation = process_generation(pid).expect("this process has a generation");
        let stale_generation = generation.wrapping_add(1);
        let path = root.path().join(format!("gamma.{pid}.{stale_generation}.0"));

        fs::create_dir(&path).expect("the stale leaf exists");

        assert!(!reap_owned_leaves(root.path()));
        assert!(!path.exists(), "a live pid from another generation retained the stale leaf");
    }

    #[test]
    fn parallel_shared_state_cgroup_sweeps_leave_no_owned_leaf_orphaned() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let pid = process::id();
        let generation = process_generation(pid).expect("this process has a generation");

        for sequence in 0..64 {
            fs::create_dir(root.path().join(format!("gamma.{pid}.{generation}.{sequence}"))).expect("leaf created");
        }

        thread::scope(|scope| {
            for _sweeper in 0..8 {
                let _worker = scope.spawn(|| {
                    let _pending = reap_owned_leaves(root.path());
                });
            }
        });

        let owned = fs::read_dir(root.path())
            .expect("root remains readable")
            .filter_map(Result::ok)
            .filter(|entry| leaf_owner(&entry.file_name()).is_some())
            .count();

        assert_eq!(owned, 0, "parallel reapers left owned leaves behind");
    }

    /// A shared cgroup is refused without moving any of its processes.
    #[test]
    fn a_cgroup_containing_foreign_processes_is_left_unchanged() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let own = directory.path().join("own");
        let supervisor = directory.path().join("supervisor");

        fs::create_dir_all(&own).expect("created");
        fs::create_dir_all(&supervisor).expect("created");
        fs::write(own.join("cgroup.procs"), format!("{}\n101\n", process::id())).expect("written");
        fs::write(supervisor.join("cgroup.procs"), "").expect("written");

        let refusal = vacate(&own, &supervisor).expect_err("a shared cgroup cannot be used");

        assert!(refusal.contains("not occupied solely by cargo-gamma"), "{refusal}");
        assert_eq!(
            fs::read(supervisor.join("cgroup.procs")).expect("readable"),
            b"",
            "no process should be moved from a shared cgroup"
        );
    }

    /// An empty listing does not prove this process is the cgroup's sole occupant.
    #[test]
    fn an_empty_cgroup_is_not_vacated_as_if_it_contained_this_process() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let own = directory.path().join("own");
        let supervisor = directory.path().join("supervisor");

        fs::create_dir_all(&own).expect("created");
        fs::create_dir_all(&supervisor).expect("created");
        fs::write(own.join("cgroup.procs"), "").expect("written");
        fs::write(supervisor.join("cgroup.procs"), "").expect("written");

        let refusal = vacate(&own, &supervisor).expect_err("an empty cgroup does not contain cargo-gamma");

        assert!(refusal.contains("not occupied solely by cargo-gamma"), "{refusal}");
        assert_eq!(fs::read(supervisor.join("cgroup.procs")).expect("readable"), b"");
    }

    /// A cgroup whose listing cannot be read is refused, rather than taken to be empty.
    #[test]
    fn a_cgroup_with_no_listing_to_read_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let supervisor = directory.path().join("supervisor");

        fs::create_dir_all(&supervisor).expect("created");
        fs::write(supervisor.join("cgroup.procs"), "").expect("written");

        assert!(vacate(directory.path(), &supervisor).is_err());
    }

    /// Usage is read from the file the kernel records a subtree's high-water mark in.
    #[test]
    fn usage_reports_the_peak_the_kernel_recorded() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        fs::write(directory.path().join("memory.peak"), "4096\n").expect("written");

        assert_eq!(over(directory.path()).usage().0, Some(4096));
    }

    /// A kernel that records no peak yields no measurement, rather than a zero.
    ///
    /// `memory.peak` arrived in Linux 5.19, and reading its absence as zero would tell the caller
    /// the workload used nothing — which is a measurement, and a wrong one.
    #[test]
    fn a_cgroup_with_no_recorded_peak_measures_nothing() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let group = over(directory.path());

        assert_eq!(group.usage().0, None);

        fs::write(directory.path().join("memory.peak"), "not a number\n").expect("written");

        assert_eq!(group.usage().0, None);
    }

    /// Reaching the ceiling is not the same as being killed at it.
    ///
    /// A `max` event is raised whenever an allocation is held back so reclaim can run, which a
    /// memory-hungry suite that then passes does routinely. Counting it would convict every such
    /// mutant of exhausting memory when the tests had simply not caught it.
    #[test]
    fn a_workload_that_was_only_held_back_was_not_killed() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        fs::write(
            directory.path().join("memory.events.local"),
            "low 0\nhigh 12\nmax 34\noom 0\noom_kill 0\n",
        )
        .expect("written");

        assert!(!over(directory.path()).usage().1);
    }

    /// Either kind of out-of-memory event convicts the workload.
    #[test]
    fn either_out_of_memory_event_marks_the_workload_exhausted() {
        for events in ["max 9\noom 1\noom_kill 0\n", "max 9\noom 0\noom_kill 1\n"] {
            let directory = tempfile::tempdir().expect("a temporary directory");

            fs::write(directory.path().join("memory.events.local"), events).expect("written");

            assert!(over(directory.path()).usage().1, "{events}");
        }
    }

    /// The local event counts are believed over the aggregate ones.
    ///
    /// `memory.events` accumulates the descendants' events too, so a leaf that was never killed
    /// would inherit a sibling's conviction from it. The local file counts this cgroup alone and
    /// is the answer whenever the kernel offers it.
    #[test]
    fn local_event_counts_are_preferred_to_aggregated_ones() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("memory.events.local"), "oom 0\noom_kill 0\n").expect("written");
        fs::write(path.join("memory.events"), "oom 1\noom_kill 1\n").expect("written");

        assert!(!over(path).usage().1, "the aggregate file should not have been consulted");

        fs::remove_file(path.join("memory.events.local")).expect("removed");

        assert!(over(path).usage().1, "the aggregate file is the fallback");
    }

    /// A cgroup with no event file at all reports no exhaustion.
    #[test]
    fn a_cgroup_reporting_no_events_is_not_exhausted() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        assert!(!over(directory.path()).usage().1);
    }

    /// Killing a cgroup asks the kernel to kill the whole subtree at once.
    ///
    /// This is what reaches processes that left the process group, which is why it is done through
    /// the cgroup rather than by signalling the child.
    #[test]
    fn killing_a_cgroup_asks_the_kernel_to_kill_the_subtree() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        over(path).kill();

        assert_eq!(fs::read_to_string(path.join("cgroup.kill")).expect("written"), "1");
    }

    /// The listed path is resolved beneath the mount, not treated as an absolute path.
    ///
    /// `/proc/self/cgroup` writes the path with a leading slash although it is relative to the
    /// mount point. Joining it as it stands would discard the mount and yield a path outside the
    /// hierarchy, which exists on no host and would report every machine as unable to bound memory.
    #[test]
    fn a_listed_cgroup_is_resolved_beneath_the_mount() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let mount = directory.path();

        fs::create_dir_all(mount.join("user.slice/session-3.scope")).expect("created");

        assert_eq!(
            resolve(mount, "12:pids:/elsewhere\n0::/user.slice/session-3.scope\n"),
            Ok(mount.join("user.slice/session-3.scope"))
        );
    }

    /// A host with no unified hierarchy is told so, rather than shown a path.
    #[test]
    fn a_host_without_a_unified_hierarchy_is_told_which_hierarchy_is_missing() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let refusal = resolve(directory.path(), "4:memory:/user.slice\n").expect_err("no v2 entry");

        assert!(refusal.contains("cgroup v2 unified hierarchy"), "{refusal}");
    }

    /// A cgroup named in the listing but absent under the mount is refused, naming the path.
    ///
    /// This is what a container that hides the hierarchy looks like from the inside: the listing
    /// names a cgroup, and nothing is there. Returning the path anyway would leave every later
    /// write failing one at a time with no explanation of the cause.
    #[test]
    fn a_cgroup_that_is_not_visible_under_the_mount_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let refusal = resolve(directory.path(), "0::/user.slice/hidden.scope\n").expect_err("nothing is there");

        assert!(refusal.contains("is not visible"), "{refusal}");
        assert!(refusal.contains("hidden.scope"), "the path has to be named, {refusal}");
    }

    /// An event count that is not a number does not convict the workload.
    ///
    /// Reading an unparsable count as though it were positive would mark a mutant as having
    /// exhausted memory on the strength of a line that was never understood.
    #[test]
    fn an_unreadable_event_count_does_not_convict() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        fs::write(directory.path().join("memory.events.local"), "oom\noom_kill bogus\n").expect("written");

        assert!(!over(directory.path()).usage().1);
    }

    /// A cgroup that can be removed is removed at once, without waiting out the retries.
    ///
    /// Removal is retried because a cgroup still holding a process cannot be removed, but the
    /// ordinary case is that it is empty. Retrying regardless would add the whole retry budget to
    /// the end of every invocation, which for a sweep of thousands is the run's pace.
    #[test]
    fn a_removable_cgroup_is_removed_without_waiting() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("leaf");

        fs::create_dir(&path).expect("created");

        let started = std::time::Instant::now();

        drop(Cgroup {
            path: path.clone(),
            kill: None,
            tracked: false,
            watch: None,
        });

        let budget = REMOVAL_PAUSE * REMOVAL_ATTEMPTS;

        assert!(!path.exists(), "the cgroup should have been removed");
        assert!(
            started.elapsed() < budget,
            "removal waited {:?}, most of the {budget:?} retry budget",
            started.elapsed()
        );
    }

    /// A cgroup that cannot be removed is left behind rather than blocking the run.
    ///
    /// A cgroup still holding an orphan the test spawned will not become removable, and an
    /// untidy directory is a far smaller cost than a run that stops until that orphan exits.
    #[test]
    fn a_cgroup_that_cannot_be_removed_is_left_behind() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("leaf");

        fs::create_dir(&path).expect("created");
        fs::write(path.join("occupied"), "").expect("written");

        let started = std::time::Instant::now();

        drop(Cgroup {
            path: path.clone(),
            kill: None,
            tracked: false,
            watch: None,
        });

        assert!(path.exists(), "a non-empty cgroup cannot be removed");
        assert!(
            started.elapsed() >= REMOVAL_PAUSE,
            "removal gave up after {:?} without pausing for the orphan to exit",
            started.elapsed()
        );
    }

    /// A cgroup offering both interface files the run needs is accepted.
    #[test]
    fn a_cgroup_offering_both_memory_files_is_accepted() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("memory.max"), "max\n").expect("written");
        fs::write(path.join("memory.peak"), "0\n").expect("written");

        assert_eq!(unoffered(path), None);
    }

    /// Each missing interface file is named on its own.
    ///
    /// `memory.max` and `memory.peak` are absent on different hosts — the second arrived only in
    /// Linux 5.19 — and they answer different questions, so a kernel that can bound memory but not
    /// measure it has to be told which of the two it is missing rather than that something is.
    #[test]
    fn each_missing_memory_file_is_named_separately() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        assert_eq!(unoffered(path), Some("memory.max"));

        fs::write(path.join("memory.max"), "max\n").expect("written");

        assert_eq!(unoffered(path), Some("memory.peak"));
    }

    /// A bounded leaf carries its ceiling, has swap denied to it, and dies whole.
    ///
    /// All three settings matter and each fails differently: without the ceiling nothing is
    /// bounded, without denying swap a runaway thrashes the disk instead of failing, and without
    /// grouping the kill one process of the tree dies while its siblings run on holding locks.
    #[test]
    fn a_bounded_leaf_carries_its_ceiling_and_denies_itself_swap() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();
        let group = over(path);

        assert_eq!(group.configure(Some(4096)), Ok(()));
        assert_eq!(fs::read_to_string(path.join("memory.max")).expect("written"), "4096");
        assert_eq!(fs::read_to_string(path.join("memory.swap.max")).expect("written"), "0");
        assert_eq!(fs::read_to_string(path.join("memory.oom.group")).expect("written"), "1");
    }

    /// An unbounded leaf still dies whole, but is given no ceiling to reach.
    ///
    /// A run with no measured baseline has no figure to bound a mutant by, and inventing one would
    /// convict mutants of exhausting a limit the run never chose.
    #[test]
    fn an_unbounded_leaf_is_given_no_ceiling() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();
        let group = over(path);

        assert_eq!(group.configure(None), Ok(()));
        assert!(!path.join("memory.max").exists(), "an unbounded leaf should carry no ceiling");
        assert!(!path.join("memory.swap.max").exists());
        assert_eq!(fs::read_to_string(path.join("memory.oom.group")).expect("written"), "1");
    }

    /// A ceiling that cannot be installed fails the leaf rather than being passed over.
    ///
    /// The two best-effort settings are degradations; the ceiling is the whole mechanism. A leaf
    /// that reported success without one would run every mutant unbounded.
    #[test]
    fn a_ceiling_that_cannot_be_written_fails_the_leaf() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("memory.max");

        fs::create_dir(&path).expect("created");

        let refusal = over(directory.path())
            .configure(Some(4096))
            .expect_err("a directory cannot be written");

        assert!(refusal.contains("could not be written"), "{refusal}");
    }

    /// An armed command puts itself in the cgroup before it runs anything.
    ///
    /// The child has to join the cgroup itself, between `fork` and `exec`, because a parent that
    /// wrote the pid afterwards would leave a window in which the child had already begun
    /// allocating outside any ceiling.
    #[test]
    fn an_armed_command_places_itself_in_the_cgroup() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let procs = directory.path().join("cgroup.procs");

        fs::write(&procs, "").expect("written");

        let group = over(directory.path());
        let mut command = Command::new("true");

        group.arm(&mut command).expect("the temporary `cgroup.procs` can be opened");
        assert!(command.status().expect("`true` should run").success());
        assert_eq!(
            fs::read(&procs).expect("readable"),
            MOVE_SELF,
            "the child should have written itself in"
        );
    }

    /// A cgroup that cannot be joined fails the spawn rather than running the child unbounded.
    ///
    /// Running a mutant outside its cgroup is the one outcome the whole mechanism exists to
    /// prevent, so an unopenable `cgroup.procs` has to be a setup failure and not a verdict.
    #[test]
    fn a_cgroup_that_cannot_be_joined_fails_the_command() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let refusal = over(directory.path())
            .arm(&mut Command::new("true"))
            .expect_err("there is no `cgroup.procs` to open");

        assert_eq!(refusal.situation(), Situation::Refused);
        assert!(refusal.to_string().contains("could not be opened"), "{refusal}");
        assert!(refusal.source().is_some(), "the operating system's own reason must be retained");
    }

    #[test]
    fn a_cgroup_that_refuses_the_child_write_fails_the_command() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        std::os::unix::fs::symlink("/dev/full", directory.path().join("cgroup.procs")).expect("linked");

        let group = over(directory.path());
        let mut command = Command::new("true");

        group.arm(&mut command).expect("the hook is installed");

        assert!(command.status().is_err(), "the child must not exec outside the cgroup");
    }

    /// The unified entry is picked out of a listing that also names v1 hierarchies.
    ///
    /// This is the shape a host running both hierarchies produces, and the v2 line is not first.
    /// Matching on the `0::` prefix rather than on position is what makes the order irrelevant.
    #[test]
    fn the_unified_entry_is_found_among_version_one_hierarchies() {
        let listed = "12:pids:/user.slice\n4:memory:/user.slice\n0::/user.slice/session-3.scope\n";

        assert_eq!(unified_entry(listed), Some("/user.slice/session-3.scope"));
    }

    /// A host with no unified hierarchy is reported as having none, rather than misread.
    ///
    /// The `0::` prefix carries two claims at once — hierarchy id zero and an empty controller
    /// list — and only an entry making both is a v2 entry. A v1 line for the memory controller
    /// looks superficially similar and would offer memory accounting that does not aggregate the
    /// way this code depends on.
    #[test]
    fn a_listing_with_no_unified_entry_yields_nothing() {
        assert_eq!(unified_entry("12:pids:/user.slice\n4:memory:/user.slice\n"), None);
        assert_eq!(unified_entry(""), None);
        assert_eq!(unified_entry("10::/not-hierarchy-zero\n"), None);
    }

    /// The root cgroup is an empty path rather than an absent one.
    #[test]
    fn the_root_cgroup_is_reported_as_an_empty_relative_path() {
        assert_eq!(unified_entry("0::/\n"), Some("/"));
    }

    /// An interface file names a controller only when it is one of the space-separated words.
    ///
    /// The listing is a word list, so a substring match would find "memory" inside a controller
    /// named `memory_recursiveprot` and conclude the controller was available when it is not —
    /// leaving the run to install a ceiling the kernel never honors.
    #[test]
    fn a_controller_listing_matches_whole_words_only() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("cgroup.controllers"), "cpu io memory pids\n").expect("written");

        assert!(lists(path, "cgroup.controllers", "memory"));
        assert!(lists(path, "cgroup.controllers", "cpu"));
        assert!(lists(path, "cgroup.controllers", "pids"));
        assert!(!lists(path, "cgroup.controllers", "mem"));
        assert!(!lists(path, "cgroup.controllers", "memor"));
        assert!(!lists(path, "cgroup.controllers", "hugetlb"));
    }

    /// A cgroup file that cannot be read names nothing, rather than failing the run.
    ///
    /// Absence is the ordinary case on a host without the controller, and it has to be answerable
    /// without an error, because the caller's next move is to say so in its own words.
    #[test]
    fn an_unreadable_controller_listing_names_nothing() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        assert!(!lists(directory.path(), "cgroup.controllers", "memory"));
    }

    /// A cgroup that already offers the memory controller to its children is used as it is.
    ///
    /// The point is that nothing is written: the process is already somewhere suitable, and moving
    /// it or rewriting `cgroup.subtree_control` would disturb a working arrangement.
    #[test]
    fn a_cgroup_already_offering_memory_to_children_is_used_unchanged() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("cgroup.subtree_control"), "memory pids\n").expect("written");

        assert_eq!(delegate(path).as_deref(), Ok(path));
        assert!(!path.join(SUPERVISOR).exists(), "nothing should have been created");
    }

    /// A cgroup without the memory controller at all is refused, naming the remedy.
    ///
    /// This is the case an undelegated host lands in, and the message is the only thing standing
    /// between a user and an unexplained absence of memory limits, so it has to name the command
    /// that fixes it.
    #[test]
    fn a_cgroup_without_the_memory_controller_is_refused_with_the_remedy() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("cgroup.controllers"), "cpu io pids\n").expect("written");

        let refusal = delegate(path).expect_err("a cgroup with no memory controller cannot be used");

        assert!(refusal.contains("memory controller is not available"), "{refusal}");
        assert!(refusal.contains("Delegate=yes"), "the remedy has to be named, {refusal}");
    }

    /// A cgroup that has the controller but is not yet offering it has it turned on.
    #[test]
    fn a_cgroup_holding_the_controller_has_it_enabled_for_children() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("cgroup.controllers"), "cpu memory\n").expect("written");

        assert_eq!(delegate(path).as_deref(), Ok(path));
        assert_eq!(fs::read_to_string(path.join("cgroup.subtree_control")).expect("written"), "+memory");
    }

    /// A probe reports what the cgroup could not offer, by name.
    ///
    /// `memory.max` and `memory.peak` answer different questions and are missing on different
    /// hosts — the second arrived only in Linux 5.19 — so a probe that said only "unsuitable"
    /// would leave a user on an older kernel with nothing to act on.
    #[test]
    fn a_probe_names_the_interface_file_that_was_missing() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let refusal = probe(directory.path()).expect_err("a plain directory offers no memory files");

        assert!(refusal.contains("memory.max"), "{refusal}");
    }

    #[test]
    fn a_probe_result_accepts_every_required_interface_file() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path();

        fs::write(path.join("memory.max"), "max\n").expect("written");
        fs::write(path.join("memory.peak"), "0\n").expect("written");

        assert_eq!(probe_result(path, path), Ok(()));
    }

    /// A probe leaves nothing behind, whether it succeeded or not.
    ///
    /// The probe creates a real child cgroup to find out whether it can. Leaving it in place would
    /// accumulate one dead cgroup per run on every host that supports the feature.
    #[test]
    fn a_probe_removes_the_child_it_created() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let _refusal = probe(directory.path());
        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .expect("readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();

        assert!(leftovers.is_empty(), "the probe left {leftovers:?} behind");
    }

    /// Whatever this host answers, the detection is decisive and repeatable.
    #[test]
    fn capability_detection_settles_on_one_answer() {
        // A detection that answered differently on the second call would install a limit for some
        // mutants and not others, and the run would report a protection it only partly had.
        let first = root().map(Path::to_path_buf);
        let second = root().map(Path::to_path_buf);

        assert_eq!(first, second);
    }

    /// An unsupported host explains itself in terms of the machine, not of this tool.
    #[test]
    fn an_undelegated_host_says_what_is_missing() {
        if let Err(reason) = root() {
            assert!(
                reason.contains("cgroup") || reason.contains("memory") || reason.contains("kernel"),
                "{reason}"
            );
        }
    }

    /// A cgroup leaf measures the memory of the child that ran in it.
    #[test]
    #[ignore = "needs a delegated cgroup: run with --ignored under `systemd-run --user --scope -p Delegate=yes`"]
    fn a_leaf_measures_what_the_child_allocated() {
        demand_delegation();

        let group = Cgroup::create(None).expect("a leaf is created on a delegated host");
        let mut command = Command::new("sh");

        // Allocated and then touched, because a mapping nothing writes to is never resident and
        // would leave the peak unchanged.
        let _ = command.args(["-c", "dd if=/dev/zero of=/dev/null bs=1M count=64 2>/dev/null"]);

        group.arm(&mut command).expect("the hook is installed");

        let mut child = command.spawn().expect("spawn");

        assert!(child.wait().expect("wait").success());

        let usage = group.usage();

        assert!(usage.0.is_some_and(|peak| peak > 0), "{usage:?}");
        assert!(!usage.1, "{usage:?}");
    }

    /// A child that passes the ceiling is killed by the kernel and reported as such.
    #[test]
    #[ignore = "needs a delegated cgroup: run with --ignored under `systemd-run --user --scope -p Delegate=yes`"]
    fn a_child_that_passes_the_ceiling_is_reported_as_exhausted() {
        demand_delegation();

        // The distinction this asserts is the whole point of reading the cgroup's own events: a
        // process killed by the kernel for reaching the ceiling and a test that simply failed both
        // exit non-zero, and only one of them is a memory verdict.
        //
        // The allocation is a shared-memory file rather than a disk one, because page cache backed
        // by a disk is reclaimable and would keep the workload under the ceiling forever instead of
        // crossing it.
        let fill = format!("/dev/shm/gamma-fill.{}", process::id());
        let group = Cgroup::create(Some(32 * 1024 * 1024)).expect("a bounded leaf is created");
        let mut command = Command::new("sh");
        let _ = command.args(["-c", &format!("dd if=/dev/zero of={fill} bs=1M count=256 2>/dev/null")]);

        group.arm(&mut command).expect("the hook is installed");

        let mut child = command.spawn().expect("spawn");
        let _status = child.wait().expect("wait");
        let usage = group.usage();
        let _removed = fs::remove_file(&fill);

        assert!(usage.1, "{usage:?}");
    }

    /// A cgroup that was never used is removed when it is dropped.
    #[test]
    #[ignore = "needs a delegated cgroup: run with --ignored under `systemd-run --user --scope -p Delegate=yes`"]
    fn a_spent_leaf_is_removed() {
        demand_delegation();

        // One directory per invocation and thousands of invocations per run: leaking them would
        // eventually reach the kernel's own limit on how many descendants a cgroup may have.
        let group = Cgroup::create(None).expect("a leaf is created on a delegated host");
        let path = group.path.clone();

        drop(group);

        assert!(!path.exists());
    }

    /// Orphans can outlast bounded foreground cleanup, but not the shared reaper.
    #[test]
    #[ignore = "needs a delegated cgroup: run with --ignored under `systemd-run --user --scope -p Delegate=yes`"]
    fn repeated_orphan_exits_do_not_accumulate_leaves() {
        demand_delegation();

        for _ in 0..3 {
            let group = Cgroup::create(None).expect("a leaf is created on a delegated host");
            let path = group.path.clone();
            let mut command = Command::new("sh");

            // Longer than the bounded foreground retry period, so dropping the leaf genuinely
            // hands this cleanup to the reaper rather than taking the ordinary fast path.
            let _ = command.args(["-c", "sleep 1"]);

            group.arm(&mut command).expect("the hook is installed");

            let mut child = command.spawn().expect("the orphan starts");

            drop(group);

            assert!(path.exists(), "foreground cleanup unexpectedly waited for the orphan");
            assert!(child.wait().expect("the orphan exits").success());

            let deadline = std::time::Instant::now() + Duration::from_secs(10);

            while path.exists() && std::time::Instant::now() < deadline {
                thread::sleep(REAPER_PAUSE);
            }

            assert!(!path.exists(), "the reaper left `{}` behind", path.display());
        }
    }
}

#[cfg(loom)]
pub(crate) mod loom_models {
    use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use loom::sync::{Arc, Mutex};

    use super::{claim_reaper, next_name_ticket, release_reaper_claim};

    /// Two workers creating leaves at the same moment never get the same name.
    ///
    /// A leaf name carries the ticket and nothing else that distinguishes one concurrent creation
    /// from another, so two workers handed the same ticket would build the same directory name.
    /// One of them then loses the `EEXIST` retry to the other and the two invocations share an
    /// accounting: the second mutant is charged the first one's peak, or is bounded by a limit that
    /// was never its own. Nothing observable says this happened.
    ///
    /// Fails as soon as the read-modify-write becomes a load followed by a store, which is the one
    /// edit that looks harmless here.
    pub(super) fn concurrent_leaf_creations_never_share_a_name() {
        loom::model(|| {
            let sequence = Arc::new(AtomicU64::new(0));
            let handed_out = Arc::new(Mutex::new(Vec::new()));

            let workers: Vec<_> = (0..2)
                .map(|_worker| {
                    let sequence = Arc::clone(&sequence);
                    let handed_out = Arc::clone(&handed_out);

                    loom::thread::spawn(move || {
                        let ticket = next_name_ticket(&sequence);

                        handed_out.lock().expect("ticket recorder").push(ticket);
                    })
                })
                .collect();

            for worker in workers {
                worker.join().expect("ticket thread");
            }

            let mut tickets = handed_out.lock().expect("ticket recorder").clone();

            tickets.sort_unstable();

            let issued = tickets.len();

            tickets.dedup();

            assert_eq!(tickets.len(), issued, "two concurrent leaf creations were handed the same name");
        });
    }

    /// However many leaves are abandoned at once, exactly one caller starts the reaper.
    ///
    /// The reaper is a permanent thread with a sleep loop, so a second one is not a duplicate piece
    /// of work that finishes: it is a thread that lives as long as the run and scans the same
    /// directory for as long. Two of them can also both see the same abandoned leaf and race each
    /// other's `remove_dir`.
    ///
    /// Fails as soon as the claim becomes a load followed by a store, which lets both callers see
    /// `false`.
    pub(super) fn only_one_caller_ever_starts_the_reaper() {
        loom::model(|| {
            let running = Arc::new(AtomicBool::new(false));
            let starters = Arc::new(Mutex::new(0_usize));

            let callers: Vec<_> = (0..2)
                .map(|_caller| {
                    let running = Arc::clone(&running);
                    let starters = Arc::clone(&starters);

                    loom::thread::spawn(move || {
                        if claim_reaper(&running) {
                            *starters.lock().expect("start recorder") += 1;
                        }
                    })
                })
                .collect();

            for caller in callers {
                caller.join().expect("claim thread");
            }

            assert_eq!(*starters.lock().expect("start recorder"), 1, "the number of reaper threads started");
        });
    }

    /// A claim handed back after a failed thread spawn leaves the next caller able to start one.
    ///
    /// The retry transition. A run whose first `thread::Builder::spawn` failed — a momentary
    /// thread-table shortage — must not be left permanently without a reaper, because the leaves
    /// that need one are exactly the ones foreground cleanup already gave up on. Modelled against
    /// a concurrent claimant so that handing the claim back cannot instead take it away from
    /// somebody who has already started the thread.
    pub(super) fn a_reaper_claim_handed_back_can_be_taken_again() {
        loom::model(|| {
            let running = Arc::new(AtomicBool::new(false));
            let started = Arc::new(Mutex::new(0_usize));

            // The caller whose thread spawn fails: it claims, cannot start, and gives the claim up.
            let failing = {
                let running = Arc::clone(&running);

                loom::thread::spawn(move || {
                    if claim_reaper(&running) {
                        release_reaper_claim(&running);
                    }
                })
            };

            let retrying = {
                let running = Arc::clone(&running);
                let started = Arc::clone(&started);

                loom::thread::spawn(move || {
                    if claim_reaper(&running) {
                        *started.lock().expect("start recorder") += 1;
                    }
                })
            };

            failing.join().expect("failing thread");
            retrying.join().expect("retrying thread");

            // Whoever ends up holding the claim, the run must not be left believing a reaper is
            // running when none is: either the second caller started one, or the claim is free for
            // the next abandoned leaf to try again.
            let running_now = running.load(Ordering::SeqCst);
            let started_now = *started.lock().expect("start recorder");

            assert!(
                started_now == 1 || !running_now,
                "the reaper claim was left held with no reaper behind it"
            );
        });
    }

    /// Runs every cgroup atomic model.
    pub(crate) fn run() {
        concurrent_leaf_creations_never_share_a_name();
        only_one_caller_ever_starts_the_reaper();
        a_reaper_claim_handed_back_can_be_taken_again();
    }
}
