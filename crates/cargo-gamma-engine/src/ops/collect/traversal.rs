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
/// Equivalent to calling [`super::check_stated`] and then [`collect_with`]: the same fault, in the
/// same wording, stops candidate collection before it starts, and the candidates returned when there
/// is no fault are the same candidates `collect_with` would have produced from the same inputs. The
/// only difference is that both passes now read the file's syntax tree once between them, instead of
/// [`super::check_stated`]'s own pass, an index-building pass `collect_with` would otherwise run
/// internally, and `collect_with`'s own candidate-collecting pass.
pub fn check_stated_and_collect_with(
    file: &SourceFile,
    selection: &Selection,
    cfg: &CfgSet,
    defaults: &Defaults,
) -> Result<Vec<Candidate>> {
    let indexes = phase_one::run(file, selection)?;
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
