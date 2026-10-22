// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::process::Output;
use std::time::Duration;

#[doc(hidden)]
pub use super::blame::{AttributionEvidence, CompilerReason, blame};
#[doc(hidden)]
pub use super::isolation::{
    FailureContext, IsolationAttempt, IsolationBudget, MAX_INVOCATION_ISOLATION_PROOFS, MAX_ISOLATION_CANDIDATES, MAX_ISOLATION_CONTEXTS,
    MAX_ISOLATION_PROOFS, Proofs, failure_contexts, isolation_proofs_exhausted,
};
#[doc(hidden)]
pub use super::{BuildScope, Convergence, Converger, Isolation};
use super::{Events, Plan, Result, Workspace};

type ScriptedCompiler = Box<dyn FnMut(&Workspace, &[String], Option<Duration>, &mut dyn Events) -> Result<Option<Output>>>;

std::thread_local! {
    pub(super) static SCRIPTED_COMPILER: std::cell::RefCell<Option<ScriptedCompiler>> = const { std::cell::RefCell::new(None) };
}

// Supplies synchronous compile results on this thread, bypassing command supervision. Installation
// cannot nest and resets after the body, including unwinding; it does not cover native supervision.
pub fn with_compiler<T>(
    runner: impl FnMut(&Workspace, &[String], Option<Duration>, &mut dyn Events) -> Result<Option<Output>> + 'static,
    body: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCRIPTED_COMPILER.with_borrow_mut(|runner| {
                let _ = runner.take();
            });
        }
    }
    SCRIPTED_COMPILER.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "a thread cannot install two Cargo runners simultaneously");
        *slot = Some(Box::new(runner));
    });
    let _reset = Reset;
    body()
}

type CandidatePolicy = Box<dyn FnMut(usize) -> bool>;

thread_local! {
    pub(super) static CANDIDATE_POLICY: std::cell::RefCell<Option<CandidatePolicy>> = const { std::cell::RefCell::new(None) };
}

pub fn with_candidate_policy<T>(policy: impl FnMut(usize) -> bool + 'static, body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CANDIDATE_POLICY.with_borrow_mut(|policy| *policy = None);
        }
    }
    CANDIDATE_POLICY.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "candidate policies must not nest");
        *slot = Some(Box::new(policy));
    });
    let _reset = Reset;
    body()
}

use super::super::cargo_options::BuildLimits;
use super::isolation_candidate;

impl Converger {
    pub fn isolate(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Option<Isolation>> {
        let eligible = plan
            .mutants
            .iter()
            .filter(|mutant| isolation_candidate(mutant, &self.withdrawn, select))
            .map(|mutant| mutant.ordinal)
            .collect();
        let attempt = self.isolate_candidates(
            work,
            plan,
            BuildScope {
                roots: select,
                mutants: select,
                publish_progress: false,
            },
            verb,
            limits,
            events,
            &eligible,
            &mut Proofs::default(),
        )?;
        Ok(match attempt {
            IsolationAttempt::Resolved(result) => Some(result),
            IsolationAttempt::NotReproduced | IsolationAttempt::Inconclusive => None,
        })
    }
}
