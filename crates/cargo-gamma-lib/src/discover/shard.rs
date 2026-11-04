// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assigning a mutant to one shard of a split run, stably across runs and shard counts.

use core::num::NonZeroU32;

/// Assigns a mutant to a shard.
///
/// This uses jump consistent hashing rather than `hash % count`, because the two behave very
/// differently when the shard count changes. With a modulus, bumping a nightly job from 8 shards
/// to 9 reshuffles roughly 8/9 of all mutants into different shards; with jump consistent hashing
/// only the fraction that must move does. Shard membership is therefore something a team can
/// reason about across a config change instead of a fresh random assignment each time.
#[must_use]
pub fn shard_of(id: &str, count: NonZeroU32) -> u32 {
    let count = count.get();
    if
    // #[gamma::skip(cond.always_false, reason = "Removing count==1 fast return executes jump hash normally. First iteration sets candidate=0 and next>=1, so count1 still returns0.", tag = "equivalent")]
    count ==
    // #[gamma::skip(literal.int_decrement, reason = "Removing count==1 fast return executes jump hash normally. First iteration sets candidate=0 and next>=1, so count1 still returns0.", tag = "equivalent")]
    1
    {
        return 0;
    }

    let mut key = fnv1a(id.as_bytes());
    let mut candidate: i64 =
    // #[gamma::skip(unary.remove_neg, reason = "count is nonzero and next starts0, so the loop always executes once and overwrites candidate with next before reading candidate. Initial candidate=-1,0,1,2 are unobserved.", tag = "equivalent")]
    -
    // #[gamma::skip(literal.int_decrement, literal.int_increment, reason = "count is nonzero and next starts0, so the loop always executes once and overwrites candidate with next before reading candidate. Initial candidate=-1,0,1,2 are unobserved.", tag = "equivalent")]
    1;
    let mut next: i64 = 0;

    while next < i64::from(count) {
        let previous = next;
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
        assert!(
            next > previous,
            "jump hashing must advance beyond the previous candidate on each iteration"
        );
    }

    u32::try_from(candidate.max(0)).unwrap_or(
        // #[gamma::skip(literal.int_increment, reason = "candidate is selected from nonnegative next<count<=u32::MAX; candidate.max(0) fits u32. unwrap_or fallback0/1 cannot execute.", tag = "equivalent")]
        0,
    )
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
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn oct25_jump_assignments_match_an_integer_quotient_oracle_with_bounded_progress() {
        for value in 0..512 {
            let id = format!("{value:012x}");
            for count in [2, 3, 5, 7, 257, 65_535] {
                let mut key = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
                });
                let mut expected = 0_u64;
                let mut jump = 0_u64;
                while jump < u64::from(count) {
                    expected = jump;
                    key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
                    jump = ((expected + 1) << 31) / ((key >> 33) + 1);
                }
                assert_eq!(u64::from(shard_of(&id, NonZeroU32::new(count).unwrap())), expected, "{id}/{count}");
            }
        }
    }

    #[test]
    fn survivor_jump_hash_assignments_match_fixed_vectors_across_small_and_wide_rotations() {
        // Golden outputs from the unchanged implementation pin compatibility for empty and hex IDs
        // across small counts, powers of two, wide counts and the maximum representable count.
        let counts = [1, 2, 3, 7, 16, 257, 65_537, u32::MAX];
        for (id, expected) in [
            ("", [0, 1, 1, 1, 13, 90, 14_284, 2_595_306_538]),
            ("000000000000", [0, 0, 0, 5, 5, 5, 11_221, 3_683_456_029]),
            ("abcdefabcdef", [0, 0, 0, 6, 7, 25, 31_037, 451_876_351]),
            ("0123456789ab", [0, 1, 1, 6, 10, 86, 45_879, 360_330_067]),
            ("deadbeef1234", [0, 0, 0, 5, 8, 19, 59_604, 906_081_268]),
        ] {
            for (count, expected) in counts.into_iter().zip(expected) {
                assert_eq!(shard_of(id, NonZeroU32::new(count).unwrap()), expected, "{id} of {count}");
            }
        }
    }

    #[test]
    fn one_count_selects_shard_zero() {
        for id in ["a", "b", "deadbeef1234"] {
            assert_eq!(shard_of(id, NonZeroU32::MIN), 0);
        }
    }

    #[test]
    fn shards_are_always_in_range() {
        for count in 1_u32..=16 {
            let nonzero = NonZeroU32::new(count).expect("the range starts at one");
            for index in 0..500_u32 {
                let id = format!("mutant{index:04}");
                let shard = shard_of(&id, nonzero);

                assert!(shard < count, "{id} landed in shard {shard} of {count}");
            }
        }
    }

    #[test]
    fn sharding_is_deterministic() {
        let count = NonZeroU32::new(7).expect("seven is nonzero");
        assert_eq!(shard_of("abc123def456", count), shard_of("abc123def456", count));
    }

    #[test]
    fn every_mutant_lands_in_exactly_one_shard() {
        let ids: Vec<String> = (0..300).map(|index| format!("mutant{index:04}")).collect();

        for count in [2_u32, 5, 7, 16] {
            let nonzero = NonZeroU32::new(count).expect("the fixture counts are nonzero");
            let total: usize = (0..count)
                .map(|shard| ids.iter().filter(|id| shard_of(id, nonzero) == shard).count())
                .sum();

            assert_eq!(total, ids.len(), "shard count {count} lost or duplicated mutants");
        }
    }

    #[test]
    fn shards_are_reasonably_balanced() {
        let ids: Vec<String> = (0..2000).map(|index| format!("mutant{index:05}")).collect();
        let count = 8_u32;
        let nonzero = NonZeroU32::new(count).expect("eight is nonzero");
        let expected = ids.len() / count as usize;

        for shard in 0..count {
            let size = ids.iter().filter(|id| shard_of(id, nonzero) == shard).count();

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
        let eight = NonZeroU32::new(8).expect("eight is nonzero");
        let nine = NonZeroU32::new(9).expect("nine is nonzero");
        let moved = ids.iter().filter(|id| shard_of(id, eight) != shard_of(id, nine)).count();
        let total = ids.len();

        // A modulus would move about 8/9 of them.
        assert!(moved < total / 4, "{moved} of {total} mutants moved when growing 8 -> 9");
    }

    #[test]
    fn different_ids_can_land_in_different_shards() {
        let ids: Vec<String> = (0..100).map(|index| format!("mutant{index:04}")).collect();
        let count = NonZeroU32::new(4).expect("four is nonzero");
        let distinct: crate::HashSet<u32> = ids.iter().map(|id| shard_of(id, count)).collect();

        assert!(distinct.len() > 1, "sharding put everything in one shard");
    }
}
