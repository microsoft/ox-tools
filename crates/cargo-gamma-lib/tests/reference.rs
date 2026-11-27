// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

//! Package-local documentation contracts.

use cargo_gamma_lib::internals::docs;

/// Pins one row of each generated block against an expectation nothing generated.
///
/// The checkout's blessing test compares the README to the generator, and the blessing command rewrites
/// the README from that same generator. Together they catch drift and nothing else: a generator
/// that renders a name wrongly, drops the alias column, or stops escaping a pipe can agree with
/// both sides of that comparison and be copied into the published reference.
///
/// These expectations were typed by hand from what the tables are supposed to say, so a rendering
/// change has to be defended here rather than copied mechanically. One representative row from
/// each generated block keeps the oracle independent without duplicating the complete tables.
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
        presets.contains("| `@boundary` | relational and boundary conditions | `relational`, `range`, `ordering` |"),
        "the preset row is not rendered as expected:\n{presets}"
    );
    assert!(
        families.contains("| [`logical`](#logical) | 6 | Is this `&&` really an `&&`? |"),
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
