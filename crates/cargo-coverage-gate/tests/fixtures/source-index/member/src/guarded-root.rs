// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(not(feature = "x"))]
#[path = "guarded-shared.rs"]
mod disabled;

#[cfg(feature = "x")]
#[path = "guarded-shared.rs"]
mod enabled;
