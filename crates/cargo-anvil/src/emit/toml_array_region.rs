// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-span-based placement of regions inside TOML arrays.

use ohno::app_err;
use toml_edit::{Array, Document, DocumentMut, Item, Table};

use crate::catalog::TomlArrayRegionSpec;
use crate::emit::managed_region::{ManagedRegionRefusal, ManagedRegionRequest, RefusalRemedy, plan_region_with_splice};
use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::{CommentSyntax, RegionPlacement, canonical_value, find_region, managed_region_ids, text_newline};

fn refusal(reason: impl std::fmt::Display, remedy: RefusalRemedy) -> ManagedRegionRefusal {
    ManagedRegionRefusal::new(app_err!("{reason}"), remedy)
}

fn lookup<'a>(mut item: &'a Item, path: &[String]) -> Option<&'a Item> {
    for key in path {
        item = item.get(key)?;
    }
    Some(item)
}

/// Validate the template independently of repository state.
pub(crate) fn validate_spec(spec: &TomlArrayRegionSpec) -> Result<(), ManagedRegionRefusal> {
    if spec.path.is_empty() || spec.region.syntax != CommentSyntax::Hash {
        return Err(refusal(
            "a TOML array region requires a nonempty key path and hash comment syntax",
            RefusalRemedy::InvalidGeneratedToml,
        ));
    }
    body_array(&spec.region.body)?;
    Ok(())
}

fn body_array(body: &str) -> Result<Array, ManagedRegionRefusal> {
    let source = format!("entries = [\n{body}\n]");
    let document = source
        .parse::<DocumentMut>()
        .map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedToml))?;
    let array = document["entries"]
        .as_array()
        .ok_or_else(|| refusal("the region must contain only array entries", RefusalRemedy::InvalidGeneratedToml))?;
    if document.as_table().len() != 1 {
        return Err(refusal(
            "the region must contain only array entries",
            RefusalRemedy::InvalidGeneratedToml,
        ));
    }
    if !array.is_empty() && !array.trailing_comma() {
        return Err(refusal(
            "TOML array-entry bodies must end with a trailing comma",
            RefusalRemedy::InvalidGeneratedToml,
        ));
    }
    Ok(array.clone())
}

fn render_body(body: &str, newline: &str) -> Result<String, ManagedRegionRefusal> {
    const PREFIX: &str = "entries = [\n";
    let source = format!("{PREFIX}{body}\n]");
    let document = Document::parse(source).map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedToml))?;
    let spans: Vec<_> = document["entries"]
        .as_array()
        .expect("the template was validated as an array")
        .iter()
        .map(|value| value.span().expect("immutable parsed values retain their source spans"))
        .collect();
    let mut rendered = String::new();
    let mut offset = PREFIX.len();
    for line in body.split_inclusive('\n') {
        // Indent entry/comment lines, but not continuations inside values:
        // indentation in a multiline string is data, not TOML decoration.
        let in_value = spans.iter().any(|span| span.start < offset && offset < span.end);
        let content = line.trim_end_matches(['\r', '\n']);
        if in_value || !content.trim().is_empty() {
            if !in_value {
                rendered.push_str("  ");
            }
            rendered.push_str(content);
        }
        rendered.push_str(newline);
        offset += line.len();
    }
    Ok(rendered)
}

/// Create only missing table/array scaffolding, leaving existing items intact.
fn scaffold(text: &str, path: &[String]) -> Result<String, ManagedRegionRefusal> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let mut item = document.as_item_mut();
    for (index, key) in path.iter().enumerate() {
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| refusal("the array's parent is not a TOML table", RefusalRemedy::HandWrittenTable))?;
        if !table.contains_key(key) {
            table.insert(
                key,
                if index + 1 == path.len() {
                    Item::Value(Array::new().into())
                } else {
                    Item::Table(Table::new())
                },
            );
        }
        item = table.get_mut(key).expect("the key was present or inserted above");
    }
    if item.as_array().is_none() {
        return Err(refusal("the selected TOML item is not an array", RefusalRemedy::HandWrittenTable));
    }
    Ok(document.to_string())
}

/// Plan through the ordinary checksum/edited-body policy with an array splice.
pub(crate) fn plan_toml_array_region(
    manifest: &Manifest,
    host_text: Option<&str>,
    host: &str,
    spec: &TomlArrayRegionSpec,
) -> Result<PlanItem, ManagedRegionRefusal> {
    validate_spec(spec)?;
    let original = host_text.unwrap_or("");
    let document = Document::parse(original).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let scaffolded;
    let base = if lookup(document.as_item(), &spec.path).is_some() {
        original
    } else {
        scaffolded = scaffold(original, &spec.path)?;
        &scaffolded
    };
    let document = Document::parse(base).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let array = lookup(document.as_item(), &spec.path)
        .and_then(Item::as_array)
        .ok_or_else(|| refusal("the selected TOML item is not an array", RefusalRemedy::HandWrittenTable))?;
    let span = array.span().expect("an immutable parsed array retains its source span");
    for id in managed_region_ids(base, CommentSyntax::Hash) {
        if id == spec.region.id.as_str() {
            continue;
        }
        let other = find_region(base, &id, CommentSyntax::Hash).map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
        if let Some(other) = other
            && span.start >= other.body.start
            && span.start < other.body.end
        {
            return Err(refusal(
                format!(
                    "the selected array belongs to managed region '{id}'; retire its enclosing ownership before managing array entries"
                ),
                RefusalRemedy::BetweenManagedRegions,
            ));
        }
    }
    let region =
        find_region(base, spec.region.id.as_str(), CommentSyntax::Hash).map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
    if let Some(region) = &region {
        if region.start_line.start <= span.start || region.end_line.end > span.end {
            return Err(refusal(
                "the managed region is outside its selected TOML array",
                RefusalRemedy::MalformedMarkers,
            ));
        }
        // A whole-value boundary is required: markers inside a multiline string
        // or a nested value must not claim part of that value.
        if array.iter().any(|value| {
            let value = value.span().expect("parsed array values retain source spans");
            (value.start < region.body.start && value.end > region.body.start)
                || (value.start < region.body.end && value.end > region.body.end)
        }) {
            return Err(refusal("the region splits a TOML array value", RefusalRemedy::MalformedMarkers));
        }
    }
    let newline = text_newline(original);
    let body = render_body(&spec.region.body, newline)?;
    let rendered = format!(
        "  # >>> anvil-managed: {}{newline}{body}  # <<< anvil-managed: {}{newline}",
        spec.region.id, spec.region.id
    );
    let request = ManagedRegionRequest {
        host_relpath: host,
        region_id: spec.region.id.as_str(),
        rendered_body: &body,
        syntax: CommentSyntax::Hash,
        placement: RegionPlacement::End,
        newline: Some(newline),
    };
    plan_region_with_splice(manifest, host_text, request, || {
        let spliced = if let Some(region) = region {
            format!("{}{}{}", &base[..region.start_line.start], rendered, &base[region.end_line.end..])
        } else {
            let adopted = adopt_entries(base, array, &spec.region.body)?;
            let at = span.start + 1;
            let rest = &adopted[at..];
            let rest = rest.strip_prefix(newline).unwrap_or(rest);
            format!("{}{newline}{rendered}{rest}", &adopted[..at])
        };
        Document::parse(&spliced).map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedToml))?;
        find_region(&spliced, spec.region.id.as_str(), CommentSyntax::Hash)
            .map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
        Ok(spliced)
    })
}

/// Remove one matching unmanaged value for each generated entry, not comments
/// or other regions. Array punctuation is bounded by parser-provided spans.
fn adopt_entries(text: &str, array: &Array, body: &str) -> Result<String, ManagedRegionRefusal> {
    let generated = body_array(body)?;
    let protected = managed_region_ids(text, CommentSyntax::Hash)
        .into_iter()
        .map(|id| find_region(text, &id, CommentSyntax::Hash))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
    let values: Vec<_> = array.iter().collect();
    let mut removals = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for generated in &generated {
        let candidate = values.iter().enumerate().find(|(index, value)| {
            let span = value.span().expect("parsed array values retain source spans");
            !used.contains(index)
                && !protected
                    .iter()
                    .flatten()
                    .any(|region| span.start < region.end_line.end && region.start_line.start < span.end)
                && canonical_value(generated) == canonical_value(value)
        });
        if let Some((index, value)) = candidate {
            used.insert(index);
            let span = value.span().expect("parsed array values retain source spans");
            let next = values
                .get(index + 1)
                .and_then(|value| value.span())
                .map_or_else(|| array.span().expect("parsed array retains its span").end - 1, |span| span.start);
            if let Some(comma) = separator_comma(&text[span.end..next]) {
                removals.push(span.end + comma..span.end + comma + 1);
            }
            removals.push(span);
        }
    }
    removals.sort_by_key(|range| range.start);
    let mut adopted = text.to_owned();
    for range in removals.into_iter().rev() {
        adopted.replace_range(range, "");
    }
    Ok(adopted)
}

fn separator_comma(gap: &str) -> Option<usize> {
    let mut comment = false;
    for (index, character) in gap.char_indices() {
        match character {
            '\n' => comment = false,
            '#' => comment = true,
            ',' if !comment => return Some(index),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::catalog::{Artifact, Catalog, HostSelector, RegionId, RegionSpec};
    use crate::checksum::checksum_str;
    use crate::decision::Decision;
    use crate::plan::{Plan, Target};
    use crate::region::remove_region;

    const BODY: &str = "# Guidance.\n\"managed\",\n";
    const REGION: &str = "  # >>> anvil-managed: entries\n  # Guidance.\n  \"managed\",\n  # <<< anvil-managed: entries\n";
    const FRESH: &str =
        "[plugins]\ndefault = [\n  # >>> anvil-managed: entries\n  # Guidance.\n  \"managed\",\n  # <<< anvil-managed: entries\n]\n";

    fn spec() -> TomlArrayRegionSpec {
        TomlArrayRegionSpec {
            region: RegionSpec {
                host: HostSelector::Path("config.toml".to_owned()),
                id: RegionId::new("entries"),
                body: BODY.to_owned(),
                syntax: CommentSyntax::Hash,
            },
            path: vec!["plugins".to_owned(), "default".to_owned()],
        }
    }

    fn plan(text: Option<&str>) -> PlanItem {
        plan_toml_array_region(&Manifest::default(), text, "config.toml", &spec()).unwrap()
    }

    fn output(text: &str) -> String {
        plan(Some(text)).spliced_host.unwrap()
    }

    #[test]
    fn absent_host_creates_unowned_scaffold_and_region_only_lock_entry() {
        let item = plan(None);
        assert_eq!(item.decision, Decision::Write);
        assert_eq!(
            item.target,
            Target::Region {
                host: "config.toml".to_owned(),
                id: "entries".to_owned()
            }
        );
        assert_eq!(item.spliced_host.as_deref(), Some(FRESH));
        assert_eq!(item.rendered.as_deref(), Some("  # Guidance.\n  \"managed\",\n"));
        assert_eq!(item.rendered_checksum, Some(checksum_str("  # Guidance.\n  \"managed\",\n")));
        let mut plan = Plan::default();
        plan.push(item);
        assert_eq!(plan.dry_run_exit_code(), 1);
        let manifest = plan.projected_manifest(&Manifest::default());
        assert_eq!(
            manifest.region_checksum("config.toml", "entries"),
            Some(checksum_str("  # Guidance.\n  \"managed\",\n").as_str())
        );
        assert_eq!(manifest.files.len(), 0);
        assert_eq!(manifest.regions.len(), 1);
    }

    #[test]
    fn existing_multiline_array_preserves_comments_settings_and_last_entry_without_comma() {
        let host = "# header\n[plugins]\nmode = true\ndefault = [\n  # User guidance.\n  \"other\"\n]\n[settings]\nvalue = 1\n";
        assert_eq!(
            output(host),
            format!("# header\n[plugins]\nmode = true\ndefault = [\n{REGION}  # User guidance.\n  \"other\"\n]\n[settings]\nvalue = 1\n")
        );
    }

    #[test]
    fn inline_array_is_expanded_without_rewriting_other_bytes() {
        assert_eq!(
            output("plugins.default = [\"other\", 'third'] # keep\n"),
            format!("plugins.default = [\n{REGION}\"other\", 'third'] # keep\n")
        );
    }

    #[test]
    fn empty_array_and_dotted_keys_remain_repository_owned() {
        assert_eq!(output("plugins.default=[]\n"), format!("plugins.default=[\n{REGION}]\n"));
        assert_eq!(
            remove_region(FRESH, "entries", CommentSyntax::Hash).unwrap(),
            "[plugins]\ndefault = [\n]\n"
        );
    }

    #[test]
    fn missing_array_scaffold_preserves_existing_table_and_other_tables() {
        assert_eq!(
            output("[plugins]\nmode = true\n\n[settings]\nvalue = 1\n"),
            format!("[plugins]\nmode = true\ndefault = [\n{REGION}]\n\n[settings]\nvalue = 1\n")
        );
    }

    #[test]
    fn identical_unmanaged_entry_is_adopted_once_with_comments_left_outside_ownership() {
        assert_eq!(
            output("[plugins]\ndefault = [\n  # Repository comment, with comma.\n  'managed',\n  \"other\"\n]\n"),
            format!("[plugins]\ndefault = [\n{REGION}  # Repository comment, with comma.\n  \n  \"other\"\n]\n")
        );
        assert_eq!(
            output("plugins.default = [\"other\", \"managed\"]\n"),
            format!("plugins.default = [\n{REGION}\"other\", ]\n")
        );
        assert_eq!(
            output("plugins.default = ['managed']\n"),
            format!("plugins.default = [\n{REGION}]\n")
        );
    }

    #[test]
    fn identical_entry_owned_by_another_region_is_not_adopted() {
        let other = "# >>> anvil-managed: other\n\"managed\",\n# <<< anvil-managed: other\n";
        assert_eq!(
            output(&format!("plugins.default = [\n{other}]\n")),
            format!("plugins.default = [\n{REGION}{other}]\n")
        );
    }

    #[test]
    fn nested_candidate_enclosing_another_region_is_not_adopted() {
        let host = "items = [[\n# >>> anvil-managed: other\n\"x\",\n# <<< anvil-managed: other\n], \"user\"]\n";
        let mut spec = spec();
        spec.path = vec!["items".to_owned()];
        spec.region.body = "[\"x\"],\n".to_owned();
        let output = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec)
            .unwrap()
            .spliced_host
            .unwrap();
        assert_eq!(
            output,
            "items = [\n  # >>> anvil-managed: entries\n  [\"x\"],\n  # <<< anvil-managed: entries\n[\n# >>> anvil-managed: other\n\"x\",\n# <<< anvil-managed: other\n], \"user\"]\n"
        );
        assert_eq!(Document::parse(&output).unwrap()["items"].as_array().unwrap().len(), 3);
        let other = find_region(&output, "other", CommentSyntax::Hash).unwrap().unwrap();
        assert_eq!(&output[other.body.start..other.body.end], "\"x\",\n");
    }

    #[test]
    fn partially_overlapping_candidate_is_skipped_in_favor_of_unmanaged_match() {
        let host = "items = [[\n# >>> anvil-managed: other\n\"x\"],\n# <<< anvil-managed: other\n[\"x\"], \"user\"]\n";
        let mut spec = spec();
        spec.path = vec!["items".to_owned()];
        spec.region.body = "[\"x\"],\n".to_owned();
        let output = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec)
            .unwrap()
            .spliced_host
            .unwrap();
        assert_eq!(
            output,
            "items = [\n  # >>> anvil-managed: entries\n  [\"x\"],\n  # <<< anvil-managed: entries\n[\n# >>> anvil-managed: other\n\"x\"],\n# <<< anvil-managed: other\n \"user\"]\n"
        );
        assert_eq!(Document::parse(&output).unwrap()["items"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn an_array_owned_by_an_enclosing_region_is_not_given_nested_ownership() {
        let host = "# >>> anvil-managed: enclosing\n[plugins]\ndefault = [\"managed\", \"other\"]\n# <<< anvil-managed: enclosing\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::BetweenManagedRegions
        );
    }
    #[test]
    fn selector_can_use_quoted_keys_and_nested_inline_table_arrays() {
        let mut spec = spec();
        spec.path = vec!["a.b".to_owned(), "x".to_owned()];
        let item = plan_toml_array_region(
            &Manifest::default(),
            Some("\"a.b\" = { x = [\"other\"], y = 1 }\n"),
            "config.toml",
            &spec,
        )
        .unwrap();
        assert_eq!(
            item.spliced_host.unwrap(),
            format!("\"a.b\" = {{ x = [\n{REGION}\"other\"], y = 1 }}\n")
        );
    }

    #[test]
    fn multiple_generated_entries_adopt_semantically_identical_nested_values() {
        let mut spec = spec();
        spec.region.body = "\"managed\",\n{ x = 1, y = [true] },\n".to_owned();
        let item = plan_toml_array_region(
            &Manifest::default(),
            Some("plugins.default = ['managed', { y=[true], x=1 }, \"other\"]\n"),
            "config.toml",
            &spec,
        )
        .unwrap();
        assert_eq!(
            item.spliced_host.unwrap(),
            "plugins.default = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  { x = 1, y = [true] },\n  # <<< anvil-managed: entries\n  \"other\"]\n"
        );
    }

    #[test]
    fn malformed_host_and_non_array_are_refused() {
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins.default = ["), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::HostAlreadyUnparsable);
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins.default = 1\n"), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::HandWrittenTable);
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins = 1\n"), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::HandWrittenTable);
    }

    #[test]
    fn invalid_templates_and_selectors_are_refused_before_planning() {
        for body in ["\"managed\"\n", "\"unterminated,\n", "]\nextra = [\n"] {
            let mut spec = spec();
            spec.region.body = body.to_owned();
            assert_eq!(validate_spec(&spec).unwrap_err().remedy, RefusalRemedy::InvalidGeneratedToml);
        }
        let mut spec = spec();
        spec.path.clear();
        assert_eq!(validate_spec(&spec).unwrap_err().remedy, RefusalRemedy::InvalidGeneratedToml);
        spec.path = vec!["array".to_owned()];
        spec.region.syntax = CommentSyntax::SlashSlash;
        assert_eq!(validate_spec(&spec).unwrap_err().remedy, RefusalRemedy::InvalidGeneratedToml);
    }

    #[test]
    fn blank_template_lines_are_unindented_without_changing_multiline_string_data() {
        let mut spec = spec();
        spec.region.body = "# Copyright.\n# License.\n\n \t\n\"\"\"first\n  \nsecond\"\"\",\n".to_owned();
        let output = plan_toml_array_region(&Manifest::default(), None, "config.toml", &spec)
            .unwrap()
            .spliced_host
            .unwrap();
        assert_eq!(
            output,
            "[plugins]\ndefault = [\n  # >>> anvil-managed: entries\n  # Copyright.\n  # License.\n\n\n  \"\"\"first\n  \nsecond\"\"\",\n  # <<< anvil-managed: entries\n]\n"
        );
        assert_eq!(
            Document::parse(&output).unwrap()["plugins"]["default"]
                .as_array()
                .unwrap()
                .get(0)
                .unwrap()
                .as_str(),
            Some("first\n  \nsecond")
        );
    }

    #[test]
    fn multiline_string_values_are_not_changed_by_indentation_or_adoption() {
        let mut spec = spec();
        spec.region.body = "\"\"\"first\nsecond\n\"\"\",\n".to_owned();
        let item = plan_toml_array_region(
            &Manifest::default(),
            Some("plugins.default = [\"\"\"first\nsecond\n\"\"\", \"other\"]\n"),
            "config.toml",
            &spec,
        )
        .unwrap();
        let output = item.spliced_host.unwrap();
        assert_eq!(
            output,
            "plugins.default = [\n  # >>> anvil-managed: entries\n  \"\"\"first\nsecond\n\"\"\",\n  # <<< anvil-managed: entries\n \"other\"]\n"
        );
        let document = Document::parse(output).unwrap();
        assert_eq!(
            document["plugins"]["default"].as_array().unwrap().get(0).unwrap().as_str(),
            Some("first\nsecond\n")
        );
    }

    #[test]
    fn empty_managed_body_is_regenerated_and_missing_inline_table_array_is_scaffolded() {
        let empty = "[plugins]\ndefault = [\n  # >>> anvil-managed: entries\n  # <<< anvil-managed: entries\n\"other\"\n]\n";
        assert_eq!(output(empty), format!("[plugins]\ndefault = [\n{REGION}\"other\"\n]\n"));
        assert_eq!(
            output("plugins = { mode = true }\n"),
            format!("plugins = {{ mode = true , default = [\n{REGION}] }}\n")
        );
    }

    #[test]
    fn matching_markers_outside_the_array_are_not_an_insync_region() {
        let host = "# >>> anvil-managed: entries\n# <<< anvil-managed: entries\nplugins.default = []\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::MalformedMarkers
        );
    }

    #[test]
    fn update_insync_edit_refusal_and_retirement_use_the_ordinary_region_contract() {
        let item = plan(None);
        let mut pending = Plan::default();
        pending.push(item);
        let previous = pending.projected_manifest(&Manifest::default());
        let item = plan_toml_array_region(&previous, Some(FRESH), "config.toml", &spec()).unwrap();
        assert_eq!(item.decision, Decision::InSync);
        let mut pending = Plan::default();
        pending.push(item);
        assert_eq!(pending.dry_run_exit_code(), 0);

        let mut updated = spec();
        updated.region.body = "\"new\",\n".to_owned();
        let item = plan_toml_array_region(&previous, Some(FRESH), "config.toml", &updated).unwrap();
        assert_eq!(item.decision, Decision::Write);
        assert_eq!(
            item.spliced_host.unwrap(),
            "[plugins]\ndefault = [\n  # >>> anvil-managed: entries\n  \"new\",\n  # <<< anvil-managed: entries\n]\n"
        );
        let edited = FRESH.replace("\"managed\"", "\"edited\"");
        assert_eq!(
            plan_toml_array_region(&previous, Some(&edited), "config.toml", &updated)
                .unwrap_err()
                .remedy,
            RefusalRemedy::EditedRegion
        );
        let retired = remove_region(FRESH, "entries", CommentSyntax::Hash).unwrap();
        let mut pending = Plan::default();
        pending.push(PlanItem::remove_region("config.toml", "entries", retired.clone()));
        assert_eq!(pending.projected_manifest(&previous).regions.len(), 0);
        assert_eq!(retired, "[plugins]\ndefault = [\n]\n");
        assert_eq!(
            Document::parse(&retired).unwrap()["plugins"]["default"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn retirement_keeps_user_entries_and_valid_trailing_commas() {
        let generated = output("plugins.default = [\"other\", \"managed\"]\n");
        let retired = remove_region(&generated, "entries", CommentSyntax::Hash).unwrap();
        assert_eq!(retired, "plugins.default = [\n\"other\", ]\n");
        assert_eq!(
            Document::parse(&retired).unwrap()["plugins"]["default"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn region_outside_selected_array_and_markers_inside_a_string_are_refused() {
        let host = format!("{REGION}plugins.default = []\n");
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::HostAlreadyUnparsable
        );
        let host = format!("plugins.default = [\"\"\"\n{REGION}\"\"\",]\n");
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::MalformedMarkers
        );
    }

    #[test]
    fn crlf_host_uses_crlf_for_body_and_markers() {
        assert_eq!(
            output("plugins.default = [\r\n\"other\"\r\n]\r\n"),
            format!("plugins.default = [\r\n{}\"other\"\r\n]\r\n", REGION.replace('\n', "\r\n"))
        );
    }

    #[test]
    fn adopted_user_host_is_insync_and_multiple_regions_compose_without_claiming_each_other() {
        let first = output("plugins.default = [\"managed\", \"other\"]\n");
        assert_eq!(first, format!("plugins.default = [\n{REGION} \"other\"]\n"));
        let item = plan(Some(&first));
        assert_eq!(item.decision, Decision::InSync);
        assert_eq!(item.spliced_host, None);

        let mut second = spec();
        second.region.id = RegionId::new("second");
        second.region.body = "\"second\",\n".to_owned();
        let second = plan_toml_array_region(&Manifest::default(), Some(&first), "config.toml", &second)
            .unwrap()
            .spliced_host
            .unwrap();
        assert_eq!(
            second,
            format!(
                "plugins.default = [\n  # >>> anvil-managed: second\n  \"second\",\n  # <<< anvil-managed: second\n{REGION} \"other\"]\n"
            )
        );
        assert_eq!(
            remove_region(
                &remove_region(&second, "entries", CommentSyntax::Hash).unwrap(),
                "second",
                CommentSyntax::Hash
            )
            .unwrap(),
            "plugins.default = [\n \"other\"]\n"
        );
    }

    #[test]
    fn duplicated_and_unpaired_markers_are_refused_without_a_write_plan() {
        for host in [
            "plugins.default = [\n# >>> anvil-managed: entries\n\"managed\",\n]\n",
            "plugins.default = [\n# <<< anvil-managed: entries\n]\n",
            "plugins.default = [\n# >>> anvil-managed: entries\n# >>> anvil-managed: entries\n\"managed\",\n# <<< anvil-managed: entries\n]\n",
        ] {
            assert_eq!(
                plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                    .unwrap_err()
                    .remedy,
                RefusalRemedy::MalformedMarkers
            );
        }
        let mut invalid = spec();
        invalid.region.body = "# >>> anvil-managed: entries\n\"managed\",\n".to_owned();
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), None, "config.toml", &invalid)
                .unwrap_err()
                .remedy,
            RefusalRemedy::MalformedMarkers
        );
    }

    #[test]
    fn catalog_identity_includes_selector_but_region_identity_does_not() {
        let artifact = Artifact::region(spec().region);
        let mut different = spec();
        different.path[1] = "other".to_owned();
        assert_eq!(artifact.key(), Artifact::region(different.region.clone()).key());
        let first = Catalog::builder(Catalog::anvil().cli().clone())
            .with_toml_array_region(spec())
            .build()
            .unwrap();
        let second = Catalog::builder(Catalog::anvil().cli().clone())
            .with_toml_array_region(different)
            .build()
            .unwrap();
        assert_ne!(first.checksum(), second.checksum());
        let replaced = artifact.with_body("\"new\",\n");
        let Artifact::Region(replaced) = replaced else {
            panic!("expected ordinary region")
        };
        assert_eq!(replaced.body, "\"new\",\n");
        let error = Catalog::builder(Catalog::anvil().cli().clone())
            .with_toml_array_region(spec())
            .with_artifact(Artifact::region(spec().region))
            .build()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid catalog for 'anvil':\n  - with_artifact: an artifact with identity Region { host: Path(\"config.toml\"), id: \"entries\" } already exists; use replace_artifact to override it"
        );
    }

    #[test]
    fn selector_survives_builder_round_trip_and_body_replacement_but_not_removal() {
        let catalog = Catalog::builder(Catalog::anvil().cli().clone())
            .with_toml_array_region(spec())
            .build()
            .unwrap();
        let Artifact::Region(region) = &catalog.artifacts()[0] else {
            panic!("expected ordinary region")
        };
        assert_eq!(region.body, BODY);
        assert_eq!(catalog.toml_array_path(region), Some(spec().path.as_slice()));
        let catalog = catalog
            .into_builder()
            .replace_artifact(Artifact::region(spec().region).with_body("\"new\",\n"))
            .build()
            .unwrap();
        let Artifact::Region(region) = &catalog.artifacts()[0] else {
            panic!("expected ordinary region")
        };
        assert_eq!(region.body, "\"new\",\n");
        assert_eq!(catalog.toml_array_path(region), Some(spec().path.as_slice()));
        let removed = catalog
            .into_builder()
            .without_artifact(Artifact::region(spec().region))
            .build()
            .unwrap();
        assert_eq!(removed.toml_array_path(&spec().region), None);
        assert_eq!(removed.artifacts(), []);
        assert_eq!(
            removed.checksum(),
            Catalog::builder(Catalog::anvil().cli().clone()).build().unwrap().checksum()
        );
        let ordinary = removed
            .into_builder()
            .with_artifact(Artifact::region(spec().region))
            .build()
            .unwrap();
        assert_eq!(ordinary.toml_array_path(&spec().region), None);
    }

    #[test]
    fn array_registration_rejects_both_ordinary_and_array_identity_conflicts() {
        for builder in [
            Catalog::builder(Catalog::anvil().cli().clone()).with_artifact(Artifact::region(spec().region)),
            Catalog::builder(Catalog::anvil().cli().clone()).with_toml_array_region(spec()),
        ] {
            assert_eq!(
                builder.with_toml_array_region(spec()).build().unwrap_err().to_string(),
                "invalid catalog for 'anvil':\n  - with_toml_array_region: an artifact with identity Region { host: Path(\"config.toml\"), id: \"entries\" } already exists; use replace_artifact to override it"
            );
        }
    }

    #[test]
    fn builder_validates_selectors_and_replaced_array_body() {
        let mut invalid = spec();
        invalid.path.clear();
        assert_eq!(
            Catalog::builder(Catalog::anvil().cli().clone())
                .with_toml_array_region(invalid)
                .build()
                .unwrap_err()
                .to_string(),
            "invalid catalog for 'anvil':\n  - TOML array region 'entries': a TOML array region requires a nonempty key path and hash comment syntax"
        );
        assert_eq!(
            Catalog::builder(Catalog::anvil().cli().clone())
                .with_toml_array_region(spec())
                .replace_artifact(Artifact::region(spec().region).with_body("\"new\"\n"))
                .build()
                .unwrap_err()
                .to_string(),
            "invalid catalog for 'anvil':\n  - TOML array region 'entries': TOML array-entry bodies must end with a trailing comma"
        );
    }

    #[test]
    fn selectors_are_host_scoped_and_checksum_order_independent() {
        let mut other = spec();
        other.region.host = HostSelector::Path("other.toml".to_owned());
        other.path = vec!["other".to_owned()];
        let first = Catalog::builder(Catalog::anvil().cli().clone())
            .with_toml_array_region(spec())
            .with_toml_array_region(other.clone())
            .build()
            .unwrap();
        let second = Catalog::builder(Catalog::anvil().cli().clone())
            .with_toml_array_region(other.clone())
            .with_toml_array_region(spec())
            .build()
            .unwrap();
        assert_eq!(first.checksum(), second.checksum());
        assert_eq!(first.toml_array_path(&spec().region), Some(spec().path.as_slice()));
        assert_eq!(first.toml_array_path(&other.region), Some(other.path.as_slice()));
        let ordinary = Catalog::builder(Catalog::anvil().cli().clone())
            .with_artifact(Artifact::region(spec().region))
            .with_artifact(Artifact::region(other.region))
            .build()
            .unwrap();
        assert_ne!(first.checksum(), ordinary.checksum());
    }
}
