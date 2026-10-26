// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

std::cfg_select! {
    feature = "backend" => {
        #[path = "macro-backend.rs"]
        pub mod backend;
    }
    _ => {
        #[path = "macro-backend.rs"]
        pub mod backend;
    }
}

#[cfg(test)]
#[path = "conservative-shared.rs"]
mod test_helpers;

#[cfg(test)]
mod tests {
    #[test]
    fn unit_behavior() {
        assert_eq!(super::test_helpers::shared(3), 4);
    }
}
