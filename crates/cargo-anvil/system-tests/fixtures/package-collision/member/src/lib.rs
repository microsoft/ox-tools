// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

published_cfg_if::cfg_if! {
    if #[cfg(unix)] {
        pub const PLATFORM: &str = "unix";
    } else {
        pub const PLATFORM: &str = "other";
    }
}
