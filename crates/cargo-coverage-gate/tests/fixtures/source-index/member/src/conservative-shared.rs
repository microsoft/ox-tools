// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn shared(value: u32) -> u32 {
    value + 1
}

pub fn production_miss() -> u32 {
    37 // Arbitrary value; coverage assertions classify source lines, not numeric results.
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_behavior() {
        assert_eq!(super::shared(3), 4);
    }
}
