// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::num::NonZeroU32;

pub fn shard_of(id: &str, count: NonZeroU32) -> u32 {
    let count = count.get();
    if count == 1 {
        return 0;
    }

    let mut key = fnv1a(id.as_bytes());
    let mut candidate: i64 = -1;
    let mut next: i64 = 0;

    while next < i64::from(count) {
        candidate = next;
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);

        #[expect(clippy::cast_precision_loss, reason = "only the leading bits steer the choice")]
        let divisor = ((key >> 33).wrapping_add(1)) as f64;

        #[expect(clippy::cast_precision_loss, reason = "the operand is a small shard ordinal")]
        let scaled = ((candidate + 1) as f64) * (f64::from(1_u32 << 31) / divisor);

        #[expect(clippy::cast_possible_truncation, reason = "the value is bounded by the shard count")]
        {
            next = scaled as i64;
        }
    }

    u32::try_from(candidate.max(0)).unwrap_or(0)
}

const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;

    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }

    hash
}
