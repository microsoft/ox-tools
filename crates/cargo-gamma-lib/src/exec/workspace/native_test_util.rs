// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub(crate) fn mark_cache_owned_for_test(root: &Utf8Path, base: &Utf8Path) {
    fs::write(base.join(CACHE_OWNER), physical(root).as_str()).expect("test cache owner");
    assert!(cache_owner_root(base).is_some(), "test cache owner must resolve");
}

pub(crate) fn create_private_dir_all_for_test(path: &Utf8Path) {
    create_private_dir_all(path).expect("the private test directory should be creatable");
}

impl Workspace {
    pub(in crate::exec) fn set_rustc_captures(&mut self, captures: Option<Utf8PathBuf>) {
        // Exclusive access prevents readers; this store does not publish the following assignment.
        self.rustc_captures_ready.store(captures.is_some(), Ordering::Release);
        self.rustc_captures = captures;
    }
}
