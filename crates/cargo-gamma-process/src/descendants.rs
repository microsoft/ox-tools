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
    fn stop_process(&mut self, pid: i32) -> io::Result<()>;
    fn kill_process(&mut self, pid: i32) -> io::Result<()>;
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

    fn stop_process(&mut self, pid: i32) -> io::Result<()> {
        group::stop_process(pid)
    }

    fn kill_process(&mut self, pid: i32) -> io::Result<()> {
        group::kill_process(pid)
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
    let (tree, listed) = walk(&mut Native, group, |_processes, _pid| {});
    listed.map(|()| tree.into_iter().map(|(pid, _member)| pid).collect())
}

/// Ends every descendant of `group` outside it, then `group` itself through `kill_group`.
///
/// The whole tree is stopped before anything is killed: `group` first, then each descendant
/// outside it before its children are listed. A stopped process can neither start a child nor
/// reap one, so every listed process keeps its pid and group id until the sweep has signalled it.
/// Descendants are then killed children first, so a killed child stays a zombie of a stopped
/// parent rather than passing to `launchd` and being reaped. A descendant that leads its group is killed
/// with that group; any other is killed alone, so a process that joins an unrelated group cannot
/// steer the sweep there.
///
/// # Errors
///
/// Returns the error from `kill_group` first, then a listing failure, then the first failure to
/// kill a descendant, including `EPERM` for a process this user cannot signal. A failed listing
/// skips only the subtree it would have revealed: every descendant already found, and so already
/// stopped, is still killed. `group` is resumed when `kill_group` fails, so a failed sweep does
/// not leave it stopped.
pub(crate) fn sweep(group: i32, kill_group: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    sweep_with(&mut Native, group, kill_group)
}

/// Walks a group's members and their descendants.
///
/// `before_children` is called on each descendant outside the group before its children are
/// listed. Each entry says whether it is a group member.
///
/// A failed child listing skips that process's subtree, not the rest of the walk, so the caller
/// still has every process found. The first listing failure is returned beside the tree.
fn walk<P: Processes>(processes: &mut P, group: i32, mut before_children: impl FnMut(&mut P, i32)) -> (Vec<(i32, bool)>, io::Result<()>) {
    let mut tree = Vec::new();
    let mut pending: VecDeque<_> = match processes.members(group) {
        Ok(members) => members.into_iter().map(|pid| (pid, true)).collect(),
        Err(cause) => return (tree, Err(cause)),
    };
    let mut seen = BTreeSet::new();
    let mut listed = Ok(());
    while let Some((pid, member)) = pending.pop_front() {
        if seen.insert(pid) {
            if !member {
                before_children(processes, pid);
            }
            tree.push((pid, member));
            match processes.children(pid) {
                Ok(children) => pending.extend(children.into_iter().map(|child| (child, false))),
                Err(cause) => listed = listed.and(Err(cause)),
            }
        }
    }
    (tree, listed)
}

fn sweep_with(processes: &mut impl Processes, group: i32, kill_group: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    // A group whose leader has exited can hold only that zombie, which macOS refuses to signal.
    // Such a group starts and reaps no children, so a failed stop costs the walk nothing.
    let _stopped = processes.stop(group);
    let (tree, mut ended) = walk(processes, group, |processes, pid| {
        // A process this user cannot stop cannot be killed either; that kill reports it below.
        // Its children are still listed: they may belong to this user, as under `sudo -u`, and
        // skipping them would leave a runaway allocation running. Each child is stopped as soon
        // as it is listed, and a stopped child cannot exit, so its running parent cannot reap it
        // and free its pid before the sweep kills it.
        let _stopped = processes.stop_process(pid);
    });
    for (pid, _member) in tree.into_iter().rev().filter(|(_pid, member)| !member) {
        let killed = if processes.group_of(pid) == Some(pid) {
            processes.kill(pid)
        } else {
            processes.kill_process(pid)
        };
        ended = ended.and(killed);
    }
    let killed = kill_group();
    if killed.is_err() {
        let _resumed = processes.resume(group);
    }
    killed.and(ended)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::*;

    type Log = Rc<RefCell<Vec<(&'static str, i32)>>>;

    /// A scripted process table that records every listing and signal in one chronological log.
    #[derive(Default)]
    struct Table {
        members: Vec<i32>,
        children: BTreeMap<i32, Vec<i32>>,
        groups: BTreeMap<i32, i32>,
        kill_errors: BTreeMap<i32, i32>,
        failing_listing: Option<i32>,
        log: Log,
    }

    impl Table {
        fn record(&self, event: &'static str, id: i32) {
            self.log.borrow_mut().push((event, id));
        }

        fn killed(&self, event: &'static str, id: i32) -> io::Result<()> {
            self.record(event, id);
            self.kill_errors
                .get(&id)
                .map_or(Ok(()), |error| Err(io::Error::from_raw_os_error(*error)))
        }
    }

    impl Processes for Table {
        fn members(&mut self, _group: i32) -> io::Result<Vec<i32>> {
            Ok(self.members.clone())
        }

        fn children(&mut self, parent: i32) -> io::Result<Vec<i32>> {
            self.record("children", parent);
            if self.failing_listing == Some(parent) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(self.children.get(&parent).cloned().unwrap_or_default())
        }

        fn group_of(&mut self, pid: i32) -> Option<i32> {
            self.groups.get(&pid).copied()
        }

        fn stop(&mut self, group: i32) -> io::Result<()> {
            self.record("stop", group);
            Ok(())
        }

        fn resume(&mut self, group: i32) -> io::Result<()> {
            self.record("resume", group);
            Ok(())
        }

        fn kill(&mut self, group: i32) -> io::Result<()> {
            self.killed("kill", group)
        }

        fn stop_process(&mut self, pid: i32) -> io::Result<()> {
            self.record("stop_process", pid);
            Ok(())
        }

        fn kill_process(&mut self, pid: i32) -> io::Result<()> {
            self.killed("kill_process", pid)
        }
    }

    /// A cargo-nextest launch with two tests, one of which has a child.
    ///
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

    /// Kills the group through the same log, as the real group kill would follow the sweep.
    fn kill_group(log: &Log, result: io::Result<()>) -> impl FnOnce() -> io::Result<()> {
        let log = Rc::clone(log);
        move || {
            log.borrow_mut().push(("kill", 10));
            result
        }
    }

    fn events(table: &Table) -> Vec<(&'static str, i32)> {
        table.log.borrow().clone()
    }

    #[test]
    fn the_tree_adds_descendants_outside_the_group_once_each_after_its_members() {
        let mut table = nextest();
        let (tree, listed) = walk(&mut table, 10, |_processes, _pid| {});
        listed.unwrap();
        assert_eq!(tree, [(10, true), (11, true), (20, false), (30, false), (21, false)]);
    }

    #[test]
    fn a_failed_child_listing_skips_only_that_subtree_and_is_returned() {
        let mut table = Table {
            failing_listing: Some(20),
            ..nextest()
        };
        let (tree, listed) = walk(&mut table, 10, |_processes, _pid| {});
        assert_eq!(listed.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(tree, [(10, true), (11, true), (20, false), (30, false)]);
    }

    #[test]
    fn a_failed_child_listing_still_kills_every_stopped_descendant() {
        let mut table = Table {
            failing_listing: Some(20),
            ..nextest()
        };
        let log = Rc::clone(&table.log);
        let error = sweep_with(&mut table, 10, kill_group(&log, Ok(()))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(
            events(&table),
            [
                ("stop", 10),
                ("children", 10),
                ("children", 11),
                ("stop_process", 20),
                ("children", 20),
                ("stop_process", 30),
                ("children", 30),
                // 20 was stopped before its listing failed, so it must not be left suspended.
                ("kill", 30),
                ("kill", 20),
                ("kill", 10),
            ]
        );
    }

    #[test]
    fn a_sweep_stops_the_whole_tree_then_kills_children_before_parents_and_the_group_last() {
        let mut table = nextest();
        let log = Rc::clone(&table.log);
        sweep_with(&mut table, 10, kill_group(&log, Ok(()))).unwrap();
        assert_eq!(
            events(&table),
            [
                ("stop", 10),
                ("children", 10),
                ("children", 11),
                // Each descendant is stopped before its children are listed, so none of them can
                // be reaped and its id reused while the sweep is still to signal it.
                ("stop_process", 20),
                ("children", 20),
                ("stop_process", 30),
                ("children", 30),
                ("stop_process", 21),
                ("children", 21),
                // Children first; 21 only belongs to 20's group and is killed alone.
                ("kill_process", 21),
                ("kill", 30),
                ("kill", 20),
                ("kill", 10),
            ]
        );
    }

    #[test]
    fn a_descendant_in_a_group_it_does_not_lead_is_killed_alone() {
        let mut table = Table {
            members: vec![10],
            children: BTreeMap::from([(10, vec![11])]),
            // 11 joined an unrelated group, such as this run's own.
            groups: BTreeMap::from([(10, 10), (11, 5)]),
            ..Table::default()
        };
        let log = Rc::clone(&table.log);
        sweep_with(&mut table, 10, kill_group(&log, Ok(()))).unwrap();
        assert_eq!(
            events(&table),
            [
                ("stop", 10),
                ("children", 10),
                ("stop_process", 11),
                ("children", 11),
                ("kill_process", 11),
                ("kill", 10),
            ]
        );
    }

    #[test]
    fn a_descendant_that_cannot_be_killed_fails_the_sweep_without_stopping_it() {
        for error in [libc::EPERM, libc::EINVAL] {
            let mut table = Table {
                kill_errors: BTreeMap::from([(30, error)]),
                ..nextest()
            };
            let log = Rc::clone(&table.log);
            let failure = sweep_with(&mut table, 10, kill_group(&log, Ok(()))).unwrap_err();
            assert_eq!(failure.raw_os_error(), Some(error));
            let kills: Vec<_> = events(&table)
                .into_iter()
                .filter(|(event, _id)| event.starts_with("kill"))
                .collect();
            assert_eq!(
                kills,
                [("kill_process", 21), ("kill", 30), ("kill", 20), ("kill", 10)],
                "one failure does not stop the sweep"
            );
        }
    }

    #[test]
    fn a_failed_listing_still_kills_the_group_and_is_reported_after_its_error() {
        let mut table = Table {
            failing_listing: Some(11),
            ..nextest()
        };
        let log = Rc::clone(&table.log);
        let error = sweep_with(&mut table, 10, kill_group(&log, Ok(()))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(events(&table).last(), Some(&("kill", 10)));

        let mut table = Table {
            failing_listing: Some(11),
            ..nextest()
        };
        let log = Rc::clone(&table.log);
        let error = sweep_with(&mut table, 10, kill_group(&log, Err(io::Error::from_raw_os_error(libc::EPERM)))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM), "the group's own failure takes precedence");
    }

    #[test]
    fn a_failed_group_kill_resumes_the_stopped_group() {
        let mut table = nextest();
        let log = Rc::clone(&table.log);
        let error = sweep_with(&mut table, 10, kill_group(&log, Err(io::Error::from_raw_os_error(libc::EPERM)))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert_eq!(events(&table)[events(&table).len() - 2..], [("kill", 10), ("resume", 10)]);
    }
}
