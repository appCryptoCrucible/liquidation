//! Fixed-point arithmetic for protocol health math (GUIDE 00 §2).
//!
//! Every operation that can lose precision names its rounding direction in its
//! name (`_down` = floor, `_up` = ceil). There is no unrounded multiply or
//! divide: the inner `U256` of [`Ray`] and [`Wad`] is private, so the only way
//! to combine two values is through a method that states the direction.
//!
//! The single primitive is [`mul_div`]: `a · b / denom` through a 512-bit
//! intermediate, rounded as requested. Everything else is a thin wrapper.
//!
//! All arithmetic is checked. Overflow returns [`FixedError::Overflow`]; it is
//! never wrapped and never panics. The requirement is bit-identical agreement
//! with the deployed contract — the direction at each call site is read out of
//! the protocol source, not chosen conservatively (GUIDE 00 §2).

use alloy_primitives::{uint, U256, U512};

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests;

/// `1e27`. Aave's `RAY`.
pub const RAY: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
/// `1e18`.
pub const WAD: U256 = uint!(1_000_000_000_000_000_000_U256);
/// `RAY / WAD = 1e9`. Aave's `WAD_RAY_RATIO`.
pub const WAD_RAY_RATIO: U256 = uint!(1_000_000_000_U256);

/// Rounding direction of a lossy operation. Every lossy operation takes one;
/// there is no default and no unrounded form.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Toward zero (floor). Solidity's plain `/`.
    Down,
    /// Away from zero (ceil).
    Up,
}

/// Fixed-point failure. Fieldless: constructing one never allocates.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FixedError {
    /// The exact result exceeds `U256::MAX` (or `u128::MAX` for [`RayU128`]).
    #[error("fixed-point overflow")]
    Overflow,
    /// The exact result is negative.
    #[error("fixed-point underflow")]
    Underflow,
    /// Denominator was zero.
    #[error("fixed-point division by zero")]
    DivisionByZero,
}

/// `a · b / denom`, computed exactly in 512 bits and rounded in `rounding`.
///
/// Errors: `denom == 0` → [`FixedError::DivisionByZero`]; a rounded result
/// above `U256::MAX` → [`FixedError::Overflow`]. Never panics on any input.
#[inline]
pub fn mul_div(a: U256, b: U256, denom: U256, rounding: Rounding) -> Result<U256, FixedError> {
    if denom.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    let prod: U512 = a.widening_mul(b);
    let (q, r) = prod.div_rem(U512::from(denom));
    let q = match rounding {
        Rounding::Down => q,
        Rounding::Up if r.is_zero() => q,
        Rounding::Up => q.checked_add(U512::ONE).ok_or(FixedError::Overflow)?,
    };
    U256::checked_from_limbs_slice(q.as_limbs()).ok_or(FixedError::Overflow)
}

macro_rules! fixed_newtype {
    ($(#[$doc:meta])* $name:ident, $one:expr) => {
        $(#[$doc])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
        #[repr(transparent)]
        pub struct $name(U256);

        impl $name {
            /// Zero.
            pub const ZERO: Self = Self(U256::ZERO);
            /// One unit (`1.0`) at this scale.
            pub const ONE: Self = Self($one);

            /// Wrap a chain-decoded raw value. No scaling is applied.
            #[inline]
            #[must_use]
            pub const fn from_raw(raw: U256) -> Self {
                Self(raw)
            }

            /// The raw scaled integer, for ABI encoding and storage.
            #[inline]
            #[must_use]
            pub const fn raw(self) -> U256 {
                self.0
            }

            /// `floor(self · rhs / ONE)`.
            #[inline]
            pub fn mul_down(self, rhs: Self) -> Result<Self, FixedError> {
                mul_div(self.0, rhs.0, $one, Rounding::Down).map(Self)
            }

            /// `ceil(self · rhs / ONE)`.
            #[inline]
            pub fn mul_up(self, rhs: Self) -> Result<Self, FixedError> {
                mul_div(self.0, rhs.0, $one, Rounding::Up).map(Self)
            }

            /// `floor(self · ONE / rhs)`.
            #[inline]
            pub fn div_down(self, rhs: Self) -> Result<Self, FixedError> {
                mul_div(self.0, $one, rhs.0, Rounding::Down).map(Self)
            }

            /// `ceil(self · ONE / rhs)`.
            #[inline]
            pub fn div_up(self, rhs: Self) -> Result<Self, FixedError> {
                mul_div(self.0, $one, rhs.0, Rounding::Up).map(Self)
            }

            /// Exact sum; [`FixedError::Overflow`] above `U256::MAX`.
            #[inline]
            pub fn checked_add(self, rhs: Self) -> Result<Self, FixedError> {
                self.0.checked_add(rhs.0).map(Self).ok_or(FixedError::Overflow)
            }

            /// Exact difference; [`FixedError::Underflow`] below zero.
            #[inline]
            pub fn checked_sub(self, rhs: Self) -> Result<Self, FixedError> {
                self.0.checked_sub(rhs.0).map(Self).ok_or(FixedError::Underflow)
            }
        }
    };
}

fixed_newtype! {
    /// `1e27` fixed point (Aave `RAY`). Indices, rates, and Aave health factors.
    Ray, RAY
}

fixed_newtype! {
    /// `1e18` fixed point.
    Wad, WAD
}

impl Ray {
    /// `floor(self / 1e9)`. Infallible: the result never exceeds the input.
    #[inline]
    #[must_use]
    pub fn to_wad_down(self) -> Wad {
        // `WAD_RAY_RATIO` is a non-zero constant, so `div_rem` cannot panic.
        Wad(self.0.div_rem(WAD_RAY_RATIO).0)
    }

    /// `ceil(self / 1e9)`. The result never exceeds the input, so `Overflow`
    /// is unreachable for any `U256`; the increment is checked regardless.
    #[inline]
    pub fn to_wad_up(self) -> Result<Wad, FixedError> {
        let (q, r) = self.0.div_rem(WAD_RAY_RATIO);
        if r.is_zero() {
            Ok(Wad(q))
        } else {
            q.checked_add(U256::ONE)
                .map(Wad)
                .ok_or(FixedError::Overflow)
        }
    }
}

impl Wad {
    /// `self · 1e9`, lossless. [`FixedError::Overflow`] if the widened value
    /// does not fit `U256`.
    #[inline]
    pub fn to_ray_exact(self) -> Result<Ray, FixedError> {
        self.0
            .checked_mul(WAD_RAY_RATIO)
            .map(Ray)
            .ok_or(FixedError::Overflow)
    }
}

/// Storage width for values the chain itself holds as `uint128` RAY (Aave
/// `liquidityIndex`, `variableBorrowIndex`, rates). Widened to [`Ray`] at the
/// arithmetic boundary; never stored wider than the chain stores it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct RayU128(u128);

impl RayU128 {
    /// Wrap a chain-decoded raw `uint128` RAY value.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// The raw scaled integer.
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u128 {
        self.0
    }
}

impl From<RayU128> for Ray {
    /// Lossless widening.
    #[inline]
    fn from(v: RayU128) -> Self {
        Self(U256::from(v.0))
    }
}

impl TryFrom<Ray> for RayU128 {
    type Error = FixedError;

    /// [`FixedError::Overflow`] above `u128::MAX`. The chain cannot produce
    /// such a value, so an `Err` here is a bug upstream, not a market state.
    #[inline]
    fn try_from(r: Ray) -> Result<Self, FixedError> {
        u128::try_from(r.0)
            .map(Self)
            .map_err(|_| FixedError::Overflow)
    }
}
