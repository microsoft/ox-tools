// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! macOS descendants that a process group alone does not reach.
//!
//! A process that moves to a group of its own leaves its parent's group but keeps its parent.
//! cargo-nextest does this for every test it runs. Walking parent links from the group's members
//! finds such descendants for as long as their parents are alive.

use std::collections::{BTreeSet, VecDeque};
use std::io;

use cargo_gamma_unsafe::{footprint, group};

/// The native calls a walk and a sweep make, so that tests can script a process table.
trait Processes {
    fn members(&mut self, group: i32) -> io::Result<Vec<i32>>;
    fn children(&mut self, parent: i32) -> io::Result<Vec<i32>>;
    fn group_of(&mut self, pid: i32) -> Option<i32>;
    fn stop(&mut self, group: i32) -> io::Result<()>;
    fn resume(&mut self, group: i32) -> io::Result<()>;
    fn kill(&mut self, group: i32) -> io::Result<()>;
}

struct Native;

impl Processes for Native {
    fn members(&mut self, group: i32) -> io::Result<Vec<i32>> {
        footprint::members(group)
    }

    fn children(&mut self, parent: i32) -> io::Result<Vec<i32>> {
        footprint::children(parent)
    }

    fn group_of(&mut self, pid: i32) -> Option<i32> {
        group::group_of(pid)
    }

    fn stop(&mut self, group: i32) -> io::Result<()> {
        group::stop(group)
    }

    fn resume(&mut self, group: i32) -> io::Result<()> {
        group::resume(group)
    }

    fn kill(&mut self, group: i32) -> io::Result<()> {
        group::kill(group)
    }
}

/// Lists a group's members and every process descended from one of them.
///
/// The listing is a snapshot. Members come first, in the order the group lists them.
///
/// # Errors
///
/// Returns the operating system's reason when a listing fails.
pub(crate) fn tree(group: i32) -> io::Result<Vec<i32>> {
    tree_with(&mut Native, group)
}

/// Ends the groups that descendants of `group` lead, then `group` itself through `kill_group`.
///
/// `group` is stopped first. A stopped process can neither start a child nor reap one, so the
/// walk sees every child of the group's members, and a killed child of a member stays a zombie,
/// keeping its group id reserved, until `kill_group` ends that member. A descendant's own group
/// is ended only when the descendant leads it, so a process that joins an unrelated group cannot
/// steer the sweep there. A group that ends only zombies or a process this user cannot signal
/// reports `EPERM`; like the sampler, which skips such processes, the sweep does not count it as
/// a failure.
///
/// # Errors
///
/// Returns the error from `kill_group` first, then a listing failure, then the first other failure
/// to end a descendant's group. `group` is resumed when `kill_group` fails, so a failed sweep does
/// not leave it stopped.
pub(crate) fn sweep(group: i32, kill_group: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    sweep_with(&mut Native, group, kill_group)
}

fn tree_with(processes: &mut impl Processes, group: i32) -> io::Result<Vec<i32>> {
    let mut pending = VecDeque::from(processes.members(group)?);
    let mut seen = BTreeSet::new();
    let mut tree = Vec::new();
    while let Some(pid) = pending.pop_front() {
        if seen.insert(pid) {
            tree.push(pid);
            pending.extend(processes.children(pid)?);
        }
    }
    Ok(tree)
}

fn sweep_with(processes: &mut impl Processes, group: i32, kill_group: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    // A group whose leader has exited can hold only that zombie, which macOS refuses to signal.
    // Such a group starts no children, so a failed stop costs the walk nothing.
    let _stopped = processes.stop(group);
    let ended = tree_with(processes, group).and_then(|tree| {
        let mut ended = Ok(());
        for pid in tree {
            if pid == group || processes.group_of(pid) != Some(pid) {
                continue;
            }
            let killed = match processes.kill(pid) {
                Err(cause) if cause.raw_os_error() == Some(libc::EPERM) => Ok(()),
                killed => killed,
            };
            ended = ended.and(killed);
        }
        ended
    });
    let killed = kill_group();
    if killed.is_err() {
        let _resumed = processes.resume(group);
    }
    killed.and(ended)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A scripted process table that records the signals a sweep sends.
    #[derive(Default)]
    struct Table {
        members: Vec<i32>,
        children: BTreeMap<i32, Vec<i32>>,
        groups: BTreeMap<i32, i32>,
        kill_errors: BTreeMap<i32, i32>,
        failing_listing: Option<i32>,
        signals: Vec<(&'static str, i32)>,
    }

    impl Processes for Table {
        fn members(&mut self, _group: i32) -> io::Result<Vec<i32>> {
            Ok(self.members.clone())
        }

        fn children(&mut self, parent: i32) -> io::Result<Vec<i32>> {
            if self.failing_listing == Some(parent) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(self.children.get(&parent).cloned().unwrap_or_default())
        }

        fn group_of(&mut self, pid: i32) -> Option<i32> {
            self.groups.get(&pid).copied()
        }

        fn stop(&mut self, group: i32) -> io::Result<()> {
            self.signals.push(("stop", group));
            Ok(())
        }

        fn resume(&mut self, group: i32) -> io::Result<()> {
            self.signals.push(("resume", group));
            Ok(())
        }

        fn kill(&mut self, group: i32) -> io::Result<()> {
            self.signals.push(("kill", group));
            self.kill_errors
                .get(&group)
                .map_or(Ok(()), |error| Err(io::Error::from_raw_os_error(*error)))
        }
    }

    /// cargo (10) leads the group and runs cargo-nextest (11), which runs each test (20, 30) in a
    /// group of its own. Test 20 has a child (21) in its group.
    fn nextest() -> Table {
        Table {
            members: vec![10, 11],
            children: BTreeMap::from([(10, vec![11]), (11, vec![20, 30]), (20, vec![21])]),
            groups: BTreeMap::from([(10, 10), (11, 10), (20, 20), (21, 20), (30, 30)]),
            ..Table::default()
        }
    }

    #[test]
    fn the_tree_adds_descendants_outside_the_group_once_each_after_its_members() {
        let mut table = nextest();
        assert_eq!(tree_with(&mut table, 10).unwrap(), [10, 11, 20, 30, 21]);
    }

    #[test]
    fn a_failed_child_listing_fails_the_tree() {
        let mut table = Table {
            failing_listing: Some(11),
            ..nextest()
        };
        assert_eq!(tree_with(&mut table, 10).unwrap_err().raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn a_sweep_stops_the_group_then_kills_descendant_led_groups_before_the_group() {
        let mut table = nextest();
        let mut signals = Vec::new();
        sweep_with(&mut table, 10, || {
            signals.push(("kill", 10));
            Ok(())
        })
        .unwrap();
        table.signals.extend(signals);
        assert_eq!(table.signals, [("stop", 10), ("kill", 20), ("kill", 30), ("kill", 10)]);
    }

    #[test]
    fn a_sweep_ignores_groups_a_descendant_joined_but_does_not_lead() {
        let mut table = Table {
            members: vec![10],
            children: BTreeMap::from([(10, vec![11])]),
            // 11 joined an unrelated group, such as this run's own.
            groups: BTreeMap::from([(10, 10), (11, 5)]),
            ..Table::default()
        };
        sweep_with(&mut table, 10, || Ok(())).unwrap();
        assert_eq!(table.signals, [("stop", 10)]);
    }

    #[test]
    fn a_descendant_group_that_cannot_be_signalled_is_skipped_but_other_failures_are_reported() {
        let mut table = Table {
            kill_errors: BTreeMap::from([(20, libc::EPERM)]),
            ..nextest()
        };
        sweep_with(&mut table, 10, || Ok(())).unwrap();
        assert_eq!(table.signals, [("stop", 10), ("kill", 20), ("kill", 30)]);

        let mut table = Table {
            kill_errors: BTreeMap::from([(20, libc::EINVAL)]),
            ..nextest()
        };
        let error = sweep_with(&mut table, 10, || Ok(())).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        assert_eq!(
            table.signals,
            [("stop", 10), ("kill", 20), ("kill", 30)],
            "one failure does not stop the sweep"
        );
    }

    #[test]
    fn a_failed_listing_still_kills_the_group_and_is_reported_after_its_error() {
        let mut table = Table {
            failing_listing: Some(11),
            ..nextest()
        };
        let mut killed = false;
        let error = sweep_with(&mut table, 10, || {
            killed = true;
            Ok(())
        })
        .unwrap_err();
        assert!(killed);
        assert_eq!(error.raw_os_error(), Some(libc::EIO));

        let mut table = Table {
            failing_listing: Some(11),
            ..nextest()
        };
        let error = sweep_with(&mut table, 10, || Err(io::Error::from_raw_os_error(libc::EPERM))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM), "the group's own failure takes precedence");
    }

    #[test]
    fn a_failed_group_kill_resumes_the_stopped_group() {
        let mut table = nextest();
        let error = sweep_with(&mut table, 10, || Err(io::Error::from_raw_os_error(libc::EPERM))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert_eq!(table.signals, [("stop", 10), ("kill", 20), ("kill", 30), ("resume", 10)]);
    }
}
