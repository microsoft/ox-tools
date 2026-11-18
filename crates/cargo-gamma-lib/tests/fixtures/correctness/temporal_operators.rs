// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, Instant, SystemTime};

macro_rules! not_implemented {
    ($ty:ty, $bound:path) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                fn check() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            impl<T: ?Sized + $bound> AmbiguousIfImpl<u8> for T {}
            let _ = <$ty as AmbiguousIfImpl<_>>::check;
        };
    };
}
not_implemented!(Instant, std::ops::Add<Instant>);
not_implemented!(Instant, std::ops::Div<Instant>);
not_implemented!(Duration, std::ops::Rem<u32>);

pub fn difference(left: Instant, right: Instant) -> Duration {
    left - right
}
pub fn shifted(left: SystemTime, right: Duration) -> SystemTime {
    left + right
}
pub fn naive_shifted(left: SystemTime, right: Duration) -> SystemTime {
    left + right
}
pub fn scaled(left: Duration, right: u32) -> Duration {
    left / right
}
pub fn endpoint(start: SystemTime, end: SystemTime) -> std::ops::Range<SystemTime> {
    start..end
}
pub fn compiled_shift(left: SystemTime, right: Duration) -> SystemTime {
    left - right
}
pub fn compiled_naive_shift(left: SystemTime, right: Duration) -> SystemTime {
    left - right
}
pub fn compiled_scale(left: Duration, right: u32) -> Duration {
    left * right
}
