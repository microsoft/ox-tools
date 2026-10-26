// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

include!(concat!(env!("OUT_DIR"), "/production.rs"));

pub fn production(value: u32) -> u32 {
    value + 1
}

#[cfg(test)]
#[path = "conservative-shared.rs"]
mod test_helpers;

#[cfg(test)]
mod tests {
    #[test]
    fn unit_behavior() {
        assert_eq!(super::production(3), 4);
    }

    #[allow(dead_code)]
    fn test_only_miss() -> u32 {
        99
    }
}

#[cfg(all(test, unix))]
fn test_all_miss() -> u32 {
    101
}

#[cfg(feature = "test-util")]
pub fn feature_production(value: u32) -> u32 {
    value * 2
}

#[cfg(any(test, feature = "test-util"))]
pub fn mixed_production(value: u32) -> u32 {
    value + 2
}

#[cfg(not(test))]
pub fn production_only(value: u32) -> u32 {
    value + 3
}
