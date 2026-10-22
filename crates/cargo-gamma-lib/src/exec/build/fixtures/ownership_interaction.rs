// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn ownership() {
    let mut value = String::from("owned");
    'done: {
        drop(value);
        value = String::new();
        break 'done;
        core::mem::drop(value);
    }
}

fn consume(value: String) {
    drop(value);
}
