// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use serde_json::Value;

use super::cli::{ListArgs, ListKind};
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::error::error;
use crate::exec::CargoOptions;
use crate::ops::registry;
use crate::report::Styler;

/// Implements `list`.
#[cfg(test)]
pub(super) fn list<H: Host>(host: &mut H, args: &ListArgs, styler: Styler) -> crate::Result<i32> {
    let config = crate::config::Config::resolve(&args.select)?;
    let cargo = config.cargo_options();

    list_with_cargo(host, args, styler, &cargo)
}

/// Implements `list` with the configuration generation dispatch already resolved.
pub(super) fn list_with_cargo<H: Host>(host: &mut H, args: &ListArgs, styler: Styler, cargo: &CargoOptions) -> crate::Result<i32> {
    match args.what {
        ListKind::Mutators => list_mutators(host, args),
        ListKind::Files => list_files(host, args, styler, cargo),
        ListKind::Mutants => list_mutants(host, args, styler, cargo),
        ListKind::Presets => list_presets(host, args),
    }
}

/// Lists the named mutator presets.
///
/// The selection is resolved against each preset so the listing says which one you are actually
/// running, rather than making you match `--mutators` against the table by eye.
fn list_presets<H: Host>(host: &mut H, args: &ListArgs) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let mut stream = host.results();

    if args.json {
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

        writeln!(
            stream,
            "{}",
            serde_json::to_string_pretty(&entries).map_err(|cause| { error!("could not serialize the presets").caused_by(cause) })?
        )?;

        return Ok(EXIT_OK);
    }

    let width = registry::PRESETS.iter().map(|preset| preset.name.len() + 1).max().unwrap_or(0);

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
fn list_mutators<H: Host>(host: &mut H, args: &ListArgs) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let mut stream = host.results();

    if args.json {
        let entries: Vec<Value> = registry::REGISTRY
            .iter()
            .map(|mutator| {
                serde_json::json!({
                    "name": mutator.name,
                    "description": mutator.description,
                    "default": mutator.default_on,
                    "enabled": selection.contains(mutator.name),
                    "aliases": mutator.aliases,
                })
            })
            .collect();

        writeln!(
            stream,
            "{}",
            serde_json::to_string_pretty(&entries)
                .map_err(|cause| { error!("could not serialize the mutator registry").caused_by(cause) })?
        )?;

        return Ok(EXIT_OK);
    }

    let width = registry::REGISTRY.iter().map(|m| m.name.len()).max().unwrap_or(0);

    for mutator in registry::REGISTRY {
        let mark = if selection.contains(mutator.name) { "*" } else { " " };

        writeln!(stream, "{mark} {:width$}  {}", mutator.name, mutator.description)?;
    }

    writeln!(stream)?;
    writeln!(stream, "* = enabled by the current selection")?;

    Ok(EXIT_OK)
}

/// Lists the files that would be analyzed.
fn list_files<H: Host>(host: &mut H, args: &ListArgs, styler: Styler, cargo: &CargoOptions) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let plan = crate::discover::plan_for_build(&args.select, &selection, args.select.shard()?, cargo, &mut |_| {})?;

    crate::report::skipped(host, &plan, styler)?;

    let mut stream = host.results();

    if args.json {
        let paths: Vec<&Utf8PathBuf> = plan.files.iter().map(|file| &file.path).collect();

        writeln!(
            stream,
            "{}",
            serde_json::to_string_pretty(&paths).map_err(|cause| error!("could not serialize the file list").caused_by(cause))?
        )?;

        return Ok(EXIT_OK);
    }

    for file in &plan.files {
        writeln!(stream, "{}", file.path)?;
    }

    Ok(EXIT_OK)
}

/// Lists the mutants that would be generated.
fn list_mutants<H: Host>(host: &mut H, args: &ListArgs, styler: Styler, cargo: &CargoOptions) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let shard = args.select.shard()?;
    let plan = crate::discover::plan_for_build(&args.select, &selection, shard, cargo, &mut |_| {})?;

    crate::report::skipped(host, &plan, styler)?;

    if let Some(path) = args.json_report.as_ref() {
        write_population(host, &plan, shard, path)?;
    }

    let mut stream = host.results();

    if args.json {
        writeln!(
            stream,
            "{}",
            serde_json::to_string_pretty(&plan.mutants).map_err(|cause| error!("could not serialize the mutant list").caused_by(cause))?
        )?;

        return Ok(EXIT_OK);
    }

    for mutant in &plan.mutants {
        writeln!(stream, "{}", describe_for_listing(mutant))?;
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

/// Describes one mutant for the plain listing, marking the ones a run will not test.
///
/// A suppressed mutant stays in the population so reports can show what was skipped and why, so
/// without the mark the listing would read as a promise to test every line it prints.
fn describe_for_listing(mutant: &crate::model::Mutant) -> String {
    let Some(channel) = mutant
        .suppression
        .as_ref()
        .filter(|_| mutant.outcome == crate::model::Outcome::Ignored)
        .map(|suppression| suppression.channel.as_str())
    else {
        return mutant.describe();
    };

    format!("{} [suppressed: {channel}]", mutant.describe())
}

/// Writes the listing as a report document.
///
/// `merge` withdraws a mutant only when a newer unsharded input states the whole population of its
/// file, and producing that from a run means paying for a run. Listing is the cheap way to say what
/// exists now, so it is the one a nightly rotation can afford beside its shard.
fn write_population<H: Host>(
    host: &mut H,
    plan: &crate::discover::Plan,
    shard: Option<(u32, u32)>,
    path: &Utf8PathBuf,
) -> crate::Result<()> {
    let info = crate::elements::RunInfo {
        started_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs()),
        merged: false,
        shard: shard.map(|(count, index)| crate::elements::ShardInfo { index, count }),
        tests: None,
        // Filled in by `build` from the plan it is given, so that it cannot disagree with the
        // mutants in the same report.
        not_built: None,
        dropped_test_packages: Vec::new(),
        merge_provenance: None,
    };

    let report = crate::elements::build(plan, crate::elements::Thresholds::default(), Some(info))?;

    crate::elements::write_json(&report, path)?;
    writeln!(host.error(), "Wrote {path}")?;

    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::fs;

    use super::*;
    use crate::testing::{BrokenHost, Sink};

    fn crate_dir(name: &str) -> tempfile::TempDir {
        crate::fixtures::crate_dir(name, "pub fn less(a: i32, b: i32) -> bool { a < b }\n").0
    }

    fn args(dir: Utf8PathBuf, what: ListKind, json: bool) -> ListArgs {
        ListArgs {
            what,
            select: crate::commands::SelectArgs {
                dir,
                ..crate::commands::SelectArgs::default()
            },
            json,
            json_report: None,
        }
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
        let value: Value = serde_json::from_str(&text).expect("json");

        assert_eq!(code, EXIT_OK);
        assert!(value.as_array().is_some_and(|entries| !entries.is_empty()), "{text}");
        assert!(text.contains("\"enabled\""), "{text}");
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
        let value: Value = serde_json::from_str(&text).expect("the --json file listing must be JSON");

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
        let value: Value = serde_json::from_str(&text).expect("the --json mutant listing must be JSON");

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
    fn presets_can_be_listed_as_json_with_the_selection_resolved_against_each() {
        let mut listing = args(Utf8PathBuf::from("."), ListKind::Presets, true);
        let mut host = Sink::default();

        listing.select.mutators = Some("relational,range".to_owned());

        let code = list(&mut host, &listing, Styler::new(false)).expect("list presets");
        let text = String::from_utf8(host.out).expect("utf-8");
        let value: Value = serde_json::from_str(&text).expect("json");
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
    fn the_text_preset_listing_marks_a_preset_selected_by_its_exact_members() {
        let preset = registry::PRESETS
            .iter()
            .find(|preset| preset.name == "boundary")
            .expect("the boundary preset exists");
        let mut listing = args(Utf8PathBuf::from("."), ListKind::Presets, false);
        listing.select.mutators = Some(preset.members.join(","));
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
    fn the_population_can_be_written_as_a_report() {
        // `merge` withdraws a retired mutant only against an unsharded population, and a rotation
        // that could afford a full run would not be sharding in the first place.
        let dir = crate_dir("list-population-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("population.json");
        let mut host = Sink::default();
        let mut listing = args(root, ListKind::Mutants, false);

        listing.json_report = Some(path.clone());

        let code = list(&mut host, &listing, Styler::new(false)).expect("list");
        let text = fs::read_to_string(&path).expect("report");
        let report: crate::elements::Report = serde_json::from_str(&text).expect("json");

        assert_eq!(code, EXIT_OK);
        assert!(report.config.as_ref().is_some_and(|run| run.shard.is_none()), "{text}");
        assert!(
            report.files.values().any(|file| !file.mutants.is_empty()),
            "the population is empty: {text}"
        );
        assert!(
            String::from_utf8(host.err).expect("utf-8").contains("Wrote"),
            "the path was not echoed"
        );
    }

    #[test]
    fn a_sharded_population_says_which_shard_it_is() {
        // A shard's silence about a mutant is not evidence that the mutant is gone, so the merge
        // has to be able to tell the two kinds of listing apart.
        let dir = crate_dir("list-population-shard-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("population.json");
        let mut host = Sink::default();
        let mut listing = args(root, ListKind::Mutants, false);

        listing.json_report = Some(path.clone());
        listing.select.shard_count = Some(4);
        listing.select.shard_index = Some(2);

        let code = list(&mut host, &listing, Styler::new(false)).expect("list");
        let text = fs::read_to_string(&path).expect("report");
        let report: crate::elements::Report = serde_json::from_str(&text).expect("json");
        let shard = report.config.as_ref().and_then(|run| run.shard.as_ref()).expect("shard");

        assert_eq!(code, EXIT_OK);
        assert_eq!((shard.index, shard.count), (2, 4));
    }
}
