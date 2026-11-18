// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, SystemTime};

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
not_implemented!(SystemTime, std::ops::Add<SystemTime>);
not_implemented!(SystemTime, std::ops::Div<SystemTime>);
not_implemented!(Duration, std::ops::Rem<u32>);

pub fn difference(left: SystemTime, right: SystemTime) -> Duration {
    left.duration_since(right).expect("left is after right")
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
