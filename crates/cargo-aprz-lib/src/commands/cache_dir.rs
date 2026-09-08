// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Platform cache-directory discovery.
//!
//! `cargo-aprz` keeps its downloaded crates.io dumps, advisory database and provider caches in
//! one directory below the user's cache location. That location is the only thing this module
//! answers, which is why it is a handful of environment lookups rather than a dependency:
//!
//! | Platform | Directory | Example |
//! |----------|-----------|---------|
//! | Windows  | `%LOCALAPPDATA%` | `C:\Users\alice\AppData\Local` |
//! | macOS    | `$HOME/Library/Caches` | `/Users/alice/Library/Caches` |
//! | Other    | `$XDG_CACHE_HOME`, else `$HOME/.cache` | `/home/alice/.cache` |
//!
//! An empty variable counts as unset, and a relative `XDG_CACHE_HOME` is ignored, both of which
//! the XDG base directory specification requires.

use std::ffi::OsString;
use std::path::PathBuf;

/// Returns the current user's platform cache directory, or `None` when the environment does not
/// say where it is.
pub(super) fn platform_cache_dir() -> Option<PathBuf> {
    resolve(|name| std::env::var_os(name))
}

/// Discards a variable that is set but empty, which callers must treat as unset.
fn lookup(read: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<OsString> {
    read(name).filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn resolve(read: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    lookup(&read, "LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn resolve(read: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(PathBuf::from(lookup(&read, "HOME")?).join("Library").join("Caches"))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn resolve(read: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    // The specification requires a relative value to be ignored, so a bad `XDG_CACHE_HOME` falls
    // back to the home directory rather than producing a path relative to the current directory.
    if let Some(configured) = lookup(&read, "XDG_CACHE_HOME") {
        let path = PathBuf::from(configured);
        if path.is_absolute() {
            return Some(path);
        }
    }

    Some(PathBuf::from(lookup(&read, "HOME")?).join(".cache"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Builds an environment reader over a fixed set of variables.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: HashMap<String, OsString> = vars.iter().map(|&(name, value)| (name.to_owned(), OsString::from(value))).collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn an_unset_environment_yields_no_directory() {
        assert_eq!(resolve(env(&[])), None);
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(resolve(env(&[("LOCALAPPDATA", ""), ("HOME", ""), ("XDG_CACHE_HOME", "")])), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_the_local_application_data_directory() {
        assert_eq!(
            resolve(env(&[("LOCALAPPDATA", r"C:\Users\alice\AppData\Local")])),
            Some(PathBuf::from(r"C:\Users\alice\AppData\Local"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_the_caches_directory_below_home() {
        assert_eq!(
            resolve(env(&[("HOME", "/Users/alice")])),
            Some(PathBuf::from("/Users/alice/Library/Caches"))
        );
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn an_absolute_cache_home_wins_over_the_home_directory() {
        assert_eq!(
            resolve(env(&[("XDG_CACHE_HOME", "/tmp/cache"), ("HOME", "/home/alice")])),
            Some(PathBuf::from("/tmp/cache"))
        );
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn a_relative_cache_home_is_ignored() {
        assert_eq!(
            resolve(env(&[("XDG_CACHE_HOME", "relative/cache"), ("HOME", "/home/alice")])),
            Some(PathBuf::from("/home/alice/.cache"))
        );
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn the_home_directory_supplies_the_default() {
        assert_eq!(resolve(env(&[("HOME", "/home/alice")])), Some(PathBuf::from("/home/alice/.cache")));
    }

    #[test]
    #[cfg_attr(miri, ignore = "reads the real process environment")]
    fn the_process_environment_is_consulted() {
        // Whatever the answer is, it has to agree with resolving over the real environment.
        assert_eq!(platform_cache_dir(), resolve(|name| std::env::var_os(name)));
    }
}
