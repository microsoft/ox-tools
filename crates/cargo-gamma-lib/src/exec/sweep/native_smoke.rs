// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native condition-variable smoke coverage, separate from controlled scheduler decisions.

use super::*;

struct Cleanup<'a> {
    scheduler: &'a Scheduler,
    abandoned: &'a OnceLock<String>,
    active: Option<Assignment<'a>>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        self.abandoned.get_or_init(|| "native smoke cleanup".to_owned());
        drop(self.active.take());
        let _state = self.scheduler.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.scheduler.changed.notify_all();
    }
}

/// Exercises the native broadcast with bounded cleanup outside active mutation runs.
#[doc(hidden)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn run_scheduler_native_smoke() {
    let resource: Arc<str> = "exclusive".into();
    let work = (0..5)
        .map(|index| ScheduledWork {
            file: index,
            item: "item".into(),
            package: "package".into(),
            cost: Duration::from_millis(1),
            sibling_benefit: 0,
            hinted: false,
            transfer: false,
            stable_order: index,
            resources: vec![Arc::clone(&resource)],
        })
        .collect();
    let scheduler = Scheduler::with_resources(work, 5, std::iter::once((Arc::clone(&resource), 1)).collect());
    let abandoned = OnceLock::new();
    let active = scheduler
        .assignment_with(&abandoned, || {})
        .expect("the first resource user has uncontended capacity");
    let (waiting, waited) = mpsc::channel();
    let (finished, joined) = mpsc::channel();
    thread::scope(|scope| {
        let _cleanup = Cleanup {
            scheduler: &scheduler,
            abandoned: &abandoned,
            active: Some(active),
        };
        for _ in 0..4 {
            let waiting = waiting.clone();
            let finished = finished.clone();
            let scheduler = &scheduler;
            let abandoned = &abandoned;
            let _worker = scope.spawn(move || {
                let mut waiting = Some(waiting);
                let assignment = scheduler.assignment_with(abandoned, || {
                    if let Some(waiting) = waiting.take() {
                        let _sent = waiting.send(());
                    }
                });
                let claimed = assignment.as_ref().map(Assignment::index);
                drop(assignment);
                let _sent = finished.send(claimed);
            });
        }
        for _ in 0..4 {
            waited
                .recv_timeout(Duration::from_secs(10))
                .expect("native smoke watchdog: a worker did not reach admission");
        }
        abandoned
            .set("checkpoint failed".to_owned())
            .expect("only the coordinator writes abandonment before cleanup");
        scheduler.abandon();
        for _ in 0..4 {
            let claimed = joined
                .recv_timeout(Duration::from_secs(10))
                .expect("native smoke watchdog: a waiter did not finish");
            assert_eq!(claimed, None);
        }
        assert_eq!(
            scheduler
                .state
                .lock()
                .expect("scheduler state is unpoisoned when every worker has completed normally")
                .active_resources
                .get(&resource),
            Some(&1)
        );
    });
    assert!(
        scheduler
            .state
            .lock()
            .expect("normal smoke cleanup does not poison scheduler state")
            .active_resources
            .is_empty()
    );
}
