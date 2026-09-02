// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How much memory a mutant may use, and what the host is able to do about it.
//!
//! A mutation can turn bounded allocation into unbounded allocation. The wall-clock and stall
//! budgets do eventually stop such a mutant, but by then it may have exhausted physical memory,
//! driven the machine deep into swap, or provoked the kernel into killing something that had
//! nothing to do with the run. The output cap already protects the tool from a test that prints
//! forever; this is the same protection for what a test allocates.
//!
//! The calibration point is the baseline. Every test binary is already run once with no mutant
//! active, so the peak that run reaches is a measurement of the same workload the mutants are
//! judged against, taken on the same machine with the same instrumentation compiled in. A ceiling
//! derived from it is therefore a statement about *this* suite rather than a guess about suites in
//! general.
//!
//! Two things about this module are deliberate.
//!
//! **The limit has to cover the whole descendant tree.** Tests launch servers, databases, helper
//! programs and nested cargo invocations, and those are exactly where a runaway allocation does the
//! most damage. Anything that accounts only for the direct child leaves the dangerous case
//! unbounded, which is why `wait4`, `getrusage` and polling `/proc` are not used here: they
//! describe one process, and they race short-lived descendants besides.
//!
//! **Support is claimed only where it exists.** cgroup v2 delegation is not universal, and a
//! container or CI runner may not have it. A run that asked for a memory ceiling and did not get
//! one has to be told so, because the alternative is a user who believes the machine is protected
//! and finds out otherwise when it is not.

/// Whether this host can meter and bound a test subtree's memory, or why it cannot.
///
/// The mechanism half of this module lives in `cargo-gamma-process`, which composes the safe
/// platform calls from `cargo-gamma-unsafe`. What is left here is the policy half: what a ceiling
/// should be, given a baseline measurement and what the user asked for. The seam is deliberate —
/// arithmetic on a measurement can be tested without a kernel, and a kernel call cannot be.
pub use cargo_gamma_process::support;
use clap::ValueEnum;
use serde::Deserialize;

/// How much memory control a run places around each test binary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryControl {
    /// Neither measure nor bound what a test binary allocates.
    Off,

    /// Measure the peak memory of each test subtree and report it, but never stop a mutant for it.
    ///
    /// The measurement is what makes a ceiling possible to choose, and it costs one accounting
    /// boundary per invocation and nothing else. It is the honest starting point for a project
    /// that does not yet know what its suite allocates.
    Measure,

    /// Measure, and hold every mutant to a ceiling derived from its binary's baseline peak.
    ///
    /// The default, on the same reasoning as the wall-clock timeout: a mutation can turn bounded
    /// allocation into unbounded allocation, and the user who most needs protecting from that is
    /// the one who never thought to ask for it. Where the host cannot provide the accounting, a
    /// run that merely defaulted into this quietly drops to `Off` and says so, rather than refusing
    /// to start — see `Demand`.
    #[default]
    Enforce,
}

/// The mode implied by command-line memory ceilings, if either was named.
pub(crate) const fn implied_memory_control(memory_limit: Option<u64>, baseline_memory_limit: Option<u64>) -> Option<MemoryControl> {
    if memory_limit.is_some() {
        Some(MemoryControl::Enforce)
    } else if baseline_memory_limit.is_some() {
        Some(MemoryControl::Measure)
    } else {
        None
    }
}

/// Whether a run's memory control was chosen by the user or inherited from the default.
///
/// This is the whole difference between an error and a note. Someone who passed `--memory` did so
/// because an unbounded mutant would cost them something — a wedged laptop, a CI runner that takes
/// the rest of the job down with it — and giving them a run that quietly lacks that protection
/// would be discovered only by the thing they were trying to prevent. Someone who passed nothing
/// asked for a mutation score, and refusing to produce one because this host has no cgroup
/// delegation would be an obstruction rather than a safeguard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Demand {
    /// The user named a memory setting, so failing to deliver it is an error.
    Stated,

    /// Nobody asked; this is the built-in default, so failing to deliver it is a note.
    #[default]
    Inherited,
}

/// Everything a run needs to decide how much memory a mutant may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryPolicy {
    /// Whether memory is measured, bounded, or neither.
    pub control: MemoryControl,

    /// Whether `control` was stated by the user or inherited from the default.
    pub demand: Demand,

    /// Multiple of a binary's baseline peak that a mutant of it may reach.
    pub multiplier: f64,

    /// Absolute headroom added to a binary's baseline peak.
    ///
    /// A multiplier alone is far too tight for a binary whose baseline peak is small: doubling a
    /// few megabytes still leaves no room for a lazily initialized table or a randomized test that
    /// happens to pick a larger input. The ceiling is the larger of the two, so the multiplier
    /// governs large suites and the headroom governs small ones.
    pub headroom: u64,

    /// An explicit ceiling for every test binary, overriding the baseline-derived one.
    ///
    /// Useful to anyone who knows their workload, and the only way to bound a run that skips the
    /// baseline, since there is then nothing to calibrate from.
    pub limit: Option<u64>,

    /// A ceiling applied to the baseline runs themselves.
    ///
    /// A limit calibrated from the baseline cannot protect the machine from a baseline that is
    /// itself runaway, which is a real risk the first time a suite is measured. This closes that
    /// bootstrap hole for environments that need it.
    pub baseline_limit: Option<u64>,
}

/// The absolute headroom a mutant gets over its binary's baseline peak, in bytes.
///
/// A single baseline observation is noisy: allocator behaviour, randomized tests, lazy
/// initialization and input-dependent work all vary legitimately between two runs of the same
/// suite. The cost of being too generous is a runaway mutant that reaches a larger peak before it
/// is stopped; the cost of being too tight is a healthy mutant reported as caught, which inflates
/// the score with a detection the suite never made. The second is much worse, so the default is
/// generous and is meant to be tightened against real measurements rather than argued about.
pub const DEFAULT_HEADROOM: u64 = 128 * 1024 * 1024;

/// The multiple of the baseline peak a mutant is allowed, before headroom is considered.
pub const DEFAULT_MULTIPLIER: f64 = 2.0;

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            control: MemoryControl::default(),
            demand: Demand::default(),
            multiplier: DEFAULT_MULTIPLIER,
            headroom: DEFAULT_HEADROOM,
            limit: None,
            baseline_limit: None,
        }
    }
}

impl MemoryPolicy {
    /// Whether each invocation needs an accounting boundary at all.
    #[must_use]
    pub const fn measuring(&self) -> bool {
        !matches!(self.control, MemoryControl::Off)
    }

    /// Whether a mutant that passes its ceiling should be stopped.
    #[must_use]
    pub const fn enforcing(&self) -> bool {
        matches!(self.control, MemoryControl::Enforce)
    }

    /// The same policy with all memory control removed, for a host that cannot provide it.
    #[must_use]
    pub const fn disabled(&self) -> Self {
        Self {
            control: MemoryControl::Off,
            ..*self
        }
    }

    /// Whether failing to deliver this policy should stop the run rather than merely be reported.
    #[must_use]
    pub const fn insisted(&self) -> bool {
        matches!(self.demand, Demand::Stated)
    }

    /// The ceiling a binary with this baseline peak should be held to, if any.
    ///
    /// `calibrated` says whether the baseline actually ran. Without it there is no measurement, and
    /// a ceiling invented from no measurement is the worst of both worlds: it neither reflects the
    /// suite nor admits that it does not. An explicit limit still applies, because that is a
    /// statement the user made rather than one this code inferred.
    #[must_use]
    pub fn ceiling(&self, peak: Option<u64>, calibrated: bool) -> Option<u64> {
        if !self.enforcing() {
            return None;
        }

        if let Some(fixed) = self.limit {
            return Some(fixed);
        }

        if !calibrated {
            return None;
        }

        let peak = peak?;

        Some(scale(peak, self.multiplier).max(peak.saturating_add(self.headroom)))
    }
}

/// Multiplies a byte count by a factor, saturating rather than wrapping or panicking.
///
/// The factor is floored at one because a ceiling below the peak the unmutated suite reached would
/// convict every mutant of a fault the baseline shares.
fn scale(peak: u64, multiplier: f64) -> u64 {
    #[expect(clippy::cast_precision_loss, reason = "a memory ceiling is not sensitive to its last few bytes")]
    let scaled = peak as f64 * multiplier.max(1.0);

    #[expect(clippy::cast_precision_loss, reason = "the comparison only needs to be right near the boundary")]
    let most = u64::MAX as f64;

    if !scaled.is_finite() || scaled >= most {
        return u64::MAX;
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is finite, non-negative and below `u64::MAX` by the test above"
    )]
    let bytes = scaled as u64;

    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that is not enforcing never produces a ceiling, however it was measured.
    #[test]
    fn measuring_without_enforcing_never_produces_a_ceiling() {
        let policy = MemoryPolicy {
            control: MemoryControl::Measure,
            ..MemoryPolicy::default()
        };

        assert!(policy.measuring());
        assert!(!policy.enforcing());
        assert_eq!(policy.ceiling(Some(1024), true), None);
    }

    #[test]
    fn the_default_policy_enforces_a_calibrated_ceiling() {
        let policy = MemoryPolicy::default();

        assert!(policy.measuring());
        assert!(policy.enforcing());
        assert!(policy.ceiling(Some(1024), true).is_some());
    }

    #[test]
    fn switching_control_off_asks_the_platform_for_nothing() {
        let policy = MemoryPolicy {
            control: MemoryControl::Off,
            ..MemoryPolicy::default()
        };

        assert!(!policy.measuring());
        assert!(!policy.enforcing());
        assert_eq!(policy.ceiling(Some(1024), true), None);
    }

    /// A policy nobody asked for degrades on a host that cannot deliver it; a stated one does not.
    #[test]
    fn only_a_stated_policy_is_insisted_upon() {
        let inherited = MemoryPolicy::default();

        assert!(!inherited.insisted());
        assert!(!inherited.disabled().measuring());

        let stated = MemoryPolicy {
            demand: Demand::Stated,
            ..MemoryPolicy::default()
        };

        assert!(stated.insisted());

        // Disabling preserves everything else, so a degraded run still reports the numbers it was
        // configured with rather than silently reverting them too.
        assert!((stated.disabled().multiplier - stated.multiplier).abs() < f64::EPSILON);
    }

    /// A small baseline peak is governed by the absolute headroom, not by the multiplier.
    #[test]
    fn a_small_baseline_peak_gets_the_absolute_headroom() {
        let policy = MemoryPolicy {
            control: MemoryControl::Enforce,
            ..MemoryPolicy::default()
        };

        // Doubling four megabytes would leave a suite four megabytes of room for one lazily
        // initialized table, and report the mutant that filled it as caught by tests that never
        // noticed it.
        assert_eq!(
            policy.ceiling(Some(4 * 1024 * 1024), true),
            Some(4 * 1024 * 1024 + DEFAULT_HEADROOM)
        );
    }

    /// A large baseline peak is governed by the multiplier.
    #[test]
    fn a_large_baseline_peak_gets_the_multiplier() {
        let policy = MemoryPolicy {
            control: MemoryControl::Enforce,
            ..MemoryPolicy::default()
        };
        let peak = 4 * 1024 * 1024 * 1024_u64;

        assert_eq!(policy.ceiling(Some(peak), true), Some(peak * 2));
    }

    /// Skipping the baseline disables the derived ceiling rather than inventing one.
    #[test]
    fn an_uncalibrated_run_gets_no_derived_ceiling_but_keeps_an_explicit_one() {
        let policy = MemoryPolicy {
            control: MemoryControl::Enforce,
            ..MemoryPolicy::default()
        };

        // There is no measurement to derive from, and a number made up here would be presented to
        // the user with exactly the same confidence as a measured one.
        assert_eq!(policy.ceiling(None, false), None);
        assert_eq!(policy.ceiling(Some(4096), false), None);

        let explicit = MemoryPolicy {
            limit: Some(4096),
            ..policy
        };

        assert_eq!(explicit.ceiling(None, false), Some(4096));
    }

    #[test]
    fn an_explicit_limit_overrides_whatever_the_baseline_measured() {
        let policy = MemoryPolicy {
            control: MemoryControl::Enforce,
            limit: Some(999),
            ..MemoryPolicy::default()
        };

        assert_eq!(policy.ceiling(Some(4 * 1024 * 1024 * 1024), true), Some(999));
    }

    /// Scaling saturates instead of wrapping, and never falls below the peak it started from.
    #[test]
    fn scaling_saturates_and_never_shrinks_the_peak() {
        // A ceiling below the peak the unmutated suite reached would convict every mutant of a
        // fault the baseline shares, so a multiplier under one is ignored rather than honoured.
        assert_eq!(scale(1024, 0.5), 1024);
        assert_eq!(scale(u64::MAX, 2.0), u64::MAX);
        assert_eq!(scale(1024, f64::INFINITY), u64::MAX);
        assert_eq!(scale(0, 2.0), 0);
    }

    /// Whatever this host answers, it answers with a reason rather than a bare failure.
    #[test]
    fn unsupported_hosts_say_why_rather_than_merely_saying_no() {
        // A run that asked for a ceiling and did not get one has to be able to explain itself:
        // "unsupported" without a cause sends the reader to the source of this tool instead of to
        // the configuration of their machine.
        if let Err(reason) = support() {
            let reason = reason.to_string();

            assert!(reason.len() > 20, "{reason}");
        }
    }
}
