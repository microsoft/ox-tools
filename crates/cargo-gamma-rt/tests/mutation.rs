// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use core::fmt::Debug;

use gamma_rt::{Boundary, decrement, default_after, increment, panic_on_failure};

fn assert_boundary<T: Boundary + Copy + Debug + PartialEq>(zero: T, one: T, two: T) {
    assert_eq!(increment(zero), one);
    assert_eq!(increment(one), two);
    assert_eq!(decrement(two), one);
    assert_eq!(decrement(one), zero);
    assert_eq!(decrement(increment(one)), one);
}

#[test]
fn primitive_boundaries_preserve_the_operand_type_in_both_directions() {
    assert_boundary(0_u8, 1, 2);
    assert_boundary(0_u16, 1, 2);
    assert_boundary(0_u32, 1, 2);
    assert_boundary(0_u64, 1, 2);
    assert_boundary(0_u128, 1, 2);
    assert_boundary(0_usize, 1, 2);
    assert_boundary(0_i8, 1, 2);
    assert_boundary(0_i16, 1, 2);
    assert_boundary(0_i32, 1, 2);
    assert_boundary(0_i64, 1, 2);
    assert_boundary(0_i128, 1, 2);
    assert_boundary(0_isize, 1, 2);
    assert_boundary(0_f32, 1.0, 2.0);
    assert_boundary(0_f64, 1.0, 2.0);
}

#[test]
fn default_replacement_drops_the_original_before_constructing_the_default() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    // Drop, Default, and the assertions execute inline on this test thread. SeqCst is a conservative
    // callback-order oracle, not a mechanism for publishing shared data between threads.
    static PHASE: AtomicUsize = AtomicUsize::new(0);

    struct Ordered(bool);

    impl Default for Ordered {
        fn default() -> Self {
            assert_eq!(PHASE.swap(2, Ordering::SeqCst), 1);
            Self(false)
        }
    }

    impl Drop for Ordered {
        fn drop(&mut self) {
            if self.0 {
                assert_eq!(PHASE.swap(1, Ordering::SeqCst), 0);
            }
        }
    }

    let replacement = default_after(Ordered(true));
    assert!(!replacement.0);
    assert_eq!(PHASE.load(Ordering::SeqCst), 2);
}

#[test]
fn propagation_success_preserves_the_value_without_requiring_debug_for_errors() {
    struct Opaque;

    assert_eq!(panic_on_failure(Some(String::from("original"))), Some(String::from("original")));
    let result: Result<u32, Opaque> = panic_on_failure(Ok(17));
    assert!(matches!(result, Ok(17)));
}

#[test]
#[should_panic(expected = "gamma: propagation replaced by panic")]
fn propagation_error_panics_without_requiring_debug_for_the_error() {
    struct Opaque;

    let _: Result<u32, Opaque> = panic_on_failure(Err(Opaque));
}

#[test]
#[should_panic(expected = "gamma: propagation replaced by panic")]
fn propagation_absence_panics_instead_of_returning_none() {
    let _: Option<u32> = panic_on_failure(None);
}
