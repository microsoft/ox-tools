// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde_json::Value;

use super::cli::{ListArgs, ListCommand};
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::error::error;
use crate::exec::CargoOptions;
use crate::ops::registry;
use crate::report::{Styler, encode_controls};

/// Implements `list`.
pub(super) fn list<H: Host>(host: &mut H, args: &ListArgs, styler: Styler) -> crate::Result<i32> {
    let mut select = selection_args(args);
    let config = crate::config::Config::resolve(&select)?;
    let cargo = config.cargo_options();
    config.apply_selection(&mut select)?;

    list_with_cargo(host, args, &select, styler, &cargo)
}

/// Implements `list` with the configuration generation dispatch already resolved.
fn list_with_cargo<H: Host>(
    host: &mut H,
    args: &ListArgs,
    select: &super::cli::SelectArgs,
    styler: Styler,
    cargo: &CargoOptions,
) -> crate::Result<i32> {
    match &args.command {
        ListCommand::Mutators(args) => list_mutators(host, select, args.json),
        ListCommand::Files(args) => list_files(host, select, args.json, styler, cargo),
        ListCommand::Mutants(args) => list_mutants(host, select, args.json, args.json_report.as_ref(), styler, cargo),
        ListCommand::Presets(args) => list_presets(host, select, args.json),
    }
}

// #[gamma::skip(fn_value.default, tag = "outofmemory", reason = "discarding the requested discovery limits exhausted memory in the campaign")]
fn selection_args(args: &ListArgs) -> super::cli::SelectArgs {
    match &args.command {
        ListCommand::Mutants(args) => args.select.clone(),
        ListCommand::Files(args) => args.select.clone(),
        ListCommand::Mutators(args) | ListCommand::Presets(args) => super::cli::SelectArgs {
            dir: args.dir.clone(),
            mutators: args.mutators.clone(),
            config: args.config.clone(),
            ..super::cli::SelectArgs::default()
        },
    }
}

/// Writes a listing as pretty JSON, straight to the stream.
///
/// Serializing into a `String` first would hold a second copy of a listing that reaches tens of
/// megabytes on a large workspace, for no benefit: the document is written out whole either way.
///
/// # Errors
///
/// Returns an error if `entries` cannot be serialized, or if the stream cannot be written to.
fn write_pretty_json<W: Write, T: Serialize + ?Sized>(stream: &mut W, entries: &T, what: &str) -> crate::Result<()> {
    serde_json::to_writer_pretty(&mut *stream, entries).map_err(|cause| error!("could not write {what} as JSON").caused_by(cause))?;

    writeln!(stream).map_err(|cause| error!("could not write {what} as JSON").caused_by(cause))?;

    Ok(())
}

/// Lists the named mutator presets.
///
/// The selection is resolved against each preset so the listing says which one you are actually
/// running, rather than making you match `--mutators` against the table by eye.
fn list_presets<H: Host>(host: &mut H, select: &super::cli::SelectArgs, json: bool) -> crate::Result<i32> {
    let selection = select.selection()?;
    let mut stream = host.results();

    if json {
        let entries: Vec<Value> = registry::PRESETS
            .iter()
            .map(|preset| {
                serde_json::json!({
                    "name": preset.name,
                    "description": preset.description,
                    "members": preset.members,
                    "enabled": registry::resolve(&format!("@{}", preset.name))
                        .is_ok_and(|resolved| resolved.iter().all(|mutator| selection.contains(mutator))),
                })
            })
            .collect();

        write_pretty_json(&mut stream, &entries, "the presets")?;

        return Ok(EXIT_OK);
    }

    let width = registry::PRESETS.iter().map(|preset| preset.name.len() + 1).max().unwrap_or(
        // #[gamma::skip(literal.int_increment, reason = "Both width computations iterate the immutable nonempty engine PRESETS or REGISTRY tables. Their max() is always Some(width), so changing only unwrap_or's0 fallback to1 is unreachable.", tag = "equivalent")]
        0,
    );

    for preset in registry::PRESETS {
        let enabled = registry::resolve(&format!("@{}", preset.name))
            .is_ok_and(|resolved| resolved.iter().all(|mutator| selection.contains(mutator)));
        let mark = if enabled { "*" } else { " " };

        writeln!(stream, "{mark} @{:width$}  {}", preset.name, preset.description)?;
    }

    writeln!(stream)?;
    writeln!(stream, "* = enabled by the current selection")?;

    Ok(EXIT_OK)
}

/// Lists the mutator registry.
fn list_mutators<H: Host>(host: &mut H, select: &super::cli::SelectArgs, json: bool) -> crate::Result<i32> {
    let selection = select.selection()?;
    let mut stream = host.results();

    if json {
        let entries: Vec<Value> = registry::REGISTRY
            .iter()
            .map(|mutator| {
                serde_json::json!({
                    "name": mutator.name,
                    "description": mutator.description,
                    "default": mutator.default_on,
                    "enabled": selection.contains(mutator.name),
                    "optimisticRequiresExplicitSelection": registry::optimistic_requires_explicit(mutator.name),
                    "aliases": mutator.aliases,
                })
            })
            .collect();

        write_pretty_json(&mut stream, &entries, "the mutator registry")?;

        return Ok(EXIT_OK);
    }

    let width = registry::REGISTRY.iter().map(|m| m.name.len()).max().unwrap_or(
        // #[gamma::skip(literal.int_increment, reason = "Both width computations iterate the immutable nonempty engine PRESETS or REGISTRY tables. Their max() is always Some(width), so changing only unwrap_or's0 fallback to1 is unreachable.", tag = "equivalent")]
        0,
    );

    for mutator in registry::REGISTRY {
        let mark = if selection.contains(mutator.name) { "*" } else { " " };
        let confidence = if registry::optimistic_requires_explicit(mutator.name) {
            " [optimistic sites require explicit selection]"
        } else {
            ""
        };

        writeln!(stream, "{mark} {:width$}  {}{confidence}", mutator.name, mutator.description)?;
    }

    writeln!(stream)?;
    writeln!(stream, "* = enabled by the current selection")?;

    Ok(EXIT_OK)
}

/// Lists the files that would be analyzed.
fn list_files<H: Host>(
    host: &mut H,
    select: &super::cli::SelectArgs,
    json: bool,
    styler: Styler,
    cargo: &CargoOptions,
) -> crate::Result<i32> {
    let selection = select.selection()?;
    let plan = crate::discover::plan_for_build(select, &selection, select.shard()?, cargo, &mut |_| {})?;

    crate::report::skipped(host, &plan, styler)?;

    let files = selected_files(&plan, !select.name_patterns.is_empty());

    let mut stream = host.results();

    if json {
        let paths: Vec<&Utf8PathBuf> = files.map(|file| &file.path).collect();

        write_pretty_json(&mut stream, &paths, "the file list")?;

        return Ok(EXIT_OK);
    }

    for file in files {
        writeln!(stream, "{}", encode_controls(file.path.as_str()))?;
    }

    Ok(EXIT_OK)
}

/// Projects a file listing from the selected mutant population.
fn selected_files(plan: &crate::discover::Plan, name_filtered: bool) -> impl Iterator<Item = &crate::discover::TargetFile> {
    // A name filter selects mutants, so a file listing under that filter names only files that
    // actually contain one of the selected mutants.
    let selected_paths = name_filtered.then(|| {
        plan.mutants
            .iter()
            .map(|mutant| mutant.file.as_ref())
            .collect::<std::collections::HashSet<&Utf8Path>>()
    });

    plan.files
        .iter()
        .filter(move |file| selected_paths.as_ref().is_none_or(|paths| paths.contains(file.path.as_path())))
}

/// Lists the mutants that would be generated.
fn list_mutants<H: Host>(
    host: &mut H,
    select: &super::cli::SelectArgs,
    json: bool,
    json_report: Option<&Utf8PathBuf>,
    styler: Styler,
    cargo: &CargoOptions,
) -> crate::Result<i32> {
    let selection = select.selection()?;
    let shard = select.shard()?;
    let plan = crate::discover::plan_for_build(select, &selection, shard, cargo, &mut |_| {})?;

    list_mutant_population(host, select, json, json_report, styler, &plan, shard)
}

/// Renders the discovered population and publishes any requested report without rediscovery.
fn list_mutant_population<H: Host>(
    host: &mut H,
    select: &super::cli::SelectArgs,
    json: bool,
    json_report: Option<&Utf8PathBuf>,
    styler: Styler,
    plan: &crate::discover::Plan,
    shard: Option<(u32, u32)>,
) -> crate::Result<i32> {
    crate::report::skipped(host, plan, styler)?;

    if let Some(path) = json_report {
        write_population(
            host,
            plan,
            shard,
            population_complete(select),
            !select.name_patterns.is_empty(),
            path,
        )?;
    }

    let mut stream = host.results();

    if json {
        write_pretty_json(&mut stream, &plan.mutants, "the mutant list")?;

        return Ok(EXIT_OK);
    }

    for mutant in &plan.mutants {
        writeln!(stream, "{}", encode_controls(&mutant.selection_name()))?;
    }

    let suppressed = plan
        .mutants
        .iter()
        .filter(|mutant| mutant.outcome == crate::model::Outcome::Ignored)
        .count();

    if suppressed > 0 {
        writeln!(stream)?;
        writeln!(stream, "{suppressed} suppressed")?;
    }

    Ok(EXIT_OK)
}

/// Grants authority to withdraw absent older identities only for complete per-file populations.
///
/// Whole-file/package selection preserves that authority; any explicit mutator selector (even an
/// exhaustive spelling), name regex, or diff filter conservatively withholds it.
fn population_complete(select: &super::cli::SelectArgs) -> bool {
    select.in_diff.is_none() && select.mutators.is_none() && select.name_patterns.is_empty()
}

/// Writes the listing as a report document.
///
/// `merge` withdraws a mutant only when a newer unsharded, unfiltered input states the whole
/// population of its file, and producing that from a run means paying for a run. Listing is the
/// cheap way to say what exists now, so it is the one a nightly rotation can afford beside its shard.
fn write_population<H: Host>(
    host: &mut H,
    plan: &crate::discover::Plan,
    shard: Option<(u32, u32)>,
    population_complete: bool,
    name_filtered: bool,
    path: &Utf8PathBuf,
) -> crate::Result<()> {
    write_population_at(host, plan, shard, population_complete, name_filtered, path, SystemTime::now())
}

fn write_population_at<H: Host>(
    host: &mut H,
    plan: &crate::discover::Plan,
    shard: Option<(u32, u32)>,
    population_complete: bool,
    name_filtered: bool,
    path: &Utf8PathBuf,
    now: SystemTime,
) -> crate::Result<()> {
    let info = crate::elements::RunInfo {
        started_at: now.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs()),
        mutant_id_version:
        // #[gamma::skip(option.some_to_none, reason = "write_population_at passes its RunInfo straight to elements::build. build_from always constructs returned config using mutant_id_version: Some(MUTANT_ID_VERSION), ..run, overwriting the supplied field regardless of Some/None. The local RunInfo is not retained elsewhere; replacing this input field with None cannot alter report fields, errors, bytes, or publication.", tag = "equivalent")]
        Some(crate::model::MUTANT_ID_VERSION),
        merged: false,
        population_complete: population_complete && shard.is_none(),
        name_filtered,
        shard: shard.map(|(count, index)| crate::elements::ShardInfo { index, count }),
        tests: None,
        // Filled in by `build` from the plan it is given, so that it cannot disagree with the
        // mutants in the same report.
        not_built: None,
        dropped_test_packages: Vec::new(),
        merge_provenance: None,
    };

    let report = crate::elements::build(plan, crate::elements::Thresholds::default(), Some(info))?;

    publish_population(&report, path)?;
    writeln!(host.error(), "Wrote {}", encode_controls(path.as_str()))?;

    Ok(())
}

/// Replaces report publication on the installing test thread, including its error result.
#[cfg(test)]
type PopulationPublisher = Box<dyn FnMut(&crate::elements::Report, &Utf8PathBuf) -> crate::Result<()>>;

#[cfg(test)]
std::thread_local! {
    static POPULATION_PUBLISHER: std::cell::RefCell<Option<PopulationPublisher>> = const { std::cell::RefCell::new(None) };
}

/// Uses the installed publisher instead of touching the filesystem.
fn publish_population(report: &crate::elements::Report, path: &Utf8PathBuf) -> crate::Result<()> {
    #[cfg(test)]
    if let Some(result) = POPULATION_PUBLISHER.with_borrow_mut(|publisher| publisher.as_mut().map(|publish| publish(report, path))) {
        return result;
    }
    crate::elements::write_json(report, path)
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::fmt::Write as _;
    use std::fs;

    use serde_json::{from_slice, from_str, json, to_value};

    use super::*;
    use crate::commands::{ListFilesArgs, ListKind, ListMutantsArgs, ListRegistryArgs};
    use crate::testing::{Broken, BrokenHost, Sink};

    /// Installs a current-thread publisher and restores the previous one, including on unwind.
    fn with_population_publisher<T>(publisher: PopulationPublisher, body: impl FnOnce() -> T) -> T {
        struct Restore(Option<PopulationPublisher>);
        impl Drop for Restore {
            fn drop(&mut self) {
                POPULATION_PUBLISHER.with_borrow_mut(|publisher| *publisher = self.0.take());
            }
        }
        let _restore = Restore(POPULATION_PUBLISHER.with_borrow_mut(|slot| slot.replace(publisher)));
        body()
    }

    #[test]
    fn recovered_mutant_listing_passes_the_actual_selection_completeness_into_the_published_population() {
        let plan = crate::discover::Survey::from_test_root("never-read-listing-root".into()).skeleton();
        let path = Utf8PathBuf::from("never-written-listing-population.json");
        for (mutators, name_patterns, complete) in [
            (None, Vec::new(), true),
            (Some("arith.add_to_sub".to_owned()), Vec::new(), false),
            (None, vec!["needle".to_owned()], false),
        ] {
            let name_filtered = !name_patterns.is_empty();
            let select = super::super::cli::SelectArgs {
                mutators,
                name_patterns,
                ..super::super::cli::SelectArgs::default()
            };
            let published = std::rc::Rc::new(std::cell::Cell::new(0));
            let mut host = Sink::default();
            let exit = with_population_publisher(
                Box::new({
                    let published = std::rc::Rc::clone(&published);
                    move |report, _path| {
                        assert_eq!(report.config.as_ref().unwrap().population_complete, complete);
                        assert_eq!(report.config.as_ref().unwrap().name_filtered, name_filtered);
                        published.set(published.get() + 1);
                        Ok(())
                    }
                }),
                || list_mutant_population(&mut host, &select, true, Some(&path), Styler::new(false), &plan, None),
            )
            .unwrap();
            assert_eq!(exit, EXIT_OK);
            assert_eq!(published.get(), 1);
            assert_eq!(from_str::<Value>(&host.out()).unwrap(), json!([]));
            assert_eq!(host.err(), format!("Wrote {path}\n"));
        }
    }

    #[test]
    fn recovered_native_population_writer_preserves_completeness_identity_and_shard_metadata_before_in_memory_publication() {
        let plan = crate::discover::Survey::from_test_root("never-read-population-root".into()).skeleton();
        let path = Utf8PathBuf::from("never-written-population.json");
        for complete in [false, true] {
            for shard in [None, Some((5, 2))] {
                let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut host = Sink::default();
                with_population_publisher(
                    Box::new({
                        let seen = std::rc::Rc::clone(&seen);
                        move |report, path| {
                            assert!(!report.config.as_ref().unwrap().merged);
                            seen.borrow_mut().push((path.clone(), to_value(report).unwrap()));
                            Ok(())
                        }
                    }),
                    || write_population(&mut host, &plan, shard, complete, false, &path),
                )
                .unwrap();
                let seen = seen.borrow();
                assert_eq!(seen.len(), 1);
                assert_eq!(seen[0].0, path);
                let config = &seen[0].1["config"];
                assert_eq!(config["populationComplete"], complete && shard.is_none());
                assert_eq!(config["mutantIdVersion"], crate::model::MUTANT_ID_VERSION);
                assert!(config.get("merged").is_none());
                assert_eq!(
                    config.get("shard"),
                    shard.map(|(count, index)| json!({"count": count, "index": index})).as_ref()
                );
                assert!(host.out().is_empty());
                assert_eq!(host.err(), format!("Wrote {path}\n"));
            }
        }
    }

    #[test]
    fn recovered_population_writer_returns_inline_source_parse_errors_before_publishing_or_announcing_success() {
        let mut plan = crate::discover::Survey::from_test_root("never-read-population-root".into()).skeleton();
        let path = Utf8PathBuf::from("broken.rs");
        plan.files.push(crate::discover::TargetFile {
            path: path.clone(),
            absolute: "never-read-inline-source".into(),
            package: "alpha".into(),
            source: Some("pub fn broken(".into()),
        });
        plan.digests.insert(path, "retained-generation".into());
        let mut host = Sink::default();
        let result = with_population_publisher(
            Box::new(|_report, _path| panic!("unparseable inline sources cannot be published")),
            || {
                write_population(
                    &mut host,
                    &plan,
                    None,
                    true,
                    false,
                    &Utf8PathBuf::from("never-written-population.json"),
                )
            },
        );
        let failure = result.expect_err("the retained inline source is syntactically incomplete");
        assert!(failure.to_string().contains("broken.rs"), "{failure}");
        assert!(host.out().is_empty());
        assert!(host.err().is_empty());
    }

    fn crate_dir(name: &str) -> tempfile::TempDir {
        crate::fixtures::crate_dir(name, "pub fn less(a: i32, b: i32) -> bool { a < b }\n").0
    }

    #[test]
    fn survivor_registry_lists_mark_partial_selections_and_preserve_machine_fields() {
        let select = super::super::cli::SelectArgs {
            mutators: Some("relational.lt_to_le".to_owned()),
            ..super::super::cli::SelectArgs::default()
        };
        let mut host = Sink::default();
        assert_eq!(list_mutators(&mut host, &select, true).unwrap(), EXIT_OK);
        assert!(host.out().ends_with('\n'));
        let entries: Value = from_str(&host.out()).unwrap();
        let entries = entries.as_array().unwrap();
        let selected = entries.iter().find(|entry| entry["name"] == "relational.lt_to_le").unwrap();
        assert_eq!(selected["enabled"], true);
        assert!(selected["description"].is_string());
        assert!(selected["default"].is_boolean());
        assert!(selected["aliases"].is_array());
        assert!(selected["optimisticRequiresExplicitSelection"].is_boolean());
        let unselected = entries.iter().find(|entry| entry["name"] == "literal.int_decrement").unwrap();
        assert_eq!(unselected["enabled"], false);
        assert_eq!(unselected["optimisticRequiresExplicitSelection"], true);
        host.out.clear();
        list_mutators(&mut host, &select, false).unwrap();
        let output = host.out();
        assert!(
            output
                .lines()
                .find(|line| line.contains("relational.lt_to_le"))
                .unwrap()
                .starts_with("* ")
        );
        let unselected = output.lines().find(|line| line.contains("literal.int_decrement")).unwrap();
        assert!(unselected.starts_with("  "));
        assert!(unselected.ends_with("[optimistic sites require explicit selection]"));
        host.out.clear();
        list_presets(&mut host, &select, false).unwrap();
        let output = host.out();
        let boundary = output.lines().find(|line| line.contains("@boundary")).unwrap();
        assert!(boundary.starts_with("  @boundary"), "{boundary}");
        assert!(output.ends_with("\n\n* = enabled by the current selection\n"));
    }

    #[test]
    fn registry_text_rows_have_exact_alignment_selection_and_confidence_suffixes() {
        let select = super::super::cli::SelectArgs {
            mutators: Some("relational.lt_to_le".into()),
            ..super::super::cli::SelectArgs::default()
        };
        let mut host = Sink::default();
        list_presets(&mut host, &select, false).unwrap();
        let width = registry::PRESETS.iter().map(|preset| preset.name.len()).max().unwrap() + 1;
        let expected = registry::PRESETS.iter().fold(String::new(), |mut output, preset| {
            writeln!(output, "  @{:width$}  {}", preset.name, preset.description).unwrap();
            output
        });
        assert_eq!(host.out(), format!("{expected}\n* = enabled by the current selection\n"));

        host.out.clear();
        list_mutators(&mut host, &select, false).unwrap();
        let width = registry::REGISTRY.iter().map(|mutator| mutator.name.len()).max().unwrap();
        let expected = registry::REGISTRY.iter().fold(String::new(), |mut output, mutator| {
            let mark = if mutator.name == "relational.lt_to_le" { "*" } else { " " };
            let suffix = if registry::optimistic_requires_explicit(mutator.name) {
                " [optimistic sites require explicit selection]"
            } else {
                ""
            };
            writeln!(output, "{mark} {:width$}  {}{suffix}", mutator.name, mutator.description).unwrap();
            output
        });
        assert_eq!(host.out(), format!("{expected}\n* = enabled by the current selection\n"));
    }

    #[test]
    fn registry_selection_arguments_keep_the_explicit_configuration_and_opt_out() {
        for kind in [ListKind::Presets, ListKind::Mutators] {
            for config in [
                crate::commands::ConfigArgs {
                    path: Some("custom-gamma.toml".into()),
                    no_config: false,
                },
                crate::commands::ConfigArgs {
                    path: None,
                    no_config: true,
                },
            ] {
                let mut listing = args("custom-project", kind, false);
                let (ListCommand::Presets(registry) | ListCommand::Mutators(registry)) = &mut listing.command else {
                    panic!("fixture is a registry listing");
                };
                registry.config = config.clone();
                registry.mutators = Some("literal.bool_flip".into());
                let select = selection_args(&listing);
                assert_eq!(select.dir, Utf8PathBuf::from("custom-project"));
                assert_eq!(select.mutators.as_deref(), Some("literal.bool_flip"));
                assert_eq!(select.config.path, config.path);
                assert_eq!(select.config.no_config, config.no_config);
            }
        }
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn listings_return_explicit_config_and_workspace_discovery_failures_without_panicking() {
        let directory = crate_dir("list-initialization-failures-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        for kind in [ListKind::Presets, ListKind::Mutators] {
            let mut listing = args(root.clone(), kind, false);
            let (ListCommand::Presets(registry) | ListCommand::Mutators(registry)) = &mut listing.command else {
                panic!("fixture is a registry listing");
            };
            registry.config.path = Some(root.join("missing-config.toml"));
            list(&mut Sink::default(), &listing, Styler::new(false))
                .expect_err("an explicitly requested missing configuration must be returned");
        }
        fs::write(root.join("Cargo.toml"), "invalid toml ]").unwrap();
        let select = super::super::cli::SelectArgs {
            dir: root,
            mutators: Some("literal.bool_flip".into()),
            ..super::super::cli::SelectArgs::default()
        };
        let cargo = CargoOptions::default();
        list_files(&mut Sink::default(), &select, false, Styler::new(false), &cargo)
            .expect_err("file discovery must return the invalid manifest error");
        list_mutants(&mut Sink::default(), &select, false, None, Styler::new(false), &cargo)
            .expect_err("mutant discovery must return the invalid manifest error");
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn discovery_listings_preserve_skipped_file_colors_and_return_diagnostic_write_errors() {
        let directory = crate_dir("list-skipped-diagnostics-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let source = format!("pub fn deep() -> i32 {{ {}1{} }}", "(".repeat(512), ")".repeat(512));
        fs::write(root.join("src/lib.rs"), source).unwrap();
        for kind in [ListKind::Files, ListKind::Mutants] {
            let listing = args(root.clone(), kind, false);
            for colored in [false, true] {
                let mut host = Sink::default();
                assert_eq!(list(&mut host, &listing, Styler::new(colored)).unwrap(), EXIT_OK);
                let diagnostics = String::from_utf8(host.err).unwrap();
                assert!(diagnostics.contains("1 file could not be analyzed:"), "{diagnostics}");
                assert_eq!(diagnostics.contains("\x1b["), colored, "{diagnostics}");
            }
            list(&mut BrokenHost, &listing, Styler::new(false))
                .expect_err("skipped-file diagnostic failures must propagate through either listing consumer");
        }
    }

    #[test]
    fn population_authority_requires_no_mutator_or_diff_filter_but_allows_whole_file_selection() {
        let mut select = super::super::cli::SelectArgs::default();
        assert!(population_complete(&select));
        select.files.push("src/*.rs".into());
        select.exclude_files.push("src/generated.rs".into());
        select.packages.push("included-package".into());
        assert!(population_complete(&select));
        for mutators in ["arith.add_to_sub", "all", "@all"] {
            select.mutators = Some(mutators.into());
            assert!(!population_complete(&select), "{mutators}");
        }
        select.mutators = None;
        select.in_diff = Some("changes.patch".into());
        assert!(!population_complete(&select));
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn population_publication_propagates_report_write_and_diagnostic_failures() {
        let directory = crate_dir("list-population-failures-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let select = super::super::cli::SelectArgs {
            dir: root.clone(),
            mutators: Some("relational.lt_to_le".into()),
            ..super::super::cli::SelectArgs::default()
        };
        let selection = select.selection().unwrap();
        let cargo = CargoOptions::default();
        let plan = crate::discover::plan_for_build(&select, &selection, None, &cargo, &mut |_| {}).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, "fixture").unwrap();
        let report = blocker.join("population.json");
        write_population(&mut Sink::default(), &plan, None, true, false, &report).expect_err("a report below a file cannot be published");
        list_mutants(&mut Sink::default(), &select, false, Some(&report), Styler::new(false), &cargo)
            .expect_err("the listing must return the population publication failure");

        let report = root.join("population.json");
        write_population(&mut BrokenHost, &plan, Some((5, 2)), true, false, &report)
            .expect_err("a failed publication diagnostic must be returned");
        let report: Value = from_slice(&fs::read(&report).unwrap()).unwrap();
        assert_eq!(report["config"]["shard"], json!({ "index": 2, "count": 5 }));
        assert_eq!(report["config"]["populationComplete"], false);

        let path = root.join("unknown-start-time.json");
        let before_epoch = UNIX_EPOCH.checked_sub(core::time::Duration::from_secs(1)).unwrap();
        write_population_at(&mut Sink::default(), &plan, None, true, false, &path, before_epoch).unwrap();
        let report: Value = from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(report["config"]["startedAt"], 0);
        write_population_at(
            &mut Sink::default(),
            &plan,
            None,
            true,
            false,
            &path,
            UNIX_EPOCH + core::time::Duration::from_secs(1_234_567),
        )
        .unwrap();
        let report: Value = from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(report["config"]["startedAt"], 1_234_567);
        assert_eq!(report["config"]["populationComplete"], true);
        write_population_at(&mut Sink::default(), &plan, None, false, false, &path, before_epoch).unwrap();
        let report: Value = from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(report["config"]["populationComplete"], false);
    }

    #[test]
    fn survivor_json_writer_reports_failure_at_the_final_newline_with_context() {
        struct NoNewline(Vec<u8>);
        impl Write for NoNewline {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if buf == b"\n" {
                    return Err(std::io::Error::other("newline rejected"));
                }
                self.0.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut stream = NoNewline(Vec::new());
        let cause = write_pretty_json(&mut stream, &17, "the survivor listing").unwrap_err();
        assert_eq!(stream.0, b"17");
        assert!(cause.to_string().contains("could not write the survivor listing as JSON"));
    }

    #[test]
    fn survivor_registry_and_discovery_lists_reject_invalid_selections_and_shards_before_reading_sources() {
        let mut select = super::super::cli::SelectArgs {
            mutators: Some("not_a_real_mutator".into()),
            ..super::super::cli::SelectArgs::default()
        };
        let cargo = CargoOptions::default();
        let styler = Styler::new(false);
        let mut host = Sink::default();
        list_presets(&mut host, &select, true).expect_err("invalid selection must reject the preset listing");
        list_mutators(&mut host, &select, true).expect_err("invalid selection must reject the mutator listing");
        list_files(&mut host, &select, true, styler, &cargo).expect_err("invalid selection must reject the file listing");
        list_mutants(&mut host, &select, true, None, styler, &cargo).expect_err("invalid selection must reject the mutant listing");
        select.mutators = None;
        select.shard_count = Some(0);
        select.shard_index = Some(0);
        list_files(&mut host, &select, false, styler, &cargo).expect_err("zero shard count must reject the file listing");
        list_mutants(&mut host, &select, false, None, styler, &cargo).expect_err("zero shard count must reject the mutant listing");
        assert!(host.out.is_empty());
    }

    struct Limited<'a>(&'a mut usize);
    impl Write for Limited<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if *self.0 == 0 {
                return Err(std::io::Error::other("registry output denied"));
            }
            *self.0 -= buf.iter().map(|&byte| usize::from(byte == b'\n')).sum::<usize>();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[derive(Debug)]
    struct LimitedHost(usize);
    impl Host for LimitedHost {
        fn output(&mut self) -> impl Write {
            Limited(&mut self.0)
        }
        fn error(&mut self) -> impl Write {
            std::io::sink()
        }
        fn is_terminal(&self) -> bool {
            false
        }
        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }
    #[test]
    fn survivor_registry_writes_return_non_pipe_errors_in_json_rows_and_both_text_footers() {
        let select = super::super::cli::SelectArgs::default();
        for remaining in 0..registry::PRESETS.len() + 2 {
            list_presets(&mut LimitedHost(remaining), &select, false).expect_err("denied preset row or footer must propagate");
        }
        for remaining in 0..registry::REGISTRY.len() + 2 {
            list_mutators(&mut LimitedHost(remaining), &select, false).expect_err("denied mutator row or footer must propagate");
        }
        assert!(
            list_presets(&mut LimitedHost(0), &select, true)
                .unwrap_err()
                .to_string()
                .contains("the presets")
        );
        assert!(
            list_mutators(&mut LimitedHost(0), &select, true)
                .unwrap_err()
                .to_string()
                .contains("the mutator registry")
        );
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn native_discovery_writer_acceptance() {
        let directory = crate_dir("list-discovery-write-failures-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        fs::write(root.join("src/lib.rs"), "#[gamma::skip]\npub fn f()->bool { true }\n").unwrap();
        let select = super::super::cli::SelectArgs {
            dir: root,
            mutators: Some("literal.bool_flip".into()),
            ..super::super::cli::SelectArgs::default()
        };
        let cargo = CargoOptions::default();
        for json in [false, true] {
            let mut host = Sink::default();
            list_files(&mut host, &select, json, Styler::new(false), &cargo).unwrap();
            for remaining in 0..host.out().lines().count() {
                let error = list_files(&mut LimitedHost(remaining), &select, json, Styler::new(false), &cargo).unwrap_err();
                if json {
                    assert!(error.to_string().contains("could not write the file list as JSON"));
                }
            }
            host.out.clear();
            list_mutants(&mut host, &select, json, None, Styler::new(false), &cargo).unwrap();
            if !json {
                assert!(host.out().ends_with("\n\n1 suppressed\n"), "{}", host.out());
            }
            for remaining in 0..host.out().lines().count() {
                let error = list_mutants(&mut LimitedHost(remaining), &select, json, None, Styler::new(false), &cargo).unwrap_err();
                if json {
                    assert!(error.to_string().contains("could not write the mutant list as JSON"));
                }
            }
        }
    }

    fn args(dir: impl Into<Utf8PathBuf>, what: ListKind, json: bool) -> ListArgs {
        let dir = dir.into();
        let select = crate::commands::SelectArgs {
            dir: dir.clone(),
            ..crate::commands::SelectArgs::default()
        };
        let registry = || ListRegistryArgs {
            dir: dir.clone(),
            mutators: None,
            config: crate::commands::ConfigArgs::default(),
            json,
        };
        let command = match what {
            ListKind::Mutants => ListCommand::Mutants(ListMutantsArgs {
                select,
                json,
                json_report: None,
            }),
            ListKind::Files => ListCommand::Files(ListFilesArgs { select, json }),
            ListKind::Mutators => ListCommand::Mutators(registry()),
            ListKind::Presets => ListCommand::Presets(registry()),
        };

        ListArgs { command }
    }

    #[test]
    fn a_stream_failure_is_reported_as_a_write_failure() {
        let failure = write_pretty_json(&mut Broken, &[1], "the fixture").expect_err("the closed stream must fail");

        assert!(failure.to_string().contains("could not write the fixture as JSON"), "{failure}");
    }

    #[test]
    fn ops_can_be_listed_as_json() {
        let mut host = Sink::default();

        let code = list(
            &mut host,
            &args(Utf8PathBuf::from("."), ListKind::Mutators, true),
            Styler::new(false),
        )
        .expect("list");
        let text = String::from_utf8(host.out).expect("utf-8");
        let value: Value = from_str(&text).expect("json");

        assert_eq!(code, EXIT_OK);
        let entries = value.as_array().expect("a registry array");
        assert!(!entries.is_empty(), "{text}");
        assert!(text.contains("\"enabled\""), "{text}");
        let by_name = |name: &str| entries.iter().find(|entry| entry["name"] == name).expect("registered mutator");
        assert_eq!(by_name("literal.int_decrement")["optimisticRequiresExplicitSelection"], true);
        assert_eq!(by_name("fn_value.default")["optimisticRequiresExplicitSelection"], false);
    }

    /// `--json` is a contract for scripts, so the oracle has to be that the output *parses* and has
    /// the promised shape. Asserting a substring the plain-text listing also contains would leave
    /// the JSON branch free to disappear entirely.
    #[test]
    fn files_can_be_listed_as_json() {
        let dir = crate_dir("list-files-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Files, true), Styler::new(false)).expect("list files");
        let text = String::from_utf8(host.out).expect("utf-8");
        let value: Value = from_str(&text).expect("the --json file listing must be JSON");

        assert_eq!(code, EXIT_OK);

        let paths = value.as_array().expect("the file listing is a JSON array");

        assert!(paths.iter().all(Value::is_string), "every entry is a path string: {text}");
        assert!(paths.iter().any(|entry| entry.as_str() == Some("src/lib.rs")), "{text}");
    }

    #[test]
    fn mutants_can_be_listed_as_json() {
        let dir = crate_dir("list-mutants-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Mutants, true), Styler::new(false)).expect("list mutants");
        let text = String::from_utf8(host.out).expect("utf-8");
        let value: Value = from_str(&text).expect("the --json mutant listing must be JSON");

        assert_eq!(code, EXIT_OK);

        let mutants = value.as_array().expect("the mutant listing is a JSON array");
        let first = mutants.first().expect("the fixture produces mutants").as_object();
        let first = first.expect("every entry is a JSON object");

        for field in ["id", "file", "line", "mutator"] {
            assert!(first.contains_key(field), "missing `{field}`: {text}");
        }

        assert!(
            mutants.iter().any(|entry| entry["mutator"].as_str() == Some("relational.lt_to_le")),
            "{text}"
        );
    }

    /// The plain listing marks the mutators the current selection turns on.
    #[test]
    fn ops_can_be_listed_as_text_with_the_selection_marked() {
        let mut host = Sink::default();

        let code = list(
            &mut host,
            &args(Utf8PathBuf::from("."), ListKind::Mutators, false),
            Styler::new(false),
        )
        .expect("list");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("* = enabled by the current selection"), "{}", host.out());
        assert!(host.out().lines().any(|line| line.starts_with("* ")), "{}", host.out());
        let width = registry::REGISTRY
            .iter()
            .map(|mutator| mutator.name.len())
            .max()
            .expect("registered mutators");
        let expected = format!(
            "* {:width$}  subtract one from an integer literal [optimistic sites require explicit selection]",
            "literal.int_decrement"
        );
        let output = host.out();
        let actual = output
            .lines()
            .find(|line| line.contains("literal.int_decrement"))
            .expect("literal.int_decrement row");
        assert_eq!(actual, expected);
    }

    /// The plain file listing is one path per line, so it can be piped into `xargs`.
    #[test]
    fn files_can_be_listed_as_text() {
        let dir = crate_dir("list-files-text-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Files, false), Styler::new(false)).expect("list files");

        assert_eq!(code, EXIT_OK);
        assert_eq!(host.out().trim(), "src/lib.rs");
    }

    /// The plain mutant listing describes each mutant on its own line.
    #[test]
    fn mutants_can_be_listed_as_text() {
        let dir = crate_dir("list-mutants-text-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Mutants, false), Styler::new(false)).expect("list mutants");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("relational.lt_to_le"), "{}", host.out());
    }

    /// A suppressed mutant is marked and counted, so the listing does not overstate the run.
    #[test]
    fn a_suppressed_mutant_is_marked_and_counted_in_the_text_listing() {
        let dir = crate_dir("list-mutants-suppressed-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        fs::write(
            root.join("src/lib.rs"),
            "#[gamma::skip]\npub fn less(a: i32, b: i32) -> bool { a < b }\n\npub fn more(a: i32, b: i32) -> bool { a > b }\n",
        )
        .expect("lib");

        let mut host = Sink::default();
        let code = list(&mut host, &args(root, ListKind::Mutants, false), Styler::new(false)).expect("list mutants");
        let text = host.out();
        let marked: Vec<&str> = text.lines().filter(|line| line.contains("[suppressed:")).collect();

        assert_eq!(code, EXIT_OK);
        assert!(!marked.is_empty(), "nothing was marked: {text}");
        assert!(marked.iter().all(|line| line.contains("[suppressed: attribute]")), "{text}");
        assert!(
            text.lines()
                .any(|line| line.contains("relational.gt_to_ge") && !line.contains("[suppressed:")),
            "an ordinary mutant was marked: {text}"
        );
        assert!(
            text.lines().any(|line| line == format!("{} suppressed", marked.len())),
            "the tail line is missing: {text}"
        );
    }

    /// Nothing suppressed means no tail line at all, rather than a `0 suppressed` line.
    #[test]
    fn the_suppressed_tail_is_omitted_when_nothing_is_suppressed() {
        let dir = crate_dir("list-mutants-unsuppressed-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Mutants, false), Styler::new(false)).expect("list mutants");

        assert_eq!(code, EXIT_OK);
        assert!(!host.out().contains("suppressed"), "{}", host.out());
    }

    /// The preset listing says which presets the current selection actually turns on, which is
    /// the whole reason it exists rather than being a table in the README.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn presets_can_be_listed_as_json_with_the_selection_resolved_against_each() {
        let mut listing = args(Utf8PathBuf::from("."), ListKind::Presets, true);
        let mut host = Sink::default();

        let ListCommand::Presets(args) = &mut listing.command else {
            panic!("expected preset listing");
        };
        args.mutators = Some("relational,range".to_owned());

        let code = list(&mut host, &listing, Styler::new(false)).expect("list presets");
        let text = String::from_utf8(host.out).expect("utf-8");
        let value: Value = from_str(&text).expect("json");
        let entries = value.as_array().expect("an array");
        let enabled = |name: &str| {
            entries
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap_or_else(|| panic!("no {name} preset: {text}"))["enabled"]
                .as_bool()
                .expect("a boolean")
        };

        assert_eq!(code, EXIT_OK);
        assert_eq!(entries.len(), registry::PRESETS.len(), "{text}");
        assert!(enabled("boundary"), "a preset whose every member was selected reads as off: {text}");
        assert!(!enabled("semantics"), "a preset no selected mutator belongs to reads as on: {text}");
        assert!(entries.iter().all(|entry| entry["description"].is_string()), "{text}");
        assert!(entries.iter().all(|entry| entry["members"].is_array()), "{text}");
    }

    /// The plain preset listing names every preset with the `@` its argument needs.
    #[test]
    fn presets_can_be_listed_as_text() {
        let mut host = Sink::default();

        let code = list(
            &mut host,
            &args(Utf8PathBuf::from("."), ListKind::Presets, false),
            Styler::new(false),
        )
        .expect("list presets");
        let text = host.out();

        assert_eq!(code, EXIT_OK);
        assert_eq!(text.lines().count(), registry::PRESETS.len() + 2, "{text}");

        for preset in registry::PRESETS {
            assert!(
                text.lines()
                    .any(|line| line.contains(&format!("@{}", preset.name)) && line.contains(preset.description)),
                "{} is missing: {text}",
                preset.name
            );
        }
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn the_text_preset_listing_marks_a_preset_selected_by_its_exact_members() {
        let preset = registry::PRESETS
            .iter()
            .find(|preset| preset.name == "boundary")
            .expect("the boundary preset exists");
        let mut listing = args(Utf8PathBuf::from("."), ListKind::Presets, false);
        let ListCommand::Presets(args) = &mut listing.command else {
            panic!("expected preset listing");
        };
        args.mutators = Some(preset.members.join(","));
        let mut host = Sink::default();

        let code = list(&mut host, &listing, Styler::new(false)).expect("list presets");

        assert_eq!(code, EXIT_OK);
        assert!(
            host.out().lines().any(|line| line.starts_with(&format!("* @{}", preset.name))),
            "{}",
            host.out()
        );
        assert!(host.out().contains("* = enabled by the current selection"), "{}", host.out());
    }

    /// Every listing shape treats a closed consumer as successful completion.
    #[test]
    fn a_closed_output_stream_ends_every_listing_successfully() {
        let dir = crate_dir("list-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        for (what, json) in [
            (ListKind::Mutators, true),
            (ListKind::Mutators, false),
            (ListKind::Files, true),
            (ListKind::Files, false),
            (ListKind::Mutants, true),
            (ListKind::Mutants, false),
            (ListKind::Presets, true),
            (ListKind::Presets, false),
        ] {
            let code = list(&mut BrokenHost, &args(root.clone(), what, json), Styler::new(false)).expect("closed pipe");

            assert_eq!(code, EXIT_OK, "{what:?} json={json}");
        }
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn the_population_can_be_written_as_a_report() {
        // `merge` withdraws a retired mutant only against an unsharded population, and a rotation
        // that could afford a full run would not be sharding in the first place.
        let dir = crate_dir("list-population-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("population.json");
        let mut host = Sink::default();
        let mut listing = args(root, ListKind::Mutants, false);

        let ListCommand::Mutants(args) = &mut listing.command else {
            panic!("expected mutant listing");
        };
        args.json_report = Some(path.clone());

        let code = list(&mut host, &listing, Styler::new(false)).expect("list");
        let text = fs::read_to_string(&path).expect("report");
        let report: crate::elements::Report = from_str(&text).expect("json");

        assert_eq!(code, EXIT_OK);
        let run = report.config.as_ref().unwrap();
        assert!(run.shard.is_none(), "{text}");
        assert_eq!(run.mutant_id_version, Some(crate::model::MUTANT_ID_VERSION));
        assert!(!run.merged);
        assert!(
            report.files.values().any(|file| !file.mutants.is_empty()),
            "the population is empty: {text}"
        );
        assert_eq!(String::from_utf8(host.err).unwrap(), format!("Wrote {path}\n"));

        let ListCommand::Mutants(args) = &mut listing.command else {
            panic!("expected mutant listing");
        };
        args.select.name_patterns.push(": less: ".to_owned());
        list(&mut Sink::default(), &listing, Styler::new(false)).expect("write a filtered population");
        let filtered: crate::elements::Report = from_str(&fs::read_to_string(&path).expect("filtered report")).expect("filtered JSON");
        assert!(filtered.config.as_ref().is_some_and(|run| run.name_filtered));
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_sharded_population_says_which_shard_it_is() {
        // A shard's silence about a mutant is not evidence that the mutant is gone, so the merge
        // has to be able to tell the two kinds of listing apart.
        let dir = crate_dir("list-population-shard-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("population.json");
        let mut host = Sink::default();
        let mut listing = args(root, ListKind::Mutants, false);

        let ListCommand::Mutants(args) = &mut listing.command else {
            panic!("expected mutant listing");
        };
        args.json_report = Some(path.clone());
        args.select.shard_count = Some(4);
        args.select.shard_index = Some(2);

        let code = list(&mut host, &listing, Styler::new(false)).expect("list");
        let text = fs::read_to_string(&path).expect("report");
        let report: crate::elements::Report = from_str(&text).expect("json");
        let shard = report.config.as_ref().and_then(|run| run.shard.as_ref()).expect("shard");

        assert_eq!(code, EXIT_OK);
        assert_eq!((shard.index, shard.count), (2, 4));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod name_filter_tests {
    use super::selected_files;
    use crate::discover::{Plan, TargetFile};

    #[test]
    fn listed_name_and_selected_files_follow_the_filtered_population() {
        let mutant = crate::model::Mutant {
            item_path: "less".to_owned().into(),
            ..crate::fixtures::mutant()
        };
        let name = mutant.selection_name();
        assert!(name.contains(": less: "), "{name}");

        let files = ["src/lib.rs", "src/other.rs"].map(|path| TargetFile {
            path: path.into(),
            absolute: format!("/fixture/{path}").into(),
            package: "subject".to_owned(),
            source: None,
        });
        let mut plan = Plan {
            root: "/fixture".into(),
            files: files.into(),
            mutants: vec![mutant],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: crate::HashMap::default(),
            skipped: Vec::new(),
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };

        let selected: Vec<_> = selected_files(&plan, true).map(|file| file.path.as_str()).collect();
        assert_eq!(selected, ["src/lib.rs"]);
        assert_eq!(selected_files(&plan, false).count(), 2);

        plan.mutants.clear();
        assert!(selected_files(&plan, true).next().is_none());
    }
}
