// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};

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
not_implemented!(DateTime<Utc>, std::ops::Add<DateTime<Utc>>);
not_implemented!(DateTime<Utc>, std::ops::Div<DateTime<Utc>>);
not_implemented!(TimeDelta, std::ops::Rem<i32>);

pub fn difference(left: DateTime<Utc>, right: DateTime<Utc>) -> TimeDelta {
    left - right
}
pub fn shifted(left: DateTime<Utc>, right: TimeDelta) -> DateTime<Utc> {
    left + right
}
pub fn naive_shifted(left: NaiveDateTime, right: std::time::Duration) -> NaiveDateTime {
    left + right
}
pub fn scaled(left: TimeDelta, right: i32) -> TimeDelta {
    left / right
}
pub fn endpoint(start: DateTime<Utc>, end: DateTime<Utc>) -> std::ops::Range<DateTime<Utc>> {
    start..end
}
pub fn compiled_shift(left: DateTime<Utc>, right: TimeDelta) -> DateTime<Utc> {
    left - right
}
pub fn compiled_naive_shift(left: NaiveDateTime, right: std::time::Duration) -> NaiveDateTime {
    left - right
}
pub fn compiled_scale(left: TimeDelta, right: i32) -> TimeDelta {
    left * right
}
