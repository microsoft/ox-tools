// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

const DIFF_LIMIT: usize = 2_000;

enum Step<'a> {
    Kept(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

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
