// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assigning a mutant to one shard of a split run, stably across runs and shard counts.

/// Assigns a mutant to a shard.
///
/// This uses jump consistent hashing rather than `hash % count`, because the two behave very
/// differently when the shard count changes. With a modulus, bumping a nightly job from 8 shards
/// to 9 reshuffles roughly 8/9 of all mutants into different shards; with jump consistent hashing
/// only the fraction that must move does. Shard membership is therefore something a team can
/// reason about across a config change instead of a fresh random assignment each time.
#[must_use]
pub fn shard_of(id: &str, count: u32) -> u32 {
    if count <= 1 {
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

/// FNV-1a, used only to turn an id into a shard key.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shard_holds_everything() {
        for id in ["a", "b", "deadbeef1234"] {
            assert_eq!(shard_of(id, 1), 0);
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "hashes 8,000 ids across every shard count; the volume is the point and Miri only re-times the hash"
    )]
    fn shards_are_always_in_range() {
        for count in 1_u32..=16 {
            for index in 0..500_u32 {
                let id = format!("mutant{index:04}");
                let shard = shard_of(&id, count);

                assert!(shard < count, "{id} landed in shard {shard} of {count}");
            }
        }
    }

    #[test]
    fn sharding_is_deterministic() {
        assert_eq!(shard_of("abc123def456", 7), shard_of("abc123def456", 7));
    }

    #[test]
    fn every_mutant_lands_in_exactly_one_shard() {
        let ids: Vec<String> = (0..300).map(|index| format!("mutant{index:04}")).collect();

        for count in [2_u32, 5, 7, 16] {
            let total: usize = (0..count)
                .map(|shard| ids.iter().filter(|id| shard_of(id, count) == shard).count())
                .sum();

            assert_eq!(total, ids.len(), "shard count {count} lost or duplicated mutants");
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "hashes 2,000 ids to measure shard balance; the volume is the point and Miri only re-times the hash"
    )]
    fn shards_are_reasonably_balanced() {
        let ids: Vec<String> = (0..2000).map(|index| format!("mutant{index:05}")).collect();
        let count = 8_u32;
        let expected = ids.len() / count as usize;

        for shard in 0..count {
            let size = ids.iter().filter(|id| shard_of(id, count) == shard).count();

            assert!(
                size > expected / 2 && size < expected * 2,
                "shard {shard} holds {size}, expected around {expected}"
            );
        }
    }

    #[test]
    fn growing_the_shard_count_moves_few_mutants() {
        // The whole reason for jump consistent hashing: a team that raises its nightly shard count
        // should keep most of its coverage history, not reshuffle everything.
        let ids: Vec<String> = (0..2000).map(|index| format!("mutant{index:05}")).collect();
        let moved = ids.iter().filter(|id| shard_of(id, 8) != shard_of(id, 9)).count();
        let total = ids.len();

        // A modulus would move about 8/9 of them.
        assert!(moved < total / 4, "{moved} of {total} mutants moved when growing 8 -> 9");
    }

    #[test]
    fn different_ids_can_land_in_different_shards() {
        let ids: Vec<String> = (0..100).map(|index| format!("mutant{index:04}")).collect();
        let distinct: crate::HashSet<u32> = ids.iter().map(|id| shard_of(id, 4)).collect();

        assert!(distinct.len() > 1, "sharding put everything in one shard");
    }
}
