// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(
    not(feature = "internals"),
    expect(
        dead_code,
        reason = "this module exists for `tests/docs.rs`, which is what keeps the README's generated tables honest;                   nothing the tool does at run time reads them, so without the feature that opens the facade the                   module has no caller"
    )
)]

//! Reference tables for the documentation, rendered from the registry that defines them.
//!
//! The mutator catalog is the tool's public vocabulary: the same names appear on `--mutators`, in every
//! suppression directive, in the report, in SARIF rule identifiers and in configuration. A
//! reference that drifts from the registry is therefore worse than no reference at all, because a
//! reader who copies a name out of it gets a usage error and no clue that the document was wrong.
//!
//! So the tables are generated here and checked against the README by a test. Adding a
//! mutator fails that test until the document is regenerated, which is the only arrangement that
//! keeps a hand-written catalog honest as the catalog grows.

use core::fmt::Write as _;

use crate::ops::registry::{PRESETS, REGISTRY, families};

/// The marker that opens a generated block in a documentation file.
///
/// The blocks are delimited rather than owning the whole file so that the prose explaining what a
/// family is *for* can live beside the table listing what it contains. A reference that is only a
/// table tells a reader what exists without telling them when to reach for it.
pub const BEGIN: &str = "<!-- begin generated: ";

/// The marker that closes a generated block.
pub const END: &str = "<!-- end generated -->";

/// Renders the block named `name`, or `None` when no such block exists.
#[must_use]
pub fn block(name: &str) -> Option<String> {
    match name {
        "mutators" => Some(mutators()),
        "presets" => Some(presets()),
        "families" => Some(family_summary()),
        "commands" => Some(commands()),
        "options" => Some(options()),
        _ => None,
    }
}

/// One row per subcommand, with the one-line summary clap prints.
fn commands() -> String {
    let command = <crate::commands::Cli as clap::CommandFactory>::command();
    let mut out = String::new();

    let _ = writeln!(out, "| Command | What it does |");
    let _ = writeln!(out, "| --- | --- |");

    for sub in command.get_subcommands() {
        let about = sub.get_about().map(ToString::to_string).unwrap_or_default();

        let _ = writeln!(
            out,
            "| [`gamma {}`](#gamma-{}) | {} |",
            sub.get_name(),
            sub.get_name(),
            escape(&about)
        );
    }

    out.trim_end().to_owned()
}

/// Every option of every subcommand, grouped by the help heading it carries.
///
/// Rendered from the same `clap` definition that answers `--help`, so an option cannot be added
/// without appearing here, and the categories a reader sees in the terminal are the categories
/// they see in the reference.
fn options() -> String {
    let command = <crate::commands::Cli as clap::CommandFactory>::command();
    let mut out = String::new();

    // The globals are declared on the root rather than on any subcommand, so walking the
    // subcommands alone would silently omit the two options that apply to all of them.
    let _ = writeln!(out, "### Accepted by every subcommand\n");

    for (heading, arguments) in grouped(&command) {
        let _ = writeln!(out, "**{heading}**\n");
        let _ = writeln!(out, "| Option | Value | What it does |");
        let _ = writeln!(out, "| --- | --- | --- |");

        for argument in arguments {
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                spelling(argument),
                value_of(argument),
                escape(&help_of(argument))
            );
        }

        out.push('\n');
    }

    for sub in command.get_subcommands() {
        let _ = writeln!(out, "### `gamma {}`\n", sub.get_name());

        if let Some(about) = sub.get_about() {
            let _ = writeln!(out, "{about}\n");
        }

        let _ = writeln!(out, "```text\n{}\n```\n", usage(sub));

        for (heading, arguments) in grouped(sub) {
            let _ = writeln!(out, "**{heading}**\n");
            let _ = writeln!(out, "| Option | Value | What it does |");
            let _ = writeln!(out, "| --- | --- | --- |");

            for argument in arguments {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} |",
                    spelling(argument),
                    value_of(argument),
                    escape(&help_of(argument))
                );
            }

            out.push('\n');
        }
    }

    out.trim_end().to_owned()
}

/// The usage line clap would print for `sub`, spelled the way a user types it.
fn usage(sub: &clap::Command) -> String {
    let mut sub = sub.clone();
    let rendered = sub.render_usage().to_string().replace("Usage: ", "");

    format!("cargo gamma {rendered}")
}

/// The arguments of `sub`, grouped by help heading in the order the headings first appear.
///
/// `--help` and `--version` are dropped: they are on every command, they are not what a reference
/// is consulted for, and a row for each would be nine rows of noise.
fn grouped(sub: &clap::Command) -> Vec<(String, Vec<&clap::Arg>)> {
    let mut groups: Vec<(String, Vec<&clap::Arg>)> = Vec::new();

    for argument in sub.get_arguments() {
        if matches!(argument.get_id().as_str(), "help" | "version") || argument.is_hide_set() {
            continue;
        }

        let heading = if argument.is_positional() {
            "Arguments".to_owned()
        } else {
            argument.get_help_heading().map_or_else(|| "Options".to_owned(), ToOwned::to_owned)
        };

        if let Some(slot) = groups.iter_mut().find(|(name, _)| *name == heading) {
            slot.1.push(argument);
        } else {
            groups.push((heading, vec![argument]));
        }
    }

    groups
}

/// How an argument is written on the command line, shorts included.
fn spelling(argument: &clap::Arg) -> String {
    let Some(long) = argument.get_long() else {
        return format!("`<{}>`", argument.get_id().as_str().to_uppercase());
    };

    argument
        .get_short()
        .map_or_else(|| format!("`--{long}`"), |short| format!("`-{short}`, `--{long}`"))
}

/// The placeholder an argument takes, or a blank cell for a flag.
fn value_of(argument: &clap::Arg) -> String {
    if matches!(argument.get_action(), clap::ArgAction::SetTrue | clap::ArgAction::SetFalse) {
        return String::new();
    }

    argument
        .get_value_names()
        .and_then(<[clap::builder::Str]>::first)
        .map_or_else(String::new, |name| format!("`<{name}>`"))
}

/// The one-line help for an argument, with the default appended when there is one.
///
/// clap strips the full stop from the end of a doc comment when it renders short help, so it is
/// put back before anything is appended — otherwise the default runs straight into the sentence.
fn help_of(argument: &clap::Arg) -> String {
    let mut text = argument.get_help().map(ToString::to_string).unwrap_or_default().replace('\n', " ");

    if !text.is_empty() && !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }

    let defaults = argument.get_default_values();

    if defaults.is_empty() {
        return text;
    }

    let shown = defaults
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");

    format!("{text} Defaults to `{shown}`.")
}

/// Every mutator, grouped by family, with its alias and default state.
fn mutators() -> String {
    let mut out = String::new();

    for family in families() {
        let members: Vec<_> = REGISTRY
            .iter()
            .filter(|mutator| mutator.name.split('.').next() == Some(family))
            .collect();

        let _ = writeln!(out, "#### `{family}`\n");
        let _ = writeln!(out, "| Mutator | What it does | Alias | Default |");
        let _ = writeln!(out, "| --- | --- | --- | --- |");

        for mutator in members {
            let aliases = if mutator.aliases.is_empty() {
                String::new()
            } else {
                format!("`{}`", mutator.aliases.join("`, `"))
            };

            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                mutator.name,
                escape(mutator.description),
                aliases,
                if mutator.default_on { "yes" } else { "no" }
            );
        }

        out.push('\n');
    }

    out.trim_end().to_owned()
}

/// Every mutator preset, with the selectors it expands to.
fn presets() -> String {
    let mut out = String::new();

    let _ = writeln!(out, "| Mutator preset | What it selects | Expands to |");
    let _ = writeln!(out, "| --- | --- | --- |");

    for preset in PRESETS {
        let members = preset
            .members
            .iter()
            .map(|member| format!("`{member}`"))
            .collect::<Vec<_>>()
            .join(", ");

        let _ = writeln!(out, "| `@{}` | {} | {members} |", preset.name, escape(preset.description));
    }

    out.trim_end().to_owned()
}

/// One row per family, with how many mutators it holds.
fn family_summary() -> String {
    let mut out = String::new();

    let _ = writeln!(out, "| Family | Mutators | What it asks |");
    let _ = writeln!(out, "| --- | ---: | --- |");

    for family in families() {
        let count = REGISTRY
            .iter()
            .filter(|mutator| mutator.name.split('.').next() == Some(family))
            .count();

        let _ = writeln!(out, "| [`{family}`](#{family}) | {count} | {} |", question(family));
    }

    let _ = writeln!(out, "| **Total** | **{}** | |", REGISTRY.len());

    out.trim_end().to_owned()
}

/// The question a family exists to ask, in the reader's terms rather than the mutator's.
///
/// A description of the transform — "replace `<` with `<=`" — says what the tool does, which the
/// per-mutator table already covers. What a reader choosing between families needs is what a
/// survivor in that family would mean about their tests, which is a different sentence.
fn question(family: &str) -> &'static str {
    match family {
        "fn_value" => "Does anything check what this function returns?",
        "relational" => "Is this comparison's boundary the right one?",
        "arith" => "Does this calculation's operator matter?",
        "bitwise" => "Is this mask or flag combination correct?",
        "shift" => "Is this shift's direction load-bearing?",
        "assign" => "Does this compound assignment's operator matter?",
        "assign_value" => "Is the value assigned here ever read in a way that would notice?",
        "logical" => "Is this `&&` really an `&&`?",
        "cond" => "Does anything depend on this branch being taken?",
        "match_guard" => "Does anything depend on this guard being right?",
        "match_arm" => "Is this arm reachable, and does anything notice when it stops matching?",
        "loop" => "Does this `break` or `continue` carry the loop's meaning?",
        "range" => "Is this bound inclusive on purpose?",
        "literal" => "Does this constant's exact value matter?",
        "expr" => "Would an off-by-one here be caught?",
        "unary" => "Does this negation or complement matter?",
        "stmt" => "Does this statement's side effect matter?",
        "struct_field" => "Does this field's value matter, or is the default good enough?",
        "option" => "Is the present case distinguished from the absent one?",
        "result" => "Is success distinguished from failure?",
        "iter" => "Does anything observe that this was ordered, deduplicated, or taken from one end?",
        "string" => "Does the prefix, the case, or the trimmed end actually matter?",
        "collection" => "Does every element of this literal earn its place?",
        _ => "",
    }
}

/// Escapes the characters that would otherwise end a table cell.
fn escape(text: &str) -> String {
    text.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(
        miri,
        ignore = "renders the whole mutator table and sweeps the registry against it; a table sweep over safe code tells Miri nothing"
    )]
    fn every_registered_mutator_appears_in_the_mutator_table() {
        // The table is the tool's published vocabulary. A name missing from it is a feature the
        // user cannot discover, and a name in it that the registry does not have is worse: it
        // reads as usable and produces a usage error.
        let rendered = mutators();

        for mutator in REGISTRY {
            assert!(
                rendered.contains(mutator.name),
                "`{}` is missing from the mutator table",
                mutator.name
            );
        }
    }

    #[test]
    fn every_family_is_given_a_question_to_ask() {
        // A blank cell in the summary would be the one row a reader skips, and it would be skipped
        // for the newest family — the one most in need of an explanation.
        for family in families() {
            assert!(
                !question(family).is_empty(),
                "family `{family}` has no question in the summary table"
            );
        }
    }

    #[test]
    fn a_description_containing_a_pipe_cannot_break_the_table() {
        // `bitwise.or_to_and` and friends describe themselves with `|`, which would otherwise end
        // the cell and silently shift every column after it.
        assert_eq!(escape("replace | with &"), "replace \\| with &");
    }

    #[test]
    fn an_unrecognized_family_name_is_given_no_question_rather_than_a_guess() {
        // `question` is keyed by family name, not driven by the registry, so a name that never
        // shipped as a family — a typo, or a check performed before the family exists — must fall
        // through to an empty string instead of panicking or fabricating a plausible-sounding
        // question that would mislead a reader.
        assert_eq!(question("nonesuch"), "");
    }

    #[test]
    fn an_unknown_block_name_is_refused_rather_than_rendered_empty() {
        // A misspelled marker that produced an empty block would delete a whole table from the
        // documentation and pass every check that follows.
        assert!(block("mutators").is_some());
        assert!(block("nonesuch").is_none());
    }
}
