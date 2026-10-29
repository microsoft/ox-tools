// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn run() -> u32 {
    macro_rules! declare_shared {
        () => {
            #[path = "conservative-shared.rs"]
            mod production;
        };
    }
    declare_shared! {}
    production::shared(3)
}

#[cfg(test)]
#[path = "conservative-shared.rs"]
mod test_helpers;
