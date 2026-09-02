// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driving the collector over a syntax tree to produce the candidates a file admits.

use syn::visit::Visit;

use super::collector::{Collector, phase_one};
use super::{Candidate, Defaults};
use crate::Result;
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

/// Collects every candidate a file admits under the given selection, with nothing stripped for
/// configuration.
///
/// Equivalent to [`collect_in`] with an unconditional set. Use it where the build's configuration
/// is not known, which is every caller that is examining a fragment of source rather than a real
/// workspace.
///
/// The result is sorted by span start, then by mutator name, so that two runs over the same source
/// produce the same order regardless of how the traversal happened to visit siblings.
#[must_use]
pub fn collect(file: &SourceFile, selection: &Selection) -> Vec<Candidate> {
    collect_in(file, selection, &CfgSet::unconditional())
}

/// Collects every candidate a file admits, under a selection and a build configuration.
///
/// `cfg` decides which conditionally compiled code is actually in the build. Code behind a
/// predicate that does not hold produces no candidates at all: the compiler strips it, so a guard
/// there would never be compiled and its mutant could never be activated by any test.
#[must_use]
pub fn collect_in(file: &SourceFile, selection: &Selection, cfg: &CfgSet) -> Vec<Candidate> {
    collect_with(file, selection, cfg, &Defaults::default())
}

/// Collects every candidate a file admits, told what the rest of the workspace implements.
///
/// The extra argument is what lets a `Default::default()` be withheld for a type the workspace
/// defines and gives no `Default`. An empty index is not a claim that nothing has one; it says
/// nothing was looked at, and every type stays optimistic, which is what [`collect_in`] passes.
#[must_use]
pub fn collect_with(file: &SourceFile, selection: &Selection, cfg: &CfgSet, defaults: &Defaults) -> Vec<Candidate> {
    let collector = Collector::new(file, selection, selection.errors(), cfg, defaults);

    finish(file, collector)
}

/// Reports a file's stated-value errors and collects its candidates in one walk of the syntax tree,
/// rather than the two [`super::check_stated`] and [`collect_with`] would run one after the other.
///
/// The candidates are exactly the candidates [`collect_with`] would have produced from the same
/// inputs, and a fault stops collection before it starts in the same wording [`super::check_stated`]
/// would have used. The one difference is *which* stated values are audited at all:
/// [`super::check_stated`] reads a whole file and knows nothing about configuration, while this
/// pass audits only what `cfg` says the measured build compiles and this tool would mutate —
/// skipping configured-out and test-gated code, the same rule that decides where a candidate may
/// be offered. A stated value there produces no mutant under either entry point, so the fused pass
/// stays silent about it rather than failing a run over code it is not measuring; `rustc` still
/// rejects a malformed one when that code is built.
///
/// Everything else is shared: both passes read the file's syntax tree once between them, instead of
/// [`super::check_stated`]'s own pass, an index-building pass `collect_with` would otherwise run
/// internally, and `collect_with`'s own candidate-collecting pass.
pub fn check_stated_and_collect_with(
    file: &SourceFile,
    selection: &Selection,
    cfg: &CfgSet,
    defaults: &Defaults,
) -> Result<Vec<Candidate>> {
    let indexes = phase_one::run(file, selection, cfg)?;
    let collector = Collector::with_indexes(file, selection, selection.errors(), cfg, defaults, indexes);

    Ok(finish(file, collector))
}

/// Drives a collector's own traversal to completion and returns its candidates in the stable order
/// every caller depends on.
fn finish<'a>(file: &'a SourceFile, mut collector: Collector<'a>) -> Vec<Candidate> {
    collector.visit_file(&file.ast);

    let mut candidates = collector.finish();

    candidates.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| left.span.end.cmp(&right.span.end))
            .then_with(|| left.mutator.cmp(right.mutator))
            .then_with(|| left.replacement_index.cmp(&right.replacement_index))
    });

    candidates
}
