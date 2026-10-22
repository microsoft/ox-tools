// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};

fn refreshed(resets: &Cell<usize>) -> String {
    resets.set(resets.get() + 1);
    String::new()
}

pub fn guard_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    {
        if ::gamma_rt::a(26u32) {
            for () in [(), ()] {
                out.push(line);
                break;
            }
        } else {
            for () in [(), ()] {
                out.push(line);
                line = refreshed(resets);
                if !::gamma_rt::a(25u32) {
                    break;
                }
            }
        }
    }
}

pub fn conditional_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    {
        if ::gamma_rt::a(27u32) {
            for flag in [false, true] {
                if flag {
                    out.push(line);
                }
                line = refreshed(resets);
            }
        } else {
            {
                if ::gamma_rt::a(28u32) {
                    for flag in [false, true] {
                        if flag {
                            out.push(line);
                            break;
                        }
                    }
                } else {
                    for flag in [false, true] {
                        if flag {
                            out.push(line);
                            break;
                        }
                        line = refreshed(resets);
                    }
                }
            }
        }
    }
}

#[expect(unreachable_code, reason = "the original exit makes the restoration unreachable")]
pub fn unreachable_pair(mut line: String, out: &mut Vec<String>, resets: &Cell<usize>) {
    {
        if ::gamma_rt::a(29u32) {
            for () in [(), ()] {
                out.push(line);
                line = refreshed(resets);
            }
        } else {
            {
                if ::gamma_rt::a(30u32) {
                    for () in [(), ()] {
                        out.push(line);
                        break;
                    }
                } else {
                    for () in [(), ()] {
                        out.push(line);
                        break;
                        line = refreshed(resets);
                    }
                }
            }
        }
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
    {
        if ::gamma_rt::a(32u32) {
            for () in [(), ()] {
                line = refreshed(resets);
                out.push(line);
            }
        } else {
            for () in [(), ()] {
                if !::gamma_rt::a(31u32) {
                    line = refreshed(resets);
                }
                out.push(line);
                line = refreshed(resets);
            }
        }
    }
}

pub fn entry_exit(out: &mut Vec<String>, resets: &Cell<usize>) {
    let mut line: String;
    {
        if ::gamma_rt::a(33u32) {
            for () in [(), ()] {
                line = refreshed(resets);
                out.push(line);
                break;
            }
        } else {
            for () in [(), ()] {
                line = refreshed(resets);
                out.push(line);
                continue;
            }
        }
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
    (if ::gamma_rt::a(1u32) {
        consume(log, None)
    } else {
        consume(log, Some(&Tracker::new(log)))
    })
}

fn record(log: &RefCell<Vec<&'static str>>, value: &str) -> String {
    log.borrow_mut().push("consume");
    value.to_owned()
}

pub fn trimmed(log: &RefCell<Vec<&'static str>>) -> String {
    (if ::gamma_rt::a(2u32) {
        record(log, Tracker::new(log).text.trim_start())
    } else {
        record(log, Tracker::new(log).text.trim_end())
    })
}

fn make_vec() -> Vec<char> {
    vec!['z']
}

pub fn last() -> Option<char> {
    (if ::gamma_rt::a(3u32) {
        Some(*::gamma_rt::panic_on_failure(make_vec().last())?)
    } else {
        Some(*make_vec().last()?)
    })
}

pub fn configured() -> bool {
    #[cfg(any())]
    {
        false
    }
    #[cfg(not(any()))]
    (if ::gamma_rt::a(4u32) { !({ true }) } else { { true } })
}

pub fn configured_body() -> bool {
    if ::gamma_rt::a(5u32) {
        false
    } else {
        {
            #[cfg(any())]
            {
                false
            }
            #[cfg(not(any()))]
            {
                true
            }
        }
    }
}

fn abort() -> ! {
    panic!("mandatory exit")
}

pub fn configured_body_first() -> bool {
    if ::gamma_rt::a(11u32) {
        false
    } else {
        {
            #[cfg(not(any()))]
            {
                true
            }
            #[cfg(any())]
            {
                false
            }
        }
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
                {
                    if ::gamma_rt::a(7u32) {
                        break;
                    }
                    continue;
                };
            }
        };
        return value;
    }
    0
}

pub fn ownership(mut line: String, out: &mut Vec<String>) {
    if !::gamma_rt::a(8u32) {
        out.push(line);
    }
    line = String::new();
    if !::gamma_rt::a(9u32) {
        out.push(line);
    }
}

pub fn reset_exit(mut line: String, out: &mut Vec<String>) {
    for () in [(), ()] {
        out.push(line);
        line = String::new();
        {
            if ::gamma_rt::a(10u32) {
                continue;
            }
            break;
        };
    }
    out.push(line);
}

pub fn neighbor(line: &mut String) {
    if !::gamma_rt::a(6u32) {
        line.push_str("x");
    }
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
        if !::gamma_rt::a(13u32) {
            state.text.push_str("x");
        }
        if !::gamma_rt::a(12u32) {
            notify(progress);
        }
        state.count += (if ::gamma_rt::a(14u32) { 0 } else { 1 });
        {
            if ::gamma_rt::a(15u32) {
                continue;
            }
            break;
        };
    }
    out.push(state.text);
    state.count
}

fn touch(line: &mut String) {
    line.push_str("b");
}

pub fn borrowed_loop(line: &mut String, progress: &Cell<usize>) {
    for () in [(), ()] {
        if !::gamma_rt::a(19u32) {
            touch(line);
        }
        if !::gamma_rt::a(16u32) {
            notify(progress);
        }
        {
            if ::gamma_rt::a(18u32) {
                continue;
            }
            break;
        };
    }
    if !::gamma_rt::a(17u32) {
        line.push_str("z");
    }
}

pub fn mandatory_break() -> usize {
    for flag in [false, true] {
        let value = match flag {
            true => 7,
            false => {
                {
                    if ::gamma_rt::a(20u32) {
                        continue;
                    }
                    break;
                };
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
            return (if ::gamma_rt::a(21u32) { Default::default() } else { 3 });
        }
    };
    value
}

fn transform(mut line: String) -> String {
    line.push_str("x");
    line
}

pub fn reassign_owned(mut line: String) -> String {
    if !::gamma_rt::a(22u32) {
        line = transform(line);
    }
    line
}

pub fn moved_exit(mut line: String, word: &str, out: &mut Vec<String>, progress: &Cell<usize>) {
    for flag in [false, true] {
        if flag {
            out.push(line);
            if !::gamma_rt::a(23u32) {
                notify(progress);
            }
            break;
        }
        if !::gamma_rt::a(24u32) {
            line.push_str(word);
        }
    }
}
