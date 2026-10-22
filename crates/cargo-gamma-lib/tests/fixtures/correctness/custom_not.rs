// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::Cell;
use std::ops::Not;
use std::rc::Rc;

#[derive(Clone)]
pub struct IncrementingNot {
    pub value: u32,
    pub calls: Rc<Cell<u32>>,
}

impl Not for IncrementingNot {
    type Output = Self;

    fn not(mut self) -> Self {
        self.calls.set(self.calls.get() + 1);
        self.value += 1;
        self
    }
}

pub fn overloaded_not(value: &IncrementingNot) -> IncrementingNot {
    !value.clone()
}

pub fn compiled_removed_not(value: &IncrementingNot) -> IncrementingNot {
    value.clone()
}
