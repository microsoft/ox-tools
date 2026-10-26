// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn production(input: u32) -> u32 {
    input + 1
}

pub fn production_miss() -> u32 {
    47
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_behavior() {
        assert_eq!(super::production(3), 4);
    }
}
