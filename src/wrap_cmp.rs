use std::cmp::Ordering;

use num::traits::ops::wrapping::{WrappingAdd, WrappingSub};

/// Compares two values that wrap around across their wrap point.
///
/// A value `a` is considered less than `b` if it is faster to get to `a` from `b` by going left
/// than by going right, and `a` is considered greater than `b` if the opposite is true.
///
/// This is useful for version numbers which may rarely wrap around, but only version numbers
/// that are very close together will ever be compared.
///
/// Do not use this to implement `Ord` because it is not transitive.  For example:
/// ```
/// # use std::cmp::Ordering;
/// # use turbulence::wrap_cmp::wrap_cmp;
/// # fn main() {
/// assert_eq!(wrap_cmp(&1u8, &100u8), Ordering::Less);
/// assert_eq!(wrap_cmp(&100u8, &200u8), Ordering::Less);
/// assert_eq!(wrap_cmp(&200u8, &1u8), Ordering::Less);
/// # }
#[inline]
pub fn wrap_cmp<I>(a: &I, b: &I) -> Ordering
where
    I: Ord + WrappingAdd + WrappingSub,
{
    b.wrapping_sub(a).cmp(&a.wrapping_sub(b))
}

#[inline]
pub fn wrap_lt<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingAdd + WrappingSub,
{
    wrap_cmp(a, b) == Ordering::Less
}

#[inline]
pub fn wrap_gt<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingAdd + WrappingSub,
{
    wrap_cmp(a, b) == Ordering::Greater
}

#[inline]
pub fn wrap_le<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingAdd + WrappingSub,
{
    wrap_cmp(a, b) != Ordering::Greater
}

#[inline]
pub fn wrap_ge<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingAdd + WrappingSub,
{
    wrap_cmp(a, b) != Ordering::Less
}
