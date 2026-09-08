// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::iter::FusedIterator;

/// Either of two iterators over the same item type, as a single type.
///
/// A function returning `impl Iterator<Item = T>` returns one concrete type, chosen by its body.
/// Guarding it writes `if a(n) { mutant } else { original }`, and the two arms of an `if` must
/// agree on a type — but `Empty<T>` and whatever the body built are two different types, so the
/// guard would not compile. Wrapping each arm in a variant of this enum gives both the same type
/// while keeping the originals intact inside it.
///
/// It is deliberately not a `Box<dyn Iterator<Item = T>>`, which would also unify the arms. A
/// trait object erases the auto traits, so a signature saying `+ Send` would stop compiling and
/// the mutant would be withdrawn as unviable. Two type parameters keep `Send`, `Sync`, `Clone`
/// and the rest, because the compiler derives them from `A` and `B` exactly as it did before.
/// Nothing is allocated and nothing is dynamically dispatched.
///
/// ```rust
/// use gamma_rt::Either;
///
/// fn counted(empty: bool) -> impl Iterator<Item = u32> {
///     if empty {
///         Either::L(core::iter::empty())
///     } else {
///         Either::R(0..3)
///     }
/// }
///
/// assert_eq!(counted(false).sum::<u32>(), 3);
/// assert_eq!(counted(true).sum::<u32>(), 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Either<A, B> {
    /// The mutant's iterator.
    L(A),

    /// The original iterator.
    R(B),
}

impl<T, A: Iterator<Item = T>, B: Iterator<Item = T>> Iterator for Either<A, B> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        match self {
            Self::L(a) => a.next(),
            Self::R(b) => b.next(),
        }
    }

    /// Forwarded rather than left at the default, because a caller that sizes a buffer from it
    /// would otherwise behave differently under instrumentation than it does without.
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::L(a) => a.size_hint(),
            Self::R(b) => b.size_hint(),
        }
    }
}

/// Kept so that a signature promising a reversible iterator still compiles once wrapped.
impl<T, A: DoubleEndedIterator<Item = T>, B: DoubleEndedIterator<Item = T>> DoubleEndedIterator for Either<A, B> {
    #[inline]
    fn next_back(&mut self) -> Option<T> {
        match self {
            Self::L(a) => a.next_back(),
            Self::R(b) => b.next_back(),
        }
    }
}

/// Kept for the same reason as [`DoubleEndedIterator`]: the bound appears in signatures, and
/// `size_hint` above is already exact whenever both sides are.
impl<T, A: ExactSizeIterator<Item = T>, B: ExactSizeIterator<Item = T>> ExactSizeIterator for Either<A, B> {}

/// Kept for the same reason, and sound because neither side resumes after returning `None`.
impl<T, A: FusedIterator<Item = T>, B: FusedIterator<Item = T>> FusedIterator for Either<A, B> {}
