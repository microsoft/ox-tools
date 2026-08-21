// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Metrics calculation for rustdoc JSON documentation
//!
//! This module handles parsing rustdoc JSON in various format versions and extracting
//! documentation metrics.

use std::sync::LazyLock;

use ohno::{IntoAppError, app_err};
use regex::Regex;

use super::provider::LOG_TARGET;
use super::{DocsData, DocsMetrics};
use crate::facts::CrateSpec;
use crate::{HashMap, Result};

/// Pattern to match intra-doc code links: [`text`]
/// Only matches backtick-enclosed links which are the standard for code references
static INTRA_DOC_LINK_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[`([^`\]]+)`\]").expect("invalid regex"));

/// Pattern to match code blocks (triple backticks)
static CODE_BLOCK_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"```[\s\S]*?```").expect("invalid regex"));

/// Pattern to match reference-style link definitions: [`text`]: target
/// These define aliases where the link text in the docs maps to a different resolution target
static LINK_REFERENCE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[`([^`\]]+)`\]:\s*(\S+)").expect("invalid regex"));

/// Macro to generate all code needed for a rustdoc JSON format version
///
/// This generates both:
/// 1. The version-specific `calculate_metrics_vN` function
/// 2. The `ItemLike` trait implementation for that version's types
macro_rules! generate_version_support {
    ($version:literal, $module:ident) => {
        pastey::paste! {
            /// Parse and calculate metrics for rustdoc JSON format version
            #[doc = $version]
            fn [<calculate_metrics_v $version>](json_bytes: &[u8], crate_spec: &CrateSpec) -> Result<DocsMetrics> {
                use $module as rustdoc_types;

                log::debug!(target: LOG_TARGET, "Parsing rustdoc JSON v{} for {crate_spec}", $version);
                let krate: rustdoc_types::Crate = serde_json::from_slice(json_bytes)
                    .into_app_err_with(|| format!("parsing rustdoc JSON v{} structure for {crate_spec}", $version))?;

                let index_len = krate.index.len();
                log::debug!(target: LOG_TARGET, "Successfully parsed rustdoc JSON v{} for {crate_spec}, found {index_len} items in index", $version);
                log::debug!(target: LOG_TARGET, "Root item ID for {crate_spec}: {:?}", krate.root);

                Ok(process_crate_items(
                    &krate.index,
                    &krate.root,
                    crate_spec,
                    |item| matches!(item.visibility, rustdoc_types::Visibility::Public),
                    |item| matches!(item.inner, rustdoc_types::ItemEnum::Use(_)),
                ))
            }
        }

        // Generate ItemLike trait implementation
        impl ItemLike for $module::Item {
            type Id = $module::Id;

            fn name(&self) -> Option<&str> {
                self.name.as_deref()
            }

            fn docs(&self) -> Option<&str> {
                self.docs.as_deref()
            }

            fn links(&self) -> &std::collections::HashMap<String, Self::Id> {
                &self.links
            }
        }
    };
}

// Generate all code for each supported version
generate_version_support!("50", rustdoc_types_v50);
generate_version_support!("51", rustdoc_types_v51);
generate_version_support!("52", rustdoc_types_v52);
generate_version_support!("53", rustdoc_types_v53);
generate_version_support!("54", rustdoc_types_v54);
generate_version_support!("55", rustdoc_types_v55);
generate_version_support!("56", rustdoc_types_v56);
generate_version_support!("57", rustdoc_types_v57);

/// Helper struct to peek at just the `format_version` field without parsing the entire document.
#[derive(serde::Deserialize)]
struct FormatVersionPeek {
    format_version: u64,
}

pub fn calculate_docs_metrics(json_bytes: &[u8], crate_spec: &CrateSpec) -> Result<DocsData> {
    log::debug!(target: LOG_TARGET, "Parsing rustdoc JSON for {crate_spec}");

    // Peek at format_version with a minimal deserialization pass (ignores all other fields)
    let format_version = serde_json::from_slice::<FormatVersionPeek>(json_bytes)
        .map(|v| v.format_version)
        .into_app_err_with(|| format!("reading format_version from rustdoc JSON for {crate_spec}"))?;

    log::debug!(target: LOG_TARGET, "Found rustdoc JSON format version {format_version} for {crate_spec}");

    let metrics = match format_version {
        50 => calculate_metrics_v50(json_bytes, crate_spec)?,
        51 => calculate_metrics_v51(json_bytes, crate_spec)?,
        52 => calculate_metrics_v52(json_bytes, crate_spec)?,
        53 => calculate_metrics_v53(json_bytes, crate_spec)?,
        54 => calculate_metrics_v54(json_bytes, crate_spec)?,
        55 => calculate_metrics_v55(json_bytes, crate_spec)?,
        56 => calculate_metrics_v56(json_bytes, crate_spec)?,
        57 => calculate_metrics_v57(json_bytes, crate_spec)?,
        _ => {
            log::debug!(target: LOG_TARGET, "Unsupported rustdoc JSON format version {format_version} for {crate_spec}");
            return Err(app_err!(
                "unsupported rustdoc JSON format version {format_version} for {crate_spec}"
            ));
        }
    };

    Ok(DocsData { metrics })
}

/// Process crate items and calculate documentation metrics
///
/// This generic function works with items from any rustdoc-types version by accepting
/// closures that check visibility and item type in a version-specific way.
fn process_crate_items<Id, Item>(
    index: &std::collections::HashMap<Id, Item>,
    root_id: &Id,
    crate_spec: &CrateSpec,
    is_public: impl Fn(&Item) -> bool,
    is_use_item: impl Fn(&Item) -> bool,
) -> DocsMetrics
where
    Id: core::fmt::Debug + Eq + core::hash::Hash,
    Item: ItemLike,
{
    let mut number_of_public_api_elements = 0;
    let mut documented_count = 0;
    let mut number_of_examples_in_docs = 0;
    let mut has_crate_level_docs = false;
    let mut broken_doc_links = 0;
    let mut private_items = 0;
    let mut use_items = 0;

    let index_len = index.len();
    // Normalize the crate name: crates.io uses hyphens (e.g., "pin-project-lite") but
    // rustdoc JSON uses underscores (e.g., "pin_project_lite") for the root module name.
    let normalized_crate_name = crate_spec.name().replace('-', "_");
    log::debug!(target: LOG_TARGET, "Starting to iterate through {index_len} items for {crate_spec}");

    for (id, item) in index {
        // Only count public API items
        if !is_public(item) {
            private_items += 1;
            continue;
        }

        // Skip re-exports (Use items) - they inherit docs from the original item
        if is_use_item(item) {
            use_items += 1;
            continue;
        }

        number_of_public_api_elements += 1;

        // Check if item has documentation
        if let Some(docs) = item.docs()
            && !docs.trim().is_empty()
        {
            documented_count += 1;

            let fences = docs.lines().filter(|line| line.trim_start().starts_with("```")).count();
            let examples = fences / 2; // Divide by 2 since each codebase block has opening and closing fence
            number_of_examples_in_docs += examples;

            let broken = count_broken_links::<Item::Id>(docs, item.links(), item.name());
            broken_doc_links += broken;

            if let Some(name) = item.name()
                && name == normalized_crate_name
                && id == root_id
            {
                log::debug!(target: LOG_TARGET, "Found crate-level docs for {crate_spec} (root item name matches)");
                has_crate_level_docs = true;
            }
        }
    }

    log::debug!(target: LOG_TARGET, "Processed {index_len} items for {crate_spec}: private={private_items}, use_items={use_items}, public_api={number_of_public_api_elements}, documented={documented_count}, examples={number_of_examples_in_docs}, broken_links={broken_doc_links}, has_crate_docs={has_crate_level_docs}");

    #[expect(clippy::cast_precision_loss, reason = "loss of precision acceptable for percentage calculation")]
    let doc_coverage_percentage = if number_of_public_api_elements > 0 {
        documented_count as f64 / number_of_public_api_elements as f64 * 100.0
    } else {
        100.0
    };

    let metrics = DocsMetrics {
        doc_coverage_percentage,
        public_api_elements: number_of_public_api_elements,
        undocumented_elements: number_of_public_api_elements - documented_count,
        examples_in_docs: number_of_examples_in_docs as u64,
        has_crate_level_docs,
        broken_doc_links,
    };

    log::debug!(target: LOG_TARGET, "Returning DocsMetrics for {crate_spec}: {metrics:?}");
    metrics
}

/// Count broken intra-doc links in documentation
///
/// Looks for markdown link patterns that appear to be intra-doc links but aren't
/// in the resolved links map. Only considers backtick-enclosed links like [`Type`]
/// which are the standard way to reference code elements in Rust documentation.
///
/// Handles reference-style link definitions where the link text in the docs
/// (e.g., `` [`anyhow::Error::from_boxed`] ``) is defined to resolve to a different target
/// (e.g., `Self::from_boxed`) via a line like: `` [`anyhow::Error::from_boxed`]: Self::from_boxed ``
fn count_broken_links<Id>(docs: &str, resolved_links: &std::collections::HashMap<String, Id>, _item_name: Option<&str>) -> u64 {
    let mut broken_count = 0;
    let mut skipped_inline = 0;
    let mut skipped_external = 0;
    let mut skipped_short = 0;
    let mut skipped_resolved = 0;

    log::trace!(target: LOG_TARGET, "Checking for broken links. Docs length: {} chars, resolved_links count: {}", docs.len(), resolved_links.len());

    // Remove code blocks to avoid false positives from examples
    let docs_without_code_blocks = CODE_BLOCK_REGEX.replace_all(docs, "");
    let docs_to_check = docs_without_code_blocks.as_ref();

    // Parse reference-style link definitions: [`link_text`]: target
    // These map the link text as written in the docs to the actual resolution target
    let mut link_references = HashMap::default();
    for cap in LINK_REFERENCE_REGEX.captures_iter(docs_to_check) {
        let link_text = cap
            .get(1)
            .expect("LINK_REFERENCE_REGEX declares group 1 unconditionally, so any match captures it")
            .as_str();
        let target = cap
            .get(2)
            .expect("LINK_REFERENCE_REGEX declares group 2 unconditionally, so any match captures it")
            .as_str();

        let _ = link_references.insert(link_text, target);
        log::trace!(target: LOG_TARGET, "Found link reference: [`{link_text}`] -> {target}");
    }

    for cap in INTRA_DOC_LINK_REGEX.captures_iter(docs_to_check) {
        let text = cap
            .get(1)
            .expect("INTRA_DOC_LINK_REGEX declares group 1 unconditionally, so any match captures it")
            .as_str();

        // Get the position after the match to check for inline link syntax
        let match_end = cap.get(0).expect("match exists").end();

        // Skip inline links like [`text`](url) - check if next char is '('
        if docs_to_check.get(match_end..).is_some_and(|s| s.starts_with('(')) {
            skipped_inline += 1;
            log::trace!(target: LOG_TARGET, "Skipping inline link: [`{text}`](...)");
            continue;
        }

        // Check for inline reference-style links like [`text`][target]
        // Extract the target if present (it's in square brackets but WITHOUT backticks)
        let inline_target = (|| {
            let remainder = docs_to_check.get(match_end..)?.strip_prefix('[')?;
            let end_pos = remainder.find(']')?;
            remainder.get(..end_pos)
        })();

        // Skip external links (contain ://)
        if text.contains("://") {
            skipped_external += 1;
            log::trace!(target: LOG_TARGET, "Skipping external link: [`{text}`]");
            continue;
        }

        // Skip very short "links" (1-2 chars) - likely false positives
        let text_len = text.len();
        if text_len <= 2 {
            skipped_short += 1;
            log::trace!(target: LOG_TARGET, "Skipping short link (len={text_len}): [`{text}`]");
            continue;
        }

        // Check if it's resolved - try multiple strategies:
        // 1. Direct match in resolved_links (with and without backticks)
        // 2. Via an inline reference target [`text`][target]
        // 3. Via a reference definition (link text -> target, then check if target is in resolved_links)
        // 4. Strip trailing () for method references and try again
        // 5. Try without module path if it contains ::

        let text_without_parens = text.strip_suffix("()").unwrap_or(text);

        let is_resolved = resolved_links.contains_key(text)
            || resolved_links.contains_key(text_without_parens)
            || resolved_links.contains_key(&format!("`{text}`"))
            || resolved_links.contains_key(&format!("`{text_without_parens}`"))
            || inline_target.is_some_and(|target| resolved_links.contains_key(target))
            || link_references.get(text).is_some_and(|target| resolved_links.contains_key(*target))
            || link_references
                .get(text_without_parens)
                .is_some_and(|target| resolved_links.contains_key(*target))
            || (text_without_parens.contains("::") && {
                // Try just the last component (e.g., "Error" from "std::error::Error", or "chain" from "Error::chain")
                let last_component = text_without_parens.rsplit("::").next().unwrap_or("");
                resolved_links.contains_key(last_component)
                    || link_references
                        .get(last_component)
                        .is_some_and(|target| resolved_links.contains_key(*target))
            });

        if is_resolved {
            skipped_resolved += 1;
            log::trace!(target: LOG_TARGET, "Resolved link: [`{text}`]");
            continue;
        }

        // This looks like an intra-doc link but isn't resolved
        broken_count += 1;
        log::trace!(target: LOG_TARGET, "Broken link: [`{text}`]");
    }

    let total_matches = broken_count + skipped_inline + skipped_external + skipped_short + skipped_resolved;
    log::trace!(target: LOG_TARGET, "Link analysis: total_matches={total_matches}, broken={broken_count}, skipped(inline={skipped_inline}, external={skipped_external}, short={skipped_short}, resolved={skipped_resolved})");

    broken_count
}

/// Trait to abstract over different rustdoc-types Item versions
trait ItemLike {
    type Id;
    fn name(&self) -> Option<&str>;
    fn docs(&self) -> Option<&str>;
    fn links(&self) -> &std::collections::HashMap<String, Self::Id>;
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use semver::Version;
    use serde_json::json;

    use super::*;

    /// Build a minimal valid rustdoc JSON `v57` value.
    ///
    /// `root_module_name` is the name of the root module item (rustdoc always uses
    /// underscores here, e.g. `"pin_project_lite"`).  Extra items can be injected
    /// through `extra_items` (id, item-json pairs) — they are automatically added
    /// to the root module's `items` array.
    fn make_rustdoc_json(root_module_name: &str, root_docs: Option<&str>, extra_items: &[(u32, serde_json::Value)]) -> serde_json::Value {
        let extra_ids: Vec<u32> = extra_items.iter().map(|(id, _)| *id).collect();

        let mut index = serde_json::Map::new();

        // Root module (id 0)
        let _ = index.insert(
            "0".into(),
            json!({
                "id": 0,
                "crate_id": 0,
                "name": root_module_name,
                "span": null,
                "visibility": "public",
                "docs": root_docs,
                "links": {},
                "attrs": [],
                "deprecation": null,
                "inner": {
                    "module": {
                        "is_crate": true,
                        "items": extra_ids,
                        "is_stripped": false
                    }
                }
            }),
        );

        for (id, item_json) in extra_items {
            let _ = index.insert(id.to_string(), item_json.clone());
        }

        json!({
            "format_version": 57,
            "root": 0,
            "crate_version": "0.1.0",
            "includes_private": false,
            "index": index,
            "paths": {
                "0": { "crate_id": 0, "path": [root_module_name], "kind": "module" }
            },
            "external_crates": {},
            "target": {
                "triple": "x86_64-unknown-linux-gnu",
                "target_features": []
            }
        })
    }

    /// Build a public struct item with the given name and optional docs.
    fn make_public_struct(id: u32, name: &str, docs: Option<&str>) -> (u32, serde_json::Value) {
        (
            id,
            json!({
                "id": id,
                "crate_id": 0,
                "name": name,
                "span": null,
                "visibility": "public",
                "docs": docs,
                "links": {},
                "attrs": [],
                "deprecation": null,
                "inner": {
                    "struct": {
                        "kind": { "plain": { "fields": [], "has_stripped_fields": false } },
                        "generics": { "params": [], "where_predicates": [] },
                        "impls": []
                    }
                }
            }),
        )
    }

    fn crate_spec(name: &str) -> CrateSpec {
        CrateSpec::from_arcs(Arc::from(name), Arc::new(Version::new(0, 1, 0)))
    }

    #[derive(Debug)]
    struct CapturingLogger;

    static CAPTURING_LOGGER: CapturingLogger = CapturingLogger;
    static CAPTURED_LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    impl log::Log for CapturingLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            CAPTURED_LOGS
                .lock()
                .expect("captured log mutex should not be poisoned")
                .push(record.args().to_string());
        }

        fn flush(&self) {}
    }

    fn install_capturing_logger() {
        CAPTURED_LOGS.lock().expect("captured log mutex should not be poisoned").clear();
        let _ = log::set_logger(&CAPTURING_LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
    }

    fn captured_logs() -> String {
        CAPTURED_LOGS.lock().expect("captured log mutex should not be poisoned").join("\n")
    }

    fn run_ignored_helper(helper_name: &str) -> String {
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let output = Command::new(std::env::current_exe().expect("test binary path should be available"))
            .env("CARGO_APRZ_CAPTURE_LOGS", "1")
            .args(["--exact", &format!("{module}::{helper_name}"), "--ignored", "--nocapture"])
            .output()
            .expect("capturing helper test should run");

        assert!(
            output.status.success(),
            "capturing helper failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout).expect("capturing helper output should be UTF-8")
    }

    // -----------------------------------------------------------------------
    // Crate-level docs detection
    // -----------------------------------------------------------------------

    #[test]
    fn crate_level_docs_detected_for_simple_name() {
        let json = make_rustdoc_json("my_crate", Some("Top-level docs"), &[]);
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        assert!(data.metrics.has_crate_level_docs);
    }

    #[test]
    fn crate_level_docs_detected_when_name_has_hyphens() {
        // The CrateSpec uses hyphens (crates.io convention) but rustdoc JSON
        // uses underscores for the root module name.
        let json = make_rustdoc_json("pin_project_lite", Some("A lightweight pin-project."), &[]);
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("pin-project-lite")).unwrap();
        assert!(
            data.metrics.has_crate_level_docs,
            "should detect crate-level docs even when crate name has hyphens"
        );
    }

    #[test]
    fn crate_level_docs_false_when_root_has_no_docs() {
        let json = make_rustdoc_json("my_crate", None, &[]);
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        assert!(!data.metrics.has_crate_level_docs);
    }

    #[test]
    fn crate_level_docs_false_when_root_docs_are_empty() {
        let json = make_rustdoc_json("my_crate", Some("   "), &[]);
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        assert!(!data.metrics.has_crate_level_docs);
    }

    // -----------------------------------------------------------------------
    // Documentation coverage
    // -----------------------------------------------------------------------

    #[test]
    fn coverage_counts_public_items() {
        let json = make_rustdoc_json(
            "my_crate",
            Some("Crate docs"),
            &[
                make_public_struct(1, "Documented", Some("Has docs.")),
                make_public_struct(2, "Undocumented", None),
            ],
        );
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        // 3 public items: root module + 2 structs
        assert_eq!(data.metrics.public_api_elements, 3);
        // 2 documented: root module + "Documented"
        assert_eq!(data.metrics.undocumented_elements, 1);
    }

    #[test]
    fn partial_coverage_reports_exact_percentage() {
        let json = make_rustdoc_json(
            "my_crate",
            Some("Crate docs"),
            &[
                make_public_struct(1, "Documented", Some("Has docs.")),
                make_public_struct(2, "UndocumentedOne", None),
                make_public_struct(3, "UndocumentedTwo", None),
            ],
        );
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        assert!((data.metrics.doc_coverage_percentage - 50.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Examples in docs
    // -----------------------------------------------------------------------

    #[test]
    fn counts_code_examples_in_docs() {
        let docs_with_two_examples = "Some docs\n\n```rust\nlet x = 1;\n```\n\nMore text\n\n```\nlet y = 2;\n```\n";
        let json = make_rustdoc_json("my_crate", Some(docs_with_two_examples), &[]);
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        assert_eq!(data.metrics.examples_in_docs, 2);
    }

    #[test]
    fn full_coverage_when_all_items_documented() {
        let json = make_rustdoc_json("my_crate", Some("Crate docs"), &[make_public_struct(1, "Foo", Some("Foo docs"))]);
        let reader = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(reader.as_slice(), &crate_spec("my_crate")).unwrap();
        assert!((data.metrics.doc_coverage_percentage - 100.0).abs() < f64::EPSILON);
        assert_eq!(data.metrics.undocumented_elements, 0);
    }

    // -----------------------------------------------------------------------
    // Format version dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn every_supported_format_version_is_parsed() {
        for version in 50..=57_u64 {
            let mut json = make_rustdoc_json("my_crate", Some("Crate docs"), &[]);
            json["format_version"] = json!(version);
            let bytes = serde_json::to_vec(&json).unwrap();

            let data = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate"))
                .unwrap_or_else(|e| panic!("format version {version} should be supported: {e}"));
            assert!(data.metrics.has_crate_level_docs, "version {version}");
        }
    }

    #[test]
    fn unsupported_format_version_is_an_error() {
        let mut json = make_rustdoc_json("my_crate", Some("Crate docs"), &[]);
        json["format_version"] = json!(9999);
        let bytes = serde_json::to_vec(&json).unwrap();

        let err = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap_err();
        assert!(err.to_string().contains("unsupported rustdoc JSON format version"), "{err}");
    }

    #[test]
    fn missing_format_version_is_an_error() {
        let err = calculate_docs_metrics(b"{}", &crate_spec("my_crate")).unwrap_err();
        assert!(err.to_string().contains("format_version"), "{err}");
    }

    #[test]
    fn malformed_document_body_is_an_error() {
        let bytes = serde_json::to_vec(&json!({ "format_version": 57 })).unwrap();
        let err = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap_err();
        assert!(err.to_string().contains("parsing rustdoc JSON"), "{err}");
    }

    #[test]
    fn coverage_is_full_when_there_is_no_public_api() {
        let mut json = make_rustdoc_json("my_crate", Some("Crate docs"), &[]);
        json["index"]["0"]["visibility"] = json!("crate");
        let bytes = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap();
        assert_eq!(data.metrics.public_api_elements, 0);
        assert!((data.metrics.doc_coverage_percentage - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn re_exports_are_not_counted_as_public_api() {
        let use_item = (
            1_u32,
            json!({
                "id": 1,
                "crate_id": 0,
                "name": "Reexported",
                "span": null,
                "visibility": "public",
                "docs": null,
                "links": {},
                "attrs": [],
                "deprecation": null,
                "inner": { "use": { "source": "other::Thing", "name": "Reexported", "id": null, "is_glob": false } }
            }),
        );
        let json = make_rustdoc_json("my_crate", Some("Crate docs"), &[use_item]);
        let bytes = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap();
        assert_eq!(data.metrics.public_api_elements, 1);
    }

    #[test]
    fn mixed_item_metrics_are_reported_exactly() {
        let private_item = (
            1_u32,
            json!({
                "id": 1,
                "crate_id": 0,
                "name": "PrivateThing",
                "span": null,
                "visibility": "crate",
                "docs": null,
                "links": {},
                "attrs": [],
                "deprecation": null,
                "inner": {
                    "struct": {
                        "kind": { "plain": { "fields": [], "has_stripped_fields": false } },
                        "generics": { "params": [], "where_predicates": [] },
                        "impls": []
                    }
                }
            }),
        );
        let use_item = (
            2_u32,
            json!({
                "id": 2,
                "crate_id": 0,
                "name": "Reexported",
                "span": null,
                "visibility": "public",
                "docs": null,
                "links": {},
                "attrs": [],
                "deprecation": null,
                "inner": { "use": { "source": "other::Thing", "name": "Reexported", "id": null, "is_glob": false } }
            }),
        );
        let documented_public = make_public_struct(3, "Documented", Some("See [`MissingOne`] and [`MissingTwo`]."));
        let undocumented_public = make_public_struct(4, "Undocumented", None);
        let json = make_rustdoc_json(
            "my_crate",
            Some("Crate docs"),
            &[private_item, use_item, documented_public, undocumented_public],
        );
        let bytes = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap();
        assert_eq!(data.metrics.public_api_elements, 3);
        assert_eq!(data.metrics.undocumented_elements, 1);
        assert!((data.metrics.doc_coverage_percentage - (200.0 / 3.0)).abs() < 1e-10);
        assert_eq!(data.metrics.examples_in_docs, 0);
        assert!(data.metrics.has_crate_level_docs);
        assert_eq!(data.metrics.broken_doc_links, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn item_classification_log_reports_private_and_use_counts() {
        let logs = run_ignored_helper("helper_capture_item_classification_log");

        assert!(
            logs.contains("private=3, use_items=4, public_api=3, documented=2, examples=0, broken_links=2, has_crate_docs=true"),
            "{logs}"
        );
    }

    #[test]
    #[ignore = "spawned by item_classification_log_reports_private_and_use_counts"]
    fn helper_capture_item_classification_log() {
        if std::env::var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
            return;
        }

        install_capturing_logger();

        let private_items: Vec<_> = (1..=3)
            .map(|id| {
                (
                    id,
                    json!({
                        "id": id,
                        "crate_id": 0,
                        "name": format!("PrivateThing{id}"),
                        "span": null,
                        "visibility": "crate",
                        "docs": null,
                        "links": {},
                        "attrs": [],
                        "deprecation": null,
                        "inner": {
                            "struct": {
                                "kind": { "plain": { "fields": [], "has_stripped_fields": false } },
                                "generics": { "params": [], "where_predicates": [] },
                                "impls": []
                            }
                        }
                    }),
                )
            })
            .collect();
        let use_items: Vec<_> = (4..=7)
            .map(|id| {
                (
                    id,
                    json!({
                        "id": id,
                        "crate_id": 0,
                        "name": format!("Reexported{id}"),
                        "span": null,
                        "visibility": "public",
                        "docs": null,
                        "links": {},
                        "attrs": [],
                        "deprecation": null,
                        "inner": { "use": { "source": "other::Thing", "name": format!("Reexported{id}"), "id": null, "is_glob": false } }
                    }),
                )
            })
            .collect();

        let mut items = Vec::new();
        items.extend(private_items);
        items.extend(use_items);
        items.push(make_public_struct(8, "Documented", Some("See [`MissingOne`] and [`MissingTwo`].")));
        items.push(make_public_struct(9, "Undocumented", None));

        let json = make_rustdoc_json("my_crate", Some("Crate docs"), &items);
        let bytes = serde_json::to_vec(&json).unwrap();
        let data = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap();
        assert_eq!(data.metrics.public_api_elements, 3);

        println!("{}", captured_logs());
    }

    // -----------------------------------------------------------------------
    // Broken link detection
    // -----------------------------------------------------------------------

    fn links(entries: &[&str]) -> std::collections::HashMap<String, u32> {
        entries
            .iter()
            .enumerate()
            .map(|(i, k)| ((*k).to_owned(), u32::try_from(i).unwrap()))
            .collect()
    }

    #[test]
    fn unresolved_link_counts_as_broken() {
        let broken = count_broken_links::<u32>("See [`MissingType`] for details.", &links(&[]), None);
        assert_eq!(broken, 1);
    }

    #[test]
    fn link_classification_is_traced() {
        // The per-link trace messages are only formatted when a logger is installed, so
        // install one that evaluates and discards every record.
        crate::facts::test_logging::enable_log_argument_evaluation();

        let broken = count_broken_links::<u32>("See [`MissingType`] and [`KnownType`].", &links(&["KnownType"]), None);
        assert_eq!(broken, 1);
    }

    #[test]
    fn directly_resolved_link_is_not_broken() {
        let broken = count_broken_links::<u32>("See [`KnownType`].", &links(&["KnownType"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn backtick_quoted_key_resolves_link() {
        let broken = count_broken_links::<u32>("See [`KnownType`].", &links(&["`KnownType`"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn method_link_resolves_without_trailing_parens() {
        let broken = count_broken_links::<u32>("See [`KnownType()`].", &links(&["KnownType"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn method_link_resolves_to_backtick_quoted_key_without_trailing_parens() {
        let broken = count_broken_links::<u32>("See [`KnownType()`].", &links(&["`KnownType`"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn inline_links_are_skipped() {
        let broken = count_broken_links::<u32>("See [`docs`](https://example.com) here.", &links(&[]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn external_links_are_skipped() {
        let broken = count_broken_links::<u32>("See [`https://example.com/page`].", &links(&[]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn very_short_links_are_skipped() {
        let broken = count_broken_links::<u32>("Compare [`ab`] and [`x`].", &links(&[]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn inline_reference_target_resolves_link() {
        let broken = count_broken_links::<u32>("See [`Alias`][real_target].", &links(&["real_target"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn reference_definition_resolves_link() {
        let docs = "See [`anyhow::Error::from_boxed`].\n\n[`anyhow::Error::from_boxed`]: Self::from_boxed\n";
        let broken = count_broken_links::<u32>(docs, &links(&["Self::from_boxed"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn reference_definition_resolves_link_with_trailing_parens() {
        let docs = "See [`Error::chain()`].\n\n[`Error::chain`]: Self::chain\n";
        let broken = count_broken_links::<u32>(docs, &links(&["Self::chain"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn last_path_component_resolves_link() {
        let broken = count_broken_links::<u32>("See [`std::error::Error`].", &links(&["Error"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn unresolved_qualified_path_counts_as_broken() {
        let broken = count_broken_links::<u32>("See [`missing::Type`].", &links(&[]), None);
        assert_eq!(broken, 1);
    }

    #[test]
    fn last_path_component_resolves_through_reference_definition() {
        let docs = "See [`std::error::Error`].\n\n[`Error`]: Self::Error\n";
        let broken = count_broken_links::<u32>(docs, &links(&["Self::Error"]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn links_inside_code_blocks_are_ignored() {
        let docs = "```rust\nlet x = [`NotALink`];\n```\n";
        let broken = count_broken_links::<u32>(docs, &links(&[]), None);
        assert_eq!(broken, 0);
    }

    #[test]
    fn broken_links_are_reported_through_metrics() {
        let json = make_rustdoc_json("my_crate", Some("See [`Nowhere`] and [`AlsoNowhere`]."), &[]);
        let bytes = serde_json::to_vec(&json).unwrap();

        let data = calculate_docs_metrics(bytes.as_slice(), &crate_spec("my_crate")).unwrap();
        assert_eq!(data.metrics.broken_doc_links, 2);
    }

    #[test]
    fn mixed_link_kinds_count_only_unresolved_candidates_as_broken() {
        let docs = concat!(
            "Broken: [`MissingOne`] [`MissingTwo`].\n",
            "Inline: [`InlineOne`](https://example.com) [`InlineTwo`](https://example.com) [`InlineThree`](https://example.com).\n",
            "External: [`https://example.com/one`] [`https://example.com/two`] [`https://example.com/three`] [`https://example.com/four`].\n",
            "Short: [`a`] [`bc`] [`x`] [`yz`] [`q`].\n",
            "Resolved: [`KnownOne`] [`KnownTwo`] [`KnownThree`] [`KnownFour`] [`KnownFive`] [`KnownSix`].\n",
        );

        let broken = count_broken_links::<u32>(
            docs,
            &links(&["KnownOne", "KnownTwo", "KnownThree", "KnownFour", "KnownFive", "KnownSix"]),
            None,
        );
        assert_eq!(broken, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn link_analysis_log_reports_each_classification_count() {
        let logs = run_ignored_helper("helper_capture_link_analysis_log");

        assert!(
            logs.contains("Link analysis: total_matches=20, broken=2, skipped(inline=3, external=4, short=5, resolved=6)"),
            "{logs}"
        );
    }

    #[test]
    #[ignore = "spawned by link_analysis_log_reports_each_classification_count"]
    fn helper_capture_link_analysis_log() {
        if std::env::var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
            return;
        }

        install_capturing_logger();

        let docs = concat!(
            "Broken: [`MissingOne`] [`MissingTwo`].\n",
            "Inline: [`InlineOne`](https://example.com) [`InlineTwo`](https://example.com) [`InlineThree`](https://example.com).\n",
            "External: [`https://example.com/one`] [`https://example.com/two`] [`https://example.com/three`] [`https://example.com/four`].\n",
            "Short: [`a`] [`bc`] [`x`] [`yz`] [`q`].\n",
            "Resolved: [`KnownOne`] [`KnownTwo`] [`KnownThree`] [`KnownFour`] [`KnownFive`] [`KnownSix`].\n",
        );

        let broken = count_broken_links::<u32>(
            docs,
            &links(&["KnownOne", "KnownTwo", "KnownThree", "KnownFour", "KnownFive", "KnownSix"]),
            None,
        );
        assert_eq!(broken, 2);

        println!("{}", captured_logs());
    }
}
