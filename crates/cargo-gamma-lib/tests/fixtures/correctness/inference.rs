// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::Cell;
use std::ops::RangeInclusive;

pub struct NoDebug;

pub fn sum(values: &[u64]) -> u64 {
    values.iter().sum()
}

pub trait Sample {
    fn sample() -> Self;
}

impl Sample for u32 {
    fn sample() -> Self {
        7
    }
}

fn make<T: Sample>(calls: &Cell<usize>) -> Option<T> {
    calls.set(calls.get() + 1);
    Some(T::sample())
}

pub fn defaulted(calls: &Cell<usize>) -> Option<u32> {
    make(calls)
}

pub fn propagate(value: Result<usize, NoDebug>) -> Result<usize, NoDebug> {
    Ok(value?)
}

pub fn optional(value: Option<usize>) -> Option<usize> {
    Some(value?)
}

pub fn inferred_collect(values: &[usize]) -> Result<Vec<usize>, NoDebug> {
    Ok(values
        .iter()
        .map(|value| if *value > 0 { Ok(*value) } else { Err(NoDebug) })
        .collect::<Result<_, _>>()?)
}

fn identity(value: f64) -> f64 {
    value
}

pub fn float_from(hits: u32) -> f64 {
    identity(f64::from(hits))
}

pub fn float_range(end: f64) -> RangeInclusive<f64> {
    0.0..=end
}

pub struct Converted;

impl From<NoDebug> for Converted {
    fn from(_: NoDebug) -> Self {
        Self
    }
}

fn produce(value: Result<usize, NoDebug>, calls: &Cell<usize>) -> Result<usize, NoDebug> {
    calls.set(calls.get() + 1);
    value
}

pub fn converted(value: Result<usize, NoDebug>, calls: &Cell<usize>) -> Result<usize, Converted> {
    Ok(produce(value, calls)?)
}
