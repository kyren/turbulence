use std::cmp::Ordering;

use num_traits::ops::wrapping::WrappingSub;

/// Compares two values that wrap around across their wrap point.
///
/// A value `a` is considered less than `b` if it is faster to get to `a` from `b` by going left
/// than by going right, and `a` is considered greater than `b` if the opposite is true.
///
/// This is useful for version numbers which may rarely wrap around, but only version numbers
/// that are very close together will ever be compared.
pub fn wrap_cmp<I>(a: &I, b: &I) -> Ordering
where
    I: Ord + WrappingSub,
{
    b.wrapping_sub(a).cmp(&a.wrapping_sub(b))
}

pub fn wrap_lt<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingSub,
{
    wrap_cmp(a, b) == Ordering::Less
}

pub fn wrap_gt<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingSub,
{
    wrap_cmp(a, b) == Ordering::Greater
}

pub fn wrap_ge<I>(a: &I, b: &I) -> bool
where
    I: Ord + WrappingSub,
{
    wrap_cmp(a, b) != Ordering::Less
}
