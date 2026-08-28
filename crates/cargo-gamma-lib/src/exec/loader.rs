// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

/// The environment variable the dynamic loader reads on this platform.
///
/// Windows has no separate loader path: the image loader searches `PATH`, so the toolchain
/// directories have to be prepended there instead. Getting this wrong is silent — the variable is
/// set, nothing reads it, and every test binary that links a proc macro dies at startup.
#[cfg(windows)]
pub(super) const LOADER_VAR: &str = "PATH";

/// The environment variable the dynamic loader reads on this platform.
#[cfg(target_os = "macos")]
pub(super) const LOADER_VAR: &str = "DYLD_FALLBACK_LIBRARY_PATH";

/// The environment variable the dynamic loader reads on this platform.
#[cfg(not(any(windows, target_os = "macos")))]
pub(super) const LOADER_VAR: &str = "LD_LIBRARY_PATH";

/// Set on every test process a run launches, so a suite can tell it is under mutation testing.
///
/// The escape hatch for suites that invoke cargo themselves; exported because it is part of the
/// tool's contract with the code it tests.
pub const UNDER_GAMMA_VAR: &str = "CARGO_GAMMA";

/// The variable libtest reads to size the stack of each spawned test thread.
pub(super) const STACK_VAR: &str = "RUST_MIN_STACK";

/// The stack a test thread gets when the caller has not asked for a size, in bytes.
///
/// Eight times the usual two megabyte default, chosen to swallow the frame growth instrumentation
/// causes without being large enough to matter on a machine running many test processes at once,
/// since a thread stack is reserved lazily and only the pages actually touched are committed.
const STACK_FLOOR: usize = 16 * 1024 * 1024;

/// Everything about a test binary's environment that is fixed for the whole run.
///
/// Both halves were being derived on every launch, and a launch is the most frequent thing this
/// tool does — once per reachable binary per mutant, plus every confirmation probe. Neither can
/// change while a run lasts: the library directories belong to the scratch workspace, and this
/// process does not modify its own environment.
///
/// Deriving the stack floor exactly once also keeps the ambient read off the launch path, where
/// other threads are concurrently spawning children.
#[derive(Debug)]
pub(super) struct Launch {
    /// The dynamic loader search path, or `None` when there is nothing worth setting.
    pub(super) loader: Option<OsString>,

    /// The thread stack size to ask for, already resolved against any larger inherited value.
    pub(super) stack: String,
}

impl Launch {
    /// Derives the launch environment from a workspace's library directories.
    pub(super) fn derive(libraries: &[Utf8PathBuf]) -> Self {
        Self {
            loader: loader_path(libraries),
            stack: stack_floor(env::var(STACK_VAR).ok().as_deref()),
        }
    }
}

/// Applies the dynamic-library search path to one command.
///
/// Every executable Cargo built needs the same path, including the `--list` launches that precede
/// the census. Leaving listing out works for statically linked tests and fails at process startup
/// for dynamically linked proc-macro tests, before they can print a diagnostic Gamma can read.
pub(super) fn configure_loader(command: &mut Command, launch: &Launch) {
    if let Some(path) = launch.loader.as_ref() {
        let _ = command.env(LOADER_VAR, path);
    }
}

/// Returns the thread stack size to ask for, respecting a larger one the caller already chose.
///
/// The ambient value is passed in rather than read here so that this is a pure function of it. The
/// alternative — reading the variable inside, and having the test set it — puts a `setenv` in a
/// test binary whose other tests are concurrently spawning children, which reads the same variable.
/// That is a data race in `libc`, not merely a confusing value, and no amount of restoring
/// afterwards fixes it.
fn stack_floor(inherited: Option<&str>) -> String {
    let inherited = inherited.and_then(|value| value.trim().parse::<usize>().ok());

    inherited.unwrap_or(0).max(STACK_FLOOR).to_string()
}

/// Builds the loader search path, keeping whatever the caller already had.
pub(super) fn loader_path(libraries: &[Utf8PathBuf]) -> Option<OsString> {
    joined(libraries, env::var_os(LOADER_VAR))
}

/// Joins the toolchain's directories ahead of whatever search path the caller inherited.
///
/// The inherited value is a parameter rather than an ambient read so that both shapes — a caller
/// who had a search path and one who had none — can be exercised without mutating the process
/// environment, which is unsafe and visible to every other test running at the same time.
fn joined(libraries: &[Utf8PathBuf], existing: Option<OsString>) -> Option<OsString> {
    if libraries.is_empty() {
        return None;
    }

    let paths = libraries.iter().map(|path| PathBuf::from(path.as_str()));

    existing.map_or_else(
        || env::join_paths(paths.clone()).ok(),
        |current| {
            let all = paths.clone().chain(env::split_paths(&current));

            env::join_paths(all).ok()
        },
    )
}

/// Finds the directories holding the toolchain's shared libraries.
///
/// A toolchain that links `std` dynamically needs the loader to find it, and the library lives
/// beside the target's rlibs rather than in the sysroot root. `rustc` runs from inside the copied
/// tree so a `rust-toolchain.toml` there picks the same toolchain that built the binaries. A
/// missing or unparseable answer is not fatal: a statically linked toolchain needs none of this.
pub(super) fn toolchain_libraries(root: &Utf8Path, target: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut libraries = vec![target.join("debug").join("deps")];

    let output = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .current_dir(root.as_std_path())
        .args(["--print", "target-libdir", "--print", "sysroot"])
        .output();

    if let Ok(output) = output
        && output.status.success()
        && let Ok(printed) = String::from_utf8(output.stdout)
    {
        let mut lines = printed.lines();

        if let Some(libdir) = lines.next() {
            libraries.push(Utf8PathBuf::from(libdir.trim()));
        }

        // Host-side libraries, such as those a proc macro links against, live here instead.
        if let Some(sysroot) = lines.next() {
            libraries.push(Utf8PathBuf::from(sysroot.trim()).join("lib"));
        }
    }

    libraries
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    /// A caller that already exported a larger stack size than the floor keeps its own choice.
    ///
    /// The floor exists to buy back headroom instrumentation spends, not to shrink a stack the
    /// caller deliberately grew for its own reasons; silently overriding it would turn a caller's
    /// explicit choice into a smaller one and reintroduce exactly the stack overflow the floor
    /// was meant to prevent for a caller that already knew it needed more room than the default.
    ///
    /// A caller asking for less than the floor, or for nothing at all, gets the floor.
    #[test]
    fn stack_floor_keeps_a_larger_inherited_size() {
        let large = (STACK_FLOOR * 3).to_string();

        assert_eq!(stack_floor(Some(large.as_str())), large, "a larger explicit choice is kept");
        assert_eq!(stack_floor(None), STACK_FLOOR.to_string(), "nothing exported means the floor");
        assert_eq!(
            stack_floor(Some("1")),
            STACK_FLOOR.to_string(),
            "a smaller choice is raised to the floor"
        );
        assert_eq!(
            stack_floor(Some(&format!("  {} ", STACK_FLOOR * 2))),
            (STACK_FLOOR * 2).to_string(),
            "surrounding whitespace is what an exported value often carries"
        );
        assert_eq!(
            stack_floor(Some("not a number")),
            STACK_FLOOR.to_string(),
            "an unparsable value is ignored"
        );
    }

    #[test]
    fn an_empty_library_list_does_not_set_a_loader_path() {
        // Most toolchains are statically linked; setting an empty loader variable would be a
        // needless change to the test process environment.
        assert_eq!(loader_path(&[]), None);
    }

    #[test]
    fn a_caller_with_no_search_path_gets_only_the_toolchain_directories() {
        // On a machine where the loader variable is unset there is nothing to preserve, and the
        // result must still be a usable path rather than nothing at all.
        let joined = joined(&[Utf8PathBuf::from("/one"), Utf8PathBuf::from("/two")], None).expect("a path");

        assert_eq!(env::split_paths(&joined).count(), 2);
    }

    #[test]
    fn an_inherited_search_path_is_kept_behind_the_toolchain_directories() {
        // The toolchain's own `std` has to win over anything the ambient environment points at,
        // but discarding the caller's path would break binaries that legitimately need it.
        let existing = env::join_paths([PathBuf::from("/inherited")]).expect("a path");
        let joined = joined(&[Utf8PathBuf::from("/one")], Some(existing)).expect("a path");
        let parts: Vec<PathBuf> = env::split_paths(&joined).collect();

        assert_eq!(parts, vec![PathBuf::from("/one"), PathBuf::from("/inherited")]);
    }

    #[test]
    fn the_loader_variable_is_the_one_this_platform_actually_reads() {
        // Windows has no separate loader path; setting `LD_LIBRARY_PATH` there is silent, and every
        // test binary that links a proc macro then dies at startup without an explanation.
        let expected = if cfg!(windows) {
            "PATH"
        } else if cfg!(target_os = "macos") {
            "DYLD_FALLBACK_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };

        assert_eq!(LOADER_VAR, expected);
    }

    #[test]
    fn no_libraries_means_no_search_path_even_with_one_inherited() {
        assert_eq!(joined(&[], Some(OsString::from("/inherited"))), None);
    }

    #[test]
    fn the_target_deps_directory_is_always_on_the_toolchain_library_path() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("root")).unwrap();
        let target = Utf8PathBuf::from_path_buf(temporary.path().join("target")).unwrap();

        std::fs::create_dir_all(root.as_std_path()).unwrap();

        let libraries = toolchain_libraries(&root, &target);

        // Even if asking rustc for its dynamic locations fails, test binaries need their own
        // freshly-built dependency directory first.
        assert_eq!(libraries[0], target.join("debug").join("deps"));
    }
}
