// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(windows, not(miri)))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static EARLY_SELECTION: AtomicU32 = AtomicU32::new(u32::MAX);
static EARLY_GUARD: AtomicBool = AtomicBool::new(false);
static USER_SELECTION: AtomicU32 = AtomicU32::new(u32::MAX);
static USER_GUARD: AtomicBool = AtomicBool::new(false);

extern "C" fn early_constructor() {
    EARLY_SELECTION.store(gamma_rt::active(), Ordering::Release);
    EARLY_GUARD.store(gamma_rt::a(7), Ordering::Release);
}

extern "C" fn user_constructor() {
    USER_SELECTION.store(gamma_rt::active(), Ordering::Release);
    USER_GUARD.store(gamma_rt::a(7), Ordering::Release);
}

// Probe the boundary after Gamma's .CRT$XCT slot but before every ordinary .CRT$XCU
// constructor, so moving Gamma back into .CRT$XCU fails regardless of object order.
// Both CRT entries are immutable extern "C" fn() pointers, ordered after Gamma's
// .CRT$XCT initialization. Their callbacks only call its guard API and store results in initialized atomics.
#[used]
#[unsafe(link_section = ".CRT$XCTU")]
static EARLY_CONSTRUCTOR: extern "C" fn() = early_constructor;

#[used]
#[unsafe(link_section = ".CRT$XCU")]
static USER_CONSTRUCTOR: extern "C" fn() = user_constructor;

#[test]
fn selection_is_installed_before_the_user_constructor_boundary() {
    let captured = EARLY_SELECTION.load(Ordering::Acquire);

    assert_ne!(captured, u32::MAX, "the early constructor must actually execute");
    assert_eq!(captured, gamma_rt::active());
    assert_eq!(EARLY_GUARD.load(Ordering::Acquire), captured == 7);
}

#[test]
fn an_ordinary_user_constructor_can_execute_instrumented_code() {
    let captured = USER_SELECTION.load(Ordering::Acquire);

    assert_ne!(captured, u32::MAX, "the user constructor must actually execute");
    assert_eq!(captured, gamma_rt::active());
    assert_eq!(USER_GUARD.load(Ordering::Acquire), captured == 7);
}
