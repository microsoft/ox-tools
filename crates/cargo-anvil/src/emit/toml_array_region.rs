// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-span-based placement of regions inside TOML arrays.

use ohno::app_err;
use toml_edit::{Array, Document, DocumentMut, Item, Table};

use crate::catalog::TomlArrayRegionSpec;
use crate::emit::managed_region::{ManagedRegionRefusal, ManagedRegionRequest, RefusalRemedy, plan_region_with_splice};
use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::{
    CommentSyntax, Region, RegionPlacement, find_toml_region as find_region, remove_toml_region as remove_region, text_newline,
    toml_comment_lines,
};

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
    if let crate::catalog::HostSelector::Path(host) = &spec.region.host
        && !host.to_ascii_lowercase().ends_with(".toml")
    {
        return Err(refusal("TOML array hosts must have a .toml suffix", RefusalRemedy::ArrayShape));
    }
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
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedToml))?;
    if toml_comment_lines(&source).into_iter().any(|line| {
        let comment = source[line.start..line.end].trim();
        comment.starts_with("# >>> anvil-managed:") || comment.starts_with("# <<< anvil-managed:")
    }) {
        return Err(refusal(
            "TOML array-entry bodies must not contain managed-region sentinel comments",
            RefusalRemedy::InvalidGeneratedToml,
        ));
    }
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
    if array.iter().any(|value| value.as_str().is_none()) {
        return Err(refusal(
            "generated array entries must be strings",
            RefusalRemedy::InvalidGeneratedToml,
        ));
    }
    Ok(std::mem::take(
        document["entries"].as_array_mut().expect("the array was validated above"),
    ))
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
    let mut rendered = String::with_capacity(body.len());
    let mut offset = PREFIX.len();
    for line in body.split_inclusive('\n') {
        // Indent entry/comment lines, but not continuations inside values:
        // indentation in a multiline string is data, not TOML decoration.
        let in_value = spans.iter().any(|span| contains_interior(span, offset));
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

fn contains_interior(span: &std::ops::Range<usize>, offset: usize) -> bool {
    span.contains(&offset) && span.start != offset
}

/// Create only missing table/array scaffolding, leaving existing items intact.
fn scaffold(text: &str, path: &[String]) -> Result<String, ManagedRegionRefusal> {
    let normalized = text.replace("\r\n", "\n");
    let mut document = normalized
        .parse::<DocumentMut>()
        .map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let mut item = document.as_item_mut();
    for (index, key) in path.iter().enumerate() {
        let table = item.as_table_mut().ok_or_else(|| {
            refusal(
                format!("the parent of TOML array {path:?} is not a table"),
                RefusalRemedy::ArrayShape,
            )
        })?;
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
    // toml_edit normalizes existing CRLF decoration when serializing. Apply
    // only its scaffold delta to the original bytes, including mixed endings.
    let edited = document.to_string();
    let prefix: usize = normalized
        .chars()
        .zip(edited.chars())
        .take_while(|(before, after)| before == after)
        .map(|(character, _)| character.len_utf8())
        .sum();
    let suffix: usize = normalized[prefix..]
        .chars()
        .rev()
        .zip(edited[prefix..].chars().rev())
        .take_while(|(before, after)| before == after)
        .map(|(character, _)| character.len_utf8())
        .sum();
    let before = source_offset(text, prefix);
    let after = source_offset(text, normalized.len() - suffix);
    if before != after {
        return Err(refusal(
            format!("the missing TOML array {path:?} cannot be scaffolded by insertion alone"),
            RefusalRemedy::ArrayScaffold,
        ));
    }
    let inserted = edited[prefix..edited.len() - suffix].replace('\n', text_newline(text));
    let scaffolded = format!("{}{inserted}{}", &text[..before], &text[after..]);
    let document = Document::parse(scaffolded.as_str()).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let array = lookup(document.as_item(), path)
        .and_then(Item::as_array)
        .expect("the scaffold inserted or validated an array");
    let span = array.span().expect("parsed array retains its span");
    let regions = validated_regions(&scaffolded)?;
    if regions
        .iter()
        .any(|region| (region.body.start..region.body.end).contains(&span.start))
    {
        return Err(refusal(
            "creating the missing array would overlap another managed region; scaffold relocation is not supported",
            RefusalRemedy::EnclosingOwnership,
        ));
    }
    Ok(scaffolded)
}

fn source_offset(text: &str, normalized_offset: usize) -> usize {
    let mut offset = 0;
    for (index, byte) in text.bytes().enumerate() {
        if offset == normalized_offset {
            return index;
        }
        if byte != b'\r' || text.as_bytes().get(index + 1) != Some(&b'\n') {
            offset += 1;
        }
    }
    text.len()
}

/// Validate all TOML ownership boundaries.
///
/// Include every comment sentinel, not only the successfully paired regions returned
/// by `managed_region_ids`. A malformed boundary anywhere makes adoption unsafe.
pub(crate) fn validated_regions(text: &str) -> Result<Vec<Region<'_>>, ManagedRegionRefusal> {
    let mut regions = Vec::new();
    let mut open = None;
    let mut seen = std::collections::BTreeSet::new();
    for line in toml_comment_lines(text) {
        let line = text[line.start..line.end].trim();
        if let Some(id) = line.strip_prefix("# >>> anvil-managed:") {
            let id = id.trim();
            if id.is_empty() || open.is_some() || !seen.insert(id) {
                return Err(refusal("duplicate or nested opening sentinel", RefusalRemedy::MalformedMarkers));
            }
            open = Some(id);
        } else if let Some(id) = line.strip_prefix("# <<< anvil-managed:") {
            let id = id.trim();
            if open.take() != Some(id) {
                return Err(refusal(
                    "closing sentinel without its matching opener",
                    RefusalRemedy::MalformedMarkers,
                ));
            }
            regions.push(
                find_region(text, id, CommentSyntax::Hash)
                    .map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?
                    .ok_or_else(|| refusal("unusable sentinel lines", RefusalRemedy::MalformedMarkers))?,
            );
        }
    }
    if open.is_some() {
        return Err(refusal(
            "opening sentinel without its matching close",
            RefusalRemedy::MalformedMarkers,
        ));
    }
    Ok(regions)
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
    let regions = validated_regions(original)?;
    let document = Document::parse(original).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let scaffolded;
    let (base, document, regions) = if lookup(document.as_item(), &spec.path).is_some() {
        (original, document, regions)
    } else {
        scaffolded = scaffold(original, &spec.path)?;
        let document = Document::parse(scaffolded.as_str()).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
        let regions = validated_regions(&scaffolded)?;
        (scaffolded.as_str(), document, regions)
    };
    let mut parent = document.as_item();
    for key in &spec.path[..spec.path.len() - 1] {
        parent = parent.get(key).expect("the selected path exists after scaffolding");
        if parent.as_table().is_none() {
            return Err(refusal("array parents must be normal TOML tables", RefusalRemedy::ArrayShape));
        }
    }
    let array = lookup(document.as_item(), &spec.path).and_then(Item::as_array).ok_or_else(|| {
        refusal(
            format!("the selected TOML item {:?} is not an array", spec.path),
            RefusalRemedy::ArrayShape,
        )
    })?;
    let span = array.span().expect("an immutable parsed array retains its source span");
    for other in regions {
        let id = &other.id;
        if id == spec.region.id.as_str() {
            continue;
        }
        // Delimiters and their token ends cannot coincide with full sentinel-line boundaries.
        let ownership = other.start_line.start..other.end_line.end;
        if ownership.contains(&span.start) || ownership.contains(&span.end) {
            return Err(refusal(
                format!(
                    "the selected array belongs to managed region '{id}'; retire its enclosing ownership before managing array entries"
                ),
                RefusalRemedy::EnclosingOwnership,
            ));
        }
        let without = remove_region(base, id, CommentSyntax::Hash).map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
        validate_neighbor_splice(base, &without, std::slice::from_ref(&spec.path))?;
    }
    let region =
        find_region(base, spec.region.id.as_str(), CommentSyntax::Hash).map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
    if let Some(region) = &region {
        if !span.contains(&region.start_line.start) || !span.contains(&region.end_line.end) {
            return Err(refusal(
                "the managed region is outside its selected TOML array",
                RefusalRemedy::MisplacedArrayMarkers,
            ));
        }
        // A whole-value boundary is required: markers inside a multiline string
        // or a nested value must not claim part of that value.
        if array.iter().any(|value| {
            let value = value.span().expect("parsed array values retain source spans");
            contains_interior(&value, region.body.start) || contains_interior(&value, region.body.end)
        }) {
            return Err(refusal(
                "the region splits a TOML array value",
                RefusalRemedy::MisplacedArrayMarkers,
            ));
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
    plan_region_with_splice(manifest, host_text, request, crate::region::HostScanner::Toml, || {
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
        validated_regions(&spliced)?;
        Ok(spliced)
    })
}

/// Validate neighboring splices against live array selectors.
///
/// A neighboring region must not remove array delimiters or rebind an existing
/// selector to a different source location, even when the result still parses.
pub(crate) fn validate_neighbor_splice(before: &str, after: &str, paths: &[Vec<String>]) -> Result<(), ManagedRegionRefusal> {
    let original = Document::parse(before).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let updated = Document::parse(after).map_err(|error| refusal(error, RefusalRemedy::BetweenManagedRegions))?;
    let map_offset = splice_offset_map(before, after);
    for path in paths {
        let Some(array) = lookup(original.as_item(), path).and_then(Item::as_array) else {
            continue;
        };
        let span = array.span().expect("parsed arrays retain source spans");
        let mapped = map_offset(span.start).zip(map_offset(span.end - 1));
        let remaining = lookup(updated.as_item(), path).and_then(Item::as_array).and_then(Array::span);
        if mapped
            .zip(remaining)
            .is_none_or(|((start, end), remaining)| remaining.start != start || remaining.end != end + 1)
        {
            return Err(refusal(
                format!("this change would remove or rebind the live TOML array selector {path:?}"),
                RefusalRemedy::ArrayDependency,
            ));
        }
    }
    Ok(())
}

/// Recover only selectors from existing array-contained markers, not ownership metadata.
/// This also protects an array's repository-owned delimiters on its final retirement.
pub(crate) fn existing_array_paths(text: &str) -> Result<Vec<Vec<String>>, ManagedRegionRefusal> {
    fn visit(item: &Item, path: &mut Vec<String>, markers: &[usize], paths: &mut Vec<Vec<String>>) {
        if let Some(array) = item.as_array()
            && let Some(span) = array.span()
            && markers.iter().any(|offset| span.contains(offset))
        {
            paths.push(path.clone());
        }
        if let Some(table) = item.as_table() {
            for (key, child) in table {
                path.push(key.to_owned());
                visit(child, path, markers, paths);
                path.pop();
            }
        }
    }
    let document = Document::parse(text).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    // Discovery must not depend on well-formed neighbors: otherwise a stray
    // sentinel would disable array protection on the final retirement.
    let markers: Vec<_> = toml_comment_lines(text)
        .filter_map(|line| {
            let comment = text[line.start..line.end].trim();
            (comment.starts_with("# >>> anvil-managed:") || comment.starts_with("# <<< anvil-managed:")).then_some(line.start)
        })
        .collect();
    let mut paths = Vec::new();
    visit(document.as_item(), &mut Vec::new(), &markers, &mut paths);
    Ok(paths)
}

/// Map only common-prefix/suffix offsets; replacement delimiters do not inherit
/// the identity of delimiters in the changed interval.
fn splice_offset_map(before: &str, after: &str) -> impl Fn(usize) -> Option<usize> + use<> {
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let suffix: usize = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let before_end = before.len() - suffix;
    let after_end = after.len() - suffix;
    move |offset| {
        if offset < prefix {
            Some(offset)
        } else if offset >= before_end {
            Some(after_end + offset - before_end)
        } else {
            None
        }
    }
}

/// Adopt matching unmanaged entries with their repetition counts.
///
/// Remove one matching unmanaged value for each generated entry, not comments
/// or other regions. Array punctuation is bounded by parser-provided spans.
fn adopt_entries(text: &str, array: &Array, body: &str) -> Result<String, ManagedRegionRefusal> {
    let generated = body_array(body)?;
    let protected = validated_regions(text)?;
    let values: Vec<_> = array.iter().collect();
    let mut removals = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for generated in &generated {
        let generated = generated.as_str().expect("generated entries were validated as strings");
        let candidate = values.iter().enumerate().find(|(index, value)| {
            let span = value.span().expect("parsed array values retain source spans");
            !used.contains(index)
                && !protected
                    .iter()
                    .any(|region| span.start < region.end_line.end && region.start_line.start < span.end)
                && value.as_str() == Some(generated)
        });
        if let Some((index, value)) = candidate {
            used.insert(index);
            let span = value.span().expect("parsed array values retain source spans");
            let next = values
                .get(index + 1)
                .and_then(|value| value.span())
                // Including the array closer is harmless; punctuation AFTER it belongs to the parent.
                .map_or_else(|| array.span().expect("parsed array retains its span").end, |span| span.start);
            if let Some(comma) = separator_comma(&text[span.end..next]) {
                let comma = span.end + comma..span.end + comma + 1;
                if protected
                    .iter()
                    .any(|region| (region.start_line.start..region.end_line.end).contains(&comma.start))
                {
                    return Err(refusal(
                        "the matching array entry's separator belongs to another managed region",
                        RefusalRemedy::EnclosingOwnership,
                    ));
                }
                removals.push(comma);
            }
            removals.push(span);
        }
    }
    removals.sort_by_key(|range| range.start);
    let mut adopted = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in removals {
        adopted.push_str(&text[cursor..range.start]);
        cursor = range.end;
    }
    adopted.push_str(&text[cursor..]);
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
    use toml_edit::Value;

    use super::*;
    use crate::catalog::{Artifact, Catalog, HostSelector, RegionId, RegionSpec};
    use crate::checksum::checksum_str;
    use crate::decision::Decision;
    use crate::plan::{Plan, Target};

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

    #[test]
    fn only_toml_suffixes_are_accepted_without_persisted_scanner_metadata() {
        for host in ["config", "config.conf", "config.toml.backup"] {
            let mut array = spec();
            array.region.host = HostSelector::Path(host.to_owned());
            let error = Catalog::builder(Catalog::anvil().cli().clone())
                .with_toml_array_region(array)
                .build()
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid catalog for 'anvil':\n  - TOML array region 'entries': TOML array hosts must have a .toml suffix"
            );
        }
        for host in ["config.toml", "config.ToML"] {
            let mut array = spec();
            array.region.host = HostSelector::Path(host.to_owned());
            Catalog::builder(Catalog::anvil().cli().clone())
                .with_toml_array_region(array)
                .build()
                .unwrap();
        }
    }
    fn plan(text: Option<&str>) -> PlanItem {
        plan_toml_array_region(&Manifest::default(), text, "config.toml", &spec()).unwrap()
    }

    fn output(text: &str) -> String {
        plan(Some(text)).spliced_host.unwrap()
    }

    fn mutation_spec(path: &[&str]) -> TomlArrayRegionSpec {
        TomlArrayRegionSpec {
            path: path.iter().map(|key| (*key).to_owned()).collect(),
            region: RegionSpec {
                body: "\"managed\",\n".to_owned(),
                ..spec().region
            },
        }
    }

    fn assert_mutation_write(host: &str, expected: &str, spec: &TomlArrayRegionSpec, manifest: &Manifest, newline: &str) {
        let item = plan_toml_array_region(manifest, Some(host), "config.toml", spec).unwrap();
        let body = format!("  \"managed\",{newline}");
        assert_eq!(
            item.target,
            Target::Region {
                host: "config.toml".to_owned(),
                id: "entries".to_owned()
            }
        );
        assert_eq!(item.decision, Decision::Write);
        assert_eq!(item.spliced_host.as_deref(), Some(expected));
        assert_eq!(item.rendered.as_deref(), Some(body.as_str()));
        assert_eq!(item.rendered_checksum, Some(checksum_str(&body)));
        let mut plan = Plan::default();
        plan.push(item);
        let projected = plan.projected_manifest(manifest);
        assert_eq!(
            projected.region_checksum("config.toml", "entries"),
            Some(checksum_str(&body).as_str())
        );
        let rerun = plan_toml_array_region(&projected, Some(expected), "config.toml", spec).unwrap();
        assert_eq!(rerun.decision, Decision::InSync);
        assert_eq!(rerun.spliced_host, None);
    }

    #[test]
    fn mutation_nested_opening_cannot_hide_an_unclosed_owner() {
        for newline in ["\n", "\r\n"] {
            let host =
                "items = []\n# >>> anvil-managed: outer\n# >>> anvil-managed: inner\n# <<< anvil-managed: inner\n".replace('\n', newline);
            let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &mutation_spec(&["items"])).unwrap_err();
            assert_eq!(error.remedy, RefusalRemedy::MalformedMarkers);
            assert_eq!(error.reason.to_string(), "duplicate or nested opening sentinel");
        }
    }

    #[test]
    fn mutation_owned_inline_parent_has_its_specific_diagnostic() {
        for newline in ["\n", "\r\n"] {
            let host = "# >>> anvil-managed: cfg\na = { b = {} }\n# <<< anvil-managed: cfg\n".replace('\n', newline);
            let error = plan_toml_array_region(
                &Manifest::default(),
                Some(&host),
                "config.toml",
                &mutation_spec(&["a", "b", "items"]),
            )
            .unwrap_err();
            assert_eq!(error.remedy, RefusalRemedy::ArrayShape);
            assert_eq!(
                error.reason.to_string(),
                "the parent of TOML array [\"a\", \"b\", \"items\"] is not a table"
            );
        }
    }

    #[test]
    fn scaffold_relocation_is_refused() {
        for newline in ["\n", "\r\n"] {
            let host = "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n\n# <<< anvil-managed: cfg\n".replace('\n', newline);
            let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec()).unwrap_err();
            assert_eq!(error.remedy, RefusalRemedy::EnclosingOwnership);
        }
    }

    #[test]
    fn mutation_one_sided_value_cuts_and_outside_closer_have_precise_diagnostics() {
        for newline in ["\n", "\r\n"] {
            for (source, reason) in [
                (
                    "items = [[\n# >>> anvil-managed: entries\n1\n],\n# <<< anvil-managed: entries\n\"user\"]\n",
                    "the region splits a TOML array value",
                ),
                (
                    "items = [\n# >>> anvil-managed: entries\n[1,\n# <<< anvil-managed: entries\n2],\n\"user\"]\n",
                    "the region splits a TOML array value",
                ),
                (
                    "items = [\n# >>> anvil-managed: entries\n1,\n]\n# <<< anvil-managed: entries\n",
                    "the managed region is outside its selected TOML array",
                ),
            ] {
                let host = source.replace('\n', newline);
                let mut manifest = Manifest::default();
                manifest.set_region(
                    "config.toml",
                    "entries",
                    checksum_str(find_region(&host, "entries", CommentSyntax::Hash).unwrap().unwrap().body_str()),
                );
                let error = plan_toml_array_region(&manifest, Some(&host), "config.toml", &mutation_spec(&["items"])).unwrap_err();
                assert_eq!(error.remedy, RefusalRemedy::MisplacedArrayMarkers);
                assert_eq!(error.reason.to_string(), reason);
            }
        }
    }

    #[test]
    fn mutation_whole_values_at_the_first_owned_byte_and_before_it_are_not_split() {
        for newline in ["\n", "\r\n"] {
            for (source, expected) in [
                (
                    "items = [\n# >>> anvil-managed: entries\n1,\n# <<< anvil-managed: entries\n]\n",
                    "items = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n]\n",
                ),
                (
                    "items = [0,\n# >>> anvil-managed: entries\n1,\n# <<< anvil-managed: entries\n]\n",
                    "items = [0,\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n]\n",
                ),
            ] {
                let host = source.replace('\n', newline);
                let expected = expected.replace('\n', newline);
                let mut manifest = Manifest::default();
                manifest.set_region("config.toml", "entries", checksum_str(&format!("1,{newline}")));
                assert_mutation_write(&host, &expected, &mutation_spec(&["items"]), &manifest, newline);
            }
        }
    }

    #[test]
    fn mutation_adoption_respects_post_closer_entries_separators_and_enclosing_commas() {
        for newline in ["\n", "\r\n"] {
            for (source, expected, path) in [
                (
                    "items = [\n# >>> anvil-managed: other\n\"user\",\n# <<< anvil-managed: other\n\"managed\"\n]\n",
                    "items = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n# >>> anvil-managed: other\n\"user\",\n# <<< anvil-managed: other\n\n]\n",
                    vec!["items"],
                ),
                (
                    "items = [\"managed\"\n# >>> anvil-managed: other\n# keep\n# <<< anvil-managed: other\n,\n\"user\"]\n",
                    "items = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n# >>> anvil-managed: other\n# keep\n# <<< anvil-managed: other\n\n\"user\"]\n",
                    vec!["items"],
                ),
            ] {
                let host = source.replace('\n', newline);
                let expected = expected.replace('\n', newline);
                assert_mutation_write(&host, &expected, &mutation_spec(&path), &Manifest::default(), newline);
                let document = Document::parse(expected.as_str()).unwrap();
                let path: Vec<_> = path.iter().map(|key| (*key).to_owned()).collect();
                let array = lookup(document.as_item(), &path).unwrap().as_array().unwrap();
                assert_eq!(array.iter().filter_map(Value::as_str).collect::<Vec<_>>(), vec!["managed", "user"]);
                assert_eq!(
                    find_region(&expected, "other", CommentSyntax::Hash).unwrap().unwrap().body_str(),
                    if source.contains("# keep") {
                        format!("# keep{newline}")
                    } else {
                        format!("\"user\",{newline}")
                    }
                );
            }
        }
    }

    #[test]
    fn mutation_adoption_never_removes_a_comment_comma_and_resets_after_newline() {
        for newline in ["\n", "\r\n"] {
            for (source, expected, values) in [
                (
                    "items = [\"managed\" # user, café\n]\n",
                    "items = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n # user, café\n]\n",
                    vec!["managed"],
                ),
                (
                    "items = [\"managed\" # user, café\n,\n\"user\"]\n",
                    "items = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n # user, café\n\n\"user\"]\n",
                    vec!["managed", "user"],
                ),
            ] {
                let host = source.replace('\n', newline);
                let expected = expected.replace('\n', newline);
                assert_mutation_write(&host, &expected, &mutation_spec(&["items"]), &Manifest::default(), newline);
                let document = Document::parse(expected.as_str()).unwrap();
                assert_eq!(
                    document["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>(),
                    values
                );
            }
        }
    }

    #[test]
    fn review_marker_data_is_not_ownership() {
        for quote in ["\"\"\"", "'''"] {
            for markers in [
                "# >>> anvil-managed: note\n",
                "# >>> anvil-managed: entries\n\"managed\",\n# <<< anvil-managed: entries\n",
            ] {
                let prefix = format!("message = {quote}\n{markers}{quote}\n");
                let host = format!("{prefix}plugins.default = [\"managed\", \"user\"]\n");
                assert_eq!(output(&host), format!("{prefix}plugins.default = [\n{REGION} \"user\"]\n"));
            }
        }
    }

    #[test]
    fn review_catalog_rejects_generated_ownership_comments() {
        for (host, region_id, path) in [
            ("config.toml", "entries", vec!["plugins".to_owned(), "default".to_owned()]),
            ("settings.TOML", "catalog-entries", vec!["items".to_owned()]),
        ] {
            let mut original = spec();
            original.region.host = HostSelector::Path(host.to_owned());
            original.region.id = RegionId::new(region_id);
            original.path = path;
            let catalog = Catalog::builder(Catalog::anvil().cli().clone())
                .with_toml_array_region(original.clone())
                .build()
                .unwrap();
            for id in [region_id, "foreign"] {
                for body in [
                    format!("# >>> anvil-managed: {id}\n\"managed\",\n"),
                    format!("\"managed\",\n# <<< anvil-managed: {id}\n"),
                    format!("# >>> anvil-managed: {id}\n\"managed\",\n# <<< anvil-managed: {id}\n"),
                    format!("# >>> anvil-managed: {id}\n\"managed\",\n# <<< anvil-managed: other\n"),
                    format!("\"one\",\n\t# >>> anvil-managed: {id}\n2,\n"),
                    format!("# <<< anvil-managed: {id}\n"),
                ] {
                    for newline in ["\n", "\r\n"] {
                        let mut invalid = original.clone();
                        invalid.region.body = body.replace('\n', newline);
                        let expected = format!(
                            "invalid catalog for 'anvil':\n  - TOML array region '{region_id}': \
                             TOML array-entry bodies must not contain managed-region sentinel comments"
                        );
                        let error = Catalog::builder(Catalog::anvil().cli().clone())
                            .with_toml_array_region(invalid.clone())
                            .build()
                            .unwrap_err();
                        assert_eq!(error.to_string(), expected);
                        let error = catalog
                            .clone()
                            .into_builder()
                            .replace_artifact(Artifact::region(invalid.region.clone()))
                            .build()
                            .unwrap_err();
                        assert_eq!(error.to_string(), expected);
                        let error = plan_toml_array_region(&Manifest::default(), None, host, &invalid).unwrap_err();
                        assert_eq!(error.remedy, RefusalRemedy::InvalidGeneratedToml);
                        assert_eq!(
                            error.reason.to_string(),
                            "TOML array-entry bodies must not contain managed-region sentinel comments"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn review_catalog_accepts_marker_data_and_non_boundary_comments() {
        for quote in ["\"\"\"", "'''"] {
            for newline in ["\n", "\r\n"] {
                let body = format!(
                    "# Guidance.\n{quote}\n# >>> anvil-managed: entries\n# <<< anvil-managed: foreign\n{quote},\n\
                     \"last\", # >>> anvil-managed: inline-comment\n"
                );
                let mut valid = spec();
                valid.region.body = body.replace('\n', newline);
                let catalog = Catalog::builder(Catalog::anvil().cli().clone())
                    .with_toml_array_region(valid.clone())
                    .build()
                    .unwrap();
                assert_eq!(catalog.toml_array_path(&valid.region), Some(valid.path.as_slice()));
                let expected = format!(
                    "[plugins]\ndefault = [\n  # >>> anvil-managed: entries\n  # Guidance.\n  {quote}\n\
                     # >>> anvil-managed: entries\n# <<< anvil-managed: foreign\n{quote},\n\
                     \x20\x20\"last\", # >>> anvil-managed: inline-comment\n  # <<< anvil-managed: entries\n]\n\n"
                )
                .replace('\n', newline);
                let item = plan_toml_array_region(&Manifest::default(), Some(newline), "config.toml", &valid).unwrap();
                assert_eq!(item.spliced_host.as_deref(), Some(expected.as_str()));
                let item = plan_toml_array_region(&Manifest::default(), Some(&expected), "config.toml", &valid).unwrap();
                assert_eq!(item.decision, Decision::InSync);
                assert_eq!(item.spliced_host, None);
            }
        }
    }

    #[test]
    fn review_scaffold_never_replaces_repository_or_retirement_bytes() {
        for newline in ["\n", "\r\n"] {
            for old in ["", "# >>> anvil-managed: old\nobsolete = \"café\"\n# <<< anvil-managed: old\n"] {
                let host = format!("{old}plugins.a=true\nother=true\nplugins.b=true\n").replace('\n', newline);
                let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec()).unwrap_err();
                assert_eq!(
                    error.reason.to_string(),
                    "the missing TOML array [\"plugins\", \"default\"] cannot be scaffolded by insertion alone"
                );
                let corrected = format!("{host}plugins.default=[]{newline}");
                let item = plan_toml_array_region(&Manifest::default(), Some(&corrected), "config.toml", &spec()).unwrap();
                assert_eq!(
                    item.spliced_host.unwrap(),
                    format!("{}plugins.default=[{newline}{}]{newline}", host, REGION.replace('\n', newline))
                );
            }
        }
    }

    #[test]
    fn review_actual_comment_markers_cannot_split_a_nested_array_value() {
        let mut spec = spec();
        spec.path = vec!["items".to_owned()];
        spec.region.body = "\"managed\",\n".to_owned();
        let host = "items = [[\n# >>> anvil-managed: entries\n1,\n# <<< anvil-managed: entries\n]]\n";
        let error = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::MisplacedArrayMarkers);
        assert_eq!(error.reason.to_string(), "the region splits a TOML array value");
    }

    #[test]
    fn compound_generated_entries_are_rejected_without_adoption() {
        for body in ["{ a.b = 1 },\n", "[1, 2],\n", "true,\n", "123,\n"] {
            let mut spec = spec();
            spec.path = vec!["items".to_owned()];
            spec.region.body = body.to_owned();
            let error = plan_toml_array_region(&Manifest::default(), Some("items = []\n"), "config.toml", &spec).unwrap_err();
            assert_eq!(error.remedy, RefusalRemedy::InvalidGeneratedToml);
            assert_eq!(error.reason.to_string(), "generated array entries must be strings");
        }
    }

    #[test]
    fn invalid_partial_migrations_are_not_reconstructed() {
        for newline in ["\n", "\r\n"] {
            for prefix in ["# Repository comment: café\n", "plugins.default = [\"user\"]\n"] {
                let old = "# >>> anvil-managed: old\n[settings]\nmode = \"café\"\n# <<< anvil-managed: old\n";
                let new = "# >>> anvil-managed: new\n[settings]\nmode = false\n# <<< anvil-managed: new\n";
                let host = format!("{prefix}{old}{new}").replace('\n', newline);
                let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec()).unwrap_err();
                assert_eq!(error.remedy, RefusalRemedy::HostAlreadyUnparsable);
            }
        }
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
    fn missing_array_cannot_use_another_regions_parent_header() {
        let cfg = crate::emit::managed_region::plan_managed_region(
            &Manifest::default(),
            None,
            ManagedRegionRequest {
                host_relpath: "config.toml",
                region_id: "cfg",
                rendered_body: "[plugins]\nmode = true\n",
                syntax: CommentSyntax::Hash,
                placement: RegionPlacement::End,
                newline: None,
            },
        )
        .unwrap();
        let host = format!("{}\n[settings]\nvalue = 1\n", cfg.spliced_host.as_deref().unwrap());
        let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::EnclosingOwnership);
    }

    #[test]
    fn scaffolding_refuses_to_move_a_key_into_another_table() {
        let host = "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n[settings]\nvalue = 1\n# <<< anvil-managed: cfg\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::EnclosingOwnership
        );
    }

    #[test]
    fn scaffolding_refuses_to_reorder_interleaved_managed_dotted_keys() {
        for body in [
            "plugins.a = true\nother = true\nplugins.b = true\n",
            "plugins.a.x = true\nplugins.b = true\nplugins.a.y = true\n",
        ] {
            for newline in ["\n", "\r\n"] {
                let host = format!("# >>> anvil-managed: cfg\n{body}# <<< anvil-managed: cfg\n").replace('\n', newline);
                let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec()).unwrap_err();
                assert_eq!(error.remedy, RefusalRemedy::ArrayScaffold);
                assert_eq!(
                    error.reason.to_string(),
                    "the missing TOML array [\"plugins\", \"default\"] cannot be scaffolded by insertion alone"
                );
            }
        }
    }

    #[test]
    fn missing_array_after_managed_parent_refuses_with_any_line_ending() {
        for newline in ["\n", "\r\n"] {
            let host = "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n# <<< anvil-managed: cfg".replace('\n', newline);
            let error = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec()).unwrap_err();
            assert_eq!(error.remedy, RefusalRemedy::EnclosingOwnership);
        }
    }

    #[test]
    fn missing_array_preserves_mixed_endings_and_multiline_string_bytes() {
        let owned = concat!("# Repository comment\r\n", "[plugins]\n", "mode = '''café\r\nsecond line\n'''\r\n",);
        let unrelated = "\n[settings]\r\nvalue = 'untouched'\n";
        assert_eq!(
            output(&format!("{owned}{unrelated}")),
            format!("{owned}default = [\r\n{}]\r\n{unrelated}", REGION.replace('\n', "\r\n"))
        );
    }

    #[test]
    fn malformed_other_regions_refuse_before_adopting_matching_entries() {
        for host in [
            "plugins.default = [\n# >>> anvil-managed: other\n\"managed\",\n]\n",
            "plugins.default = [\n\"managed\",\n# <<< anvil-managed: other\n]\n",
            "plugins.default = [\n# >>> anvil-managed: other\n\"managed\",\n# <<< anvil-managed: different\n]\n",
            "plugins.default = [\n# >>> anvil-managed: other\n# >>> anvil-managed: nested\n\"managed\",\n# <<< anvil-managed: nested\n# <<< anvil-managed: other\n]\n",
            "plugins.default = [\"managed\"]\n# >>> anvil-managed: other\n",
            "plugins.default = [\n# >>> anvil-managed: other\n\"managed\",\n# <<< anvil-managed: other\n# <<< anvil-managed: other\n]\n",
        ] {
            assert_eq!(
                plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                    .unwrap_err()
                    .remedy,
                RefusalRemedy::MalformedMarkers
            );
        }
    }

    #[test]
    fn existing_inline_parent_refuses_with_shape_guidance() {
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins = { default = [] }\n"), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::ArrayShape);
        assert_eq!(error.reason.to_string(), "array parents must be normal TOML tables");
    }

    #[test]
    fn neighbor_validation_allows_missing_selectors_but_rejects_replaced_delimiters() {
        let paths = vec![vec!["plugins".to_owned(), "default".to_owned()]];
        validate_neighbor_splice("", "[plugins]\ndefault = []\n", &paths).unwrap();
        let error = validate_neighbor_splice(
            "[plugins]\ndefault = [\"old\"]\n",
            "[plugins]\ndefault = { replacement = [\"old\"] }\n",
            &paths,
        )
        .unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::ArrayDependency);
        assert_eq!(
            error.reason.to_string(),
            "this change would remove or rebind the live TOML array selector [\"plugins\", \"default\"]"
        );
    }

    #[test]
    fn adoption_refuses_a_separator_owned_by_another_region() {
        let host = "plugins.default = [\n  \"managed\"\n  # >>> anvil-managed: other\n  ,\n  # <<< anvil-managed: other\n  \"user\"\n]\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::BetweenManagedRegions
        );
        let document = Document::parse(host).unwrap();
        let error = adopt_entries(host, document["plugins"]["default"].as_array().unwrap(), "\"managed\",\n").unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::EnclosingOwnership);
        assert_eq!(
            error.reason.to_string(),
            "the matching array entry's separator belongs to another managed region"
        );
    }

    #[test]
    fn nonstring_templates_refuse_even_when_matching_repository_values() {
        let mut spec = spec();
        for (body, host) in [
            (
                "[\"x\"],\n",
                "plugins.default = [[\n# Keep this explanation.\n\"x\"\n], \"user\"]\n",
            ),
            (
                "{ a = [\"x\"] },\n",
                "plugins.default = [{ a = [\n# Keep this explanation.\n\"x\"\n] }, \"user\"]\n",
            ),
            (
                "[[\"x\"]],\n",
                "plugins.default = [[[\n# Keep this explanation.\n\"x\"\n]], \"user\"]\n",
            ),
        ] {
            spec.region.body = body.to_owned();
            assert_eq!(
                plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec)
                    .unwrap_err()
                    .remedy,
                RefusalRemedy::InvalidGeneratedToml
            );
        }
    }

    #[test]
    fn repository_compounds_and_their_comments_are_preserved() {
        let host = "plugins.default = [{ '#key' = ['#data', '''\n# multiline string data\n'''] }, \"user\"]\n";
        assert_eq!(
            output(host),
            format!("plugins.default = [\n{REGION}{{ '#key' = ['#data', '''\n# multiline string data\n'''] }}, \"user\"]\n")
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
    fn duplicate_adoption_preserves_generated_order_and_unmatched_host_values() {
        let mut spec = spec();
        spec.region.body = "\"x\",\n\"managed\",\n\"x\",\n".to_owned();
        let item = plan_toml_array_region(
            &Manifest::default(),
            Some("plugins.default = [\"x\", 'managed', \"x\", \"x\", \"user\"]\n"),
            "config.toml",
            &spec,
        )
        .unwrap();
        assert_eq!(
            item.spliced_host.unwrap(),
            "plugins.default = [\n  # >>> anvil-managed: entries\n  \"x\",\n  \"managed\",\n  \"x\",\n  # <<< anvil-managed: entries\n   \"x\", \"user\"]\n"
        );
    }

    #[test]
    fn array_boundary_overlaps_are_refused_in_both_directions() {
        for text in [
            "# >>> anvil-managed: other\nplugins.default = [\n# <<< anvil-managed: other\n\"managed\",\n]\n",
            "plugins.default = [\n\"managed\",\n# >>> anvil-managed: other\n]\n# <<< anvil-managed: other\n",
        ] {
            assert_eq!(
                plan_toml_array_region(&Manifest::default(), Some(text), "config.toml", &spec())
                    .unwrap_err()
                    .remedy,
                RefusalRemedy::EnclosingOwnership
            );
        }
    }

    #[test]
    fn nested_candidate_enclosing_another_region_is_not_adopted() {
        let host = "items = [[\n# >>> anvil-managed: other\n\"x\",\n# <<< anvil-managed: other\n], \"user\"]\n";
        let mut spec = spec();
        spec.path = vec!["items".to_owned()];
        let output = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec)
            .unwrap()
            .spliced_host
            .unwrap();
        assert_eq!(
            output,
            format!("items = [\n{REGION}[\n# >>> anvil-managed: other\n\"x\",\n# <<< anvil-managed: other\n], \"user\"]\n")
        );
        assert_eq!(Document::parse(&output).unwrap()["items"].as_array().unwrap().len(), 3);
        let other = find_region(&output, "other", CommentSyntax::Hash).unwrap().unwrap();
        assert_eq!(&output[other.body.start..other.body.end], "\"x\",\n");
    }

    #[test]
    fn partially_overlapping_ownership_refuses() {
        let host = "items = [[\n# >>> anvil-managed: other\n\"x\"],\n# <<< anvil-managed: other\n[\"x\"], \"user\"]\n";
        let mut spec = spec();
        spec.path = vec!["items".to_owned()];
        let error = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::BetweenManagedRegions);
    }

    #[test]
    fn an_array_owned_by_an_enclosing_region_is_not_given_nested_ownership() {
        let host = "# >>> anvil-managed: enclosing\n[plugins]\ndefault = [\"managed\", \"other\"]\n# <<< anvil-managed: enclosing\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::EnclosingOwnership
        );
    }
    #[test]
    fn selector_can_use_quoted_keys() {
        let mut spec = spec();
        spec.path = vec!["a.b".to_owned(), "x".to_owned()];
        let item = plan_toml_array_region(
            &Manifest::default(),
            Some("[\"a.b\"]\nx = [\"other\"]\ny = 1\n"),
            "config.toml",
            &spec,
        )
        .unwrap();
        assert_eq!(item.spliced_host.unwrap(), format!("[\"a.b\"]\nx = [\n{REGION}\"other\"]\ny = 1\n"));
    }

    #[test]
    fn matching_strings_are_adopted_but_repository_compounds_are_not() {
        let mut spec = spec();
        spec.region.body = "\"managed\",\n".to_owned();
        let item = plan_toml_array_region(
            &Manifest::default(),
            Some("plugins.default = ['managed', { y=[true], x=1 }, \"other\"]\n"),
            "config.toml",
            &spec,
        )
        .unwrap();
        assert_eq!(
            item.spliced_host.unwrap(),
            "plugins.default = [\n  # >>> anvil-managed: entries\n  \"managed\",\n  # <<< anvil-managed: entries\n { y=[true], x=1 }, \"other\"]\n"
        );
    }

    #[test]
    fn malformed_host_and_non_array_are_refused() {
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins.default = ["), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::HostAlreadyUnparsable);
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins.default = 1\n"), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::ArrayShape);
        let error = plan_toml_array_region(&Manifest::default(), Some("plugins = 1\n"), "config.toml", &spec()).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::ArrayShape);
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
    fn empty_managed_body_is_regenerated_but_inline_table_scaffolding_refuses() {
        let empty = "[plugins]\ndefault = [\n  # >>> anvil-managed: entries\n  # <<< anvil-managed: entries\n\"other\"\n]\n";
        assert_eq!(output(empty), format!("[plugins]\ndefault = [\n{REGION}\"other\"\n]\n"));
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some("plugins = { mode = true }\n"), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::ArrayShape
        );
    }

    #[test]
    fn matching_markers_outside_the_array_are_not_an_insync_region() {
        let host = "# >>> anvil-managed: entries\n# <<< anvil-managed: entries\nplugins.default = []\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::MisplacedArrayMarkers
        );
    }

    #[test]
    fn changing_a_selector_requires_retiring_the_old_region_first() {
        let mut pending = Plan::default();
        pending.push(plan(None));
        let manifest = pending.projected_manifest(&Manifest::default());
        let host = format!("{FRESH}extra = []\n");
        let mut changed = spec();
        changed.path[1] = "extra".to_owned();
        assert_eq!(
            plan_toml_array_region(&manifest, Some(&host), "config.toml", &changed)
                .unwrap_err()
                .remedy,
            RefusalRemedy::MisplacedArrayMarkers
        );
        let retired = remove_region(&host, "entries", CommentSyntax::Hash).unwrap();
        let mut pending = Plan::default();
        pending.push(PlanItem::remove_region("config.toml", "entries", retired.clone()));
        let manifest = pending.projected_manifest(&manifest);
        assert_eq!(
            plan_toml_array_region(&manifest, Some(&retired), "config.toml", &changed)
                .unwrap()
                .spliced_host
                .unwrap(),
            format!("[plugins]\ndefault = [\n]\nextra = [\n{REGION}]\n")
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
    fn region_outside_selected_array_refuses_but_string_data_is_preserved() {
        let host = format!("{REGION}plugins.default = []\n");
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::HostAlreadyUnparsable
        );
        let host = format!("plugins.default = [\"\"\"\n{REGION}\"\"\",]\n");
        assert_eq!(output(&host), format!("plugins.default = [\n{REGION}\"\"\"\n{REGION}\"\"\",]\n"));
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
            RefusalRemedy::InvalidGeneratedToml
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
