// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// (Attribute input, libtest case name, ignored test marker) at the macro/scheduler boundary.
pub const CASES: &[(&str, &str, &str)] = &[
    (
        "resolves_metadata",
        "resolves_metadata",
        "__cargo_gamma_resource_636172676f2d73756270726f63657373_test_7265736f6c7665735f6d65746164617461",
    ),
    (
        "r#type",
        "type",
        "__cargo_gamma_resource_636172676f2d73756270726f63657373_test_74797065",
    ),
    (
        "café",
        "café",
        "__cargo_gamma_resource_636172676f2d73756270726f63657373_test_636166c3a9",
    ),
];
