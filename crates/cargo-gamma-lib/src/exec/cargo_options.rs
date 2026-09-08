// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;

use crate::error::error;

/// Replaces lockfile promises Gamma cannot honor with their offline half.
fn adjust_lock_flags(args: &[String]) -> Vec<String> {
    let mut adjusted = Vec::with_capacity(args.len());

    for arg in args {
        let replacement = if matches!(arg.as_str(), "--locked" | "--frozen") {
            "--offline"
        } else {
            arg
        };

        if replacement != "--offline" || !adjusted.iter().any(|kept| kept == "--offline") {
            adjusted.push(replacement.to_owned());
        }
    }

    adjusted
}

/// How cargo and the test binaries are invoked.
///
/// A run builds once and then executes that build thousands of times, so these are the settings
/// that decide what gets compiled and what the compiled thing is asked to do.
#[derive(Debug, Clone, Default)]
pub struct CargoOptions {
    /// Feature arguments, already rendered in the form cargo accepts.
    pub features: Vec<String>,

    /// The cargo profile to build with.
    pub profile: Option<String>,

    /// Extra arguments appended to every cargo invocation.
    pub extra: Vec<String>,

    /// Extra arguments appended to every test binary's command line.
    pub test_args: Vec<String>,

    /// Whether cargo should color its own output.
    ///
    /// Cargo's stdio is always a pipe here, so it would otherwise decide "no" every time — and the
    /// progress bar gamma borrows from it would arrive as plain text even on a terminal that had
    /// asked for color everywhere else.
    pub color: bool,
}

impl CargoOptions {
    /// Refuses Cargo configuration that gamma cannot apply while discovering the build.
    ///
    /// Cargo accepts both an inline TOML value and a path after `--config`. Those settings can
    /// change the target, flags and profiles before rustc sees a source file, while gamma's cfg
    /// discovery and cache provenance deliberately resolve configuration from the workspace. Do
    /// not let the two builds silently diverge; supporting this needs to model Cargo's full
    /// configuration precedence, so it is rejected before either discovery or Cargo starts.
    pub fn validate(&self) -> crate::Result<()> {
        if let Some(argument) = self
            .extra
            .iter()
            .find(|argument| argument.as_str() == "--config" || argument.starts_with("--config="))
        {
            return Err(error!(
                "pass-through Cargo configuration `{argument}` is not supported; put the setting in a Cargo configuration file gamma can inspect"
            )
            .usage());
        }

        Ok(())
    }

    /// Describes the compilation these options ask for, for a workspace at `root`.
    ///
    /// Discovery evaluates `#[cfg(...)]` against the build that will actually be run, and this is
    /// where it learns what that build is: the profile decides `debug_assertions`, the passthrough
    /// arguments can carry `--target`, and the environment and cargo configuration carry the rest.
    /// Derived from these options rather than resolved independently, so the tree that is surveyed
    /// and the tree that is compiled cannot describe different builds.
    #[must_use]
    pub fn cfg_build(&self, root: &camino::Utf8Path) -> crate::cfg::Build {
        crate::cfg::Build::resolve(root, self.profile.as_deref(), &self.extra)
    }

    /// Appends the build-shaping arguments to a cargo command line.
    ///
    /// The flags that promise the lockfile will not change are the one thing not passed through as
    /// written. Gamma adds the guard runtime to the manifest before it builds, so the lockfile
    /// *will* change; `--locked` would fail the build before a single mutant ran, which is a worse
    /// answer than the honest one.
    pub fn extend_build_args(&self, args: &mut Vec<String>) {
        args.extend(self.features.iter().cloned());

        if let Some(profile) = self.profile.as_ref() {
            args.push("--profile".to_owned());
            args.push(profile.clone());
        }

        args.extend(adjust_lock_flags(&self.extra));
    }

    /// Appends the build-shaping arguments in nextest's spelling.
    pub fn extend_nextest_args(&self, args: &mut Vec<String>) {
        args.extend(self.features.iter().cloned());

        if let Some(profile) = self.profile.as_ref() {
            args.push("--cargo-profile".to_owned());
            args.push(profile.clone());
        }

        args.extend(adjust_lock_flags(&self.extra));
    }
}

/// How many build-and-withdraw rounds are allowed before a run gives up.
///
/// Some mutants are speculative — replacing a function body with `Some(Default::default())` only
/// compiles when the type happens to implement `Default` — and rustc reports only the errors it
/// reaches before it stops, so a large tree can need many rounds to converge. The cost of a round is
/// a rebuild of a tree that is already warm, whereas the cost of stopping too early is a run that
/// cannot complete at all, so the limit is deliberately lopsided.
pub const DEFAULT_ROLLBACK_ROUNDS: u32 = 256;

/// Limits on how long the build may take.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildLimits {
    /// A fixed budget for the build, whatever it turns out to cost.
    pub timeout: Option<Duration>,

    /// The multiple of the first round's duration a later rollback round is allowed.
    ///
    /// Rollback rounds recompile the same tree with strictly fewer live mutants, so a round that
    /// takes far longer than the first is not converging.
    pub multiplier: Option<f64>,

    /// How many build-and-withdraw rounds are allowed before the run gives up.
    ///
    /// Zero means the built-in default, so that a caller that does not care about rollback does not
    /// have to know what the default is.
    pub rollback_rounds: u32,
}

impl BuildLimits {
    /// Returns how many build-and-withdraw rounds are allowed.
    #[must_use]
    pub const fn rounds(&self) -> u32 {
        if self.rollback_rounds == 0 {
            DEFAULT_ROLLBACK_ROUNDS
        } else {
            self.rollback_rounds
        }
    }

    /// Returns the budget for a round, given how long the first round took.
    #[must_use]
    pub fn budget(&self, first: Option<Duration>) -> Option<Duration> {
        let scaled = self
            .multiplier
            .zip(first)
            .map(|(multiplier, first)| first.mul_f64(multiplier).max(MINIMUM_BUILD_BUDGET));

        match (self.timeout, scaled) {
            (Some(fixed), Some(scaled)) => Some(fixed.min(scaled)),
            (fixed, scaled) => fixed.or(scaled),
        }
    }
}

/// Floor under a scaled build budget, so a first round that finished instantly cannot produce one
/// that the next round trips over for reasons of scheduling alone.
const MINIMUM_BUILD_BUDGET: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rollback_rounds_means_the_default() {
        // A caller that does not care about rollback should not have to know what the default is,
        // and a build that allowed zero rounds could never even try once.
        assert_eq!(BuildLimits::default().rounds(), DEFAULT_ROLLBACK_ROUNDS);
        assert_eq!(
            BuildLimits {
                rollback_rounds: 7,
                ..BuildLimits::default()
            }
            .rounds(),
            7
        );
    }

    #[test]
    fn no_limits_means_no_budget() {
        assert_eq!(BuildLimits::default().budget(Some(Duration::from_secs(10))), None);
    }

    #[test]
    fn a_fixed_timeout_applies_from_the_first_round() {
        let limits = BuildLimits {
            timeout: Some(Duration::from_mins(1)),
            multiplier: None,
            rollback_rounds: 0,
        };

        assert_eq!(limits.budget(None), Some(Duration::from_mins(1)));
    }

    #[test]
    fn a_multiplier_needs_a_first_round_to_scale_from() {
        let limits = BuildLimits {
            timeout: None,
            multiplier: Some(2.0),
            rollback_rounds: 0,
        };

        assert_eq!(limits.budget(None), None);
        assert_eq!(limits.budget(Some(Duration::from_secs(100))), Some(Duration::from_secs(200)));
    }

    #[test]
    fn a_scaled_budget_never_falls_below_the_floor() {
        let limits = BuildLimits {
            timeout: None,
            multiplier: Some(2.0),
            rollback_rounds: 0,
        };

        assert_eq!(limits.budget(Some(Duration::from_secs(1))), Some(MINIMUM_BUILD_BUDGET));
    }

    #[test]
    fn the_tighter_of_the_two_wins() {
        let limits = BuildLimits {
            timeout: Some(Duration::from_mins(1)),
            multiplier: Some(2.0),
            rollback_rounds: 0,
        };

        assert_eq!(limits.budget(Some(Duration::from_secs(100))), Some(Duration::from_mins(1)));
    }

    #[test]
    fn build_args_are_rendered_in_cargo_order() {
        let options = CargoOptions {
            features: vec!["--all-features".to_owned()],
            profile: Some("release".to_owned()),
            extra: vec!["--offline".to_owned()],
            test_args: Vec::new(),
            color: false,
        };

        let mut args = Vec::new();

        options.extend_build_args(&mut args);

        assert_eq!(args, vec!["--all-features", "--profile", "release", "--offline"]);
    }

    #[test]
    fn lockfile_promises_become_one_offline_flag() {
        let options = CargoOptions {
            extra: vec![
                "--locked".to_owned(),
                "--frozen".to_owned(),
                "--offline".to_owned(),
                "--verbose".to_owned(),
            ],
            ..CargoOptions::default()
        };
        let mut args = Vec::new();

        options.extend_build_args(&mut args);

        assert_eq!(args, ["--offline", "--verbose"]);
    }

    /// The build discovery evaluates predicates against has to be the one these options describe.
    ///
    /// Not merely a delegation: if this ever ignored the profile or passthrough arguments, a run
    /// built with `--profile release --target …` would be surveyed as a different build, and every
    /// item behind a gate those settings decide would be misjudged.
    #[test]
    #[cfg(not(miri))]
    fn the_described_build_carries_the_profile_and_the_passthrough_target() {
        let options = CargoOptions {
            profile: Some("release".to_owned()),
            extra: vec!["--target".to_owned(), "x86_64-pc-solaris".to_owned()],
            ..CargoOptions::default()
        };

        let build = options.cfg_build(camino::Utf8Path::new("."));

        assert_eq!(build.target.as_deref(), Some("x86_64-pc-solaris"));
        assert_eq!(
            build,
            crate::cfg::Build::resolve(camino::Utf8Path::new("."), Some("release"), &options.extra)
        );
    }

    #[test]
    fn inline_and_file_pass_through_cargo_configuration_are_refused() {
        for extra in [
            vec!["--config".to_owned(), "build.target = \"wasm32-wasip1\"".to_owned()],
            vec!["--config=extra.toml".to_owned()],
        ] {
            let failure = CargoOptions {
                extra,
                ..CargoOptions::default()
            }
            .validate()
            .expect_err("unmodelled Cargo configuration must stop before discovery");

            assert!(failure.is_usage(), "{failure}");
            assert!(failure.to_string().contains("--config"), "{failure}");
        }
    }

    #[test]
    fn nextest_args_use_its_cargo_profile_spelling() {
        let options = CargoOptions {
            features: vec!["--all-features".to_owned()],
            profile: Some("mutants".to_owned()),
            extra: vec!["--offline".to_owned()],
            ..CargoOptions::default()
        };
        let mut args = Vec::new();

        options.extend_nextest_args(&mut args);

        assert_eq!(args, vec!["--all-features", "--cargo-profile", "mutants", "--offline"]);
    }

    /// The bug this guards: `--locked` was accepted and then failed the build, because gamma adds
    /// the guard runtime to the manifest and that forces the lockfile to be written. The flag is
    /// substituted rather than obeyed or refused, and the run gets as far as it would have.
    #[test]
    fn a_lockfile_promise_gamma_cannot_keep_becomes_the_half_it_can() {
        crate::notes::alone(|| {
            let options = CargoOptions {
                extra: vec!["--locked".to_owned(), "--verbose".to_owned()],
                ..CargoOptions::default()
            };

            let mut args = Vec::new();

            options.extend_build_args(&mut args);

            assert_eq!(args, vec!["--offline", "--verbose"]);
        });
    }

    /// `--frozen` is `--locked` plus `--offline`, so what survives it is the substitute itself.
    #[test]
    fn a_frozen_lockfile_is_treated_the_same_way() {
        crate::notes::alone(|| {
            let options = CargoOptions {
                extra: vec!["--frozen".to_owned()],
                ..CargoOptions::default()
            };

            let mut args = Vec::new();

            options.extend_build_args(&mut args);

            assert_eq!(args, vec!["--offline"]);
        });
    }

    /// Lock-flag substitution is an implementation detail and does not emit user-facing output.
    #[test]
    fn substituting_a_lock_flag_is_silent() {
        crate::notes::alone(|| {
            let options = CargoOptions {
                extra: vec!["--locked".to_owned()],
                ..CargoOptions::default()
            };

            // Once per build the run does: the check build, the test build, every rollback round.
            for _round in 0..3 {
                options.extend_build_args(&mut Vec::new());
            }
            let raised = crate::notes::drain();

            assert!(raised.is_empty(), "{raised:?}");
        });
    }

    /// A command line with nothing substituted has nothing to say about it.
    #[test]
    fn a_lockfile_promise_gamma_can_keep_is_announced_not_at_all() {
        crate::notes::alone(|| {
            let options = CargoOptions {
                extra: vec!["--offline".to_owned()],
                ..CargoOptions::default()
            };

            options.extend_build_args(&mut Vec::new());

            assert!(crate::notes::drain().is_empty());
        });
    }
}
