// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn library_function() {
    println!("Library function");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_library_function() {
        library_function();
    }
}
