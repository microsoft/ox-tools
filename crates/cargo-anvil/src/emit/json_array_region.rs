// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-span-based placement of regions inside JSON arrays.

use std::collections::BTreeSet;
use std::ops::Range;

use jsonc_parser::ast::{Array, Object, Value};
use jsonc_parser::{CollectOptions, ParseOptions, parse_to_ast};
use ohno::app_err;

use crate::catalog::JsonArrayRegionSpec;
use crate::emit::managed_region::{ManagedRegionRefusal, ManagedRegionRequest, RefusalRemedy, plan_region_with_splice};
use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::{
    CommentSyntax, HostScanner, Region, RegionPlacement, find_region_in_lines, json_comment_lines, remove_located_region, text_newline,
};

fn refusal(reason: impl std::fmt::Display, remedy: RefusalRemedy) -> ManagedRegionRefusal {
    ManagedRegionRefusal::new(app_err!("{reason}"), remedy)
}

fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

fn parse(text: &str) -> Result<jsonc_parser::ParseResult<'_>, ManagedRegionRefusal> {
    parse_to_ast(text, &CollectOptions::default(), &parse_options()).map_err(|error| refusal(error, RefusalRemedy::HostAlreadyUnparsable))
}

fn value_range(value: &Value<'_>) -> Range<usize> {
    match value {
        Value::StringLit(value) => value.range.start..value.range.end,
        Value::NumberLit(value) => value.range.start..value.range.end,
        Value::BooleanLit(value) => value.range.start..value.range.end,
        Value::NullKeyword(value) => value.range.start..value.range.end,
        Value::Object(value) => value.range.start..value.range.end,
        Value::Array(value) => value.range.start..value.range.end,
    }
}

fn lookup<'a>(root: &'a Value<'a>, path: &[String]) -> Result<Option<&'a Value<'a>>, ManagedRegionRefusal> {
    let mut value = root;
    for (index, key) in path.iter().enumerate() {
        let object = value.as_object().ok_or_else(|| {
            refusal(
                format!("the parent of JSON array {path:?} is not an object"),
                RefusalRemedy::ArrayShape,
            )
        })?;
        let mut properties = object.properties.iter().filter(|property| property.name.as_str() == key);
        let Some(property) = properties.next() else {
            return Ok(None);
        };
        if properties.next().is_some() {
            return Err(refusal(
                format!("the JSON array selector {path:?} is ambiguous because property {key:?} occurs more than once"),
                RefusalRemedy::ArrayShape,
            ));
        }
        value = &property.value;
        if index + 1 != path.len() && value.as_object().is_none() {
            return Err(refusal(
                format!("the parent of JSON array {path:?} is not an object"),
                RefusalRemedy::ArrayShape,
            ));
        }
    }
    Ok(Some(value))
}

/// Validate the template independently of repository state.
pub(crate) fn validate_spec(spec: &JsonArrayRegionSpec) -> Result<(), ManagedRegionRefusal> {
    if let crate::catalog::HostSelector::Path(host) = &spec.region.host
        && !host.to_ascii_lowercase().ends_with(".json")
    {
        return Err(refusal("JSON array hosts must have a .json suffix", RefusalRemedy::ArrayShape));
    }
    if spec.path.is_empty() || spec.region.syntax != CommentSyntax::SlashSlash {
        return Err(refusal(
            "a JSON array region requires a nonempty property path and slash-slash comment syntax",
            RefusalRemedy::InvalidGeneratedJson,
        ));
    }
    body_strings(&spec.region.body)?;
    Ok(())
}

fn body_strings(body: &str) -> Result<Vec<String>, ManagedRegionRefusal> {
    let source = format!("[\n{body}\n]");
    let parsed = parse_to_ast(&source, &CollectOptions::default(), &parse_options())
        .map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedJson))?;
    let array = parsed.value.as_ref().and_then(Value::as_array).ok_or_else(|| {
        refusal(
            "the region must contain only JSON array entries",
            RefusalRemedy::InvalidGeneratedJson,
        )
    })?;
    if array.elements.is_empty() {
        return Err(refusal(
            "generated array bodies must contain at least one string entry",
            RefusalRemedy::InvalidGeneratedJson,
        ));
    }
    let strings = array
        .elements
        .iter()
        .map(|value| {
            value
                .as_string_lit()
                .map(|value| value.value.to_string())
                .ok_or_else(|| refusal("generated array entries must be strings", RefusalRemedy::InvalidGeneratedJson))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut unique = BTreeSet::new();
    if let Some(duplicate) = strings.iter().find(|value| !unique.insert(value.as_str())) {
        return Err(refusal(
            format!("generated JSON array entry {duplicate:?} occurs more than once"),
            RefusalRemedy::InvalidGeneratedJson,
        ));
    }
    if json_comment_lines(&source).any(|line| {
        let comment = source[line.start..line.end].trim();
        comment.starts_with("// >>> anvil-managed:") || comment.starts_with("// <<< anvil-managed:")
    }) {
        return Err(refusal(
            "JSON array-entry bodies must not contain managed-region sentinel comments",
            RefusalRemedy::InvalidGeneratedJson,
        ));
    }
    let last = value_range(array.elements.last().expect("the nonempty array was checked above"));
    if separator_comma(&source[last.end..array.range.end - 1]).is_none() {
        return Err(refusal(
            "JSON array-entry bodies must end with a trailing comma",
            RefusalRemedy::InvalidGeneratedJson,
        ));
    }
    Ok(strings)
}

fn render_body(body: &str, indent: &str, newline: &str) -> String {
    let mut rendered = String::with_capacity(body.len() + indent.len() * body.lines().count());
    for line in body.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        if !content.trim().is_empty() {
            rendered.push_str(indent);
            rendered.push_str(content);
        }
        rendered.push_str(newline);
    }
    rendered
}

fn line_indent(text: &str, offset: usize) -> &str {
    let start = text[..offset].rfind(['\r', '\n']).map_or(0, |index| index + 1);
    &text[start..offset.min(start + text[start..offset].len() - text[start..offset].trim_start_matches([' ', '\t']).len())]
}

fn indent_unit(text: &str, object: &Object<'_>) -> String {
    let parent = line_indent(text, object.range.end - 1);
    object
        .properties
        .first()
        .map(|property| line_indent(text, property.range.start))
        .and_then(|indent| indent.strip_prefix(parent))
        .filter(|indent| !indent.is_empty())
        .unwrap_or("  ")
        .to_owned()
}

fn nested_property(path: &[String], base_indent: &str, unit: &str, newline: &str) -> Result<String, ManagedRegionRefusal> {
    let mut output = String::new();
    for (index, key) in path.iter().enumerate() {
        let indent = format!("{base_indent}{}", unit.repeat(index + 1));
        let encoded = serde_json::to_string(key).map_err(|error| refusal(error, RefusalRemedy::InvalidGeneratedJson))?;
        output.push_str(&indent);
        output.push_str(&encoded);
        if index + 1 == path.len() {
            output.push_str(": []");
        } else {
            output.push_str(": {");
            output.push_str(newline);
        }
    }
    for index in (0..path.len().saturating_sub(1)).rev() {
        output.push_str(newline);
        output.push_str(base_indent);
        output.push_str(&unit.repeat(index + 1));
        output.push('}');
    }
    Ok(output)
}

/// Create only missing object/array scaffolding, leaving existing bytes intact.
fn scaffold(text: &str, path: &[String]) -> Result<String, ManagedRegionRefusal> {
    if text.trim().is_empty() {
        let newline = text_newline(text);
        let property = nested_property(path, "", "  ", newline)?;
        return Ok(format!("{{{newline}{property}{newline}}}{newline}"));
    }
    let parsed = parse(text)?;
    let root = parsed
        .value
        .as_ref()
        .ok_or_else(|| refusal("JSON array hosts must contain an object", RefusalRemedy::ArrayShape))?;
    let mut value = root;
    for (index, key) in path.iter().enumerate() {
        let object = value.as_object().ok_or_else(|| {
            refusal(
                format!("the parent of JSON array {path:?} is not an object"),
                RefusalRemedy::ArrayShape,
            )
        })?;
        let mut properties = object.properties.iter().filter(|property| property.name.as_str() == key);
        if let Some(property) = properties.next() {
            if properties.next().is_some() {
                return Err(refusal(
                    format!("the JSON array selector {path:?} is ambiguous because property {key:?} occurs more than once"),
                    RefusalRemedy::ArrayShape,
                ));
            }
            value = &property.value;
            continue;
        }

        let newline = text_newline(text);
        let parent_indent = line_indent(text, object.range.end - 1);
        let unit = indent_unit(text, object);
        let property = nested_property(&path[index..], parent_indent, &unit, newline)?;
        let close = object.range.end - 1;
        let insert_at = close - parent_indent.len();
        let mut output = String::with_capacity(text.len() + property.len() + newline.len() * 2 + 1);
        if let Some(last) = object.properties.last() {
            output.push_str(&text[..last.range.end]);
            if separator_comma(&text[last.range.end..insert_at]).is_none() {
                output.push(',');
            }
            output.push_str(&text[last.range.end..insert_at]);
            if !text[last.range.end..insert_at].ends_with(['\r', '\n']) {
                output.push_str(newline);
            }
        } else {
            output.push_str(&text[..insert_at]);
            output.push_str(newline);
        }
        output.push_str(&property);
        output.push_str(newline);
        output.push_str(parent_indent);
        output.push_str(&text[close..]);
        parse(&output)?;
        return Ok(output);
    }
    Err(refusal(
        "the selected JSON property is missing after scaffolding",
        RefusalRemedy::ArrayScaffold,
    ))
}

/// Validate all JSON ownership boundaries.
pub(crate) fn validated_regions(text: &str) -> Result<Vec<Region<'_>>, ManagedRegionRefusal> {
    let mut regions = Vec::new();
    let mut open = None;
    let mut seen = BTreeSet::new();
    for line in json_comment_lines(text) {
        let content = text[line.start..line.end].trim();
        if let Some(id) = content.strip_prefix("// >>> anvil-managed:") {
            let id = id.trim();
            if id.is_empty() || open.is_some() || !seen.insert(id) {
                return Err(refusal("duplicate or nested opening sentinel", RefusalRemedy::MalformedMarkers));
            }
            open = Some((id, line));
        } else if let Some(id) = content.strip_prefix("// <<< anvil-managed:") {
            let id = id.trim();
            let Some((_, start)) = open.take().filter(|(opening, _)| *opening == id) else {
                return Err(refusal(
                    "closing sentinel without its matching opener",
                    RefusalRemedy::MalformedMarkers,
                ));
            };
            regions.push(
                find_region_in_lines(text, id, CommentSyntax::SlashSlash, [start, line].into_iter())
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

/// Plan through the ordinary checksum/edited-body policy with a JSON array splice.
pub(crate) fn plan_json_array_region(
    manifest: &Manifest,
    host_text: Option<&str>,
    host: &str,
    spec: &JsonArrayRegionSpec,
) -> Result<PlanItem, ManagedRegionRefusal> {
    validate_spec(spec)?;
    let original = host_text.unwrap_or("");
    let regions = validated_regions(original)?;
    let parsed = parse(original)?;
    let scaffolded;
    let (base, parsed, regions) = if parsed
        .value
        .as_ref()
        .is_some_and(|root| lookup(root, &spec.path).ok().flatten().is_some())
    {
        (original, parsed, regions)
    } else {
        scaffolded = scaffold(original, &spec.path)?;
        let parsed = parse(&scaffolded)?;
        let regions = validated_regions(&scaffolded)?;
        (scaffolded.as_str(), parsed, regions)
    };
    let root = parsed
        .value
        .as_ref()
        .ok_or_else(|| refusal("JSON array hosts must contain an object", RefusalRemedy::ArrayShape))?;
    let array = lookup(root, &spec.path)?.and_then(Value::as_array).ok_or_else(|| {
        refusal(
            format!("the selected JSON item {:?} is not an array", spec.path),
            RefusalRemedy::ArrayShape,
        )
    })?;
    let span = array.range.start..array.range.end;
    for other in regions {
        if other.id == spec.region.id.as_str() {
            continue;
        }
        let ownership = other.start_line.start..other.end_line.end;
        if ownership.contains(&span.start) || ownership.contains(&span.end) {
            return Err(refusal(
                format!(
                    "the selected array belongs to managed region '{}'; retire its enclosing ownership before managing array entries",
                    other.id
                ),
                RefusalRemedy::EnclosingOwnership,
            ));
        }
        let without = remove_region_exact(base, &other);
        validate_neighbor_splice(base, &without, std::slice::from_ref(&spec.path))?;
    }
    let region = find_region_in_lines(base, spec.region.id.as_str(), CommentSyntax::SlashSlash, json_comment_lines(base))
        .map_err(|error| refusal(error, RefusalRemedy::MalformedMarkers))?;
    if let Some(region) = &region {
        if !span.contains(&region.start_line.start) || !span.contains(&region.end_line.end) {
            return Err(refusal(
                "the managed region is outside its selected JSON array",
                RefusalRemedy::MisplacedArrayMarkers,
            ));
        }
        if array.elements.iter().any(|value| {
            let value = value_range(value);
            contains_interior(&value, region.body.start) || contains_interior(&value, region.body.end)
        }) {
            return Err(refusal(
                "the region splits a JSON array value",
                RefusalRemedy::MisplacedArrayMarkers,
            ));
        }
    }
    let newline = text_newline(original);
    let indent = array_indent(base, array);
    let body = render_body(&spec.region.body, &indent, newline);
    let rendered = format!(
        "{indent}// >>> anvil-managed: {}{newline}{body}{indent}// <<< anvil-managed: {}{newline}",
        spec.region.id, spec.region.id
    );
    let request = ManagedRegionRequest {
        host_relpath: host,
        region_id: spec.region.id.as_str(),
        rendered_body: &body,
        syntax: CommentSyntax::SlashSlash,
        placement: RegionPlacement::End,
        newline: Some(newline),
    };
    plan_region_with_splice(manifest, host_text, request, HostScanner::Json, || {
        let spliced = if let Some(region) = region {
            format!("{}{}{}", &base[..region.start_line.start], rendered, &base[region.end_line.end..])
        } else {
            let adopted = adopt_entries(base, array, &spec.region.body)?;
            let at = span.start + 1;
            let rest = &adopted[at..];
            let rest = rest.strip_prefix(newline).unwrap_or(rest);
            format!("{}{newline}{rendered}{rest}", &adopted[..at])
        };
        parse(&spliced).map_err(|refusal| ManagedRegionRefusal::new(refusal.reason, RefusalRemedy::InvalidGeneratedJson))?;
        validated_regions(&spliced)?;
        Ok(spliced)
    })
}

fn array_indent(text: &str, array: &Array<'_>) -> String {
    let parent = line_indent(text, array.range.start);
    if let Some(indent) = array
        .elements
        .first()
        .map(|value| line_indent(text, value_range(value).start))
        .filter(|indent| indent.len() > parent.len())
    {
        indent.to_owned()
    } else {
        format!("{parent}  ")
    }
}

fn contains_interior(span: &Range<usize>, offset: usize) -> bool {
    span.contains(&offset) && span.start != offset
}

/// Validate neighboring splices against live JSON array selectors.
pub(crate) fn validate_neighbor_splice(before: &str, after: &str, paths: &[Vec<String>]) -> Result<(), ManagedRegionRefusal> {
    let original = parse(before)?;
    let updated = parse(after).map_err(|refusal| ManagedRegionRefusal::new(refusal.reason, RefusalRemedy::BetweenManagedRegions))?;
    let map_offset = splice_offset_map(before, after);
    for path in paths {
        let Some(original_array) = original
            .value
            .as_ref()
            .and_then(|root| lookup(root, path).ok().flatten())
            .and_then(Value::as_array)
        else {
            continue;
        };
        let span = original_array.range.start..original_array.range.end;
        let mapped = map_offset(span.start).zip(map_offset(span.end - 1));
        let remaining = updated
            .value
            .as_ref()
            .and_then(|root| lookup(root, path).ok().flatten())
            .and_then(Value::as_array)
            .map(|array| array.range.start..array.range.end);
        if mapped
            .zip(remaining)
            .is_none_or(|((start, end), remaining)| remaining != (start..end + 1))
        {
            return Err(refusal(
                format!("this change would remove or rebind the live JSON array selector {path:?}"),
                RefusalRemedy::ArrayDependency,
            ));
        }
    }
    Ok(())
}

/// Recover selectors from existing array-contained markers.
pub(crate) fn existing_array_paths(text: &str) -> Result<Vec<Vec<String>>, ManagedRegionRefusal> {
    fn visit(
        value: &Value<'_>,
        path: &mut Vec<String>,
        markers: &[usize],
        paths: &mut Vec<Vec<String>>,
    ) -> Result<(), ManagedRegionRefusal> {
        if let Some(array) = value.as_array()
            && markers.iter().any(|offset| (array.range.start..array.range.end).contains(offset))
        {
            paths.push(path.clone());
        }
        if let Some(object) = value.as_object() {
            let mut names = BTreeSet::new();
            for property in &object.properties {
                let name = property.name.as_str();
                if !names.insert(name) {
                    return Err(refusal(
                        format!("JSON object property {name:?} occurs more than once, so array ownership is ambiguous"),
                        RefusalRemedy::ArrayShape,
                    ));
                }
                path.push(name.to_owned());
                visit(&property.value, path, markers, paths)?;
                path.pop();
            }
        }
        Ok(())
    }
    let parsed = parse(text)?;
    let markers: Vec<_> = json_comment_lines(text)
        .filter_map(|line| {
            let comment = text[line.start..line.end].trim();
            (comment.starts_with("// >>> anvil-managed:") || comment.starts_with("// <<< anvil-managed:")).then_some(line.start)
        })
        .collect();
    let mut paths = Vec::new();
    if let Some(root) = &parsed.value {
        visit(root, &mut Vec::new(), &markers, &mut paths)?;
    }
    Ok(paths)
}

pub(crate) fn offset_inside_array(text: &str, offset: usize) -> bool {
    fn visit(value: &Value<'_>, offset: usize) -> bool {
        value
            .as_array()
            .is_some_and(|array| (array.range.start..array.range.end).contains(&offset))
            || value
                .as_object()
                .is_some_and(|object| object.properties.iter().any(|property| visit(&property.value, offset)))
    }
    parse(text)
        .ok()
        .and_then(|parsed| parsed.value)
        .is_some_and(|root| visit(&root, offset))
}

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

fn adopt_entries(text: &str, array: &Array<'_>, body: &str) -> Result<String, ManagedRegionRefusal> {
    let generated = body_strings(body)?;
    let protected = validated_regions(text)?;
    let values: Vec<_> = array.elements.iter().collect();
    let mut removals = Vec::new();
    for generated in generated {
        let matching = values.iter().enumerate().filter(|(_, value)| {
            let span = value_range(value);
            value.as_string_lit().is_some_and(|value| value.value == generated)
                && !protected
                    .iter()
                    .any(|region| (region.start_line.start..region.end_line.end).contains(&span.start))
        });
        let protected_match = values.iter().any(|value| {
            let span = value_range(value);
            value.as_string_lit().is_some_and(|value| value.value == generated)
                && protected
                    .iter()
                    .any(|region| (region.start_line.start..region.end_line.end).contains(&span.start))
        });
        if protected_match {
            return Err(refusal(
                format!("required JSON array entry {generated:?} is already owned by another managed region"),
                RefusalRemedy::EnclosingOwnership,
            ));
        }
        for (index, value) in matching {
            let span = value_range(value);
            let next = values.get(index + 1).map_or(array.range.end - 1, |value| value_range(value).start);
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
    let bytes = gap.as_bytes();
    let mut index = 0;
    let mut string = false;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
        } else if line_comment {
            if matches!(byte, b'\r' | b'\n') {
                line_comment = false;
            }
        } else if block_comment {
            if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                block_comment = false;
                index += 1;
            }
        } else if byte == b'"' {
            string = true;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            line_comment = true;
            index += 1;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            block_comment = true;
            index += 1;
        } else if byte == b',' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn remove_region_exact(text: &str, region: &Region<'_>) -> String {
    format!("{}{}", &text[..region.start_line.start], &text[region.end_line.end..])
}

pub(crate) fn remove_json_region(text: &str, id: &str, syntax: CommentSyntax) -> Result<String, ohno::AppError> {
    let region = find_region_in_lines(text, id, syntax, json_comment_lines(text))?;
    if let Some(region) = &region
        && offset_inside_array(text, region.start_line.start)
    {
        return Ok(remove_region_exact(text, region));
    }
    Ok(remove_located_region(text, region))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, CliMeta, HostSelector, RegionId, RegionSpec};
    use crate::checksum::checksum_str;
    use crate::decision::Decision;

    const ENTRY: &str = r#""build\\Extensions\\Anvil","#;

    fn spec() -> JsonArrayRegionSpec {
        JsonArrayRegionSpec {
            region: RegionSpec {
                host: HostSelector::Path("CloudBuild.json".to_owned()),
                id: RegionId::new("anvil-source-root"),
                body: format!("{ENTRY}\n"),
                syntax: CommentSyntax::SlashSlash,
            },
            path: vec!["EnlistmentRootPaths".to_owned(), "SourceRootDirs".to_owned()],
        }
    }

    fn plan(text: Option<&str>, manifest: &Manifest) -> Result<PlanItem, ManagedRegionRefusal> {
        plan_json_array_region(manifest, text, "CloudBuild.json", &spec())
    }

    #[test]
    fn catalog_rejects_invalid_json_array_specs() {
        let cases = [
            (
                "CloudBuild.json",
                CommentSyntax::Hash,
                format!("{ENTRY}\n"),
                "slash-slash comment syntax",
            ),
            ("CloudBuild.jsonc", CommentSyntax::SlashSlash, format!("{ENTRY}\n"), ".json suffix"),
            (
                "CloudBuild.json",
                CommentSyntax::SlashSlash,
                String::new(),
                "at least one string entry",
            ),
            (
                "CloudBuild.json",
                CommentSyntax::SlashSlash,
                "\"one\",\n\"one\",\n".to_owned(),
                "occurs more than once",
            ),
            (
                "CloudBuild.json",
                CommentSyntax::SlashSlash,
                "\"one\"\n".to_owned(),
                "trailing comma",
            ),
        ];
        for (host, syntax, body, expected) in cases {
            let mut invalid = spec();
            invalid.region.host = HostSelector::Path(host.to_owned());
            invalid.region.syntax = syntax;
            invalid.region.body = body;
            let error = Catalog::builder(CliMeta::new("anvil"))
                .with_json_array_region(invalid)
                .build()
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn introduction_preserves_jsonc_and_adopts_all_duplicate_entries() {
        let source = concat!(
            "{\n",
            "  // repository comment\n",
            "  \"EnlistmentRootPaths\": {\n",
            "    \"SourceRootDirs\": [\n",
            "      \"build\\\\Extensions\\\\Anvil\",\n",
            "      /* retained */ \"src\",\n",
            "      \"build\\\\Extensions\\\\Anvil\",\n",
            "    ],\n",
            "    \"Other\": true,\n",
            "  },\n",
            "  \"Tail\": 1,\n",
            "}\n",
        );
        let item = plan(Some(source), &Manifest::default()).unwrap();
        let output = item.spliced_host.as_deref().unwrap();

        assert_eq!(output.matches(r#""build\\Extensions\\Anvil""#).count(), 1);
        assert!(output.contains("// repository comment"));
        assert!(output.contains("/* retained */ \"src\","));
        assert!(output.contains("\"Other\": true"));
        assert!(output.contains("\"Tail\": 1"));
        assert!(output.contains("// >>> anvil-managed: anvil-source-root"));
        assert_eq!(item.decision, Decision::Write);

        let mut manifest = Manifest::default();
        manifest.set_region(
            "CloudBuild.json",
            "anvil-source-root",
            item.rendered_checksum.clone().expect("a write records the rendered body checksum"),
        );
        assert_eq!(plan(Some(output), &manifest).unwrap().decision, Decision::InSync);
    }

    #[test]
    fn missing_path_is_scaffolded_by_insertion() {
        let source = "{\n  \"RepositoryOwned\": true\n}\n";
        let output = plan(Some(source), &Manifest::default())
            .unwrap()
            .spliced_host
            .expect("the missing path requires a write");

        assert!(output.contains("\"RepositoryOwned\": true,"));
        assert!(output.contains("\"EnlistmentRootPaths\": {"));
        assert!(output.contains("\"SourceRootDirs\": ["));
        assert!(output.contains(ENTRY));
        assert_eq!(parse(&output).unwrap().value.unwrap().as_object().unwrap().properties.len(), 2);
    }

    #[test]
    fn comments_and_marker_shaped_strings_are_not_ownership_boundaries() {
        let source = concat!(
            "{\n",
            "  \"text\": \"// >>> anvil-managed: anvil-source-root\",\n",
            "  /* // <<< anvil-managed: anvil-source-root */\n",
            "  \"EnlistmentRootPaths\": { \"SourceRootDirs\": [] }\n",
            "}\n",
        );
        let output = plan(Some(source), &Manifest::default())
            .unwrap()
            .spliced_host
            .expect("the region is absent");

        assert!(output.contains("\"text\": \"// >>> anvil-managed: anvil-source-root\""));
        assert!(output.contains("/* // <<< anvil-managed: anvil-source-root */"));
        assert_eq!(validated_regions(&output).unwrap().len(), 1);
    }

    #[test]
    fn malformed_wrong_typed_and_ambiguous_hosts_refuse() {
        let cases = [
            "{",
            "{\"EnlistmentRootPaths\": 1}",
            "{\"EnlistmentRootPaths\":{\"SourceRootDirs\":{}}}",
            "{\"EnlistmentRootPaths\":{},\"EnlistmentRootPaths\":{}}",
        ];
        for source in cases {
            assert!(plan(Some(source), &Manifest::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn edited_managed_content_refuses_and_empty_content_regenerates() {
        let introduced = plan(Some("{}\n"), &Manifest::default()).unwrap();
        let output = introduced.spliced_host.as_deref().unwrap();
        let checksum = introduced
            .rendered_checksum
            .as_deref()
            .expect("a region write records its body checksum");
        let mut manifest = Manifest::default();
        manifest.set_region("CloudBuild.json", "anvil-source-root", checksum.to_owned());

        let edited = output.replace("build\\\\Extensions\\\\Anvil", "edited");
        let refusal = plan(Some(&edited), &manifest).unwrap_err();
        assert_eq!(refusal.remedy, RefusalRemedy::EditedRegion);

        let empty = output.replace(&format!("      {ENTRY}\n"), "");
        assert_eq!(plan(Some(&empty), &manifest).unwrap().decision, Decision::Write);
    }

    #[test]
    fn array_retirement_removes_only_owned_bytes() {
        let introduced = plan(
            Some("{\"EnlistmentRootPaths\":{\"SourceRootDirs\":[\"src\"]}}\n"),
            &Manifest::default(),
        )
        .unwrap()
        .spliced_host
        .expect("the region is introduced");
        let retired = remove_json_region(&introduced, "anvil-source-root", CommentSyntax::SlashSlash).unwrap();

        assert!(!retired.contains("anvil-managed"));
        assert!(!retired.contains(ENTRY));
        assert!(retired.contains("\"src\""));
        parse(&retired).unwrap();
    }

    #[test]
    fn checksum_includes_json_array_selector_metadata() {
        let first = Catalog::builder(CliMeta::new("anvil"))
            .with_json_array_region(spec())
            .build()
            .unwrap();
        let mut changed = spec();
        changed.path.push("Nested".to_owned());
        let second = Catalog::builder(CliMeta::new("anvil"))
            .with_json_array_region(changed)
            .build()
            .unwrap();

        assert_ne!(first.checksum(), second.checksum());
        assert_eq!(
            checksum_str(first.artifacts()[0].body()),
            checksum_str(second.artifacts()[0].body())
        );
    }
}
