// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn tokens(kind: u8) -> i32 {
    loop {
        let token = match kind {
            0 => {
                continue;
            }
            1 => 11,
            _ => 22,
        };
        return token;
    }
}
