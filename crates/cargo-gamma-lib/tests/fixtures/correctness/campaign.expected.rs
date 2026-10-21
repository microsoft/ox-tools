// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::Cell;

fn date(calls: &Cell<usize>) -> String {
    calls.set(calls.get() + 1);
    String::from("2026-10-15")
}

pub fn fragment(calls: &Cell<usize>) -> Result<String, ()> {
    (if ::gamma_rt::a(1u32) {
        Ok(::gamma_rt::panic_on_failure(date(calls).get(..10).ok_or_else(|| ()))?.to_owned())
    } else {
        Ok(date(calls).get(..10).ok_or_else(|| ())?.to_owned())
    })
}

pub fn fragment_direct(calls: &Cell<usize>) -> Result<String, ()> {
    (if ::gamma_rt::a(2u32) {
        Ok(::gamma_rt::panic_on_failure(date(calls).get(..10).ok_or(()))?.to_owned())
    } else {
        Ok(date(calls).get(..10).ok_or(())?.to_owned())
    })
}

fn rendered(value: &str) -> String {
    value.to_owned()
}

pub fn matched(values: &[Option<String>]) -> String {
    for value in values {
        let text = match value {
            Some(value) => rendered(value),
            None => {
                {
                    if ::gamma_rt::a(3u32) {
                        break;
                    }
                    continue;
                };
            }
        };
        return text;
    }
    String::new()
}

pub struct State {
    pub values: Vec<String>,
}

pub fn rebuilt(mut state: State) -> State {
    let values = state.values.into_iter().collect();
    state.values = values;
    if !::gamma_rt::a(4u32) {
        state.values.push(String::from("tail"));
    }
    state
}

pub fn unit_match(values: &[Option<String>], out: &mut Vec<String>) {
    for value in values {
        match value {
            Some(value) => out.push(value.clone()),
            None => {
                continue;
            }
        }
        out.push(String::new());
    }
}
