// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};

fn refreshed(resets: &Cell<usize>) -> String {
    resets.set(resets.get() + 1);
    String::new()
}

pub fn guard_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    for () in [(), ()] {
        out.push(line);
        line = refreshed(resets);
        break;
    }
}

pub fn conditional_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    for flag in [false, true] {
        if flag {
            out.push(line);
            break;
        }
        line = refreshed(resets);
    }
}

#[expect(unreachable_code, reason = "the original exit makes the restoration unreachable")]
pub fn unreachable_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    for () in [(), ()] {
        out.push(line);
        break;
        line = refreshed(resets);
    }
}

#[expect(
    clippy::never_loop,
    reason = "standalone reinitialization deletion remains valid because the break is unconditional"
)]
pub fn pair_without_reinitialization(line: String, out: &mut Vec<String>) {
    for () in [(), ()] {
        out.push(line);
        break;
    }
}

pub fn pair_without_break(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    for () in [(), ()] {
        out.push(line);
        line = refreshed(resets);
    }
}

pub fn entry_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    for () in [(), ()] {
        line = refreshed(resets);
        out.push(line);
        line = refreshed(resets);
    }
}

pub fn entry_exit(out: &mut Vec<String>, resets: &Cell<usize>) {
    let mut line: String;
    for () in [(), ()] {
        line = refreshed(resets);
        out.push(line);
        continue;
    }
}

pub struct Tracker<'a> {
    log: &'a RefCell<Vec<&'static str>>,
    text: String,
}

impl<'a> Tracker<'a> {
    fn new(log: &'a RefCell<Vec<&'static str>>) -> Self {
        log.borrow_mut().push("new");
        Self {
            log,
            text: " x ".to_owned(),
        }
    }
}

impl Drop for Tracker<'_> {
    fn drop(&mut self) {
        self.log.borrow_mut().push("drop");
    }
}

fn consume(log: &RefCell<Vec<&'static str>>, value: Option<&Tracker<'_>>) -> usize {
    log.borrow_mut().push("consume");
    value.map_or(0, |value| value.text.len())
}

pub fn borrowed(log: &RefCell<Vec<&'static str>>) -> usize {
    consume(log, Some(&Tracker::new(log)))
}

fn record(log: &RefCell<Vec<&'static str>>, value: &str) -> String {
    log.borrow_mut().push("consume");
    value.to_owned()
}

pub fn trimmed(log: &RefCell<Vec<&'static str>>) -> String {
    record(log, Tracker::new(log).text.trim_end())
}

fn make_vec() -> Vec<char> {
    vec!['z']
}

pub fn last() -> Option<char> {
    Some(*make_vec().last()?)
}

pub fn configured() -> bool {
    #[cfg(any())]
    {
        false
    }
    #[cfg(not(any()))]
    {
        true
    }
}

pub fn configured_body() -> bool {
    #[cfg(any())]
    {
        false
    }
    #[cfg(not(any()))]
    {
        true
    }
}

fn abort() -> ! {
    panic!("mandatory exit")
}

pub fn configured_body_first() -> bool {
    #[cfg(not(any()))]
    {
        true
    }
    #[cfg(any())]
    {
        false
    }
}

pub fn mandatory(flag: bool) -> usize {
    match flag {
        true => 3,
        false => {
            abort();
        }
    }
}

pub fn mandatory_continue() -> usize {
    for flag in [false, true] {
        let value = match flag {
            true => 7,
            false => {
                continue;
            }
        };
        return value;
    }
    0
}

pub fn ownership(mut line: String, out: &mut Vec<String>) {
    out.push(line);
    line = String::new();
    out.push(line);
}

pub fn reset_exit(mut line: String, out: &mut Vec<String>) {
    for () in [(), ()] {
        out.push(line);
        line = String::new();
        break;
    }
    out.push(line);
}

pub fn neighbor(line: &mut String) {
    line.push_str("x");
}

pub struct Partial {
    pub text: String,
    pub count: usize,
}

fn notify(progress: &Cell<usize>) {
    progress.set(progress.get() + 1);
}

pub fn partial_move(mut state: Partial, out: &mut Vec<String>, progress: &Cell<usize>) -> usize {
    for () in [(), ()] {
        out.push(state.text);
        state.text = String::new();
        state.text.push_str("x");
        notify(progress);
        state.count += 1;
        break;
    }
    out.push(state.text);
    state.count
}

fn touch(line: &mut String) {
    line.push_str("b");
}

pub fn borrowed_loop(line: &mut String, progress: &Cell<usize>) {
    for () in [(), ()] {
        touch(line);
        notify(progress);
        break;
    }
    line.push_str("z");
}

pub fn mandatory_break() -> usize {
    for flag in [false, true] {
        let value = match flag {
            true => 7,
            false => {
                break;
            }
        };
        return value;
    }
    0
}

pub fn mandatory_return(flag: bool) -> usize {
    let value = match flag {
        true => 7,
        false => {
            return 3;
        }
    };
    value
}

fn transform(mut line: String) -> String {
    line.push_str("x");
    line
}

pub fn reassign_owned(mut line: String) -> String {
    line = transform(line);
    line
}

pub fn moved_exit(mut line: String, word: &str, out: &mut Vec<String>, progress: &Cell<usize>) {
    for flag in [false, true] {
        if flag {
            out.push(line);
            notify(progress);
            break;
        }
        line.push_str(word);
    }
}
