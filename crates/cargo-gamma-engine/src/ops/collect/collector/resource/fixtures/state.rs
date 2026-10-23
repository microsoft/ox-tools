// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn pad(left: &mut Vec<char>, target: usize) {
    while left.len() < target {
        left.insert(0, ' ');
    }
}

fn owned_pad(mut left: Vec<char>, target: usize) {
    while left.len() < target {
        left.insert(0, ' ');
    }
}

fn cursor(characters: Vec<char>, mut index: usize, mut tokens: Vec<u8>) {
    while index < characters.len() {
        match characters[index] {
            '?' => {
                tokens.push(1_u8);
                index += 1;
            }
            '*' => {
                while index < characters.len() && characters[index] == '*' {
                    index += 1;
                }
                tokens.push(2_u8);
            }
            _ => {
                index += 1;
            }
        }
    }
}

fn scalar(n: u16) {
    let bound: u32 = u32::from(n);
    let mut next: u32 = 0;
    while next < bound {
        next = 1_u32 << 31;
    }
}

fn boundary(n: u32, offset: u32, mut groups: Vec<u32>) {
    let mut start: u32 = 0;
    while start < n {
        let end = start + offset;
        groups.push(end);
        start = end;
    }
}

fn finite_negated_padding(mut left: Vec<char>, target: usize) {
    while left.len() < target {
        left.insert(0, ' ');
    }
}

fn suffixes(mut used: std::collections::HashSet<u64>) {
    for ordinal in 2_u64.. {
        if used.insert(ordinal) {
            break;
        }
    }
}

fn retained_identity(mut left: Vec<char>, target: usize) {
    while left.len() < target {
        left.insert(0, ' ');
    }
    left.insert(0, ' ');
}

struct Frame {
    edge: usize,
}

fn projected(frame: &mut Frame, target: usize) {
    while frame.edge < target {
        frame.edge += 1;
    }
}

fn immutable_boundary(input: Vec<char>, offset: usize, mut groups: Vec<usize>) {
    let mut start: usize = 0;
    while start < input.len() {
        let end = start + offset;
        groups.push(end);
        start = end;
    }
}

fn mutable_boundary(mut input: Vec<char>, offset: usize, mut groups: Vec<usize>) {
    let mut start: usize = 0;
    while start < input.len() {
        input.push('*');
        let end = start + offset;
        groups.push(end);
        start = end;
    }
}

fn finite_offset(offset: usize) -> usize {
    0 + offset
}
