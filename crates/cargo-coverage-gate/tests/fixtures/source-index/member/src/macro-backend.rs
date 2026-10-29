// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn production(input: u32) -> u32 {
    input + 1
}

pub fn production_miss() -> u32 {
    47 // Arbitrary value; coverage assertions classify source lines, not numeric results.
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_behavior() {
        assert_eq!(super::production(3), 4);
    }
}
