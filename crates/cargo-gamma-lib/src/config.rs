// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The project configuration file, `gamma.toml`.
//!
//! A mutation run has a lot of knobs, and a project that has settled on its selection and run shape
//! should not have to repeat them in every CI job and every developer's shell history. One-off
//! output and control options remain command-line concerns.
//!
//! Two decisions shape the whole module.
//!
//! **Unknown keys are errors.** A configuration file whose settings are silently ignored is worse
//! than no configuration file, because the project believes it is configured. A misspelled key, or a
//! key for a feature this build does not have, stops the run and names the offender.
//!
//! **`.cargo/mutants.toml` is never read.** It is a different schema for a different tool, and
//! honoring it silently would mean that file's `exclude_re` entries quietly changing which
//! mutants this one suppresses.

use std::fs;
use std::io::ErrorKind;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::commands::{RunArgs, SelectArgs};
use crate::error::{Error, error};
use crate::{Result, bounds};

/// Where the file lives, relative to the directory being analyzed.
const RELATIVE_PATH: &str = "gamma.toml";

/// A foreign configuration file that is noticed but deliberately not read.
const FOREIGN_PATH: &str = ".cargo/mutants.toml";

/// A parsed `gamma.toml`.
///
/// Every field is optional: a file that sets one key is a valid file, and the rest keep whatever
/// the command line or the built-in default says.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// The mutator selector list, as it would be written after `--mutators`.
    ///
    /// A list rather than a string, because a configuration file has room to put one selector per
    /// line with a comment explaining why. The entries are joined with commas and parsed by exactly
    /// the same code that parses the flag, so the two cannot drift.
    pub mutators: Option<Vec<String>>,

    /// Globs limiting which files are mutated.
    pub files: Vec<String>,

    /// Globs excluding files from mutation.
    pub exclude_files: Vec<String>,

    /// Fail the run below this mutation score.
    pub min_score: Option<f64>,

    /// How many mutants to test at once.
    pub jobs: Option<usize>,

    /// The multiple of each test binary's baseline duration a mutant is allowed.
    pub test_timeout_multiplier: Option<f64>,

    /// How an incremental run reuses the last run: `no` or `build`.
    pub incremental: Option<crate::exec::IncrementalMode>,

    /// Skip the baseline measurement.
    pub no_baseline: Option<bool>,

    /// Believe a failing test without re-running it with no mutant active.
    pub no_confirm: Option<bool>,

    /// Packages to mutate. Empty means every package in the workspace.
    pub packages: Vec<String>,

    /// Packages whose tests decide a verdict. Empty means each mutant's own package.
    pub test_packages: Vec<String>,

    /// Let tests from every workspace package judge mutants they can reach.
    pub test_workspace: Option<bool>,

    /// Run every selected test in each reachable test binary instead of selecting cases by reachability.
    pub whole_test_binaries: Option<bool>,

    /// Test target name globs whose tests may decide a verdict. Empty means all of them.
    pub include_tests: Vec<String>,

    /// Test target name globs whose tests must not decide a verdict.
    pub exclude_tests: Vec<String>,

    /// Cargo features to activate.
    pub features: Vec<String>,

    /// Activate every feature of every selected package.
    pub all_features: Option<bool>,

    /// Do not activate the `default` feature.
    pub no_default_features: Option<bool>,

    /// The cargo profile to build with.
    pub profile: Option<String>,

    /// Extra arguments for every cargo invocation.
    pub cargo_args: Vec<String>,

    /// Extra arguments for every test binary.
    pub cargo_test_args: Vec<String>,

    /// Additional `Err(...)` values for `fn_value.err_with`.
    pub errors: Vec<String>,

    /// A lower bound on the per-mutant timeout, in seconds.
    pub minimum_test_timeout: Option<f64>,

    /// Run test binaries through `cargo nextest` for per-test process isolation.
    pub nextest: Option<bool>,

    /// How much memory control to place around each test binary.
    pub memory: Option<crate::exec::MemoryControl>,

    /// The multiple of a test binary's baseline peak memory a mutant of it may reach.
    pub memory_multiplier: Option<f64>,

    /// Absolute headroom added to a test binary's baseline peak memory, as a size such as `128MiB`.
    pub memory_headroom: Option<String>,

    /// An explicit memory ceiling for every test binary, as a size such as `2GiB`.
    pub memory_limit: Option<String>,

    /// A memory ceiling for the baseline runs themselves, as a size such as `4GiB`.
    pub baseline_memory_limit: Option<String>,

    /// A fixed build timeout, in seconds.
    pub build_timeout: Option<f64>,

    /// The multiple of the first build's duration a later build round is allowed.
    pub build_timeout_multiplier: Option<f64>,

    /// Directory for all user-facing artifacts.
    pub artifact_dir: Option<Utf8PathBuf>,

    /// Sharding.
    #[serde(default)]
    pub shard: Shard,

    /// File reports.
    #[serde(default)]
    pub reporters: Reporters,
}

/// Reads a size key that [`Config::validate`] has already accepted.
///
/// A key that did not parse stopped the run before this point, so there is nothing left here to
/// report and nothing to fall back to but leaving the setting unset.
fn size(text: Option<&str>) -> Option<u64> {
    let text = text?;

    bounds::size(text).ok()
}

/// The `[shard]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Shard {
    /// How many shards to divide the mutants into.
    pub count: Option<u32>,

    /// Which shard to run, from zero.
    pub index: Option<u32>,
}

/// The `[reporters]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Reporters {
    /// Load the viewer from a CDN instead of embedding it.
    pub html_external: Option<bool>,
}

impl Config {
    /// The Cargo-only settings discovery must share with the eventual run.
    #[must_use]
    pub(crate) fn cargo_options(&self) -> crate::exec::CargoOptions {
        crate::exec::CargoOptions {
            profile: self.profile.clone(),
            extra: self.cargo_args.clone(),
            ..crate::exec::CargoOptions::default()
        }
    }

    /// Loads the configuration named by the command line, honoring `--config` and `--no-config`.
    ///
    /// An explicit path must exist: asking for a file and silently getting the defaults because it
    /// was misspelled is the failure this guards against, whereas a missing conventional file is
    /// the ordinary case.
    pub fn resolve(select: &SelectArgs) -> Result<Self> {
        if select.config.no_config {
            return Ok(Self::default());
        }

        let Some(path) = select.config.path.as_ref() else {
            return Self::load(&select.dir);
        };

        let text = fs::read_to_string(path).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

        Self::parse(&text).map_err(|cause| error!("{path}: {cause}").usage())
    }

    /// Loads the configuration for a directory, if there is one.
    ///
    /// Returns the default configuration when the file is absent, which is the overwhelmingly
    /// common case and is not worth distinguishing from an empty file.
    pub fn load(dir: &Utf8Path) -> Result<Self> {
        let path = dir.join(RELATIVE_PATH);

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,

            Err(cause) if cause.kind() == ErrorKind::NotFound => return Ok(Self::default()),

            Err(cause) => return Err(error!("could not read `{path}`").caused_by(cause)),
        };

        Self::parse(&text).map_err(|cause| error!("{path}: {cause}").usage())
    }

    /// Parses configuration text.
    ///
    /// Separated from [`Self::load`] so the schema can be tested without touching a file system.
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|cause| {
            // toml's own message carries the line, the column and a caret, so it is a better
            // diagnostic than anything reconstructed here would be.
            cause.message().to_owned()
        })?;

        config.validate()?;

        Ok(config)
    }

    /// Range-checks the numeric keys.
    ///
    /// The command line checks the same values through its own parsers, but a setting can arrive
    /// from either place and only one of the two would otherwise be guarded.
    fn validate(&self) -> Result<(), String> {
        /// A key, its value if set, and the range check that applies to it.
        type Check = (&'static str, Option<f64>, fn(&str, f64) -> Result<f64, String>);

        let checks: [Check; 6] = [
            ("test-timeout-multiplier", self.test_timeout_multiplier, bounds::factor),
            ("minimum-test-timeout", self.minimum_test_timeout, bounds::seconds),
            ("build-timeout", self.build_timeout, bounds::seconds),
            ("build-timeout-multiplier", self.build_timeout_multiplier, bounds::factor),
            ("min-score", self.min_score, bounds::percentage),
            ("memory-multiplier", self.memory_multiplier, bounds::factor),
        ];

        for (key, value, check) in checks {
            if let Some(value) = value {
                let _checked = check(&value.to_string(), value).map_err(|cause| format!("{key}: {cause}"))?;
            }
        }

        let sizes = [
            ("memory-headroom", self.memory_headroom.as_deref()),
            ("memory-limit", self.memory_limit.as_deref()),
            ("baseline-memory-limit", self.baseline_memory_limit.as_deref()),
        ];

        for (key, value) in sizes {
            if let Some(value) = value {
                let _checked = bounds::size(value).map_err(|cause| format!("{key}: {cause}"))?;
            }
        }

        Ok(())
    }

    /// Reports whether a foreign configuration file exists but is not being read.
    ///
    /// A project with only this foreign file would otherwise see its settings silently do nothing,
    /// so the run says so out loud.
    #[must_use]
    pub fn foreign_present(dir: &Utf8Path) -> bool {
        dir.join(FOREIGN_PATH).is_file() && !dir.join(RELATIVE_PATH).is_file()
    }

    /// Applies the configuration underneath the command line.
    ///
    /// Scalars set on the command line win outright: a flag typed for this one run is the most
    /// specific statement of intent available. Lists concatenate, with the command line first, so a
    /// configured exclusion cannot be lost by adding one more on the command line.
    ///
    /// # Errors
    ///
    /// Returns a usage error if the merged settings contradict one another; see
    /// [`validate_effective`](Self::validate_effective).
    pub fn apply(&self, args: &mut RunArgs) -> Result<()> {
        self.apply_selection(&mut args.select)?;

        let implied_by_cli = crate::exec::implied_memory_control(args.measure.memory_limit, args.measure.baseline_memory_limit);

        args.min_score = args.min_score.or(self.min_score);
        args.measure.jobs = args.measure.jobs.or(self.jobs);
        args.measure.test_timeout_multiplier = args.measure.test_timeout_multiplier.or(self.test_timeout_multiplier);
        args.measure.minimum_test_timeout = args.measure.minimum_test_timeout.or(self.minimum_test_timeout);
        args.measure.nextest = args.measure.nextest || self.nextest.unwrap_or(false);
        args.measure.memory = args.measure.memory.or(implied_by_cli).or(self.memory);
        args.measure.memory_multiplier = args.measure.memory_multiplier.or(self.memory_multiplier);
        args.measure.memory_headroom = args.measure.memory_headroom.or_else(|| size(self.memory_headroom.as_deref()));
        args.measure.memory_limit = args.measure.memory_limit.or_else(|| size(self.memory_limit.as_deref()));
        args.measure.baseline_memory_limit = args
            .measure
            .baseline_memory_limit
            .or_else(|| size(self.baseline_memory_limit.as_deref()));
        args.limits.build_timeout = args.limits.build_timeout.or(self.build_timeout);
        args.limits.build_timeout_multiplier = args.limits.build_timeout_multiplier.or(self.build_timeout_multiplier);
        args.incremental = args.incremental.or(self.incremental);
        args.measure.profile = args.measure.profile.take().or_else(|| self.profile.clone());
        args.measure.cargo_args.extend(self.cargo_args.iter().cloned());
        args.measure.cargo_test_args.extend(self.cargo_test_args.iter().cloned());
        args.measure.test_packages.extend(self.test_packages.iter().cloned());
        args.measure.test_workspace = args.measure.test_workspace || self.test_workspace.unwrap_or(false);
        args.measure.whole_test_binaries = args.measure.whole_test_binaries || self.whole_test_binaries.unwrap_or(false);
        args.measure.include_tests.extend(self.include_tests.iter().cloned());
        args.measure.exclude_tests.extend(self.exclude_tests.iter().cloned());
        args.no_baseline = args.no_baseline || self.no_baseline.unwrap_or(false);
        args.no_confirm = args.no_confirm || self.no_confirm.unwrap_or(false);
        args.artifact_dir = args.artifact_dir.take().or_else(|| self.artifact_dir.clone());
        args.html_external = args.html_external || self.reporters.html_external.unwrap_or(false);

        if !args.measure.test_packages.is_empty() && args.measure.test_workspace {
            return Err(contradiction(
                "test-packages",
                !self.test_packages.is_empty(),
                "test-workspace",
                self.test_workspace == Some(true),
            ));
        }

        Ok(())
    }

    /// Folds the file's selection keys into `select` — the step `list`, `unsuppress`, and `hints`
    /// each take before discovery, and that `run` and `suppress` reach through [`apply`](Self::apply).
    ///
    /// `explain` is deliberately not in that set: it resolves a named subject rather than a
    /// selection, so it never calls this and the file's selection keys do not reach it.
    ///
    /// # Errors
    ///
    /// Returns a usage error if the merged settings contradict one another; see
    /// [`validate_effective`](Self::validate_effective).
    pub fn apply_selection(&self, select: &mut SelectArgs) -> Result<()> {
        if select.mutators.is_none()
            && let Some(selectors) = self.mutators.as_ref()
        {
            select.mutators = Some(selectors.join(","));
        }

        select.files.extend(self.files.iter().cloned());
        select.exclude_files.extend(self.exclude_files.iter().cloned());
        select.packages.extend(self.packages.iter().cloned());
        select.errors.extend(self.errors.iter().cloned());
        select.features.features.extend(self.features.iter().cloned());
        select.features.all_features = select.features.all_features || self.all_features.unwrap_or(false);
        select.features.no_default_features = select.features.no_default_features || self.no_default_features.unwrap_or(false);

        // Merged one field at a time on purpose, so that a count in the file and an index on the
        // command line make a whole shard between them — the split every CI matrix wants, since
        // the width is shared and the index is not. That the pair must end up whole is checked
        // afterwards, by `SelectArgs::shard`, on these effective values rather than on either
        // source alone.
        select.shard_count = select.shard_count.or(self.shard.count);
        select.shard_index = select.shard_index.or(self.shard.index);

        self.validate_effective(select)
    }

    /// Re-checks the mutually exclusive settings on the merged values.
    ///
    /// clap's `conflicts_with` constrains argument *occurrences*, so it sees only what was typed.
    /// The file writes into the same structures afterwards and can therefore populate a field clap
    /// has already decided must stay empty, and nothing looks again. The two pairs that reach a
    /// decision by different routes are re-checked here, on the effective values, where the source
    /// of each half no longer matters.
    ///
    /// `packages` beats `workspace` in `selected_packages`, so a file naming packages silently
    /// overrules `--workspace` and mutates a fraction of what was asked for.
    ///
    /// # Errors
    ///
    /// Returns a usage error naming both settings and where each came from.
    fn validate_effective(&self, select: &SelectArgs) -> Result<()> {
        if !select.packages.is_empty() && select.workspace {
            return Err(contradiction("packages", !self.packages.is_empty(), "workspace", false));
        }

        Ok(())
    }
}

/// Builds the usage error for a pair of settings that cannot both apply, naming each one's source.
///
/// Which source stated what is the whole content of the message: the same two settings from the
/// command line alone are caught by clap, so anyone reading this error has one of them in a file
/// they may have forgotten is there.
fn contradiction(first: &str, first_from_file: bool, second: &str, second_from_file: bool) -> Error {
    let source = |from_file: bool| if from_file { RELATIVE_PATH } else { "the command line" };

    error!(
        "`{first}` from {} and `{second}` from {} cannot both apply.\n\
             Drop one of them, or state the one you want on the command line and remove the other from {RELATIVE_PATH}.",
        source(first_from_file),
        source(second_from_file)
    )
    .usage()
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn select_args(dir: &Utf8Path) -> SelectArgs {
        SelectArgs {
            dir: dir.to_path_buf(),
            ..SelectArgs::default()
        }
    }

    /// `packages` in the file and `--workspace` on the command line both reach `selected_packages`,
    /// where `packages` wins — so a committed file quietly reduces `--workspace` to a fraction of
    /// the workspace and says nothing. clap cannot see it: `conflicts_with` constrains what was
    /// typed, and the file is merged in afterwards.
    #[test]
    fn a_configured_package_list_contradicts_workspace_on_the_command_line() {
        let dir = TempDir::new().expect("temp dir");
        let path = Utf8Path::from_path(dir.path()).expect("utf-8");

        let config = Config::parse("packages = [\"a\"]\n").expect("a package list parses");
        let mut select = SelectArgs {
            workspace: true,
            ..select_args(path)
        };

        let failure = config.apply_selection(&mut select).expect_err("the pair cannot both apply");
        let text = failure.to_string();

        assert!(text.contains("packages"), "{text}");
        assert!(text.contains("workspace"), "{text}");
        assert!(text.contains(RELATIVE_PATH), "{text}");
        assert!(text.contains("the command line"), "{text}");
    }

    /// The oracle half of the same defect, and the worse one: the file narrows which packages' tests
    /// judge every mutant while the flag says to widen it, so mutants another package's tests would
    /// kill are reported as survivors.
    #[test]
    fn a_configured_test_package_list_contradicts_test_workspace_on_the_command_line() {
        let dir = TempDir::new().expect("temp dir");
        let path = Utf8Path::from_path(dir.path()).expect("utf-8");

        let config = Config::parse("test-packages = [\"a\"]\n").expect("a test package list parses");
        let mut args = RunArgs {
            select: select_args(path),
            ..RunArgs::default()
        };

        args.measure.test_workspace = true;

        let failure = config.apply(&mut args).expect_err("the pair cannot both apply");
        let text = failure.to_string();

        assert!(text.contains("test-packages"), "{text}");
        assert!(text.contains("test-workspace"), "{text}");
    }

    /// Neither pair is a contradiction when only one half is stated, which is the ordinary case and
    /// must keep working.
    #[test]
    fn a_configured_list_on_its_own_is_not_a_contradiction() {
        let dir = TempDir::new().expect("temp dir");
        let path = Utf8Path::from_path(dir.path()).expect("utf-8");

        let config = Config::parse("packages = [\"a\"]\ntest-packages = [\"b\"]\n").expect("both lists parse");
        let mut args = RunArgs {
            select: select_args(path),
            ..RunArgs::default()
        };

        config.apply(&mut args).expect("one half of each pair is no contradiction");

        assert_eq!(args.select.packages, vec!["a".to_owned()]);
        assert_eq!(args.measure.test_packages, vec!["b".to_owned()]);
    }

    /// Every `conflicts_with` in the CLI whose two ids are both config-reachable needs a post-merge
    /// check, because the file can set one half after clap has stopped looking at the other.
    ///
    /// The pairs are listed rather than derived: clap does not expose its conflict graph, and the
    /// list is short enough that keeping it beside the checks it justifies is the honest way to
    /// notice a new one. `--features`/`--all-features` and the build-timeout pair are deliberately
    /// absent — cargo lets one win, and `cargo_options` takes the tighter of the two.
    #[test]
    fn every_config_reachable_conflicting_pair_is_checked_after_the_merge() {
        /// Each pair as the file spells its list half, plus the flag the command line sets.
        type Pair = (&'static str, fn(&mut RunArgs));

        let dir = TempDir::new().expect("temp dir");
        let path = Utf8Path::from_path(dir.path()).expect("utf-8");
        let pairs: [Pair; 2] = [
            ("packages", |args| args.select.workspace = true),
            ("test-packages", |args| args.measure.test_workspace = true),
        ];

        for (key, raise) in pairs {
            let config = Config::parse(&format!("{key} = [\"a\"]\n")).expect("the list parses");
            let mut args = RunArgs {
                select: select_args(path),
                ..RunArgs::default()
            };

            raise(&mut args);

            let failure = config.apply(&mut args).expect_err("the pair cannot both apply");

            assert!(failure.to_string().contains(key), "{key}");
        }
    }

    #[test]
    fn no_config_wins_over_a_file_that_is_there() {
        let dir = TempDir::new().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 path");

        fs::write(root.join(RELATIVE_PATH), "jobs = 7\n").expect("write");

        let mut select = select_args(root);

        select.config.no_config = true;
        assert_eq!(Config::resolve(&select).expect("resolves").jobs, None);

        select.config.no_config = false;
        assert_eq!(Config::resolve(&select).expect("resolves").jobs, Some(7));
    }

    #[test]
    fn an_explicit_config_path_is_read_instead_of_the_default_one() {
        let dir = TempDir::new().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 path");
        let elsewhere = root.join("elsewhere.toml");

        fs::write(root.join(RELATIVE_PATH), "jobs = 7\n").expect("write");
        fs::write(&elsewhere, "jobs = 3\n").expect("write");

        let mut select = select_args(root);

        select.config.path = Some(elsewhere);
        assert_eq!(Config::resolve(&select).expect("resolves").jobs, Some(3));
    }

    #[test]
    fn an_explicit_config_path_that_is_missing_is_an_error() {
        // An absent default file is ordinary; an absent file the user named by hand is a typo, and
        // silently running with no configuration would hide it.
        let dir = TempDir::new().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 path");
        let mut select = select_args(root);

        select.config.path = Some(root.join("nope.toml"));
        let _cause = Config::resolve(&select).unwrap_err();
    }

    #[test]
    fn memory_sizes_in_the_file_are_parsed_and_merged_into_the_arguments() {
        let config = Config::parse(
            "memory = \"enforce\"\nmemory-headroom = \"256MiB\"\nmemory-limit = \"2GiB\"\nbaseline-memory-limit = \"4GiB\"\n",
        )
        .expect("parses");

        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.measure.memory, Some(crate::exec::MemoryControl::Enforce));
        assert_eq!(args.measure.memory_headroom, Some(256 * 1024 * 1024));
        assert_eq!(args.measure.memory_limit, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(args.measure.baseline_memory_limit, Some(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn command_line_memory_limits_override_a_configured_memory_mode() {
        let config = Config::parse("memory = \"off\"\n").expect("parses");

        let mut enforcing = RunArgs::default();
        enforcing.measure.memory_limit = Some(1024);
        config.apply(&mut enforcing).expect("merges");

        assert_eq!(enforcing.measure.memory, Some(crate::exec::MemoryControl::Enforce));

        let mut measuring = RunArgs::default();
        measuring.measure.baseline_memory_limit = Some(1024);
        config.apply(&mut measuring).expect("merges");

        assert_eq!(measuring.measure.memory, Some(crate::exec::MemoryControl::Measure));
    }

    #[test]
    fn an_explicit_command_line_memory_mode_overrides_a_size_flag_implication() {
        let config = Config::parse("memory = \"measure\"\n").expect("parses");
        let mut args = RunArgs::default();

        args.measure.memory = Some(crate::exec::MemoryControl::Off);
        args.measure.memory_limit = Some(1024);
        config.apply(&mut args).expect("merges");

        assert_eq!(args.measure.memory, Some(crate::exec::MemoryControl::Off));
    }

    /// The file enables nextest when the command line does not, and yields when it does.
    #[test]
    fn nextest_in_the_file_is_merged_in_and_adds_to_the_command_line() {
        let config = Config::parse("nextest = true\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert!(args.measure.nextest);

        let mut chosen = RunArgs::default();

        chosen.measure.nextest = false;
        config
            .apply(&mut chosen)
            .expect("the merged settings do not contradict one another");

        assert!(chosen.measure.nextest);
    }

    /// The module promises that anything expressible on the command line is expressible here, and
    /// `--test-workspace` was the one flag that was not. With `deny_unknown_fields` the key was
    /// rejected outright, so a project could not commit its whole-workspace oracle policy and had
    /// to edit CI separately — which silently changes which tests judge each mutant.
    #[test]
    fn test_workspace_in_the_file_selects_the_same_oracle_as_the_flag() {
        let config = Config::parse("test-workspace = true\n").expect("the documented equivalence must hold");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert!(args.measure.test_workspace);
    }

    /// The file must not be able to switch the oracle off once the command line has asked for it.
    #[test]
    fn test_workspace_on_the_command_line_survives_a_file_that_does_not_set_it() {
        let config = Config::parse("test-workspace = false\n").expect("parses");
        let mut args = RunArgs::default();

        args.measure.test_workspace = true;
        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert!(args.measure.test_workspace);
    }

    #[test]
    fn whole_test_binaries_in_the_file_selects_the_same_oracle_as_the_flag() {
        let config = Config::parse("whole-test-binaries = true\n").expect("the documented equivalence must hold");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert!(args.measure.whole_test_binaries);
    }

    #[test]
    fn whole_test_binaries_on_the_command_line_survives_a_false_file_setting() {
        let config = Config::parse("whole-test-binaries = false\n").expect("parses");
        let mut args = RunArgs::default();

        args.measure.whole_test_binaries = true;
        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert!(args.measure.whole_test_binaries);
    }

    #[test]
    fn a_memory_size_that_is_not_a_size_is_reported_rather_than_ignored() {
        // A ceiling read as a handful of bytes would report every mutant as caught by tests that
        // could never have started, which is a far more expensive failure than a rejected file.
        let cause = Config::parse("memory-limit = \"lots\"\n").expect_err("must be rejected");

        assert!(cause.contains("memory-limit"), "{cause}");
    }

    #[test]
    fn an_empty_file_is_valid() {
        let config = Config::parse("").expect("an empty file is a valid file");

        assert!(config.mutators.is_none());
        assert!(config.files.is_empty());
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_a_silent_no_op() {
        // The whole point of `deny_unknown_fields`: a project that believes it has configured
        // something and has not is in a worse position than one with no configuration at all.
        let cause = Config::parse("exclude-file = [\"src/main.rs\"]\n").expect_err("must be rejected");

        assert!(cause.contains("unknown field"), "{cause}");
    }

    #[test]
    fn a_misspelled_key_in_a_table_is_also_an_error() {
        let cause = Config::parse("[shard]\ncount = 4\nidx = 0\n").expect_err("must be rejected");

        assert!(cause.contains("unknown field"), "{cause}");
    }

    #[test]
    fn keys_are_spelled_in_kebab_case() {
        let config =
            Config::parse("exclude-files = [\"tests/**\"]\ntest-timeout-multiplier = 3.0\n").expect("kebab-case is the file's spelling");

        assert_eq!(config.exclude_files, vec!["tests/**".to_owned()]);
        assert_eq!(config.test_timeout_multiplier, Some(3.0));
    }

    #[test]
    fn ops_are_joined_into_the_selector_list_the_flag_parses() {
        // One selector per line, with room for a comment, is the reason this is a list. It has to
        // arrive at exactly the same parser the flag uses, or the two spellings will drift.
        let config = Config::parse("mutators = [\"@arithmetic\", \"!bitwise\"]\n").expect("parses");
        let mut select = SelectArgs::default();

        config
            .apply_selection(&mut select)
            .expect("the merged settings do not contradict one another");

        assert_eq!(select.mutators.as_deref(), Some("@arithmetic,!bitwise"));
    }

    #[test]
    fn the_command_line_wins_for_scalars() {
        let config = Config::parse("mutators = [\"stmt\"]\nmin-score = 10.0\njobs = 1\n").expect("parses");
        let mut args = RunArgs {
            select: SelectArgs {
                mutators: Some("relational".to_owned()),
                ..SelectArgs::default()
            },
            min_score: Some(90.0),
            ..RunArgs::default()
        };

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.select.mutators.as_deref(), Some("relational"));
        assert_eq!(args.min_score, Some(90.0));

        // A key the command line did not speak to still applies.
        assert_eq!(args.measure.jobs, Some(1));
    }

    #[test]
    fn lists_concatenate_rather_than_replace() {
        // Replacing would mean that adding one exclusion on the command line silently drops every
        // exclusion the project has agreed on, which is the opposite of what typing it means.
        let config = Config::parse("exclude-files = [\"generated/**\"]\n").expect("parses");
        let mut args = RunArgs {
            select: SelectArgs {
                exclude_files: vec!["tests/**".to_owned()],
                ..SelectArgs::default()
            },
            ..RunArgs::default()
        };

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.select.exclude_files, vec!["tests/**".to_owned(), "generated/**".to_owned()]);
    }

    #[test]
    fn a_configured_flag_turns_on_and_the_command_line_cannot_turn_it_off() {
        // Boolean flags have no "off" spelling on the command line, so the configured value can
        // only ever add. This is worth a test because it is the one place the precedence rule
        // above does not apply, and it is easy to "fix" into a bug.
        let config = Config::parse("no-baseline = true\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert!(args.no_baseline);
    }

    #[test]
    fn artifact_directory_comes_from_the_file_when_the_command_line_is_silent() {
        let config = Config::parse("artifact-dir = \"out\"\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.artifact_dir.as_deref(), Some(Utf8Path::new("out")));

        args.artifact_dir = Some("cli-out".into());
        config.apply(&mut args).expect("command line wins");
        assert_eq!(args.artifact_dir.as_deref(), Some(Utf8Path::new("cli-out")));
    }

    #[test]
    fn individual_report_destinations_are_not_configurable() {
        for key in ["html", "json", "sarif", "advice"] {
            let text = format!("[reporters]\n{key} = \"out/report\"\n");
            let failure = Config::parse(&text).expect_err("individual destinations are gone");

            assert!(failure.contains("unknown field"), "{key}: {failure}");
        }
    }

    #[test]
    fn sharding_can_be_set_entirely_from_the_file() {
        let config = Config::parse("[shard]\ncount = 30\nindex = 7\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.select.shard().expect("valid sharding"), Some((30, 7)));
    }

    /// The split the file exists to support: every job in the matrix agrees on the width, which is
    /// committed, and each supplies the slice it is, which is not.
    #[test]
    fn a_count_from_the_file_and_an_index_from_the_command_line_make_one_shard() {
        let config = Config::parse("[shard]\ncount = 8\n").expect("parses");
        let mut args = RunArgs::default();

        args.select.shard_index = Some(3);
        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.select.shard_count, Some(8));
        assert_eq!(args.select.shard().expect("the pair is whole after merging"), Some((8, 3)));
    }

    /// And the mirror of it, for a file that pins the index and a command line that says how many.
    #[test]
    fn an_index_from_the_file_and_a_count_from_the_command_line_make_one_shard() {
        let config = Config::parse("[shard]\nindex = 0\n").expect("parses");
        let mut args = RunArgs::default();

        args.select.shard_count = Some(2);
        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.select.shard().expect("the pair is whole after merging"), Some((2, 0)));
    }

    /// The command line wins over the file for both halves, as it does for every other scalar.
    #[test]
    fn a_shard_named_on_the_command_line_overrides_the_file() {
        let config = Config::parse("[shard]\ncount = 8\nindex = 7\n").expect("parses");
        let mut args = RunArgs::default();

        args.select.shard_count = Some(3);
        args.select.shard_index = Some(1);
        config.apply(&mut args).expect("the merged settings do not contradict one another");

        assert_eq!(args.select.shard().expect("valid sharding"), Some((3, 1)));
    }

    /// A file that names half a shard and a command line that supplies nothing is not "no
    /// sharding": it is a setting that cannot be honored, and running the whole population under
    /// it would be eight times the work the job asked for, reported as a complete run.
    #[test]
    fn half_a_shard_in_the_file_and_nothing_on_the_command_line_is_refused() {
        let config = Config::parse("[shard]\ncount = 8\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        let error = args.select.shard().expect_err("a count with no index is not a shard");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("--shard-index"), "{error}");
    }

    /// A file that pins only the index is refused the same way, and says which half is missing.
    #[test]
    fn an_index_alone_in_the_file_is_refused() {
        let config = Config::parse("[shard]\nindex = 2\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        let error = args.select.shard().expect_err("an index with no count is not a shard");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("--shard-count"), "{error}");
    }

    /// The bounds are checked on the merged pair too, not only on what the command line carried.
    #[test]
    fn a_file_count_that_the_command_line_index_falls_outside_is_refused() {
        let config = Config::parse("[shard]\ncount = 4\n").expect("parses");
        let mut args = RunArgs::default();

        args.select.shard_index = Some(4);
        config.apply(&mut args).expect("the merged settings do not contradict one another");

        let error = args.select.shard().expect_err("index 4 of 4 shards does not exist");

        assert!(error.to_string().contains("out of range"), "{error}");
    }

    /// A zero count in the file is as impossible as one on the command line.
    #[test]
    fn a_zero_count_in_the_file_is_refused() {
        let config = Config::parse("[shard]\ncount = 0\nindex = 0\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args).expect("the merged settings do not contradict one another");

        let error = args.select.shard().expect_err("zero shards is not a division");

        assert!(error.to_string().contains("at least 1"), "{error}");
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        let config = Config::load(path).expect("an absent file is the common case");

        assert!(config.mutators.is_none());
    }

    #[test]
    fn a_present_file_is_read() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::write(path.join(RELATIVE_PATH), "jobs = 3\n").expect("could not write the config");

        let config = Config::load(path).expect("the file is valid");

        assert_eq!(config.jobs, Some(3));
    }

    #[test]
    fn a_malformed_file_is_a_usage_error_naming_the_path() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::write(path.join(RELATIVE_PATH), "jobs = \n").expect("could not write the config");

        let cause = Config::load(path).expect_err("a malformed file must stop the run");

        assert!(cause.is_usage(), "{cause}");
        assert!(cause.to_string().contains("gamma.toml"), "{cause}");
    }

    #[test]
    fn a_foreign_config_file_is_noticed_but_never_read() {
        // Reading it would mean another tool's settings quietly changing which mutants are
        // suppressed here. Noticing it is what lets the run say so out loud.
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::create_dir_all(path.join(".cargo")).expect("could not create .cargo");
        fs::write(path.join(FOREIGN_PATH), "exclude_re = [\"impl Debug\"]\n").expect("could not write the foreign config");

        assert!(Config::foreign_present(path));

        let config = Config::load(path).expect("the foreign file must not be parsed as ours");

        assert!(config.mutators.is_none());
    }

    #[test]
    fn a_config_that_cannot_be_read_is_an_error_rather_than_the_defaults() {
        // Only an absent file means "this project has no configuration". Anything else — a
        // directory in its place, a permission problem — has to be reported, because silently
        // falling back to the defaults would run with settings nobody chose.
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::create_dir_all(path.join(RELATIVE_PATH)).expect("could not create a directory in the config's place");

        let error = Config::load(path).expect_err("an unreadable config must not be treated as absent");

        assert!(error.to_string().contains(RELATIVE_PATH), "{error}");
    }
}
