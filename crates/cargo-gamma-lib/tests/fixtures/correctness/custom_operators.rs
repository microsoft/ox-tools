// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ops::{Add, Mul, Range, Sub};

pub struct Position(pub u32);
pub fn positions(start: Position, end: Position) -> Range<Position> {
    start..end
}
pub struct Number(pub u32);
impl Add<u32> for Number {
    type Output = Self;
    fn add(self, rhs: u32) -> Self {
        Self(self.0 + rhs)
    }
}
impl Sub<u32> for Number {
    type Output = Self;
    fn sub(self, rhs: u32) -> Self {
        Self(self.0 - rhs)
    }
}
impl Mul<u32> for Number {
    type Output = u32;
    fn mul(self, rhs: u32) -> u32 {
        self.0 * rhs
    }
}
pub fn overloaded(left: Number, right: u32) -> Number {
    left + right
}
pub fn numbers(start: Number, end: Number) -> Range<Number> {
    start..end
}
pub fn compiled_subtraction(left: Number, right: u32) -> Number {
    left - right
}
pub fn compiled_changed_result(left: Number, right: u32) -> u32 {
    left * right
}
pub fn compiled_endpoint(start: Number, end: Number) -> Range<Number> {
    start..((end) + 1)
}
