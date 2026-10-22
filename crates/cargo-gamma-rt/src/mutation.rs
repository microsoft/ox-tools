// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Evaluates and drops a call's result before producing its default, retaining its expected type.
pub fn default_after<T: Default>(value: T) -> T {
    drop(value);
    T::default()
}

/// A standard propagation container whose failure can be replaced by a panic.
pub trait PanicOnFailure {
    /// Returns the original success, or panics without formatting the failure.
    #[must_use]
    fn panic_on_failure(self) -> Self;
}

impl<T, E> PanicOnFailure for Result<T, E> {
    #[expect(
        clippy::match_wild_err_arm,
        reason = "the mutation deliberately panics on any error without requiring Debug"
    )]
    fn panic_on_failure(self) -> Self {
        match self {
            Ok(value) => Ok(value),
            Err(_error) => panic!("gamma: propagation replaced by panic"),
        }
    }
}

impl<T> PanicOnFailure for Option<T> {
    fn panic_on_failure(self) -> Self {
        match self {
            Some(value) => Some(value),
            None => panic!("gamma: propagation replaced by panic"),
        }
    }
}

/// Preserves the container type, including error inference and `?` conversions.
#[inline]
pub fn panic_on_failure<T: PanicOnFailure>(value: T) -> T {
    value.panic_on_failure()
}

/// Primitive numeric operations retaining the expected type of their operand.
pub trait Boundary {
    /// Adds one in the primitive's own arithmetic.
    #[must_use]
    fn increment(self) -> Self;
    /// Subtracts one in the primitive's own arithmetic.
    #[must_use]
    fn decrement(self) -> Self;
}

macro_rules! boundary {
    ($($ty:ty => $one:expr),* $(,)?) => {$(
        impl Boundary for $ty {
            fn increment(self) -> Self { self + $one }
            fn decrement(self) -> Self { self - $one }
        }
    )*};
}

boundary!(
    u8 => 1, u16 => 1, u32 => 1, u64 => 1, u128 => 1, usize => 1,
    i8 => 1, i16 => 1, i32 => 1, i64 => 1, i128 => 1, isize => 1,
    f32 => 1.0, f64 => 1.0,
);

/// Adds one while propagating the expected result type to `value`.
#[inline]
pub fn increment<T: Boundary>(value: T) -> T {
    value.increment()
}

/// Subtracts one while propagating the expected result type to `value`.
#[inline]
pub fn decrement<T: Boundary>(value: T) -> T {
    value.decrement()
}
