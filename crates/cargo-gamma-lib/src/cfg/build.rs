// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What the build cargo will actually run compiles for.
//!
//! `rustc --print cfg` answers about the compiler's own host under its own defaults, and that is
//! not the build a run measures. Cargo is asked for a target, for a profile and with whatever
//! `RUSTFLAGS` the environment or the cargo configuration hold, and every one of those changes
//! which `#[cfg(...)]` predicates hold. Discovery that ignored them would classify target-only,
//! release-only or `--cfg`-gated code as absent and drop its mutants from the population, which
//! raises the score by removing the code nobody tested.
//!
//! So the settings are resolved here, from the same places cargo reads them, and handed to the
//! probe as command-line flags — because `rustc` itself does not read `RUSTFLAGS`, which is a
//! Cargo-facing variable, and inheriting it changes nothing at all.

use std::process::Command;
use std::{env, fs};

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_engine::cfg::{CfgSet, Verdict};
use toml::{Table, Value};

use crate::HashMap;

/// How many `inherits` hops a profile chain may take before it is called malformed.
const PROFILE_DEPTH: usize = 16;

/// The parts of a cargo build that decide which configuration predicates hold.
///
/// Built by [`Build::resolve`] from the settings cargo will read, or by
/// [`CargoOptions::cfg_build`](crate::exec::CargoOptions::cfg_build) from the options the run will
/// build with, so that discovery and the build cannot describe different compilations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Build {
    /// The target triple cargo will compile for, or `None` for the compiler's host.
    pub target: Option<String>,

    /// The custom predicates `--cfg` puts in force, as `loom` or `flavor="x"`.
    pub cfgs: Vec<String>,

    /// The codegen options that change which predicates hold, as `target-feature=+avx2`.
    ///
    /// Only the ones that decide a `cfg` are carried. A probe handed the whole of `RUSTFLAGS`
    /// would fail on the first nightly-only or lint-shaped flag in it and take the entire
    /// evaluation down with it, which costs far more than the predicates it would have refined.
    pub codegen: Vec<String>,

    /// Whether `debug_assertions` holds, or `None` when nothing consulted here can say.
    ///
    /// `None` is not "off". A profile whose chain cannot be followed leaves both halves of
    /// `#[cfg(debug_assertions)]` in the population rather than deleting one of them on a guess.
    pub debug_assertions: Option<bool>,

    /// Names some configuration puts in force under conditions this cannot evaluate.
    ///
    /// A `--cfg` in a `[target.'cfg(…)'.rustflags]` table applies only when its own predicate
    /// holds, and that predicate is about the very target being resolved. Rather than decide it,
    /// the name is left unanswerable, which keeps the code it gates mutable.
    pub undecided: Vec<String>,

    /// Whether the build compiles for more than one target at once.
    ///
    /// One set of predicates cannot describe two targets, so this suppresses evaluation entirely
    /// rather than describing whichever target happened to be written first.
    pub several_targets: bool,
}

impl Build {
    /// Works out what cargo will compile, for a workspace at `root`.
    ///
    /// `profile` and `extra` are the run's own `--profile` and passthrough cargo arguments, which
    /// outrank the configuration files. Everything else comes from where cargo would read it: the
    /// `CARGO_BUILD_TARGET`, `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` variables, the
    /// `.cargo/config.toml` files above the workspace, and the profile tables in the workspace
    /// manifest.
    ///
    /// Nothing here fails. A file that cannot be read or parsed simply says nothing, and what it
    /// would have said is either supplied by a file further out or left unanswered.
    #[must_use]
    pub fn resolve(root: &Utf8Path, profile: Option<&str>, extra: &[String]) -> Self {
        Self::resolve_in(root, profile, extra, &Environment::ambient())
    }

    /// Resolves against a given environment, so the tests do not depend on the ambient one.
    ///
    /// The variables cargo reads are taken as a value so tests need not write the process
    /// environment.
    fn resolve_in(root: &Utf8Path, profile: Option<&str>, extra: &[String], environment: &Environment) -> Self {
        let config = CargoConfig::load(root, environment);
        let (target, several_targets) = target(extra, environment, &config);
        let (flags, undecided) = rustflags(environment, &config, target.as_deref());
        let profile = named_profile(extra).or_else(|| profile.map(ToOwned::to_owned));

        Self {
            debug_assertions: assertions(&flags).or_else(|| profile_assertions(root, &config, profile.as_deref())),
            cfgs: valued(&flags, "--cfg"),
            codegen: codegen(&flags),
            target,
            undecided,
            several_targets,
        }
    }

    /// Everything [`Self::resolve`] reads other than the run's own `--profile` and passthrough
    /// arguments.
    ///
    /// Named here, beside the code that reads them, because another module has to know this set:
    /// the run record decides whether a cached "this mutant does not compile" was reached under the
    /// same build, and a second list of build inputs kept over there would fall behind this one
    /// without anything failing. When it did, unviability would be carried across a change that
    /// decides what compiles, and a mutant withheld from the denominator on that basis turns a gap
    /// in the suite into a better score. Anything the resolution learns to read belongs here.
    #[cfg_attr(
        not(any(test, feature = "internals")),
        expect(
            dead_code,
            reason = "the list exists to be compared against, not to be read: its only consumer is \
                      the invariant test that fails when the resolution learns to read an input \
                      nobody added here, so it is live only where that test can see it"
        )
    )]
    pub const INPUTS: &'static [&'static str] = &[
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_TARGET_<triple>_RUSTFLAGS",
        "CARGO_HOME",
        "cargo config files",
        "build.target",
        "build.rustflags",
        "target.*.rustflags",
        "profile.*",
    ];

    /// The triples the command line and the environment ask for, in cargo's precedence order.
    ///
    /// Split out of the resolution for the run record, which has to key what it cached on the
    /// target it was built for and cannot call [`Self::resolve`]: that needs a workspace root, and
    /// the record's context is settled before the workspace has been located. The configured
    /// `build.target` is not consulted here for exactly that reason — it lives in a file, so the
    /// record covers it through [`Self::settings`] instead.
    ///
    /// Several triples come back as several rather than as one answer, because a build for two
    /// targets is one no single set of predicates describes and a caller has to be able to tell
    /// that apart from a build for one.
    #[must_use]
    pub fn requested_targets(extra: &[String], configured: Option<&str>) -> Vec<String> {
        let mut named = valued(extra, "--target");

        if named.is_empty() {
            named = configured.map(ToOwned::to_owned).into_iter().collect();
        }

        named.sort();
        named.dedup();

        named
    }

    /// The values of the [`Self::INPUTS`] that live in files, rendered so a change moves the bytes.
    ///
    /// The variables are left out because a caller that digests this has the environment in hand
    /// already; what it cannot see without a root is the workspace. Rendered as named parts rather
    /// than as a resolved [`Build`] because the resolution collapses its inputs — a `--cfg` in a
    /// table this build does not read comes back as one undecided name whatever the flag was, and a
    /// profile body reached through `inherits` is folded into a single boolean — so two genuinely
    /// different configurations can resolve to the same value. What is wanted here is noticing the
    /// change, and the settings themselves are cheaper to be right about than their consequences.
    ///
    /// Deliberately over-inclusive: every profile table is rendered, not the one profile this run
    /// builds, and every target table, not the ones whose predicate holds. Being over-inclusive
    /// costs a cache that could have been kept, which costs time; being under-inclusive costs a
    /// mutant that is silently dropped from the denominator.
    #[must_use]
    pub fn settings(root: &Utf8Path) -> Vec<String> {
        Self::settings_in(root, &Environment::ambient())
    }

    /// Reads the settings against a given environment, so the tests do not depend on the ambient
    /// one; see [`Self::resolve_in`].
    fn settings_in(root: &Utf8Path, environment: &Environment) -> Vec<String> {
        let config = CargoConfig::load(root, environment);
        let mut parts = Vec::new();

        // Cargo configurations can set far more than the predicates this resolver projects. The
        // run record must still notice every one: `[env]`, wrappers and future Cargo keys can
        // change the build or test process without changing a selected `build.*` value.
        parts.extend(config.sources.iter().cloned());

        if let Some(target) = config.string(&["build", "target"]) {
            parts.push(format!("build.target={target}"));
        }

        for flag in config.strings(&["build", "rustflags"]) {
            parts.push(format!("build.rustflags={flag}"));
        }

        for table in config.keys(&["target"]) {
            for flag in config.strings(&["target", &table, "rustflags"]) {
                parts.push(format!("target.{table}.rustflags={flag}"));
            }
        }

        // Whole tables rather than the keys the profile chain happens to follow, and one part per
        // file rather than a merged view, because the merge is the resolution's business: a key
        // that moves from one file to another changes which of them wins, and a reader that only
        // kept the winner could not see that it had.
        let manifest = read_table(&root.join("Cargo.toml"));

        for table in config.tables.iter().chain(manifest.as_ref()) {
            if let Some(profiles) = table.get("profile") {
                parts.push(format!("profile={profiles}"));
            }
        }

        parts
    }

    /// The `rustc` command line that answers which predicates hold for this build.
    pub(super) fn probe_args(&self) -> Vec<String> {
        let mut args = vec!["--print".to_owned(), "cfg".to_owned()];

        if let Some(target) = self.target.as_ref() {
            args.push("--target".to_owned());
            args.push(target.clone());
        }

        if let Some(on) = self.debug_assertions {
            args.push("-C".to_owned());
            args.push(format!("debug-assertions={}", if on { "on" } else { "off" }));
        }

        for option in &self.codegen {
            args.push("-C".to_owned());
            args.push(option.clone());
        }

        for predicate in &self.cfgs {
            args.push("--cfg".to_owned());
            args.push(predicate.clone());
        }

        args
    }
}

/// The environment variables cargo reads that decide what a build compiles.
#[derive(Clone, Debug, Default)]
struct Environment {
    /// `CARGO_BUILD_TARGET`, the variable spelling of `build.target`.
    target: Option<String>,

    /// `CARGO_ENCODED_RUSTFLAGS`, whose entries are separated by unit separators.
    encoded_rustflags: Option<String>,

    /// `RUSTFLAGS`, whose entries are separated by spaces.
    rustflags: Option<String>,

    /// `CARGO_BUILD_RUSTFLAGS`, the variable spelling of `build.rustflags`.
    build_rustflags: Option<String>,

    /// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, the variable spelling of `target.<triple>.rustflags`.
    ///
    /// Keyed by the variable's own triple component rather than by the triple, because the mapping
    /// from a config key to a variable name loses information — `-` and `.` both become `_` — and
    /// cannot be run backwards. A lookup normalizes the triple it holds the same way instead.
    target_rustflags: HashMap<String, String>,

    /// Where the user-wide cargo configuration lives.
    cargo_home: Option<Utf8PathBuf>,
}

impl Environment {
    /// The variable-spelled rustflags for a triple, if the environment sets them.
    fn target_flags(&self, triple: &str) -> Option<&str> {
        self.target_rustflags.get(&variable_component(triple)).map(String::as_str)
    }

    // #[gamma::skip(fn_value.default, reason = "this adapter reads process-wide variables that parallel tests cannot safely replace; Environment::read is tested with an injected lookup")]
    /// Reads the variables this process was launched with.
    fn ambient() -> Self {
        Self::read(
            |name| env::var(name),
            env::vars_os().filter_map(|(name, _value)| name.into_string().ok()),
        )
    }

    /// Reads the variables, given a lookup and the names the environment holds.
    ///
    /// The names are needed as well as the lookup because one of the variables cargo reads has a
    /// name this cannot know in advance: `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` names its own triple,
    /// and the triple in force is not settled until the configuration has been read.
    fn read(mut get: impl FnMut(&str) -> Result<String, env::VarError>, names: impl IntoIterator<Item = String>) -> Self {
        let home = get("CARGO_HOME").ok().map(Utf8PathBuf::from).or_else(|| {
            get("HOME")
                .or_else(|_absent| get("USERPROFILE"))
                .ok()
                .map(|home| Utf8PathBuf::from(home).join(".cargo"))
        });

        let target = get("CARGO_BUILD_TARGET").ok();
        let encoded_rustflags = get("CARGO_ENCODED_RUSTFLAGS").ok();
        let rustflags = get("RUSTFLAGS").ok();
        let build_rustflags = get("CARGO_BUILD_RUSTFLAGS").ok();

        let target_rustflags = names
            .into_iter()
            .filter_map(|name| {
                let component = name.strip_prefix("CARGO_TARGET_")?.strip_suffix("_RUSTFLAGS")?.to_owned();

                Some((component, get(&name).ok()?))
            })
            .collect();

        Self {
            target,
            encoded_rustflags,
            rustflags,
            build_rustflags,
            target_rustflags,
            cargo_home: home,
        }
    }
}

/// The cargo configuration files that apply to a build under `dir`, nearest first.
///
/// Cargo reads `.cargo/config.toml` from the directory it runs in and every directory above it,
/// then the user-wide file. A setting from a nearer file wins, and array-valued settings are
/// joined, which is what this preserves by keeping the files in order rather than merging them.
#[derive(Debug, Default)]
struct CargoConfig {
    tables: Vec<Table>,
    sources: Vec<String>,
}

impl CargoConfig {
    fn load(dir: &Utf8Path, environment: &Environment) -> Self {
        let mut config = Self::default();
        let mut at = Some(dir);

        while let Some(directory) = at {
            for name in ["config.toml", "config"] {
                let path = directory.join(".cargo").join(name);
                if config.include(&path) {
                    break;
                }
            }

            // #[gamma::skip(stmt.delete_assign, reason = "without advancing to the parent this loop is intrinsically nonterminating; the mutation runner observes that only as its timeout")]
            at = directory.parent();
        }

        if let Some(home) = environment.cargo_home.as_ref()
            && !cargo_home_was_walked(dir, home)
        {
            for name in ["config.toml", "config"] {
                let path = home.join(name);
                if config.include(&path) {
                    break;
                }
            }
        }

        config
    }

    /// Reads one Cargo configuration as both a parsed resolver input and opaque record input.
    ///
    /// A syntactically invalid file still changes Cargo's behavior by making the build fail, so
    /// its bytes must invalidate a prior successful record even though no table can be resolved.
    fn include(&mut self, path: &Utf8Path) -> bool {
        let bytes = match fs::read(path.as_std_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(error) => {
                self.sources.push(format!("cargo-config:{path}:unreadable:{:?}", error.kind()));
                return true;
            }
        };

        self.sources.push(format!("cargo-config:{path}:{}", blake3::hash(&bytes).to_hex()));

        if let Ok(text) = core::str::from_utf8(&bytes)
            && let Ok(table) = toml::from_str(text)
        {
            self.tables.push(table);
        }

        true
    }

    /// The nearest file's value for a dotted key, when it is a string.
    fn string(&self, path: &[&str]) -> Option<&str> {
        self.tables.iter().find_map(|table| lookup(table, path)?.as_str())
    }

    /// Every file's value for a dotted key, as the list of strings cargo would join them into.
    ///
    /// A cargo configuration key of this shape accepts either one string or a list of them, and
    /// the lists in several files are concatenated rather than shadowing one another.
    fn strings(&self, path: &[&str]) -> Vec<String> {
        self.tables
            .iter()
            .filter_map(|table| lookup(table, path))
            .flat_map(|value| match value {
                Value::String(one) => vec![one.clone()],
                Value::Array(many) => many.iter().filter_map(|entry| entry.as_str().map(ToOwned::to_owned)).collect(),
                _other => Vec::new(),
            })
            .collect()
    }

    /// The keys of a table, across every file that declares one, nearest first.
    fn keys(&self, path: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = self
            .tables
            .iter()
            .filter_map(|table| lookup(table, path)?.as_table())
            .flat_map(|table| table.keys().cloned())
            .collect();

        names.sort();
        names.dedup();
        names
    }
}

/// Whether the upward `.cargo` search already visited this Cargo home.
fn cargo_home_was_walked(dir: &Utf8Path, home: &Utf8Path) -> bool {
    home.file_name() == Some(".cargo") && home.parent().is_some_and(|parent| dir.starts_with(parent))
}

/// Reads a TOML file, or nothing at all when it is absent or malformed.
fn read_table(path: &Utf8Path) -> Option<Table> {
    let text = fs::read_to_string(path.as_std_path()).ok()?;

    toml::from_str(&text).ok()
}

/// Follows a dotted key through nested tables.
fn lookup<'table>(table: &'table Table, path: &[&str]) -> Option<&'table Value> {
    let (last, leading) = path.split_last()?;
    let mut at = table;

    for name in leading {
        at = at.get(*name)?.as_table()?;
    }

    at.get(*last)
}

/// The target triple the build compiles for, and whether there is more than one of them.
///
/// `--target` on the command line outranks `CARGO_BUILD_TARGET`, which outranks `build.target`,
/// exactly as cargo orders them.
fn target(extra: &[String], environment: &Environment, config: &CargoConfig) -> (Option<String>, bool) {
    let mut named = Build::requested_targets(extra, environment.target.as_deref());

    if named.is_empty() {
        named = config.strings(&["build", "target"]);
    }

    // #[gamma::skip(iter.remove_sort, reason = "sorting only makes equal targets adjacent before `dedup`; zero, one, or several distinct targets is unchanged because an all-equal input is already adjacent")]
    named.sort();
    named.dedup();

    match named.len() {
        0 => (None, false),
        1 => (named.pop(), false),
        _several => (None, true),
    }
}

/// The rustc flags cargo will pass, and the names it may pass under a predicate of its own.
///
/// Cargo takes these from the first source that has anything to say, rather than merging them:
/// `CARGO_ENCODED_RUSTFLAGS`, then `RUSTFLAGS`, then the target tables — the triple's own joined
/// with every `cfg(…)` table whose predicate holds for that target, as cargo joins them — then
/// `build.rustflags`, whose environment spelling `CARGO_BUILD_RUSTFLAGS` shares that last slot
/// because it is the same setting written another way.
///
/// A target table this build does not read decides nothing, and what it *would* have decided is
/// reported as unanswerable rather than answered from a lower slot: a `--cfg` name, and equally a
/// `-C debug-assertions`, `-C panic` or `-C target-feature`, each settles a predicate, and letting
/// the profile chain or the compiler's own default answer instead describes a different
/// compilation from the one cargo will run.
fn rustflags(environment: &Environment, config: &CargoConfig, target: Option<&str>) -> (Vec<String>, Vec<String>) {
    let tables = target_tables(config, target, environment);
    let joined: Vec<String> = tables
        .iter()
        .filter(|(applies, _flags)| *applies == Verdict::Yes)
        .flat_map(|(_applies, flags)| flags.iter().cloned())
        .collect();

    let ambient = environment
        .encoded_rustflags
        .as_ref()
        .map(|encoded| {
            encoded
                .split('\u{1f}')
                .filter(|flag| !flag.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .or_else(|| environment.rustflags.as_ref().map(|flags| split(flags)));

    // Whether the tables are the source in force decides both the flags and what is left hanging:
    // a table whose flags this build really passes has answered its own names, and every other
    // table has not.
    let reading_tables = ambient.is_none() && !joined.is_empty();

    let chosen = ambient
        .or_else(|| reading_tables.then(|| joined.clone()))
        .or_else(|| environment.build_rustflags.as_ref().map(|flags| split(flags)))
        .or_else(|| {
            let flags = config.strings(&["build", "rustflags"]);

            (!flags.is_empty()).then_some(flags)
        })
        .unwrap_or_default();

    let mut undecided: Vec<String> = Vec::new();

    for (applies, flags) in &tables {
        if reading_tables && *applies == Verdict::Yes {
            continue;
        }

        undecided.extend(decided_names(flags));
    }

    // #[gamma::skip(iter.remove_sort, reason = "the only consumer collects these names into a HashSet, so their order cannot affect cfg evaluation")]
    undecided.sort();
    undecided.dedup();

    (chosen, undecided)
}

/// Every `[target.…]` table's rustflags, paired with whether they are in force for this build.
///
/// A table named by a triple is in force when that triple is the one being built; a table named by
/// a `cfg(…)` predicate is in force when the predicate holds for it, which is a question about the
/// target and so is put to `rustc`. Asking costs one process, and only for a workspace that
/// actually writes such a table — but not asking means either applying flags that are not in force
/// or ignoring flags that are, and both describe a compilation cargo will not run.
///
/// The probe answers about the bare target rather than about the target as these very flags will
/// leave it, which is the circularity cargo has too. A predicate that turns on a name some
/// rustflag sets is therefore answered `Unknown`, and the table it names is left unread and
/// unanswerable rather than resolved on a guess.
fn target_tables(config: &CargoConfig, target: Option<&str>, environment: &Environment) -> Vec<(Verdict, Vec<String>)> {
    let names = config.keys(&["target"]);
    let asked_about = !names.is_empty() || !environment.target_rustflags.is_empty();
    let triple = || target.or_else(|| host_triple(config, asked_about));
    let mut cfgs: Option<Option<CfgSet>> = None;

    let mut tables: Vec<(Verdict, Vec<String>)> = names
        .iter()
        .map(|name| {
            let applies = predicate_of(name).map_or_else(
                || triple().map_or(Verdict::Unknown, |triple| Verdict::from(triple == name)),
                |predicate| {
                    cfgs.get_or_insert_with(|| probe_cfgs(target))
                        .as_ref()
                        .map_or(Verdict::Unknown, |cfgs| cfgs.decide_str(predicate))
                },
            );

            // The variable spelling of a key wins over every file that states it, as it does for
            // any cargo configuration value. Only a table named by a triple has one: the variable
            // form of a `cfg(…)` key is not a name cargo will construct.
            let flags = predicate_of(name)
                .is_none()
                .then(|| environment.target_flags(name).map(split))
                .flatten()
                .unwrap_or_else(|| config.strings(&["target", name, "rustflags"]));

            (applies, flags)
        })
        .collect();

    // A variable can also name a triple no file mentions at all. Only the triple in force is
    // reachable, because the variable's name is an uppercased, underscored spelling of the triple
    // and that mapping cannot be run backwards to recover the triples the environment names.
    if let Some(triple) = triple()
        && !names.iter().any(|name| name == triple)
        && let Some(flags) = environment.target_flags(triple)
    {
        tables.push((Verdict::Yes, split(flags)));
    }

    tables
}

/// The component a triple contributes to a `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` variable name.
///
/// Cargo's rule for spelling any configuration key as a variable: uppercase it, and replace `-` and
/// `.` with `_`.
fn variable_component(triple: &str) -> String {
    triple.to_uppercase().replace(['-', '.'], "_")
}

/// The predicate inside a `cfg(…)` table name, or nothing for a table named by a triple.
fn predicate_of(table: &str) -> Option<&str> {
    table.strip_prefix("cfg(")?.strip_suffix(')')
}

/// The predicates that hold for a target, as the compiler's own answer about it.
///
/// Deliberately asked without the flags being resolved: they are what this is being asked in order
/// to choose. Failing to run says nothing, which leaves every `cfg(…)` table unread and its names
/// unanswerable — the same direction every other uncertainty in this module resolves in.
fn probe_cfgs(target: Option<&str>) -> Option<CfgSet> {
    let program = env::var("RUSTC").unwrap_or_else(|_absent| "rustc".to_owned());
    let mut command = Command::new(program);

    let _builder = command.arg("--print").arg("cfg");

    if let Some(triple) = target {
        let _builder = command.arg("--target").arg(triple);
    }

    let output = command.output().ok().filter(|output| output.status.success())?;

    Some(CfgSet::parse(&String::from_utf8_lossy(&output.stdout)))
}

/// The predicate names a flag vector settles, whichever spelling it settles them in.
///
/// Read from a table this build does not use, so that each name can be marked unanswerable. The
/// `-C` options are here for the same reason the `--cfg` names are: [`assertions`] and [`codegen`]
/// read them out of the chosen vector, so a table that sets one and is not chosen leaves that
/// predicate to be answered by something describing a different compilation.
fn decided_names(flags: &[String]) -> Vec<String> {
    let mut names: Vec<String> = valued(flags, "--cfg").into_iter().map(|predicate| name_of(&predicate)).collect();

    for option in options(flags) {
        if option
            .strip_prefix("debug-assertions")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
        {
            names.push("debug_assertions".to_owned());
        } else if option.starts_with("panic=") {
            names.push("panic".to_owned());
        } else if option.starts_with("target-feature=") {
            names.push("target_feature".to_owned());
        }
    }

    names
}

/// The compiler's own triple, asked for only when a target table might apply to it.
///
/// A `target.<triple>.rustflags` table is in force for a build with no `--target` when the triple
/// is the host's, so answering that needs the host's name — and nothing else here does, which is
/// why it is not asked for up front. `asked_about` is the caller's statement that something —
/// a configuration table or a `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` variable — names a triple at all.
fn host_triple(config: &CargoConfig, asked_about: bool) -> Option<&'static str> {
    use std::sync::OnceLock;

    /// One `rustc -vV` per process, since the answer cannot change under a running run.
    static HOST: OnceLock<Option<String>> = OnceLock::new();

    if !asked_about && config.keys(&["target"]).is_empty() {
        return None;
    }

    HOST.get_or_init(|| {
        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "the process-wide compiler override cannot be mutated safely by parallel tests; host parsing and command behavior are exercised independently")]
        let program = env::var("RUSTC").unwrap_or_else(|_absent| "rustc".to_owned());
        let output = Command::new(program).arg("-vV").output().ok()?;
        let printed = String::from_utf8_lossy(&output.stdout).into_owned();

        parse_host(&printed)
    })
    .as_deref()
}

fn parse_host(printed: &str) -> Option<String> {
    printed
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|triple| triple.trim().to_owned())
}

/// Splits a space-separated flag string, as cargo does for `RUSTFLAGS`.
fn split(flags: &str) -> Vec<String> {
    flags.split_whitespace().map(ToOwned::to_owned).collect()
}

/// Collects the values of a flag, written either as two arguments or joined by `=`.
///
/// The prefix has to be matched exactly before the `=`, or `--target-dir` would be read as a
/// target triple and every predicate would then describe a build that does not exist.
fn valued(args: &[String], flag: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut expecting = false;

    for argument in args {
        if expecting {
            found.push(argument.clone());
            // #[gamma::skip(assign_value.default, reason = "`expecting` is bool, whose `Default::default()` is exactly false")]
            expecting = false;

            continue;
        }

        if argument == flag {
            expecting = true;
        } else if let Some(value) = argument.strip_prefix(flag).and_then(|rest| rest.strip_prefix('=')) {
            found.push(value.to_owned());
        }
    }

    found
}

/// The name a `--cfg` value puts in force, without whatever it is set to.
fn name_of(predicate: &str) -> String {
    predicate.split_once('=').map_or(predicate, |(name, _value)| name).trim().to_owned()
}

/// The codegen options that decide a predicate, in the spelling `-C` takes.
///
/// `target-feature` decides the `target_feature` values and `panic` decides the `panic` one. The
/// profile's own `panic` setting is not consulted, because cargo ignores it for the `test` profile
/// — which is the profile gamma's `cargo test --no-run` build uses — and a flag is the only way it
/// can reach the compilation this describes.
fn codegen(flags: &[String]) -> Vec<String> {
    options(flags)
        .filter(|option| option.starts_with("target-feature=") || option.starts_with("panic="))
        .map(ToOwned::to_owned)
        .collect()
}

/// Whether the flags settle `debug_assertions` themselves.
///
/// `RUSTFLAGS` reaches rustc after the flags cargo derives from the profile, so a
/// `-C debug-assertions` there is the last word whatever the profile says.
fn assertions(flags: &[String]) -> Option<bool> {
    options(flags)
        .filter_map(|option| option.strip_prefix("debug-assertions"))
        .filter_map(|rest| match rest {
            // A bare `-C debug-assertions` turns them on.
            "" => Some(Some(true)),

            // Anything else that merely starts with the name — `-C debug-assertions-foo` — is a
            // different option, and must not be read as this one.
            _other => rest.strip_prefix('=').map(|value| match value.trim() {
                "y" | "yes" | "on" | "true" => Some(true),
                "n" | "no" | "off" | "false" => Some(false),
                _unrecognised => None,
            }),
        })
        .last()
        .flatten()
}

/// The `-C` options among a flag list, however they were spelled.
fn options(flags: &[String]) -> impl Iterator<Item = &str> {
    let mut expecting = false;

    flags.iter().filter_map(move |flag| {
        if expecting {
            // #[gamma::skip(assign_value.default, reason = "`expecting` is bool, whose `Default::default()` is exactly false")]
            expecting = false;

            return Some(flag.as_str());
        }

        if flag == "-C" || flag == "--codegen" {
            expecting = true;

            return None;
        }

        flag.strip_prefix("-C").or_else(|| flag.strip_prefix("--codegen="))
    })
}

/// The profile named on the command line, which outranks anything configured.
fn named_profile(extra: &[String]) -> Option<String> {
    if extra.iter().any(|argument| argument == "--release" || argument == "-r") {
        return Some("release".to_owned());
    }

    valued(extra, "--profile").pop()
}

/// Whether the profile the build will use has debug assertions on.
///
/// A run that names no profile builds with `cargo test`, which uses the `test` profile, so that is
/// what an unnamed profile resolves to rather than `dev`.
///
/// Returns `None` for a profile whose chain cannot be followed — a custom profile that inherits
/// from nothing, or from itself — because a guess either way deletes half of the conditionally
/// compiled code from the population.
fn profile_assertions(root: &Utf8Path, config: &CargoConfig, profile: Option<&str>) -> Option<bool> {
    let manifest = read_table(&root.join("Cargo.toml"));
    let declared = |name: &str, key: &str| {
        config.string(&["profile", name, key]).map(ToOwned::to_owned).or_else(|| {
            manifest
                .as_ref()
                .and_then(|table| lookup(table, &["profile", name, key]))?
                .as_str()
                .map(ToOwned::to_owned)
        })
    };
    let switched = |name: &str| {
        ["debug-assertions", "debug_assertions"]
            .into_iter()
            .find_map(|key| lookup_bool(config, manifest.as_ref(), name, key))
    };

    let mut current = profile.unwrap_or("test").to_owned();

    for _hop in 0..PROFILE_DEPTH {
        if let Some(on) = switched(&current) {
            return Some(on);
        }

        // The two built-in profiles that settle the question outright. Both can be overridden,
        // which is why the declared value is consulted first.
        match current.as_str() {
            "dev" => return Some(true),
            "release" => return Some(false),
            _custom => {}
        }

        let parent = declared(&current, "inherits").or_else(|| match current.as_str() {
            "test" => Some("dev".to_owned()),
            "bench" => Some("release".to_owned()),
            _custom => None,
        })?;

        current = parent;
    }

    None
}

/// A profile's boolean setting, from the cargo configuration first and the manifest second.
fn lookup_bool(config: &CargoConfig, manifest: Option<&Table>, profile: &str, key: &str) -> Option<bool> {
    let path = ["profile", profile, key];

    config
        .tables
        .iter()
        .find_map(|table| lookup(table, &path)?.as_bool())
        .or_else(|| lookup(manifest?, &path)?.as_bool())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// A workspace root with the given files written into it, and no environment at all.
    fn tree(files: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        for (relative, contents) in files {
            let path = root.join(relative);

            fs::create_dir_all(path.parent().expect("every fixture path has a parent").as_std_path()).expect("directories");
            fs::write(path.as_std_path(), contents).expect("the fixture file is written");
        }

        (directory, root)
    }

    /// An environment holding nothing, so a fixture answers for itself.
    ///
    /// `cargo_home` points into the fixture, which has no user-wide file, so whatever the machine
    /// running the tests has configured cannot reach them.
    fn empty(root: &Utf8Path) -> Environment {
        Environment {
            cargo_home: Some(root.join("nowhere")),
            ..Environment::default()
        }
    }

    #[test]
    fn environment_names_and_home_fallbacks_are_read_exactly() {
        let values = std::collections::HashMap::from([
            ("HOME", "home"),
            ("USERPROFILE", "profile"),
            ("CARGO_BUILD_TARGET", "target"),
            ("CARGO_ENCODED_RUSTFLAGS", "encoded"),
            ("RUSTFLAGS", "plain"),
            ("CARGO_BUILD_RUSTFLAGS", "build"),
        ]);
        let mut asked = Vec::new();
        let environment = Environment::read(
            |name| {
                asked.push(name.to_owned());
                values.get(name).map(|value| (*value).to_owned()).ok_or(env::VarError::NotPresent)
            },
            [],
        );

        assert_eq!(
            asked,
            [
                "CARGO_HOME",
                "HOME",
                "CARGO_BUILD_TARGET",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTFLAGS",
                "CARGO_BUILD_RUSTFLAGS",
            ]
        );
        assert_eq!(environment.cargo_home.as_deref(), Some(Utf8Path::new("home/.cargo")));
        assert_eq!(environment.target.as_deref(), Some("target"));
        assert_eq!(environment.encoded_rustflags.as_deref(), Some("encoded"));
        assert_eq!(environment.rustflags.as_deref(), Some("plain"));
        assert_eq!(environment.build_rustflags.as_deref(), Some("build"));

        let profile = Environment::read(
            |name| {
                (name == "USERPROFILE")
                    .then(|| "profile".to_owned())
                    .ok_or(env::VarError::NotPresent)
            },
            [],
        );
        assert_eq!(profile.cargo_home.as_deref(), Some(Utf8Path::new("profile/.cargo")));
    }

    fn resolve(root: &Utf8Path, profile: Option<&str>, extra: &[&str]) -> Build {
        let extra: Vec<String> = extra.iter().map(|argument| (*argument).to_owned()).collect();

        Build::resolve_in(root, profile, &extra, &empty(root))
    }

    /// The settings a record has to notice are read whole, not through the resolution that
    /// collapses them: a target table this build does not read still decides what a build that does
    /// read it compiles.
    #[test]
    fn the_settings_name_every_file_borne_input_the_resolution_reads() {
        let (_directory, root) = tree(&[
            (
                ".cargo/config.toml",
                "[build]\ntarget = \"wasm32-unknown-unknown\"\nrustflags = [\"--cfg\", \"loom\"]\n\n[target.'cfg(unix)']\nrustflags = [\"-Cdebug-assertions=on\"]\n\n[profile.dev]\ndebug-assertions = false\n",
            ),
            ("Cargo.toml", "[profile.mutants]\ninherits = \"release\"\ndebug-assertions = true\n"),
        ]);

        let settings = Build::settings_in(&root, &empty(&root));

        assert!(settings.contains(&"build.target=wasm32-unknown-unknown".to_owned()), "{settings:?}");
        assert!(settings.contains(&"build.rustflags=--cfg".to_owned()), "{settings:?}");
        assert!(settings.contains(&"build.rustflags=loom".to_owned()), "{settings:?}");
        assert!(
            settings.contains(&"target.cfg(unix).rustflags=-Cdebug-assertions=on".to_owned()),
            "{settings:?}"
        );
        assert_eq!(
            settings.iter().filter(|part| part.starts_with("profile=")).count(),
            2,
            "both the configuration's profiles and the manifest's are read: {settings:?}"
        );
    }

    /// A workspace nobody has configured says nothing, rather than saying something that varies
    /// with the machine the tests run on.
    #[test]
    fn an_unconfigured_workspace_holds_no_settings() {
        let (_directory, root) = tree(&[]);

        assert!(Build::settings_in(&root, &empty(&root)).is_empty());
    }

    #[test]
    fn an_unprojected_cargo_setting_still_moves_the_record_input() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[env]\nSUBJECT_MODE = \"one\"\n")]);
        let before = Build::settings_in(&root, &empty(&root));

        fs::write(root.join(".cargo/config.toml").as_std_path(), "[env]\nSUBJECT_MODE = \"two\"\n").expect("configuration is writable");

        assert_ne!(before, Build::settings_in(&root, &empty(&root)));
    }

    #[test]
    fn a_user_cargo_setting_still_moves_the_record_input() {
        let (_workspace, root) = tree(&[]);
        let (_home_directory, home) = tree(&[("config.toml", "[env]\nSUBJECT_MODE = \"one\"\n")]);
        let environment = Environment {
            cargo_home: Some(home.clone()),
            ..Environment::default()
        };
        let before = Build::settings_in(&root, &environment);

        fs::write(home.join("config.toml").as_std_path(), "[env]\nSUBJECT_MODE = \"two\"\n").expect("configuration is writable");

        assert_ne!(before, Build::settings_in(&root, &environment));
    }

    /// The record keys on the target before it has a workspace, so this answers from the two
    /// sources that do not need one — and from neither of them when they disagree with each other,
    /// which is a build no single set of predicates describes.
    #[test]
    fn the_requested_targets_are_the_ones_the_command_line_and_the_environment_ask_for() {
        let extra = ["--target=wasm32-unknown-unknown".to_owned()];
        let both = [
            "--target=wasm32-unknown-unknown".to_owned(),
            "--target".to_owned(),
            "aarch64-apple-darwin".to_owned(),
        ];

        assert_eq!(Build::requested_targets(&[], None), Vec::<String>::new());
        assert_eq!(
            Build::requested_targets(&[], Some("x86_64-unknown-linux-musl")),
            ["x86_64-unknown-linux-musl"]
        );
        assert_eq!(
            Build::requested_targets(&extra, Some("x86_64-unknown-linux-musl")),
            ["wasm32-unknown-unknown"],
            "a passthrough target outranks the variable, as it does for cargo"
        );
        assert_eq!(
            Build::requested_targets(&both, None),
            ["aarch64-apple-darwin", "wasm32-unknown-unknown"]
        );
    }

    #[test]
    fn a_passthrough_target_is_read_however_it_is_written() {
        let (_directory, root) = tree(&[]);

        assert_eq!(
            resolve(&root, None, &["--target", "wasm32-unknown-unknown"]).target.as_deref(),
            Some("wasm32-unknown-unknown")
        );
        assert_eq!(
            resolve(&root, None, &["--target=wasm32-unknown-unknown"]).target.as_deref(),
            Some("wasm32-unknown-unknown")
        );
    }

    /// `--target-dir` shares the first eight characters with `--target` and says nothing about the
    /// triple. Reading it as one would describe a build that does not exist.
    #[test]
    fn a_target_directory_is_not_a_target() {
        let (_directory, root) = tree(&[]);
        let build = resolve(&root, None, &["--target-dir=elsewhere"]);

        assert_eq!(build.target, None);
        assert!(!build.several_targets);
    }

    #[test]
    fn public_resolution_reads_the_workspace_configuration() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\ntarget=\"configured-publicly\"\n")]);

        assert_eq!(Build::resolve(&root, None, &[]).target.as_deref(), Some("configured-publicly"));
    }

    #[test]
    fn the_cargo_configuration_supplies_the_target() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\ntarget = \"aarch64-apple-darwin\"\n")]);

        assert_eq!(resolve(&root, None, &[]).target.as_deref(), Some("aarch64-apple-darwin"));
    }

    /// A configured target is what the build uses only until something more specific says
    /// otherwise, which is the same order cargo itself applies.
    #[test]
    fn a_passthrough_target_outranks_the_configured_one() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\ntarget = \"aarch64-apple-darwin\"\n")]);

        assert_eq!(
            resolve(&root, None, &["--target", "wasm32-unknown-unknown"]).target.as_deref(),
            Some("wasm32-unknown-unknown")
        );
    }

    /// A configuration file above the workspace still applies, because cargo reads every one of
    /// them on the way up.
    #[test]
    fn a_configuration_file_above_the_workspace_is_read() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\ntarget = \"aarch64-apple-darwin\"\n")]);
        let inner = root.join("member");

        fs::create_dir_all(inner.as_std_path()).expect("a member directory");

        assert_eq!(
            Build::resolve_in(&inner, None, &[], &empty(&root)).target.as_deref(),
            Some("aarch64-apple-darwin")
        );
    }

    #[test]
    fn configuration_search_obeys_cargo_precedence_and_reads_every_level() {
        let (_directory, root) = tree(&[
            (
                ".cargo/config.toml",
                "[build]\ntarget = \"near\"\nrustflags = [\"--cfg\", \"near\"]\n",
            ),
            (
                ".cargo/config",
                "[build]\ntarget = \"legacy-near\"\nrustflags = [\"--cfg\", \"legacy-near\"]\n",
            ),
            ("member/.cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"member\"]\n"),
            ("member/.cargo/config", "[build]\nrustflags = [\"--cfg\", \"legacy-member\"]\n"),
        ]);
        let config = CargoConfig::load(&root.join("member"), &empty(&root));

        assert_eq!(config.string(&["build", "target"]), Some("near"));
        assert_eq!(config.strings(&["build", "rustflags"]), vec!["--cfg", "member", "--cfg", "near"]);
    }

    #[test]
    fn legacy_configuration_names_are_used_when_toml_is_absent() {
        let (_directory, root) = tree(&[
            ("workspace/.cargo/config", "[build]\ntarget=\"legacy-workspace\"\n"),
            ("home/config", "[build]\nrustflags=[\"legacy-home\"]\n"),
        ]);
        let workspace = root.join("workspace");
        let environment = Environment {
            cargo_home: Some(root.join("home")),
            ..Environment::default()
        };
        let config = CargoConfig::load(&workspace, &environment);

        assert_eq!(config.string(&["build", "target"]), Some("legacy-workspace"));
        assert_eq!(config.strings(&["build", "rustflags"]), vec!["legacy-home"]);
    }

    #[test]
    fn user_configuration_is_read_only_when_it_is_outside_the_workspace() {
        let (_directory, root) = tree(&[
            ("workspace/.cargo/config.toml", "[build]\nrustflags = [\"workspace\"]\n"),
            ("home/config.toml", "[build]\nrustflags = [\"home\"]\n"),
            ("home/config", "[build]\nrustflags = [\"legacy-home\"]\n"),
        ]);
        let workspace = root.join("workspace");
        let outside = Environment {
            cargo_home: Some(root.join("home")),
            ..Environment::default()
        };
        let inside = Environment {
            cargo_home: Some(workspace.join(".cargo")),
            ..Environment::default()
        };

        assert_eq!(
            CargoConfig::load(&workspace, &outside).strings(&["build", "rustflags"]),
            vec!["workspace", "home"]
        );
        assert_eq!(
            CargoConfig::load(&workspace, &inside).strings(&["build", "rustflags"]),
            vec!["workspace"]
        );
    }

    #[test]
    fn descendant_non_dot_cargo_home_reaches_resolution_and_record_settings_once() {
        let (_directory, root) = tree(&[("cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"from_descendant_home\"]\n")]);
        let environment = Environment {
            cargo_home: Some(root.join("cargo")),
            ..Environment::default()
        };

        let build = Build::resolve_in(&root, None, &[], &environment);
        let settings = Build::settings_in(&root, &environment);

        assert_eq!(build.cfgs, ["from_descendant_home"]);
        assert_eq!(
            settings.iter().filter(|part| part.starts_with("cargo-config:")).count(),
            1,
            "the configuration digest must contain the descendant home exactly once: {settings:?}"
        );
        assert!(
            settings.contains(&"build.rustflags=from_descendant_home".to_owned()),
            "{settings:?}"
        );
    }

    #[test]
    fn a_deeply_nested_cargo_cfg_key_is_left_undecided_before_parsing() {
        let predicate = format!("{}unix{}", "all(".repeat(100), ")".repeat(100));
        let config = format!("[target.'cfg({predicate})']\nrustflags = [\"--cfg\", \"deep_guard\"]\n");
        let (_directory, root) = tree(&[(".cargo/config.toml", &config)]);
        let build = resolve(&root, None, &[]);

        assert_eq!(build.cfgs, Vec::<String>::new());
        assert_eq!(build.undecided, ["deep_guard"]);
    }

    #[test]
    fn table_keys_are_sorted_and_deduplicated() {
        let (_directory, root) = tree(&[
            (".cargo/config.toml", "[target.z]\nrustflags=[]\n[target.a]\nrustflags=[]\n"),
            ("member/.cargo/config.toml", "[target.z]\nrustflags=[]\n[target.m]\nrustflags=[]\n"),
        ]);
        let config = CargoConfig::load(&root.join("member"), &empty(&root));

        assert_eq!(config.keys(&["target"]), vec!["a", "m", "z"]);
        assert!(config.keys(&["missing"]).is_empty());
    }

    /// No single set of predicates describes two targets, so a build of both is not evaluated at
    /// all rather than evaluated as whichever was written first.
    #[test]
    fn several_targets_are_reported_rather_than_picked_between() {
        let (_directory, root) = tree(&[]);
        let build = resolve(
            &root,
            None,
            &["--target", "wasm32-unknown-unknown", "--target", "aarch64-apple-darwin"],
        );

        assert!(build.several_targets);
        assert_eq!(build.target, None);

        // The same triple twice is still one target.
        let repeated = resolve(
            &root,
            None,
            &["--target", "wasm32-unknown-unknown", "--target", "wasm32-unknown-unknown"],
        );

        assert!(!repeated.several_targets);
        assert_eq!(repeated.target.as_deref(), Some("wasm32-unknown-unknown"));
    }

    #[test]
    fn target_sources_fall_back_in_order_and_are_normalised() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\ntarget = [\"z\", \"a\", \"z\"]\n")]);
        let config = CargoConfig::load(&root, &empty(&root));
        let environment = Environment {
            target: Some("environment".to_owned()),
            ..empty(&root)
        };

        assert_eq!(target(&[], &environment, &config), (Some("environment".to_owned()), false));
        assert_eq!(target(&[], &empty(&root), &config), (None, true));
    }

    #[test]
    fn configured_rustflags_carry_their_custom_predicates() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cfg\", \"loom\", \"-C\", \"target-feature=+avx2\", \"-C\", \"panic=abort\", \"-C\", \"opt-level=3\"]\n",
        )]);
        let build = resolve(&root, None, &[]);

        assert_eq!(build.cfgs, vec!["loom".to_owned()]);

        // `opt-level` decides no predicate, and a probe handed every flag would fail on the first
        // one the compiler refuses in this position.
        assert_eq!(build.codegen, vec!["target-feature=+avx2".to_owned(), "panic=abort".to_owned()]);
    }

    /// A target table's flags are what cargo uses when that target is the one being built, and
    /// they replace `build.rustflags` rather than adding to them.
    #[test]
    fn a_target_table_supplies_the_flags_for_its_own_triple() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cfg\", \"everywhere\"]\n\n[target.wasm32-unknown-unknown]\nrustflags = [\"--cfg\", \"woven\"]\n",
        )]);
        let build = resolve(&root, None, &["--target", "wasm32-unknown-unknown"]);

        assert_eq!(build.cfgs, vec!["woven".to_owned()]);
    }

    /// The variable spelling of a target table is read, and wins over the file that states it.
    ///
    /// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` is a level of cargo's rustflags precedence of its own: it
    /// applies where none of the global variables do, so a resolver that only modeled those would
    /// resolve `#[cfg(live)]` code as absent and drop every mutant in it from the population.
    #[test]
    fn the_variable_spelling_of_a_target_table_is_read_and_beats_the_file() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cfg\", \"everywhere\"]\n\n[target.wasm32-unknown-unknown]\nrustflags = [\"--cfg\", \"filed\"]\n",
        )]);
        let environment = Environment {
            target_rustflags: core::iter::once(("WASM32_UNKNOWN_UNKNOWN".to_owned(), "--cfg live".to_owned())).collect(),
            ..empty(&root)
        };
        let extra = ["--target".to_owned(), "wasm32-unknown-unknown".to_owned()];
        let build = Build::resolve_in(&root, None, &extra, &environment);

        assert_eq!(build.cfgs, vec!["live".to_owned()]);
    }

    /// The variable is read even when no configuration file names that triple at all, which is the
    /// case a reader that only ever walked the `[target.…]` tables could not reach.
    #[test]
    fn a_target_variable_applies_with_no_table_to_hang_it_on() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"everywhere\"]\n")]);
        let environment = Environment {
            target_rustflags: core::iter::once(("WASM32_UNKNOWN_UNKNOWN".to_owned(), "--cfg live".to_owned())).collect(),
            ..empty(&root)
        };
        let extra = ["--target".to_owned(), "wasm32-unknown-unknown".to_owned()];

        assert_eq!(Build::resolve_in(&root, None, &extra, &environment).cfgs, vec!["live".to_owned()]);

        // A variable naming some other triple is not in force and decides nothing.
        let elsewhere = Environment {
            target_rustflags: core::iter::once(("AARCH64_APPLE_DARWIN".to_owned(), "--cfg live".to_owned())).collect(),
            ..empty(&root)
        };

        assert_eq!(
            Build::resolve_in(&root, None, &extra, &elsewhere).cfgs,
            vec!["everywhere".to_owned()]
        );
    }

    /// The variable names its triple the way cargo spells a configuration key as a variable.
    #[test]
    fn a_triple_is_spelled_into_a_variable_name_the_way_cargo_spells_it() {
        assert_eq!(variable_component("x86_64-unknown-linux-gnu"), "X86_64_UNKNOWN_LINUX_GNU");
        assert_eq!(variable_component("thumbv8m.main-none-eabi"), "THUMBV8M_MAIN_NONE_EABI");
    }

    /// Every `CARGO_TARGET_…_RUSTFLAGS` in the environment is read, since the triple in force is
    /// not known until the configuration has been.
    #[test]
    fn the_target_specific_variables_are_read_by_enumeration() {
        let environment = Environment::read(
            |name| match name {
                "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS" => Ok("--cfg live".to_owned()),
                _other => Err(env::VarError::NotPresent),
            },
            [
                "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS".to_owned(),
                "CARGO_TARGET_DIR".to_owned(),
                "RUSTFLAGS".to_owned(),
            ],
        );

        assert_eq!(environment.target_flags("wasm32-unknown-unknown"), Some("--cfg live"));
        assert_eq!(environment.target_flags("x86_64-unknown-linux-gnu"), None);
        assert_eq!(environment.target_rustflags.len(), 1, "a variable that is not one was read");
    }

    /// A `cfg(…)` table is in force only when its own predicate holds, which is a question about
    /// the target being resolved. The names it would set are left unanswerable, so the code they
    /// gate stays in the population instead of being deleted on a guess.
    #[test]
    fn a_predicate_gated_table_leaves_its_names_unanswerable() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cfg\", \"everywhere\"]\n\n[target.'cfg(target_os = \"redox\")']\nrustflags = [\"--cfg\", \"sometimes\"]\n",
        )]);
        let build = resolve(&root, None, &[]);

        assert_eq!(build.cfgs, vec!["everywhere".to_owned()]);
        assert_eq!(build.undecided, vec!["sometimes".to_owned()]);
    }

    #[test]
    fn a_valued_predicate_in_a_gated_table_leaves_its_bare_name_unanswerable() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[target.'cfg(target_os = \"redox\")']\nrustflags = [\"--cfg\", \"flavor=\\\"strawberry\\\"\"]\n",
        )]);
        let build = resolve(&root, None, &[]);

        assert_eq!(build.undecided, vec!["flavor".to_owned()]);
    }

    /// Cargo joins the triple's table with every `cfg(…)` table whose predicate holds for the
    /// target it is building. Looking the triple up alone means the flags of a workspace that
    /// configures itself through `cfg(…)` — the common spelling for "every unix" — never reach the
    /// probe, so the predicates they decide are answered from the compiler's own defaults instead.
    #[test]
    fn a_matching_predicate_table_is_joined_with_the_triples_own() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[target.wasm32-unknown-unknown]\nrustflags = [\"--cfg\", \"triple\"]\n\n\
             [target.'cfg(target_family = \"wasm\")']\nrustflags = [\"--cfg\", \"family\"]\n\n\
             [target.'cfg(target_os = \"linux\")']\nrustflags = [\"--cfg\", \"elsewhere\"]\n",
        )]);
        let build = resolve(&root, None, &["--target", "wasm32-unknown-unknown"]);

        assert!(build.cfgs.contains(&"triple".to_owned()), "{:?}", build.cfgs);
        assert!(build.cfgs.contains(&"family".to_owned()), "{:?}", build.cfgs);

        // The table whose predicate is false for this target decides nothing, and is not in force
        // either, so it stays unanswerable rather than becoming a name the build passes.
        assert!(!build.cfgs.contains(&"elsewhere".to_owned()), "{:?}", build.cfgs);
        assert_eq!(build.undecided, vec!["elsewhere".to_owned()]);

        // A table this run does read is decided, so nothing about it is left hanging.
        assert!(!build.undecided.contains(&"family".to_owned()));
    }

    /// `-C debug-assertions`, `-C panic` and `-C target-feature` each decide a predicate exactly as
    /// a `--cfg` does. A table that sets one and is not the table this build reads leaves that
    /// predicate answered by the profile chain or by the compiler's own default, which is an answer
    /// about a different compilation — so the name is unanswerable instead.
    #[test]
    fn a_table_this_build_does_not_read_leaves_its_codegen_predicates_unanswerable() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[target.wasm32-unknown-unknown]\nrustflags = [\"-C\", \"debug-assertions=off\", \"-C\", \"panic=abort\", \"-C\", \"target-feature=+simd128\"]\n",
        )]);
        let build = resolve(&root, None, &[]);

        assert_eq!(
            build.undecided,
            vec!["debug_assertions".to_owned(), "panic".to_owned(), "target_feature".to_owned()]
        );
    }

    /// `CARGO_BUILD_RUSTFLAGS` is the environment spelling of `build.rustflags`, so it belongs in
    /// that slot rather than above the target tables. Ranking it higher means probing with one set
    /// of flags while cargo builds with another, and whichever set carries a `--cfg` then decides
    /// predicates the other contradicts.
    #[test]
    fn the_build_rustflags_variable_ranks_below_the_target_tables() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[target.wasm32-unknown-unknown]\nrustflags = [\"--cfg\", \"tabled\"]\n",
        )]);
        let environment = Environment {
            target: Some("wasm32-unknown-unknown".to_owned()),
            build_rustflags: Some("--cfg from_the_variable".to_owned()),
            ..empty(&root)
        };
        let build = Build::resolve_in(&root, None, &[], &environment);

        assert_eq!(build.cfgs, vec!["tabled".to_owned()]);
    }

    /// The slot it does hold is the one `build.rustflags` holds, and it outranks the file there:
    /// an environment variable is the more specific statement of the same setting.
    #[test]
    fn the_build_rustflags_variable_outranks_the_configured_build_rustflags() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"configured\"]\n")]);
        let environment = Environment {
            build_rustflags: Some("--cfg from_the_variable".to_owned()),
            ..empty(&root)
        };
        let build = Build::resolve_in(&root, None, &[], &environment);

        assert_eq!(build.cfgs, vec!["from_the_variable".to_owned()]);

        // And `RUSTFLAGS` still outranks it, since that is a different setting and a higher slot.
        let outranked = Build::resolve_in(
            &root,
            None,
            &[],
            &Environment {
                rustflags: Some("--cfg plain".to_owned()),
                build_rustflags: Some("--cfg from_the_variable".to_owned()),
                ..empty(&root)
            },
        );

        assert_eq!(outranked.cfgs, vec!["plain".to_owned()]);
    }

    #[test]
    fn the_environment_outranks_the_configuration_files() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"configured\"]\n")]);
        let environment = Environment {
            rustflags: Some("--cfg loom -C debug-assertions=off".to_owned()),
            ..empty(&root)
        };
        let build = Build::resolve_in(&root, None, &[], &environment);

        assert_eq!(build.cfgs, vec!["loom".to_owned()]);
        assert_eq!(build.debug_assertions, Some(false), "the flags say so outright");
    }

    #[test]
    fn cargo_build_rustflags_are_used_when_the_other_variables_are_absent() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"configured\"]\n")]);
        let environment = Environment {
            build_rustflags: Some("--cfg environment".to_owned()),
            ..empty(&root)
        };

        assert_eq!(Build::resolve_in(&root, None, &[], &environment).cfgs, vec!["environment"]);
    }

    #[test]
    fn target_table_uncertainty_is_sorted_deduplicated_and_excludes_the_chosen_flags() {
        let (_directory, root) = tree(&[(
            ".cargo/config.toml",
            "[target.chosen]\nrustflags=[\"--cfg\", \"same\"]\n\
             [target.z]\nrustflags=[\"--cfg\", \"maybe\", \"--cfg\", \"maybe\"]\n\
             [target.a]\nrustflags=[\"--cfg\", \"alpha\"]\n",
        )]);
        let build = resolve(&root, None, &["--target", "chosen"]);

        assert_eq!(build.cfgs, vec!["same"]);
        assert_eq!(build.undecided, vec!["alpha", "maybe"]);
    }

    /// The encoded variable is what cargo writes when a flag contains a space, and it outranks the
    /// space-separated one; splitting it on spaces would tear one flag into two.
    #[test]
    fn encoded_flags_are_split_on_the_unit_separator() {
        let (_directory, root) = tree(&[]);
        let environment = Environment {
            encoded_rustflags: Some("--cfg\u{1f}flavor=\"two words\"\u{1f}".to_owned()),
            rustflags: Some("--cfg ignored".to_owned()),
            ..empty(&root)
        };
        let build = Build::resolve_in(&root, None, &[], &environment);

        assert_eq!(build.cfgs, vec!["flavor=\"two words\"".to_owned()]);
    }

    #[test]
    fn flag_value_parsing_requires_exact_spellings_and_keeps_order() {
        let args = [
            "--cfg",
            "one",
            "--cfg=two",
            "--cfg-dir=wrong",
            "--cfg",
            "three",
            "--cfg",
            "--cfg=four",
        ]
        .map(ToOwned::to_owned);

        assert_eq!(valued(&args, "--cfg"), vec!["one", "two", "three", "--cfg=four"]);
    }

    #[test]
    fn codegen_options_accept_every_rustc_spelling_but_only_cfg_relevant_values() {
        let flags = [
            "-C",
            "target-feature=+avx2",
            "--codegen",
            "panic=abort",
            "-Cdebug-assertions",
            "--codegen=target-feature=-sse",
            "-C",
            "opt-level=3",
        ]
        .map(ToOwned::to_owned);

        assert_eq!(codegen(&flags), vec!["target-feature=+avx2", "panic=abort", "target-feature=-sse"]);
        assert_eq!(assertions(&flags), Some(true));
    }

    #[test]
    fn debug_assertion_flags_use_the_last_recognised_exact_value() {
        for enabled in ["y", "yes", "on", "true"] {
            assert_eq!(assertions(&["-C".to_owned(), format!("debug-assertions={enabled}")]), Some(true));
        }
        for disabled in ["n", "no", "off", "false"] {
            assert_eq!(
                assertions(&["--codegen".to_owned(), format!("debug-assertions={disabled}")]),
                Some(false)
            );
        }
        assert_eq!(
            assertions(&["-Cdebug-assertions=true", "-Cdebug-assertions=perhaps", "-Cdebug-assertions=false"].map(ToOwned::to_owned)),
            Some(false)
        );
        assert_eq!(assertions(&["-Cdebug-assertions=perhaps".to_owned()]), None);
        assert_eq!(assertions(&["-Cdebug-assertions-extra=true".to_owned()]), None);
        assert_eq!(assertions(&["-C".to_owned()]), None);
        assert!(options(&["-C".to_owned()]).next().is_none());
    }

    #[test]
    fn release_shorthand_must_match_exactly() {
        assert_eq!(named_profile(&["-r".to_owned()]).as_deref(), Some("release"));
        assert_eq!(named_profile(&["--release".to_owned()]).as_deref(), Some("release"));
        assert_eq!(named_profile(&["-really-not-release".to_owned()]), None);
    }

    #[test]
    fn host_output_parsing_requires_the_host_field() {
        assert_eq!(
            parse_host("rustc 1.0\nhost:   x86_64-example\nrelease: 1.0\n").as_deref(),
            Some("x86_64-example")
        );
        assert_eq!(parse_host("rustc 1.0\ntarget: x86_64-example\n"), None);
    }

    #[test]
    fn host_lookup_is_skipped_without_target_tables_and_answers_with_them() {
        let with_target = CargoConfig {
            tables: vec![toml::from_str("[target.host]\nrustflags=[]\n").expect("the fixture parses")],
            sources: Vec::new(),
        };
        let host = host_triple(&with_target, false).expect("the compiler that built the tests reports its host");

        assert!(!host.is_empty());
        assert_ne!(host, "xyzzy");
        assert_eq!(host_triple(&CargoConfig::default(), false), None);
    }

    /// A run that names no profile builds with `cargo test`, whose profile inherits `dev`.
    #[test]
    fn the_default_profile_has_debug_assertions_on() {
        let (_directory, root) = tree(&[]);

        assert_eq!(resolve(&root, None, &[]).debug_assertions, Some(true));
    }

    #[test]
    fn the_release_profile_has_them_off() {
        let (_directory, root) = tree(&[]);

        assert_eq!(resolve(&root, Some("release"), &[]).debug_assertions, Some(false));
        assert_eq!(resolve(&root, None, &["--release"]).debug_assertions, Some(false));
        assert_eq!(resolve(&root, None, &["--profile", "bench"]).debug_assertions, Some(false));
    }

    /// A passthrough `--profile` is the run's own last word, above whatever was configured.
    #[test]
    fn a_passthrough_profile_outranks_the_configured_one() {
        let (_directory, root) = tree(&[]);

        assert_eq!(resolve(&root, Some("dev"), &["--profile=release"]).debug_assertions, Some(false));
    }

    #[test]
    fn a_custom_profile_follows_what_it_inherits() {
        let (_directory, root) = tree(&[(
            "Cargo.toml",
            "[workspace]\n\n[profile.mutants]\ninherits = \"release\"\n\n[profile.loud]\ninherits = \"dev\"\n",
        )]);

        assert_eq!(resolve(&root, Some("mutants"), &[]).debug_assertions, Some(false));
        assert_eq!(resolve(&root, Some("loud"), &[]).debug_assertions, Some(true));
    }

    #[test]
    fn a_profile_that_switches_them_on_is_believed_over_what_it_inherits() {
        let (_directory, root) = tree(&[(
            "Cargo.toml",
            "[workspace]\n\n[profile.mutants]\ninherits = \"release\"\ndebug-assertions = true\n",
        )]);

        assert_eq!(resolve(&root, Some("mutants"), &[]).debug_assertions, Some(true));
    }

    #[test]
    fn the_underscore_spelling_of_debug_assertions_is_accepted() {
        let (_directory, root) = tree(&[(
            "Cargo.toml",
            "[workspace]\n\n[profile.mutants]\ninherits = \"release\"\ndebug_assertions = true\n",
        )]);

        assert_eq!(resolve(&root, Some("mutants"), &[]).debug_assertions, Some(true));
    }

    /// The cargo configuration can override the manifest's profile tables, so it is asked first.
    #[test]
    fn a_configured_profile_overrides_the_manifest() {
        let (_directory, root) = tree(&[
            ("Cargo.toml", "[workspace]\n\n[profile.release]\ndebug-assertions = true\n"),
            (".cargo/config.toml", "[profile.release]\ndebug-assertions = false\n"),
        ]);

        assert_eq!(resolve(&root, Some("release"), &[]).debug_assertions, Some(false));
    }

    /// A profile nothing describes is unanswerable rather than assumed, because assuming either
    /// answer deletes one half of every `#[cfg(debug_assertions)]` from the population.
    #[test]
    fn a_profile_that_cannot_be_followed_is_left_unanswered() {
        let (_directory, root) = tree(&[("Cargo.toml", "[workspace]\n")]);

        assert_eq!(resolve(&root, Some("nowhere"), &[]).debug_assertions, None);
    }

    #[test]
    fn a_missing_parent_is_not_replaced_with_the_empty_profile() {
        let (_directory, root) = tree(&[(
            "Cargo.toml",
            "[workspace]\n\n[profile.custom]\n\n[profile.\"\"]\ndebug-assertions = true\n",
        )]);

        assert_eq!(resolve(&root, Some("custom"), &[]).debug_assertions, None);
    }

    /// A profile that inherits from itself is malformed, and must not spin.
    #[test]
    fn a_cyclic_profile_chain_terminates() {
        let (_directory, root) = tree(&[(
            "Cargo.toml",
            "[workspace]\n\n[profile.a]\ninherits = \"b\"\n\n[profile.b]\ninherits = \"a\"\n",
        )]);

        assert_eq!(resolve(&root, Some("a"), &[]).debug_assertions, None);
    }

    fn profile_chain(length: usize, switched_at: usize) -> String {
        use core::fmt::Write as _;
        let mut manifest = String::from("[workspace]\n");
        for index in 0..=length {
            let _ = writeln!(manifest, "\n[profile.p{index}]");
            if index < length {
                let _ = writeln!(manifest, "inherits = \"p{}\"", index + 1);
            }
            if index == switched_at {
                manifest.push_str("debug-assertions = true\n");
            }
        }
        manifest
    }

    #[test]
    fn profile_chain_iteration_starts_at_zero_and_ends_before_the_limit() {
        for (switched_at, expected) in [(0, Some(true)), (PROFILE_DEPTH - 1, Some(true)), (PROFILE_DEPTH, None)] {
            let manifest = profile_chain(PROFILE_DEPTH, switched_at);
            let (_directory, root) = tree(&[("Cargo.toml", &manifest)]);

            assert_eq!(resolve(&root, Some("p0"), &[]).debug_assertions, expected);
        }
    }

    /// A malformed configuration file says nothing rather than stopping discovery, which is the
    /// same direction every other uncertainty here resolves in.
    #[test]
    fn an_unparsable_configuration_file_is_ignored() {
        let (_directory, root) = tree(&[(".cargo/config.toml", "[build\ntarget =\n")]);

        assert_eq!(resolve(&root, None, &[]).target, None);
    }

    #[test]
    fn the_probe_command_line_carries_every_setting() {
        let build = Build {
            target: Some("wasm32-unknown-unknown".to_owned()),
            cfgs: vec!["loom".to_owned()],
            codegen: vec!["target-feature=+atomics".to_owned()],
            debug_assertions: Some(false),
            undecided: Vec::new(),
            several_targets: false,
        };

        assert_eq!(
            build.probe_args(),
            vec![
                "--print",
                "cfg",
                "--target",
                "wasm32-unknown-unknown",
                "-C",
                "debug-assertions=off",
                "-C",
                "target-feature=+atomics",
                "--cfg",
                "loom",
            ]
        );

        // An unanswered profile leaves the flag off entirely, so the compiler answers with its own
        // default and the name is marked unanswerable by the set that is built from it.
        assert!(
            !Build::default()
                .probe_args()
                .iter()
                .any(|argument| argument.starts_with("debug-assertions"))
        );
    }
}
