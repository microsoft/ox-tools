// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intercepts public-wrapper metadata discovery without launching Cargo in unit tests.

use std::cell::RefCell;
use std::path::Path;

use cargo_metadata::Metadata;

use crate::CoverageGateError;

type Loader = Box<dyn Fn(Option<&Path>) -> Result<Metadata, CoverageGateError>>;

thread_local! {
    static LOADER: RefCell<Option<Loader>> = const { RefCell::new(None) };
}

pub(super) fn load(path: Option<&Path>) -> Option<Result<Metadata, CoverageGateError>> {
    LOADER.with_borrow(|loader| loader.as_ref().map(|loader| loader(path)))
}

/// Override only this thread's discovery, restoring the previous loader on return or unwind.
pub(crate) fn with_loader<T>(
    loader: impl Fn(Option<&Path>) -> Result<Metadata, CoverageGateError> + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Loader>);

    impl Drop for Restore {
        fn drop(&mut self) {
            LOADER.with_borrow_mut(|loader| *loader = self.0.take());
        }
    }

    let _restore = Restore(LOADER.with_borrow_mut(|previous| previous.replace(Box::new(loader))));
    operation()
}
