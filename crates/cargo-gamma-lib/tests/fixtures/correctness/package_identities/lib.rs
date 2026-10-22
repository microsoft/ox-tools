// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod alternate;
pub mod catalog;
pub mod consumers;
pub mod dependency;
pub mod features;
#[cfg(any())]
#[path = "inactive.rs"]
pub mod inactive;
#[path = "selected.rs"]
pub mod relocated;
