// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Keeps the generated reference tables in `README.md` honest.
//!
//! The mutator catalog is the tool's published vocabulary: the same names appear on `--mutators`, in
//! every suppression directive, in the report, in SARIF rule identifiers and in configuration. A
//! reference that has drifted is worse than none, because a reader who copies a name out of it
//! gets a usage error with nothing to suggest the document was at fault.
//!
//! Run with `GAMMA_BLESS_DOCS=1` to rewrite the files instead of failing.

use std::collections::BTreeSet;
use std::fs;

use camino::Utf8PathBuf;
use cargo_gamma_lib::internals::config::Config;
use cargo_gamma_lib::internals::docs;

/// The documentation files carrying generated blocks, relative to the cargo-gamma crate.
const FILES: &[&str] = &["README.md", "docs/CMDLINE.md", "docs/MUTATORS.md"];

/// Returns the cargo-gamma crate directory.
fn root_dir() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cargo-gamma")
}

/// Returns the ox-tools workspace root.
fn workspace_dir() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Rewrites every generated block in `text`, returning the result.
///
/// A block is delimited rather than owning the file so that the prose explaining what a family is
/// *for* can live beside the table listing what it contains. A reference that is only a table says
/// what exists without saying when to reach for it.
fn regenerate(text: &str, path: &str) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some((before, after_marker)) = rest.split_once(docs::BEGIN) {
        out.push_str(before);

        assert!(
            after_marker.contains(" -->"),
            "{path}: a `{}` marker is never terminated",
            docs::BEGIN
        );
        let (name, after_name) = after_marker.split_once(" -->").expect("the marker terminator is present");

        assert!(docs::block(name).is_some(), "{path}: there is no generated block named `{name}`");
        let body = docs::block(name).expect("the block name is known");

        assert!(
            after_name.contains(docs::END),
            "{path}: block `{name}` is never closed with `{}`",
            docs::END
        );
        let (_, after_end) = after_name.split_once(docs::END).expect("the closing marker is present");

        out.push_str(docs::BEGIN);
        out.push_str(name);
        out.push_str(" -->\n\n");
        out.push_str(&body);
        out.push_str("\n\n");
        out.push_str(docs::END);

        rest = after_end;
    }

    out.push_str(rest);
    out
}

#[test]
fn the_generated_reference_tables_match_the_registry() {
    let dir = root_dir();
    let blessing = std::env::var_os("GAMMA_BLESS_DOCS").is_some();
    let mut stale = Vec::new();

    for name in FILES {
        let path = dir.join(name);
        let text = fs::read_to_string(path.as_std_path()).unwrap_or_else(|_| panic!("could not read {path}"));
        let expected = regenerate(&text, name);

        if text == expected {
            continue;
        }

        if blessing {
            fs::write(path.as_std_path(), &expected).unwrap_or_else(|_| panic!("could not write {path}"));
        } else {
            stale.push((*name).to_owned());
        }
    }

    assert!(
        stale.is_empty(),
        "{} is out of date with the mutator registry. Run `GAMMA_BLESS_DOCS=1 cargo test --all-features --test docs` to regenerate.",
        stale.join(", ")
    );
}

#[test]
fn every_documentation_file_the_readme_points_at_exists() {
    // A reference kept beside the README is only useful if the link works. A broken one sends a
    // reader looking for the design notes to a 404 on the crate's own front page. The links are
    // read out of the README rather than listed here, because a list would have to be maintained
    // in step with the prose and the failure it exists to catch is exactly somebody forgetting to.
    let root = root_dir();
    let readme = fs::read_to_string(root.join("README.md").as_std_path()).expect("could not read README.md");

    let mut checked = 0;

    for link in linked_documents(&readme) {
        assert!(
            root.join(&link).as_std_path().exists(),
            "{link} is linked from README.md but missing"
        );
        checked += 1;
    }

    assert!(checked > 0, "no documentation links were found, so this test proves nothing");
}

#[test]
fn the_optimized_campaign_profile_is_valid_and_preserves_development_checks() {
    let readme = fs::read_to_string(root_dir().join("README.md").as_std_path()).expect("could not read README.md");
    let section = readme
        .split_once("### Optimizing compute-heavy suites")
        .expect("README.md has no optimized-profile guidance")
        .1
        .split_once("\n### ")
        .map_or_else(|| readme.as_str(), |(section, _rest)| section);
    let example = section
        .split_once("```toml\n")
        .expect("the optimized-profile guidance has no TOML example")
        .1
        .split_once("\n```")
        .expect("the optimized-profile TOML fence is never closed")
        .0;
    let manifest: toml::Value = toml::from_str(example).expect("the optimized-profile example is not valid TOML");
    let profile = &manifest["profile"]["gamma"];

    assert_eq!(profile["inherits"].as_str(), Some("dev"));
    assert_eq!(profile["opt-level"].as_integer(), Some(2));
    assert_eq!(profile["debug-assertions"].as_bool(), Some(true));
    assert_eq!(profile["overflow-checks"].as_bool(), Some(true));
    let prose = section.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(prose.contains("cargo gamma run --profile gamma"));
    assert!(prose.contains("not directly comparable"));
    assert!(prose.contains("invalidates unviability reuse"));
}

/// Every `docs/*.md` path the README mentions.
fn linked_documents(readme: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();

    for tail in readme.split("docs/").skip(1) {
        // A link ends at the first character a file name cannot contain, which is the closing
        // bracket in Markdown and whitespace in prose.
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || "_-.".contains(*c))
            .collect();

        let Some((stem, _)) = name.split_once(".md") else {
            continue;
        };

        let link = format!("docs/{stem}.md");

        if !found.contains(&link) {
            found.push(link);
        }
    }

    found
}

/// Kebab-cases a Rust field name the way serde's `rename_all` does.
fn kebab(name: &str) -> String {
    name.replace('_', "-")
}

/// Returns the serialized field names declared in one struct in `config.rs`.
///
/// The struct is found by name and read to its closing brace. Parsing the source is crude, but it
/// is the only source of truth available: serde's `deny_unknown_fields` is generated at compile
/// time and leaves no runtime list of accepted keys behind to compare against.
fn fields(source: &str, name: &str) -> Vec<String> {
    let public = format!("pub struct {name} {{");
    let private = format!("struct {name} {{");
    let header = if source.contains(&public) { public } else { private };

    assert!(source.contains(&header), "config.rs declares no struct named {name}");
    let (_, body) = source.split_once(header.as_str()).expect("the struct header is present");

    body.lines()
        .take_while(|line| !line.starts_with('}'))
        .filter_map(|line| line.strip_prefix("    "))
        .map(|line| line.strip_prefix("pub ").unwrap_or(line))
        .filter_map(|rest| rest.split_once(':').map(|(field, _type)| field))
        .filter(|field| field.chars().all(|character| character.is_ascii_alphanumeric() || character == '_'))
        .map(kebab)
        .collect()
}

#[test]
fn every_configuration_key_is_documented() {
    // An undocumented key is a key nobody can use: `deny_unknown_fields` means a reader cannot
    // discover one by guessing, and there is no `--help` for a configuration file. Adding a field
    // without adding a row is therefore a silent feature.
    //
    // Both files are checked because they serve different readers and neither subsumes the other:
    // `CONFIG.md` is what someone consults to find out whether a setting exists, and `gamma.toml`
    // is what they copy to start from. A key present in only one of them is invisible to half the
    // people looking for it.
    let source = include_str!("../src/config.rs");
    let keys: Vec<String> = ["Config", "Shard"].iter().flat_map(|name| fields(source, name)).collect();

    for doc in ["docs/CONFIG.md", "docs/gamma.toml"] {
        let path = root_dir().join(doc);
        let text = fs::read_to_string(path.as_std_path()).unwrap_or_else(|_| panic!("could not read {doc}"));

        for key in &keys {
            // A nested table is documented by its heading rather than as a row of its parent's
            // table, so `[shard]` counts as documenting the `shard` key. The other two forms are
            // how a key appears in a TOML snippet and in the commented example file, both of which
            // document it at least as well as prose does.
            assert!(
                text.contains(&format!("`{key}`"))
                    || text.contains(&format!("[{key}]"))
                    || text.contains(&format!("\n{key} ="))
                    || text.contains(&format!("# {key} =")),
                "{doc} never mentions the `{key}` key"
            );
        }
    }
}

/// Checks that the example configuration file is one a reader can actually copy.
///
/// The file's whole promise is that copying it to `gamma.toml` changes nothing until you
/// uncomment something. That promise has two halves, and both are easy to break by hand: an
/// uncommented key would silently impose a setting on everyone who copied the file, and a value
/// that is merely illustrative rather than valid would make the copy fail to parse. Parsing it and
/// comparing against the defaults checks both at once.
#[test]
fn the_example_configuration_is_inert_and_parses() {
    let path = root_dir().join("docs/gamma.toml");
    let text = fs::read_to_string(path.as_std_path()).expect("could not read docs/gamma.toml");

    let config = Config::parse(&text).expect("docs/gamma.toml is not a valid configuration file");

    // Compared through `Debug` rather than `PartialEq` so the check costs the configuration schema
    // no derive it would not otherwise carry.
    assert_eq!(
        format!("{config:?}"),
        format!("{:?}", Config::default()),
        "docs/gamma.toml sets a key rather than only documenting it, so copying it would change behavior"
    );
}

/// Keeps the configuration used to mutate this workspace loadable by cargo-gamma itself.
///
/// Loaded the way a run loads it — [`Config::load`] against the workspace directory — rather than
/// by reading the file and parsing the text. The two are not the same check: `load` is what resolves
/// the file name, and it answers a missing file with the defaults, so a test that reads the text
/// itself would still pass if the file were renamed out from under the tool while every real run
/// silently lost every setting in it.
///
/// The settings are asserted rather than merely accepted, because "parses" is satisfied by an empty
/// file. This workspace's own mutation runs depend on these exclusions being in force: without them
/// every `Debug` and redaction implementation contributes mutants that no test can meaningfully
/// convict, and the score they drag down is the one the gate is set against.
#[test]
fn the_workspace_configuration_loads_with_its_settings_intact() {
    let config = Config::load(&workspace_dir()).expect("the workspace gamma.toml is not a valid configuration file");

    assert!(
        config.exclude_files.iter().any(|pattern| pattern == "crates/automation/**"),
        "the workspace configuration no longer excludes the automation crate: {:?}",
        config.exclude_files
    );

    assert_eq!(
        config.exclude_trait_impls,
        ["Debug"],
        "the diagnostic-output trait exclusion changed"
    );
}

/// Pins one row of each generated block against an expectation nothing generated.
///
/// The blessing test above compares the README to the generator, and the blessing command rewrites
/// the README from that same generator. Together they catch drift and nothing else: a generator
/// that renders a name wrongly, drops the alias column, or stops escaping a pipe is agreed with by
/// both sides of the comparison, blessed into the published reference, and the suite stays green.
///
/// These expectations were typed by hand from what the tables are supposed to say, so a rendering
/// change has to be defended here rather than blessed. They are deliberately three rows and not
/// three tables — a full snapshot would be regenerated by whoever the failure inconvenienced, which
/// puts it right back where it started.
#[test]
fn one_row_of_every_generated_block_is_pinned_to_a_hand_written_expectation() {
    let mutators = docs::block("mutators").expect("the mutator block exists");
    let presets = docs::block("presets").expect("the preset block exists");
    let families = docs::block("families").expect("the family block exists");

    assert!(
        mutators.contains("| `relational.lt_to_le` | replace < with <= | `ROR` | yes |"),
        "the mutator row is not rendered as expected:\n{mutators}"
    );
    assert!(
        presets.contains("| `@boundary` | relational and boundary conditions | `relational`, `range` |"),
        "the preset row is not rendered as expected:\n{presets}"
    );
    assert!(
        families.contains("| [`logical`](#logical) | 2 | Is this `&&` really an `&&`? |"),
        "the family row is not rendered as expected:\n{families}"
    );

    // The family heading is what the README's own anchors point at, so it is part of the contract
    // and not merely how the table happens to look today.
    assert!(mutators.contains("#### `relational`\n"), "{mutators}");
}

/// Every documented option states which category it belongs to, rather than inheriting one.
///
/// `next_help_heading` applies to every argument declared after it, including the ones belonging
/// to a *different* struct that happens to be flattened next. An option that never names its own
/// heading is therefore not uncategorized — it is silently filed under whichever group came
/// before it, which reads as deliberate and is how `--min-score` and `--artifact-dir` came to be
/// listed under "Building". Requiring the heading to be written down is the only way to tell the
/// two apart, because by the time clap reports it the inherited and the explicit look identical.
#[test]
fn every_option_names_its_own_help_heading() {
    // The structs whose fields become options. `Cli` itself is excluded: its two globals carry
    // headings, but its `command` field is the subcommand enum rather than an option.
    const STRUCTS: &[&str] = &[
        "MergeArgs",
        "SuppressArgs",
        "UnsuppressArgs",
        "SelectArgs",
        "FeatureArgs",
        "ConfigArgs",
        "BuildLimitArgs",
        "MeasureArgs",
        "RunArgs",
        "CompletionsArgs",
        "ListArgs",
        "ExplainArgs",
    ];

    let source = include_str!("../src/commands/cli.rs");

    let mut checked = 0;

    for name in STRUCTS {
        let header = format!("pub struct {name} {{");
        let (_, body) = source
            .split_once(header.as_str())
            .unwrap_or_else(|| panic!("cli.rs declares no struct named {name}"));
        let body: String = body
            .lines()
            .take_while(|line| !line.starts_with('}'))
            .collect::<Vec<_>>()
            .join("\n");

        // A struct-wide heading covers every field in it, so the per-field rule does not apply.
        if struct_has_group(source, name) {
            continue;
        }

        for field in body.split("    pub ").skip(1) {
            let field_name = field.split(':').next().unwrap_or_default();

            // A flattened struct contributes its own fields, which are checked on their own turn.
            let declaration = field.lines().next().unwrap_or_default();

            if declaration.contains("Args") || field_name == "command" {
                continue;
            }

            // A positional is rendered under "Arguments" wherever it is declared, so it cannot be
            // filed under a neighbouring option's category and needs no heading of its own.
            let attributes = body.split(&format!("pub {field_name}:")).next().unwrap_or_default();
            let last = attributes.rfind("#[arg(").unwrap_or(0);
            let attributes = attributes.get(last..).unwrap_or_default();

            if !attributes.contains("long") {
                continue;
            }

            assert!(
                attributes.contains("help_heading"),
                "{name}::{field_name} inherits its help heading instead of naming one"
            );

            checked += 1;
        }
    }

    assert!(checked > 20, "too few options were checked to prove anything: {checked}");
}

/// Whether a struct carries a struct-wide `next_help_heading`.
fn struct_has_group(source: &str, name: &str) -> bool {
    let header = format!("pub struct {name} {{");
    let before = source.split(header.as_str()).next().unwrap_or_default();
    let start = before.rfind("#[derive(").unwrap_or(0);

    before.get(start..).unwrap_or_default().contains("next_help_heading")
}

/// Every crate that can forbid `unsafe` does, so that the two that cannot are visible.
///
/// The raw platform calls the tool has to make — killing a process subtree, bounding what it
/// allocates — live in `cargo-gamma-unsafe`, and the guard runtime vendored into the tree under
/// test lives in `cargo-gamma-rt`, which can depend on nothing and so cannot delegate to the first.
/// Everything else carries `#![forbid(unsafe_code)]`.
///
/// The point of the rule is not that `unsafe` is forbidden — it plainly is not, twice — but that
/// where it may appear is a decision rather than an accident. A new crate added without thinking
/// about it inherits nothing and could quietly grow a raw call; this test is what makes that a
/// failure at the moment it is introduced rather than a discovery during the next audit.
///
/// `forbid` rather than `deny` deliberately: `deny` can be turned off again by an inner `allow`
/// in the file that wants it, which is exactly the move this is here to prevent.
#[test]
fn every_crate_that_can_forbid_unsafe_code_does() {
    /// The crates that may contain `unsafe`, and the reason each may.
    const ALLOWED: &[&str] = &[
        // The whole purpose of the crate: it exists so the others do not have to.
        "cargo-gamma-unsafe",
        // Injected into the dependency graph of the crate under test, so it carries zero
        // dependencies and cannot reach `cargo-gamma-unsafe` — or anything else.
        "cargo-gamma-rt",
    ];

    let crates = workspace_dir().join("crates");
    let mut unguarded = Vec::new();

    for entry in fs::read_dir(crates.as_std_path())
        .expect("the workspace has a crates directory")
        .flatten()
    {
        let path = Utf8PathBuf::from_path_buf(entry.path()).expect("the repository has no non-UTF-8 paths");
        let name = path.file_name().unwrap_or_default().to_owned();

        if !path.is_dir() || !name.starts_with("cargo-gamma") || ALLOWED.contains(&name.as_str()) {
            continue;
        }

        // The attribute has to be somewhere in the crate root, which is `lib.rs` for a library and
        // `main.rs` for the binary. Nowhere else can carry an inner attribute for the whole crate.
        let guarded = ["src/lib.rs", "src/main.rs"]
            .iter()
            .any(|root| fs::read_to_string(path.join(root).as_std_path()).is_ok_and(|text| text.contains("#![forbid(\n    unsafe_code,")));

        if !guarded {
            unguarded.push(name);
        }
    }

    assert!(
        unguarded.is_empty(),
        "these crates neither forbid `unsafe_code` nor are listed as exempt: {}",
        unguarded.join(", ")
    );
}

/// The functions allowed to write the process environment, and nothing else is.
const ENVIRONMENT_WRITERS: [(&str, &str); 0] = [];

/// Nothing writes the process environment through Rust's standard-library API.
///
/// The suite runs tests on threads, and `setenv` racing `getenv` is a data race in `libc` rather
/// than merely a confusing value — which is why edition 2024 made `set_var` unsafe. A mutex only
/// excludes the threads that take it, and the readers are everywhere: `env::var` inside production
/// code that any test calls looks nothing like touching shared mutable state, and is. So the rule
/// is not "take a lock", it is **do not write the environment at all**. A value one test needs
/// reaches the code under test on a child process's `Command`, never by mutating this process.
///
/// The rule is enforced twice over. `set_var` is an unsafe call, and every crate in this workspace
/// except `cargo-gamma-unsafe` carries `#![forbid(unsafe_code)]` — so a write anywhere else does
/// not merely break this test, it does not compile. This test additionally guards the exempt crate
/// and fails if a standard-library environment write appears anywhere.
///
/// Enforced here rather than written down and hoped for, because the discipline held right up
/// until it did not: the harness width was once published with `set_var`, which was sound for the
/// real binary and unsound for the forty end-to-end tests that call `run` in-process at the same
/// time. A value that has to reach a child process belongs on that child's `Command`, which is
/// where the loader path, the stack floor and the harness width all now live — and where the few
/// tests that need cargo to read `RUSTFLAGS` or `CARGO` now put them too, by re-executing a child.
#[test]
fn nothing_writes_the_process_environment() {
    let root = workspace_dir();
    let mut offenders = Vec::new();
    let mut allowances_used = BTreeSet::new();

    for path in gamma_crates() {
        for entry in walk(&path.join("src")).into_iter().chain(walk_optional(&path.join("tests"))) {
            let relative = entry
                .strip_prefix(&root)
                .map_or_else(|_| entry.to_string(), ToString::to_string)
                .replace('\\', "/");
            let text = fs::read_to_string(entry.as_std_path()).unwrap_or_else(|_| panic!("could not read {entry}"));
            let mut enclosing = "";

            for (number, line) in text.lines().enumerate() {
                if let Some(name) = function_name(line) {
                    enclosing = name;
                }

                // A mention in prose is what every doc comment about this rule is, including this
                // test's own. Only a call counts.
                // Assembled rather than written out, so that this line does not match itself.
                if !["set_var(", "remove_var("]
                    .iter()
                    .any(|call| line.contains(&format!("env::{call}")))
                {
                    continue;
                }

                let allowed = ENVIRONMENT_WRITERS
                    .iter()
                    .position(|(file, function)| *file == relative && *function == enclosing);

                if let Some(index) = allowed {
                    let _new = allowances_used.insert(index);
                } else {
                    offenders.push(format!("{relative}:{} in `{enclosing}`", number + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these write the process environment, which races every other thread's read: {}",
        offenders.join(", ")
    );

    let stale: Vec<String> = ENVIRONMENT_WRITERS
        .iter()
        .enumerate()
        .filter(|(index, _allowance)| !allowances_used.contains(index))
        .map(|(_index, (file, function))| format!("{file} `{function}`"))
        .collect();

    assert!(
        stale.is_empty(),
        "these allowances match nothing and have to be deleted rather than left to cover the next write: {}",
        stale.join(", ")
    );
}

/// The name of the function this line declares, when it declares one.
fn function_name(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("pub ").unwrap_or(rest);
    let rest = rest
        .strip_prefix("pub(crate) ")
        .or_else(|| rest.strip_prefix("pub(super) "))
        .unwrap_or(rest);
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let rest = rest.strip_prefix("const ").unwrap_or(rest);
    let rest = rest.strip_prefix("unsafe ").unwrap_or(rest);
    let rest = rest.strip_prefix("extern \"C\" ").unwrap_or(rest);

    let name = rest.strip_prefix("fn ")?.split(['(', '<', ' ']).next()?;

    (!name.is_empty()).then_some(name)
}

/// Every `.rs` file under `directory`, recursively, failing if it is not there to walk.
///
/// A missing directory is a mistake in the caller rather than a codebase with nothing in it, and
/// returning an empty list for one makes every policy check built on this pass by finding nothing.
/// Use [`walk_optional`] where absence is a legitimate answer.
fn walk(directory: &Utf8PathBuf) -> Vec<Utf8PathBuf> {
    assert!(
        directory.is_dir(),
        "{directory} is not a directory, so a check walking it would pass by reading nothing"
    );

    walk_optional(directory)
}

/// Every `.rs` file under `directory`, recursively, or none when the directory does not exist.
fn walk_optional(directory: &Utf8PathBuf) -> Vec<Utf8PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(directory.as_std_path()) else {
        return found;
    };

    for entry in entries.flatten() {
        let path = Utf8PathBuf::from_path_buf(entry.path()).expect("the repository has no non-UTF-8 paths");

        if path.is_dir() {
            found.extend(walk_optional(&path));
        } else if path.extension() == Some("rs") {
            found.push(path);
        }
    }

    found
}

/// Every `cargo-gamma*` crate directory in the workspace, in read order.
fn gamma_crates() -> Vec<Utf8PathBuf> {
    let mut found = Vec::new();

    for entry in fs::read_dir(workspace_dir().join("crates").as_std_path())
        .expect("the workspace has a crates directory")
        .flatten()
    {
        let path = Utf8PathBuf::from_path_buf(entry.path()).expect("the repository has no non-UTF-8 paths");

        if path.is_dir() && path.file_name().unwrap_or_default().starts_with("cargo-gamma") {
            found.push(path);
        }
    }

    assert!(
        !found.is_empty(),
        "no cargo-gamma crate was found, so this check would pass vacuously"
    );

    found
}

/// The names of every module declared anywhere under one crate's `src` tree.
///
/// Used to tell a re-export of this crate's own module from one that reaches into a dependency.
/// Collected across the whole crate rather than per file because a module declared in `foo/mod.rs`
/// is re-exported from its siblings by bare name, with nothing in the re-exporting file to say
/// where the name came from.
fn declared_modules(sources: &[Utf8PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();

    for path in sources {
        let text = fs::read_to_string(path.as_std_path()).unwrap_or_else(|_| panic!("could not read {path}"));

        for line in text.lines() {
            let trimmed = line.trim_start();
            let declaration = trimmed
                .strip_prefix("pub(crate) mod ")
                .or_else(|| trimmed.strip_prefix("pub(super) mod "))
                .or_else(|| trimmed.strip_prefix("pub mod "))
                .or_else(|| trimmed.strip_prefix("mod "));

            if let Some(rest) = declaration {
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();

                if !name.is_empty() {
                    let _inserted = names.insert(name);
                }
            }
        }
    }

    names
}

/// The first path segment a `use` statement names, if the line is a re-export.
///
/// Returns `None` for anything that is not a public re-export, including `pub(crate) use`, which
/// widens nothing outside this crate.
fn reexported_root(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("pub use ")?;
    let segment = rest.split(&[':', ' ', ';', '{', ','][..]).next()?.trim();

    (!segment.is_empty()).then_some(segment)
}

/// No public facade re-exports by glob, anywhere in the tool's crates.
///
/// A `pub use module::*` promises whatever that module happens to hold *next*, which makes adding a
/// `pub` item to an implementation module a silent change to another crate's published surface. The
/// feature-gated `internals` facade was the last holdout: it could not name a private module, since
/// re-exporting one is a hard error rather than a lint, so the feature that opens the facade is now
/// also what widens the module declarations it names.
///
/// Enforced here rather than by review because a glob is one character and reads as a convenience.
/// Nothing else in the build fails when one is added, and the surface it widens is documentation-
/// hidden, so the usual signals — a docs diff, a broken downstream build — are all absent.
#[test]
fn no_public_facade_re_exports_by_glob() {
    // Assembled rather than written out, so that this line does not match itself if the check ever
    // grows to cover its own file.
    let glob = format!("::{}", '*');
    let mut globs = Vec::new();

    for path in gamma_crates() {
        for source in walk(&path.join("src")) {
            let text = fs::read_to_string(source.as_std_path()).unwrap_or_else(|_| panic!("could not read {source}"));

            for (number, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("pub use ") && line.contains(&glob) {
                    globs.push(format!("{source}:{}", number + 1));
                }
            }
        }
    }

    assert!(
        globs.is_empty(),
        "these re-export by glob, so an item added to the module behind them joins a public surface without review: {}",
        globs.join(", ")
    );
}

/// Every public re-export of a local item says how it is documented, and no foreign one is inlined.
///
/// rustdoc renders a re-export of an item from *this* crate as a link back to the implementation
/// module unless the re-export is inlined, so a reader following the published path arrives
/// somewhere they were never meant to import from — and the canonical path in the documentation
/// stops matching the one the crate intends. `#[doc(inline)]` is what pins the item to the facade
/// that exports it. `#[doc(hidden)]` counts as an answer too: an export nobody is meant to find
/// does not need a canonical path, and saying so explicitly is what tells the two cases apart.
///
/// A re-export that reaches into another crate is the opposite case and is held to the opposite
/// rule. rustdoc already inlines those by default, because the crate they came from may not be
/// documented alongside this one, so `#[doc(inline)]` there states nothing that is not already
/// true — and reading one suggests the attribute is what makes the item appear, which is exactly
/// the belief that leaves a local re-export bare. They are required to carry nothing.
#[test]
fn every_local_re_export_states_how_it_is_documented() {
    let mut bare = Vec::new();
    let mut redundant = Vec::new();

    for path in gamma_crates() {
        let sources = walk(&path.join("src"));
        let modules = declared_modules(&sources);

        for source in &sources {
            let text = fs::read_to_string(source.as_std_path()).unwrap_or_else(|_| panic!("could not read {source}"));
            let lines: Vec<&str> = text.lines().collect();

            for (index, line) in lines.iter().enumerate() {
                let Some(root) = reexported_root(line) else {
                    continue;
                };

                // The attribute need not be adjacent: a `cfg` gate commonly sits between it and the
                // statement, in either order.
                let attributes: Vec<&str> = lines[..index]
                    .iter()
                    .rev()
                    .take_while(|previous| previous.trim_start().starts_with('#'))
                    .copied()
                    .collect();

                let local = ["crate", "self", "super"].contains(&root) || modules.contains(root);

                if !local {
                    if attributes
                        .iter()
                        .any(|previous| previous.trim_start().starts_with("#[doc(inline)]"))
                    {
                        redundant.push(format!("{source}:{}", index + 1));
                    }

                    continue;
                }

                if !attributes.iter().any(|previous| previous.trim_start().starts_with("#[doc(")) {
                    bare.push(format!("{source}:{}", index + 1));
                }
            }
        }
    }

    assert!(
        bare.is_empty(),
        "these re-export a local item without saying whether it is inlined or hidden, so rustdoc \
         publishes a canonical path pointing at the implementation module: {}",
        bare.join(", ")
    );

    assert!(
        redundant.is_empty(),
        "these inline a re-export from another crate, which rustdoc already does, copying that \
         crate's documentation in where it goes stale silently: {}",
        redundant.join(", ")
    );
}
