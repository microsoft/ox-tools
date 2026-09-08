// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rendering a unified-style diff of one file, for a dry run.

use core::fmt::Write as _;

use camino::Utf8Path;

/// Renders a unified-style diff of one file, for a dry run.
///
/// Whole-file rather than hunked: the edits are a handful of one-line directives scattered through
/// a source file, and a reviewer deciding whether to let the tool touch their tree is reading the
/// code around them, not counting lines.
#[must_use]
pub fn diff(path: &Utf8Path, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut out = format!("--- {path}\n+++ {path}\n");

    for step in script(&old, &new) {
        let _ = match step {
            Step::Kept(line) => writeln!(out, " {line}"),
            Step::Added(line) => writeln!(out, "+{line}"),
            Step::Removed(line) => writeln!(out, "-{line}"),
        };
    }

    out
}

/// One line's fate in a diff.
enum Step<'a> {
    Kept(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

/// The number of single-line differences past which the diff is not worth computing exactly.
///
/// The exact algorithm costs `O((n + m) · d)` time and `O(d²)` memory in the number of differing
/// lines, which is ideal for the handful of directive lines this module writes and ruinous for two
/// texts with nothing in common. Past the cap the answer degrades to "all of this became all of
/// that", which is both honest and cheap — and only reachable by a caller doing something this
/// module does not do.
const DIFF_LIMIT: usize = 2_000;

/// The line-by-line edit script turning `old` into `new`.
///
/// Myers' greedy algorithm: walk diagonals of the edit graph outwards from the origin, recording
/// each round, and once the far corner is reached walk the rounds backwards to recover the moves
/// that got there. It is worth the fifty lines over the obvious "anything that does not match is an
/// insertion" because that one cannot express a deletion at all, and removing a directive is
/// exactly a deletion.
#[expect(
    clippy::many_single_char_names,
    reason = "n, m, d, k, x and y are Myers' own names for these; renaming them would make the algorithm harder to check against the paper, not easier"
)]
fn script<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Step<'a>> {
    let (n, m) = (old.len(), new.len());
    let max = n + m;

    // Two empty texts are already at the far corner, and the loop below cannot express that. Its
    // first round reads the diagonal to the right of `k = 0`, which exists only because `max` is at
    // least one; with nothing on either side the shifted index runs off the end of a one-element
    // vector. An empty file rewritten to an empty file is a real call — a source file holding
    // nothing but directives, all of them removed — so this is a returned answer, not an assertion.
    if max == 0 {
        return Vec::new();
    }

    if max > DIFF_LIMIT && old != new {
        let mut steps: Vec<Step<'a>> = old.iter().map(|line| Step::Removed(line)).collect();

        steps.extend(new.iter().map(|line| Step::Added(line)));

        return steps;
    }

    // Indexed by diagonal `k = x - y`, which runs from `-max` to `max`, so it is stored shifted.
    let mut furthest = vec![0_isize; 2 * max + 1];

    // Each round's state, kept as one flat buffer rather than a vector of snapshots. Round `d` only
    // ever needs the diagonals `-d..=d` — every other entry is either impossible to have reached or
    // never read by the walk back — so a window of `2d + 1` values is recorded, at offset `d²`,
    // which is the sum of every earlier window. Cloning the whole vector each round instead copied
    // `2 · max + 1` values into a fresh allocation `d` times, for tens of megabytes of transient
    // garbage on a large diff and no more information.
    let mut rounds: Vec<isize> = Vec::new();
    let shift = |k: isize| usize::try_from(k + isize::try_from(max).unwrap_or(isize::MAX)).unwrap_or(0);

    for d in 0..=isize::try_from(max).unwrap_or(isize::MAX) {
        rounds.extend_from_slice(furthest.get(shift(-d)..=shift(d)).unwrap_or_default());

        let mut k = -d;

        while k <= d {
            // Which of the two neighbouring diagonals to extend from: going down (an insertion)
            // when there is no diagonal to the left, or when the one to the right reaches further.
            let down = k == -d || (k != d && furthest[shift(k - 1)] < furthest[shift(k + 1)]);
            let mut x = if down { furthest[shift(k + 1)] } else { furthest[shift(k - 1)] + 1 };
            let mut y = x - k;

            // The free part: every line the two texts already agree on costs nothing to cross.
            while let (Ok(xi), Ok(yi)) = (usize::try_from(x), usize::try_from(y))
                && xi < n
                && yi < m
                && old[xi] == new[yi]
            {
                x += 1;
                y += 1;
            }

            furthest[shift(k)] = x;

            if usize::try_from(x).unwrap_or(0) >= n && usize::try_from(y).unwrap_or(0) >= m {
                return walk_back(old, new, &rounds, d);
            }

            k += 2;
        }
    }

    Vec::new()
}

/// Recovers the edit script from the recorded rounds, by walking the corner back to the origin.
///
/// `rounds` is the flat record `script` built: round `r`'s furthest-reached endpoints for the
/// diagonals `-r..=r`, starting at offset `r²`. Only those diagonals are ever asked for — the
/// endpoint reached after `r` edits lies on a diagonal `|k| <= r` of the same parity, and the two
/// neighbours consulted are only consulted when `k` is neither `-r` nor `r`.
fn walk_back<'a>(old: &[&'a str], new: &[&'a str], rounds: &[isize], d: isize) -> Vec<Step<'a>> {
    let mut steps = Vec::new();
    let mut x = isize::try_from(old.len()).unwrap_or(isize::MAX);
    let mut y = isize::try_from(new.len()).unwrap_or(isize::MAX);

    for round in (0..=d).rev() {
        let reached = |k: isize| -> isize {
            let index = round
                .checked_mul(round)
                .zip(k.checked_add(round))
                .and_then(|(base, offset)| base.checked_add(offset));

            index
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| rounds.get(index).copied())
                .unwrap_or(0)
        };

        let k = x - y;
        let down = k == -round || (k != round && reached(k - 1) < reached(k + 1));
        let previous = if down { k + 1 } else { k - 1 };
        let start = reached(previous);
        let (before_x, before_y) = (start, start - previous);

        // The diagonal run at the end of this round: lines both texts have, emitted as context.
        while x > before_x && y > before_y {
            x -= 1;
            y -= 1;

            if let Some(line) = usize::try_from(x).ok().and_then(|index| old.get(index)) {
                steps.push(Step::Kept(line));
            }
        }

        if round == 0 {
            break;
        }

        if down {
            y -= 1;

            if let Some(line) = usize::try_from(y).ok().and_then(|index| new.get(index)) {
                steps.push(Step::Added(line));
            }
        } else {
            x -= 1;

            if let Some(line) = usize::try_from(x).ok().and_then(|index| old.get(index)) {
                steps.push(Step::Removed(line));
            }
        }
    }

    steps.reverse();
    steps
}
