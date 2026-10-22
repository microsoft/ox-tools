// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(super) struct Input {
    pub(super) entries: Vec<Entry>,
}

pub(super) struct Entry {
    pub(super) path: String,
    pub(super) ordinal: u32,
}

#[derive(Debug, PartialEq)]
pub(super) struct Output {
    pub(super) path: String,
    pub(super) ids: Vec<u32>,
}

#[expect(
    clippy::unnecessary_sort_by,
    clippy::redundant_closure_for_method_calls,
    reason = "the proof fixture exercises an unexecuted comparator"
)]
pub(super) fn groups(input: &Input) -> Vec<Output> {
    let mut files = Vec::new();
    let mut cursor = 0;
    while cursor < input.entries.len() {
        let path = input.entries[cursor].path.clone();
        let end = input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| cursor + offset);
        let group = &input.entries[cursor..end];
        let mut ids = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();
        ids.sort_by(|left, right| left.cmp(right));
        ids.dedup();
        files.push(Output { path, ids });
        cursor = end;
    }
    files
}

pub(super) fn nonzero(input: &Input) -> Vec<Output> {
    let mut files = Vec::new();
    let mut cursor = 2;
    while cursor < input.entries.len() {
        let path = input.entries[cursor].path.clone();
        let end = input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| cursor + offset);
        let group = &input.entries[cursor..end];
        let ids = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();
        files.push(Output { path, ids });
        cursor = end;
    }
    files
}

pub(super) fn finite_multiplication(input: &Input) -> Vec<Output> {
    let mut files = Vec::new();
    let mut cursor = 2;
    while cursor < input.entries.len() {
        let path = input.entries[cursor].path.clone();
        let end = input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| cursor * offset);
        let group = &input.entries[cursor..end];
        let ids = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();
        files.push(Output { path, ids });
        cursor = end;
    }
    files
}

#[expect(
    clippy::needless_pass_by_ref_mut,
    reason = "the negative fixture preserves mutable input uncertainty"
)]
pub(super) fn mutable(input: &mut Input) -> Vec<Output> {
    let mut files = Vec::new();
    let mut cursor = 0;
    while cursor < input.entries.len() {
        let path = input.entries[cursor].path.clone();
        let end = input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| cursor + offset);
        let group = &input.entries[cursor..end];
        let ids = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();
        files.push(Output { path, ids });
        cursor = end;
    }
    files
}

pub(super) fn unknown_factory(input: &Input) -> Vec<Output> {
    let mut files = Vec::new();
    let mut cursor = 0;
    while cursor < input.entries.len() {
        let path = input.entries[cursor].path.clone();
        let end = input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| cursor + offset);
        let group = &input.entries[cursor..end];
        let ids = group.iter().map(mapper()).collect::<Vec<_>>();
        files.push(Output { path, ids });
        cursor = end;
    }
    files
}

fn mapper() -> impl Fn(&Entry) -> u32 {
    |entry| entry.ordinal
}
