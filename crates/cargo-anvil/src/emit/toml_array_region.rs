// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-span-based placement of regions inside TOML arrays.

use ohno::app_err;
use toml_edit::{Array, Document, DocumentMut, Item, Table, Value};

use crate::catalog::TomlArrayRegionSpec;
use crate::emit::managed_region::{ManagedRegionRefusal, ManagedRegionRequest, RefusalRemedy, plan_region_with_splice};
use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::{
    CommentSyntax, Region, RegionPlacement, canonical_value, find_toml_region as find_region,
    mask_retiring_toml_regions as mask_retiring_managed_regions, remove_toml_region as remove_region, text_newline, toml_comment_lines,
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
fn scaffold(text: &str, path: &[String], retiring: &std::collections::BTreeSet<String>) -> Result<String, ManagedRegionRefusal> {
    let projected = mask_retiring_managed_regions(text, CommentSyntax::Hash, retiring);
    let normalized = projected.replace("\r\n", "\n");
    let mut document = normalized
        .parse::<DocumentMut>()
        .map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let mut item = document.as_item_mut();
    for (index, key) in path.iter().enumerate() {
        let table = item.as_table_like_mut().ok_or_else(|| {
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
    let document = Document::parse(mask_retiring_managed_regions(&scaffolded, CommentSyntax::Hash, retiring))
        .map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let array = lookup(document.as_item(), path)
        .and_then(Item::as_array)
        .expect("the scaffold inserted or validated an array");
    let span = array.span().expect("parsed array retains its span");
    let regions = validated_regions(&scaffolded)?;
    let Some(owner) = regions
        .iter()
        .find(|region| region.body.start <= span.start && span.start < region.body.end)
    else {
        return Ok(scaffolded);
    };
    // toml_edit inserts ahead of a following closing sentinel. Move the whole
    // scaffold delta, including any new parent headers, but no existing bytes.
    let parent = lookup(document.as_item(), &path[..path.len() - 1]).expect("the scaffold created the parent table");
    if parent.as_inline_table().is_some() {
        return Err(refusal(
            "the array's inline parent belongs to a managed region",
            RefusalRemedy::EnclosingOwnership,
        ));
    }
    let start = before;
    let end = start + inserted.len();
    if start < owner.body.start || end > owner.body.end {
        return Err(refusal(
            "the missing array cannot be scaffolded without changing existing managed content",
            RefusalRemedy::EnclosingOwnership,
        ));
    }
    let separator = if scaffolded[..owner.end_line.end].ends_with('\n') {
        ""
    } else {
        text_newline(text)
    };
    let moved = format!(
        "{}{}{separator}{}{}",
        &scaffolded[..start],
        &scaffolded[end..owner.end_line.end],
        &scaffolded[start..end],
        &scaffolded[owner.end_line.end..]
    );
    let moved_document = Document::parse(mask_retiring_managed_regions(&moved, CommentSyntax::Hash, retiring))
        .map_err(|error| refusal(error, RefusalRemedy::EnclosingOwnership))?;
    let moved_array = lookup(moved_document.as_item(), path).and_then(Item::as_array);
    let expected_start = owner.end_line.end - (end - start) + separator.len() + (span.start - start);
    if moved_array.and_then(Array::span).is_none_or(|span| span.start != expected_start) {
        return Err(refusal(
            "the missing array cannot be scaffolded outside managed ownership while remaining in its parent table",
            RefusalRemedy::EnclosingOwnership,
        ));
    }
    Ok(moved)
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
fn validated_regions(text: &str) -> Result<Vec<Region<'_>>, ManagedRegionRefusal> {
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
#[cfg(test)]
pub(crate) fn plan_toml_array_region(
    manifest: &Manifest,
    host_text: Option<&str>,
    host: &str,
    spec: &TomlArrayRegionSpec,
) -> Result<PlanItem, ManagedRegionRefusal> {
    plan_toml_array_region_with_retirements(manifest, host_text, host, spec, &std::collections::BTreeSet::new())
}

/// Plan an array-entry region with pending retirements.
///
/// Parse the pending-retirement projection, but splice original bytes. Masking
/// preserves offsets; ownership and marker validation still see the raw host.
pub(crate) fn plan_toml_array_region_with_retirements(
    manifest: &Manifest,
    host_text: Option<&str>,
    host: &str,
    spec: &TomlArrayRegionSpec,
    retiring: &std::collections::BTreeSet<String>,
) -> Result<PlanItem, ManagedRegionRefusal> {
    validate_spec(spec)?;
    let original = host_text.unwrap_or("");
    let regions = validated_regions(original)?;
    let document = Document::parse(mask_retiring_managed_regions(original, CommentSyntax::Hash, retiring))
        .map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let scaffolded;
    let (base, document, regions) = if lookup(document.as_item(), &spec.path).is_some() {
        (original, document, regions)
    } else {
        scaffolded = scaffold(original, &spec.path, retiring)?;
        let document = Document::parse(mask_retiring_managed_regions(&scaffolded, CommentSyntax::Hash, retiring))
            .map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
        let regions = validated_regions(&scaffolded)?;
        (scaffolded.as_str(), document, regions)
    };
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
        if (other.start_line.start <= span.start && span.start < other.end_line.end)
            || (other.start_line.start < span.end && span.end <= other.end_line.end)
        {
            return Err(refusal(
                format!(
                    "the selected array belongs to managed region '{id}'; retire its enclosing ownership before managing array entries"
                ),
                RefusalRemedy::EnclosingOwnership,
            ));
        }
    }
    let region =
        find_region(base, spec.region.id.as_str(), CommentSyntax::Hash).map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
    if let Some(region) = &region {
        if region.start_line.start <= span.start || region.end_line.end > span.end {
            return Err(refusal(
                "the managed region is outside its selected TOML array",
                RefusalRemedy::MisplacedArrayMarkers,
            ));
        }
        // A whole-value boundary is required: markers inside a multiline string
        // or a nested value must not claim part of that value.
        if array.iter().any(|value| {
            let value = value.span().expect("parsed array values retain source spans");
            (value.start < region.body.start && value.end > region.body.start)
                || (value.start < region.body.end && value.end > region.body.end)
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
        Document::parse(mask_retiring_managed_regions(&spliced, CommentSyntax::Hash, retiring))
            .map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedToml))?;
        validated_regions(&spliced)?;
        Ok(spliced)
    })
}

/// Validate neighboring splices against live array selectors.
///
/// A neighboring region must not remove array delimiters or rebind an existing
/// selector to a different source location, even when the result still parses.
pub(crate) fn validate_neighbor_splice(
    before: &str,
    after: &str,
    paths: &[Vec<String>],
    id: &str,
    syntax: CommentSyntax,
    retiring: &std::collections::BTreeSet<String>,
) -> Result<(), ManagedRegionRefusal> {
    let masked_before = mask_retiring_managed_regions(before, syntax, retiring);
    let original = if retiring.is_empty() {
        None
    } else {
        // Earlier writes can temporarily duplicate a table whose independently
        // validated retirement is still pending in this same pass. The projected
        // document below must still parse; an invalid raw intermediate alone is
        // not sufficient to refuse a migration.
        Document::parse(before).ok()
    };
    let projected_original =
        Document::parse(masked_before.as_str()).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))?;
    let masked_after = mask_retiring_managed_regions(after, syntax, retiring);
    let updated = Document::parse(masked_after.as_str()).map_err(|error| refusal(error, RefusalRemedy::BetweenManagedRegions))?;
    // A relocation is a removal followed by an insertion, not one replacement
    // that claims every byte between the old and new region positions.
    let intermediate = remove_region(before, id, syntax).map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
    let remove_offset = splice_offset_map(before, &intermediate);
    let insert_offset = splice_offset_map(&intermediate, after);
    for path in paths {
        // A parseable intermediate can temporarily rebind a key too. Protect
        // dependencies present in either the raw or retirement-projected host.
        for document in original.iter().chain(std::iter::once(&projected_original)) {
            let Some(array) = lookup(document.as_item(), path).and_then(Item::as_array) else {
                continue;
            };
            let span = array.span().expect("parsed arrays retain source spans");
            let map_offset = |offset| remove_offset(offset).and_then(&insert_offset);
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
    }
    Ok(())
}

/// Map only common-prefix/suffix offsets; the changed interval intentionally has no
/// identity, so replacement delimiters cannot stand in for original ones. Relocation
/// composes separate removal and insertion maps in the caller.
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

/// Adopt a matching multiset of unmanaged entries.
///
/// Remove one matching unmanaged value for each generated entry, not comments
/// or other regions. Array punctuation is bounded by parser-provided spans.
fn adopt_entries(text: &str, array: &Array, body: &str) -> Result<String, ManagedRegionRefusal> {
    let generated = body_array(body)?;
    let protected = validated_regions(text)?;
    let values: Vec<_> = array.iter().collect();
    let canonical: Vec<_> = values.iter().map(|value| canonical_value(value)).collect();
    let mut removals = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for generated in &generated {
        let generated = canonical_value(generated);
        let candidate = values.iter().enumerate().find(|(index, value)| {
            let span = value.span().expect("parsed array values retain source spans");
            !used.contains(index)
                && !protected
                    .iter()
                    .any(|region| span.start < region.end_line.end && region.start_line.start < span.end)
                && generated == canonical[*index]
        });
        if let Some((index, value)) = candidate {
            if has_interior_comments(text, value) {
                return Err(refusal(
                    "a matching compound array entry contains repository-owned comments",
                    RefusalRemedy::CommentedArrayEntry,
                ));
            }
            used.insert(index);
            let span = value.span().expect("parsed array values retain source spans");
            let next = values
                .get(index + 1)
                .and_then(|value| value.span())
                .map_or_else(|| array.span().expect("parsed array retains its span").end - 1, |span| span.start);
            if let Some(comma) = separator_comma(&text[span.end..next]) {
                let comma = span.end + comma..span.end + comma + 1;
                if protected
                    .iter()
                    .any(|region| comma.start < region.end_line.end && region.start_line.start < comma.end)
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

fn has_interior_comments(text: &str, value: &Value) -> bool {
    let span = value.span().expect("parsed values retain source spans");
    // The outer entry is a real source value. Dotted inline keys create implicit
    // table proxies whose spans do not enclose their children; do not recurse into
    // those proxies. Lexing the actual container also distinguishes quoted '#'.
    toml_parser::Source::new(&text[span])
        .lex()
        .any(|token| token.kind() == toml_parser::lexer::TokenKind::Comment)
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
    fn review_scaffold_never_replaces_repository_or_retirement_bytes() {
        for newline in ["\n", "\r\n"] {
            for old in ["", "# >>> anvil-managed: old\nobsolete = \"café\"\n# <<< anvil-managed: old\n"] {
                let host = format!("{old}plugins.a=true\nother=true\nplugins.b=true\n").replace('\n', newline);
                let retiring = std::collections::BTreeSet::from(["old".to_owned()]);
                let error = plan_toml_array_region_with_retirements(&Manifest::default(), Some(&host), "config.toml", &spec(), &retiring)
                    .unwrap_err();
                assert_eq!(
                    error.reason.to_string(),
                    "the missing TOML array [\"plugins\", \"default\"] cannot be scaffolded by insertion alone"
                );
                let corrected = format!("{host}plugins.default=[]{newline}");
                let item =
                    plan_toml_array_region_with_retirements(&Manifest::default(), Some(&corrected), "config.toml", &spec(), &retiring)
                        .unwrap();
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
        spec.region.body = "[1],\n".to_owned();
        let host = "items = [[\n# >>> anvil-managed: entries\n1,\n# <<< anvil-managed: entries\n]]\n";
        let error = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::MisplacedArrayMarkers);
        assert_eq!(error.reason.to_string(), "the region splits a TOML array value");
    }

    #[test]
    fn review_dotted_inline_proxy_adoption_uses_real_container() {
        for (body, entry) in [
            ("{ a.b = 1 },\n", "{ a.b = 1 }"),
            (
                "{ \"a#\".'b#' = [1, { c.d = \"#data\" }] },\n",
                "{ \"a#\".'b#' = [1, { c.d = \"#data\" }] }",
            ),
        ] {
            let mut spec = spec();
            spec.path = vec!["items".to_owned()];
            spec.region.body = body.to_owned();
            let host = format!("items = [{entry}]\n");
            let item = plan_toml_array_region(&Manifest::default(), Some(&host), "config.toml", &spec).unwrap();
            assert_eq!(item.decision, Decision::Write);
            let output = item.spliced_host.as_ref().unwrap();
            assert_eq!(
                output,
                &format!("items = [\n  # >>> anvil-managed: entries\n  {entry},\n  # <<< anvil-managed: entries\n]\n")
            );
            let document = Document::parse(&output).unwrap();
            assert_eq!(document["items"].as_array().unwrap().len(), 1);
            assert_eq!(
                canonical_value(document["items"].as_array().unwrap().get(0).unwrap()),
                canonical_value(body_array(body).unwrap().get(0).unwrap())
            );
            assert_eq!(
                plan_toml_array_region(&Manifest::default(), Some(output), "config.toml", &spec)
                    .unwrap()
                    .decision,
                Decision::InSync
            );
        }
        let mut spec = spec();
        spec.path = vec!["items".to_owned()];
        spec.region.body = "{ a.b = [1, 2] },\n".to_owned();
        let host = "items = [{ a.b = [1, # repository guidance\n2] }]\n";
        let error = plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec).unwrap_err();
        assert_eq!(error.remedy, RefusalRemedy::CommentedArrayEntry);
        assert_eq!(
            error.reason.to_string(),
            "a matching compound array entry contains repository-owned comments"
        );
    }

    #[test]
    fn pending_retirement_projection_preserves_original_bytes_and_scaffold_offsets() {
        for newline in ["\n", "\r\n"] {
            for prefix in ["# Repository comment: café\n", "plugins.default = [\"user\"]\n"] {
                let old = "# >>> anvil-managed: old\n[settings]\nmode = \"café\"\n# <<< anvil-managed: old\n".replace('\n', newline);
                let new = "# >>> anvil-managed: new\n[settings]\nmode = false\n# <<< anvil-managed: new\n".replace('\n', newline);
                let host = format!("{}{old}{new}", prefix.replace('\n', newline));
                let retiring = std::collections::BTreeSet::from(["old".to_owned()]);
                let item =
                    plan_toml_array_region_with_retirements(&Manifest::default(), Some(&host), "config.toml", &spec(), &retiring).unwrap();
                assert_eq!(item.decision, Decision::Write);
                let spliced = item.spliced_host.unwrap();
                assert!(spliced.contains(&old));
                assert!(spliced.contains(&new), "{spliced:?}");
                assert!(spliced.starts_with(&prefix.split('=').next().unwrap().replace('\n', newline)));
                let retired = remove_region(&spliced, "old", CommentSyntax::Hash).unwrap();
                let document = Document::parse(&retired).unwrap();
                let array = document["plugins"]["default"].as_array().unwrap();
                assert_eq!(
                    array.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
                    if prefix.starts_with("plugins") {
                        vec!["managed", "user"]
                    } else {
                        vec!["managed"]
                    }
                );
                assert_eq!(document["settings"]["mode"].as_bool(), Some(false));
                assert_eq!(
                    find_region(&spliced, "old", CommentSyntax::Hash).unwrap().unwrap().body_str(),
                    format!("[settings]{newline}mode = \"café\"{newline}")
                );
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
    fn missing_array_is_scaffolded_after_the_parent_keys_managed_region() {
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
        let generated = output(&host);
        assert_eq!(
            generated,
            format!(
                "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n# <<< anvil-managed: cfg\ndefault = [\n{REGION}]\n\n[settings]\nvalue = 1\n"
            )
        );
        assert_eq!(plan(Some(&generated)).decision, Decision::InSync);
        assert_eq!(
            remove_region(&generated, "entries", CommentSyntax::Hash).unwrap(),
            "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n# <<< anvil-managed: cfg\ndefault = [\n]\n\n[settings]\nvalue = 1\n"
        );
    }

    #[test]
    fn scaffolding_refuses_to_move_a_key_into_another_table_or_out_of_an_inline_parent() {
        for host in [
            "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n[settings]\nvalue = 1\n# <<< anvil-managed: cfg\n",
            "# >>> anvil-managed: cfg\nplugins = { mode = true }\n# <<< anvil-managed: cfg\n",
        ] {
            assert_eq!(
                plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                    .unwrap_err()
                    .remedy,
                RefusalRemedy::EnclosingOwnership
            );
        }
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
    fn missing_array_after_managed_parent_preserves_crlf_and_unterminated_closer() {
        for newline in ["\n", "\r\n"] {
            let host = "# >>> anvil-managed: cfg\n[plugins]\nmode = true\n# <<< anvil-managed: cfg".replace('\n', newline);
            let generated = output(&host);
            assert_eq!(
                generated,
                format!("{}{newline}default = [{newline}{}]{newline}", host, REGION.replace('\n', newline))
            );
        }
    }

    #[test]
    fn missing_array_preserves_mixed_endings_and_multiline_string_bytes() {
        let owned = concat!(
            "# >>> anvil-managed: cfg\r\n",
            "[plugins]\n",
            "mode = '''café\r\nsecond line\n'''\r\n",
            "# <<< anvil-managed: cfg\n"
        );
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
    fn adoption_refuses_a_separator_owned_by_another_region() {
        let host = "plugins.default = [\n  \"managed\"\n  # >>> anvil-managed: other\n  ,\n  # <<< anvil-managed: other\n  \"user\"\n]\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec())
                .unwrap_err()
                .remedy,
            RefusalRemedy::EnclosingOwnership
        );
    }

    #[test]
    fn adoption_refuses_interior_comments_in_nested_arrays_and_inline_tables() {
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
                RefusalRemedy::CommentedArrayEntry
            );
        }
    }

    #[test]
    fn hash_in_string_tokens_and_quoted_keys_is_not_an_interior_comment() {
        let mut spec = spec();
        spec.region.body = "{ \"#key\" = [\"#data\", '''\n# multiline string data\n'''] },\n".to_owned();
        let host = "plugins.default = [{ '#key' = ['#data', '''\n# multiline string data\n'''] }, \"user\"]\n";
        assert_eq!(
            plan_toml_array_region(&Manifest::default(), Some(host), "config.toml", &spec)
                .unwrap()
                .spliced_host
                .unwrap(),
            "plugins.default = [\n  # >>> anvil-managed: entries\n  { \"#key\" = [\"#data\", '''\n# multiline string data\n'''] },\n  # <<< anvil-managed: entries\n \"user\"]\n"
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
            RefusalRemedy::EnclosingOwnership
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
