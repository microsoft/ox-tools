// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Additive Dependabot exclusions for catalog-owned GitHub Actions files.
//!
//! YAML events locate insertion points; the repository's original source is
//! never serialized or replaced. This is not whole-file or managed-region
//! ownership: exclusions supplied by the repository remain its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;

use ohno::{AppError, IntoAppError as _, app_err, bail};
use saphyr_parser::{Event, Parser};

use crate::backend::Backend;
use crate::catalog::{Artifact, Catalog};

const KEEP_UPDATES: &str = "# anvil: keep-generated-action-updates";

/// Exact paths Dependabot would otherwise update in the selected catalog.
pub(crate) fn generated_action_paths(catalog: &Catalog, backends: &[Backend]) -> Vec<&'static str> {
    if !backends.contains(&Backend::GitHub) {
        return Vec::new();
    }
    let mut paths: Vec<_> = catalog
        .artifacts()
        .iter()
        .filter_map(|artifact| match artifact {
            Artifact::OwnedFile(spec) if spec.gate.is_none_or(|gate| backends.contains(&gate)) => Some(spec.path),
            _ => None,
        })
        .filter(|path| {
            let workflow = path.strip_prefix(".github/workflows/").is_some_and(|name| {
                !name.contains('/')
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml"))
            });
            let action = path.starts_with(".github/actions/") && (path.ends_with("/action.yml") || path.ends_with("/action.yaml"));
            workflow || action
        })
        .collect();
    paths.sort_unstable();
    paths
}

/// Add missing exclusions, returning no replacement when nothing needs changing.
pub(crate) fn update(text: &str, paths: &[&str]) -> Result<Option<String>, AppError> {
    if paths.is_empty()
        || text
            .trim_start_matches('\u{feff}')
            .split(['\r', '\n'])
            .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            .any(|line| line.trim() == KEEP_UPDATES)
    {
        return Ok(None);
    }

    let root = parse(text)?;
    let fields = root.fields()?;
    if fields.get("version").map(|node| node.scalar()).transpose()? != Some("2") {
        bail!("expected Dependabot configuration version 2");
    }
    let updates = fields.get("updates").ok_or_else(|| app_err!("missing Dependabot `updates` list"))?;
    let mut insertions = Vec::new();
    for job in updates.sequence()? {
        let fields = job.fields()?;
        let ecosystem = fields
            .get("package-ecosystem")
            .ok_or_else(|| app_err!("Dependabot update job is missing `package-ecosystem`"))?
            .scalar()?;
        if ecosystem != "github-actions" {
            continue;
        }
        if fields.contains_key("<<") {
            bail!("expand YAML merge keys in the GitHub Actions Dependabot job before adding exclusions");
        }
        job.require_unshared()?;
        require_root_directory(&fields)?;
        let exclusions = fields.get("exclude-paths");
        let existing = exclusions.map(|node| node.sequence()).transpose()?.unwrap_or_default();
        let existing = existing.iter().map(Node::scalar).collect::<Result<Vec<_>, _>>()?;
        let missing: Vec<_> = paths.iter().copied().filter(|path| !existing.contains(path)).collect();
        if missing.is_empty() {
            continue;
        }
        insertions.push(match exclusions {
            Some(sequence) => sequence_insertion(text, sequence, &missing),
            None => job_insertion(text, job, &missing),
        });
    }
    if insertions.is_empty() {
        return Ok(None);
    }
    insertions.sort_unstable_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    let mut output = text.to_owned();
    for (offset, body) in insertions {
        output.insert_str(offset, &body);
    }
    parse(&output).into_app_err("adding generated-file exclusions would produce invalid YAML")?;
    Ok(Some(output))
}

fn require_root_directory(fields: &BTreeMap<&str, &Node>) -> Result<(), AppError> {
    let root = match (fields.get("directory"), fields.get("directories")) {
        (Some(directory), None) => directory.scalar()? == "/",
        (None, Some(directories)) => {
            let entries = directories.sequence()?;
            entries.len() == 1 && entries[0].scalar()? == "/"
        }
        _ => false,
    };
    if !root {
        bail!("GitHub Actions Dependabot jobs must use `directory: \"/\"` (or `directories: [\"/\"]`)");
    }
    Ok(())
}

fn quoted_paths(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|path| serde_json::to_string(path).expect("serializing a Rust string to JSON cannot fail"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sequence_insertion(text: &str, sequence: &Node, paths: &[&str]) -> (usize, String) {
    if text[sequence.span.start..].starts_with('[') {
        let separator = if sequence.children.is_empty() { "" } else { ", " };
        return (sequence.span.start + 1, format!("{}{separator}", quoted_paths(paths)));
    }
    let start = line_start(text, sequence.span.start);
    let indent = &text[start..sequence.span.start];
    (start, block_entries(paths, indent, crate::region::text_newline(text)))
}

fn job_insertion(text: &str, job: &Node, paths: &[&str]) -> (usize, String) {
    if text[job.span.start..].starts_with('{') {
        return (job.span.start + 1, format!("exclude-paths: [{}], ", quoted_paths(paths)));
    }
    let key = job
        .children
        .first()
        .expect("the matched GitHub Actions job has a package-ecosystem mapping key");
    let start = line_start(text, key.span.start);
    let indent = " ".repeat(key.span.start - start);
    let newline = crate::region::text_newline(text);
    let entries = block_entries(paths, &format!("{indent}  "), newline);
    (key.span.start, format!("exclude-paths:{newline}{entries}{indent}"))
}

fn block_entries(paths: &[&str], indent: &str, newline: &str) -> String {
    let mut body = String::new();
    for path in paths {
        let quoted = serde_json::to_string(path).expect("serializing a Rust string to JSON cannot fail");
        let _ = write!(body, "{indent}- {quoted}{newline}");
    }
    body
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind(['\r', '\n']).map_or(0, |index| index + 1)
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Scalar(String),
    Sequence,
    Mapping,
    Alias,
}

#[derive(Debug)]
struct Node {
    kind: Kind,
    span: Range<usize>,
    shared: bool,
    children: Vec<Self>,
}

impl Node {
    fn require_unshared(&self) -> Result<(), AppError> {
        if self.shared || self.kind == Kind::Alias {
            bail!("expand YAML aliases, anchors, or tags on the Dependabot containers that need editing");
        }
        Ok(())
    }

    fn scalar(&self) -> Result<&str, AppError> {
        match &self.kind {
            Kind::Scalar(value) => Ok(value),
            _ => bail!("expected a scalar Dependabot setting; expand YAML aliases used for this value"),
        }
    }

    fn sequence(&self) -> Result<&[Self], AppError> {
        self.require_unshared()?;
        if self.kind != Kind::Sequence {
            bail!("expected a YAML list for Dependabot `updates`, `directories`, or `exclude-paths`");
        }
        Ok(&self.children)
    }

    fn fields(&self) -> Result<BTreeMap<&str, &Self>, AppError> {
        if self.kind != Kind::Mapping {
            bail!("expected a YAML mapping for the Dependabot configuration or update job; expand aliases used here");
        }
        let mut fields = BTreeMap::new();
        for pair in self.children.chunks_exact(2) {
            let key = pair[0].scalar()?;
            if fields.insert(key, &pair[1]).is_some() {
                bail!("duplicate Dependabot mapping key `{key}`; remove the duplicate before adding exclusions");
            }
        }
        Ok(fields)
    }
}

fn parse(text: &str) -> Result<Node, AppError> {
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let source = text.strip_prefix('\u{feff}').unwrap_or(text);
    let prefix = text.len() - source.len();
    // The string parser reports character offsets, not UTF-8 byte offsets.
    let offsets: Vec<_> = source
        .char_indices()
        .map(|(index, _)| index + prefix)
        .chain(std::iter::once(text.len()))
        .collect();
    for event in Parser::new_from_str(source) {
        let (event, span) = event.into_app_err("Dependabot configuration is not valid YAML")?;
        let span = offsets[span.start.index()]..offsets[span.end.index()];
        let (kind, shared, opening) = match event {
            Event::Scalar(value, _, anchor, tag) => (Kind::Scalar(value.into_owned()), anchor != 0 || tag.is_some(), false),
            Event::SequenceStart(anchor, tag) => (Kind::Sequence, anchor != 0 || tag.is_some(), true),
            Event::MappingStart(anchor, tag) => (Kind::Mapping, anchor != 0 || tag.is_some(), true),
            Event::Alias(_) => (Kind::Alias, true, false),
            Event::SequenceEnd | Event::MappingEnd => {
                let mut node = stack.pop().expect("YAML parser balances collection start and end events");
                node.span.end = span.end;
                push_node(&mut stack, &mut root, node)?;
                continue;
            }
            _ => continue,
        };
        let node = Node {
            kind,
            span,
            shared,
            children: Vec::new(),
        };
        if opening {
            stack.push(node);
        } else {
            push_node(&mut stack, &mut root, node)?;
        }
    }
    root.ok_or_else(|| app_err!("Dependabot configuration is empty; restore its version and update jobs"))
}

fn push_node(stack: &mut [Node], root: &mut Option<Node>, node: Node) -> Result<(), AppError> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.replace(node).is_some() {
        bail!("Dependabot configuration must contain exactly one YAML document");
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::catalog::CliMeta;

    const PATHS: &[&str] = &[".github/actions/anvil-setup/action.yml", ".github/workflows/anvil-pr-impl.yml"];
    const JOB: &str = concat!(
        "version: 2\n",
        "updates:\n",
        "  - package-ecosystem: github-actions\n",
        "    directory: /\n",
        "    schedule: {interval: weekly}\n",
    );

    #[test]
    fn exclusions_are_exact_selected_catalog_paths_not_prefix_globs() {
        let catalog = Catalog::builder(CliMeta::new("test"))
            .with_artifact(Artifact::backend_file(Backend::GitHub, ".github/workflows/generated.yaml", ""))
            .with_artifact(Artifact::owned_file(".github/actions/generated/action.yml", ""))
            .with_artifact(Artifact::owned_file(".github/actions/generated/helper.json", ""))
            .with_artifact(Artifact::owned_file("justfiles/anvil/extra.just", ""))
            .with_artifact(Artifact::backend_file(Backend::Ado, ".github/workflows/ado-only.yml", ""))
            .build()
            .unwrap();
        assert_eq!(
            generated_action_paths(&catalog, &[Backend::GitHub]),
            [".github/actions/generated/action.yml", ".github/workflows/generated.yaml"]
        );
        assert!(generated_action_paths(&catalog, &[]).is_empty());
    }

    #[test]
    fn adds_exclusions_without_replacing_repository_settings() {
        let source = format!(
            "{JOB}    cooldown:\n      default-days: 7\n    groups:\n      actions:\n        patterns: ['*']\n\
             \n  - package-ecosystem: cargo\n    directory: /\n    schedule: {{interval: daily}}\n"
        );
        let output = update(&source, PATHS).unwrap().unwrap();
        let addition = concat!(
            "exclude-paths:\n",
            "      - \".github/actions/anvil-setup/action.yml\"\n",
            "      - \".github/workflows/anvil-pr-impl.yml\"\n",
            "    ",
        );
        assert_eq!(output.replace(addition, ""), source);
        assert!(update(&output, PATHS).unwrap().is_none());
        assert!(!output.contains("codeql.yml"));
        assert!(!output.contains("anvil-*"));
    }

    #[test]
    fn merges_block_and_flow_exclusions_without_duplicates() {
        for exclusions in [
            "    exclude-paths:\n      # repository policy\n      - vendor/**\n",
            "    exclude-paths: [vendor/**] # repository policy\n",
            "    exclude-paths: [] # repository policy\n",
            "    exclude-paths: [ # repository policy\n      vendor/**,\n    ]\n",
        ] {
            let source = format!("{JOB}{exclusions}");
            let output = update(&source, PATHS).unwrap().unwrap();
            assert!(output.contains("# repository policy"));
            assert_eq!(output.contains("vendor/**"), source.contains("vendor/**"));
            assert!(update(&output, PATHS).unwrap().is_none());
            let expanded = update(&output, &[".github/workflows/anvil-pr-impl.yml", ".github/workflows/new.yml"])
                .unwrap()
                .unwrap();
            assert_eq!(expanded.matches(".github/workflows/anvil-pr-impl.yml").count(), 1);
            assert!(expanded.contains(".github/workflows/new.yml"));
            assert!(expanded.contains(".github/actions/anvil-setup/action.yml"));
        }
    }

    #[test]
    fn handles_flow_jobs_multiple_branches_unicode_and_crlf() {
        let source = concat!(
            "# repository policy\n",
            "version: 2\n",
            "updates:\n",
            "  - {package-ecosystem: github-actions, directory: '/', schedule: {interval: weekly}}\n",
            "  - package-ecosystem: github-actions\n",
            "    directories: ['/']\n",
            "    target-branch: release\n",
            "    schedule: {interval: monthly}\n",
            "    exclude-paths: ['vendor/**']\n",
        );
        for newline in ["\n", "\r\n", "\r"] {
            let source = source.replace("repository policy", "pol\u{ed}tica").replace('\n', newline);
            let output = update(&source, PATHS).unwrap().unwrap();
            assert!(output.contains("# pol\u{ed}tica"));
            assert!(output.contains("target-branch: release"));
            assert_eq!(output.matches(".github/workflows/anvil-pr-impl.yml").count(), 2);
            assert!(!output.replace(newline, "").contains(['\r', '\n']));
            assert!(update(&output, PATHS).unwrap().is_none());
        }
        let with_bom = format!("\u{feff}{source}");
        let output = update(&with_bom, PATHS).unwrap().unwrap();
        assert!(output.starts_with('\u{feff}'));
        assert!(update(&output, PATHS).unwrap().is_none());
    }

    #[test]
    fn leaves_other_ecosystems_and_unrelated_anchors_unchanged() {
        let cargo = JOB.replace("github-actions", "cargo");
        assert!(update(&cargo, PATHS).unwrap().is_none());
        let source = format!("{JOB}    cooldown: &cooldown {{default-days: 7}}\n    labels: []\n");
        assert!(update(&source, PATHS).unwrap().unwrap().contains("&cooldown {default-days: 7}"));
        let unrelated = concat!(
            "version: 2\n",
            "updates:\n",
            "  - &cargo\n",
            "    package-ecosystem: cargo\n",
            "    directory: /\n",
            "    schedule: {interval: daily}\n",
            "  - <<: *cargo\n",
            "    package-ecosystem: cargo\n",
            "    directory: /tools\n",
        );
        assert!(update(unrelated, PATHS).unwrap().is_none());
        let source = format!("{unrelated}{}", JOB.split_once("updates:\n").unwrap().1);
        let updated = update(&source, PATHS).unwrap().unwrap();
        assert!(updated.starts_with(unrelated));
        assert!(updated.contains(".github/workflows/anvil-pr-impl.yml"));
        assert!(update(JOB, &[]).unwrap().is_none());
    }

    #[test]
    fn source_repository_keeps_receiving_generated_action_updates() {
        let source = include_str!("../../../.github/dependabot.yml");
        assert!(!source.contains("exclude-paths:"));
        assert!(update(source, PATHS).unwrap().is_none());
        let opted_out = format!("# source repository\n{KEEP_UPDATES}\n{JOB}");
        for newline in ["\n", "\r\n", "\r"] {
            assert!(update(&opted_out.replace('\n', newline), PATHS).unwrap().is_none());
        }
        let ordinary_comment = format!("{JOB}    # anvil: keep-generated-action-updates\n");
        assert!(update(&ordinary_comment, PATHS).unwrap().is_some());
        let quoted = format!("{JOB}    labels: [\"{KEEP_UPDATES}\"]\n");
        assert!(update(&quoted, PATHS).unwrap().is_some());
    }

    #[test]
    fn refuses_unsafe_or_invalid_configuration_instead_of_replacing_it() {
        for source in [
            String::new(),
            "[]\n".to_owned(),
            "version: 2\nupdates: [".to_owned(),
            "version: 1\nupdates: []\n".to_owned(),
            "version: 2\n".to_owned(),
            "version: 2\nupdates: [{}]\n".to_owned(),
            "version: 2\nupdates: [github-actions]\n".to_owned(),
            "version: 2\nupdates: [{package-ecosystem: []}]\n".to_owned(),
            format!("{JOB}    directory: /\n"),
            JOB.replace("    directory: /\n", ""),
            format!("{JOB}    directories: ['/']\n"),
            JOB.replace("directory: /", "directory: /other"),
            JOB.replace("directory: /", "directories: [/, /other]"),
            JOB.replace("directory: /", "directories: []"),
            format!("{JOB}    exclude-paths: null\n"),
            format!("{JOB}    exclude-paths: &paths [vendor/**]\n"),
            format!("paths: &paths [vendor/**]\n{JOB}    exclude-paths: *paths\n"),
            format!("{JOB}    <<: {{labels: []}}\n"),
            format!("{JOB}---\nversion: 2\nupdates: []\n"),
            format!("{JOB}  - *missing\n"),
            JOB.replace("- package-ecosystem", "- &shared package-ecosystem"),
        ] {
            assert!(update(&source, PATHS).is_err(), "{source}");
        }
    }
}
