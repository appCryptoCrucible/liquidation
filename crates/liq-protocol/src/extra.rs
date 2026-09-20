//! `PositionExtraRepr` — the fixed-size, protocol-specific per-position state
//! (GUIDE 02 §4), a union expressed through `bytemuck` so the crate keeps
//! `#![forbid(unsafe_code)]`.
//!
//! Each adapter defines its own `Pod` view (`AaveV4Extra { risk_premium,
//! premium_accrued, premium_last_update }`, `AaveV3Extra { emode, isolated,
//! collateral_mask }`, `CompoundV2Extra { borrow_index_snapshot }`, …) and
//! reads/writes it through [`PositionExtraRepr::view`] /
//! [`PositionExtraRepr::view_mut`]. The adapter asserts at compile time that
//! its view fits: `const _: () = assert!(size_of::<MyExtra>() <=
//! PositionExtraRepr::SIZE)`.
//!
//! **Sizing.** 64 bytes = one cache line, from GUIDE 02 §4's worst case
//! (V4: a RAY premium rate, a `u128` accrued premium and a `u32` timestamp,
//! padded to 16-byte alignment). GUIDE 01 §7 says to size this once from the
//! worst protocol after GUIDE 15's scoping rather than grow it incrementally;
//! widening is a decision-log entry because it adds a line to every
//! `health()` (GUIDE 02 acceptance budget).

use bytemuck::{Pod, Zeroable};

use crate::error::{ProtocolError, Result};

/// 64 opaque bytes, 16-byte aligned (so views containing `u128`/`RayU128`
/// are placeable). `Pod`: memory-mappable by the snapshot, comparable
/// byte-for-byte by the undo round-trip test.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub struct PositionExtraRepr([u128; 4]);

impl PositionExtraRepr {
    /// Size in bytes.
    pub const SIZE: usize = 64;
    /// All-zero representation (a fresh position).
    pub const ZERO: Self = Self([0; 4]);

    /// Typed read of the leading `size_of::<T>()` bytes.
    /// [`ProtocolError::ExtraLayout`] when `T` is larger than `SIZE` or needs
    /// more than 16-byte alignment. Never panics.
    #[inline]
    pub fn view<T: Pod>(&self) -> Result<&T> {
        let bytes = bytemuck::bytes_of(self)
            .get(..core::mem::size_of::<T>())
            .ok_or(ProtocolError::ExtraLayout)?;
        bytemuck::try_from_bytes(bytes).map_err(|_| ProtocolError::ExtraLayout)
    }

    /// Typed write access to the leading `size_of::<T>()` bytes.
    #[inline]
    pub fn view_mut<T: Pod>(&mut self) -> Result<&mut T> {
        let bytes = bytemuck::bytes_of_mut(self)
            .get_mut(..core::mem::size_of::<T>())
            .ok_or(ProtocolError::ExtraLayout)?;
        bytemuck::try_from_bytes_mut(bytes).map_err(|_| ProtocolError::ExtraLayout)
    }
}

/// GUIDE 02 acceptance: `size_of::<PositionExtraRepr>()` asserted at compile
/// time; one cache line; alignment admits `u128` fields in views.
const _: () = {
    assert!(core::mem::size_of::<PositionExtraRepr>() == PositionExtraRepr::SIZE);
    assert!(PositionExtraRepr::SIZE == 64);
    assert!(core::mem::align_of::<PositionExtraRepr>() == 16);
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::PositionExtraRepr;
    use crate::error::ProtocolError;
    use bytemuck::{Pod, Zeroable};

    /// The V4 shape GUIDE 02 §4 sizes the union from. `risk_premium` is the
    /// raw `uint128` RAY (`RayU128::raw()`); `RayU128` itself is not yet
    /// `Pod` (a `liq-types` change, see `MarketRow` docs).
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    struct V4Like {
        risk_premium: u128,
        premium_accrued: u128,
        premium_last_update: u32,
        _pad: [u8; 12],
    }

    /// Larger than the representation: must be refused, not truncated.
    #[derive(Copy, Clone, Debug, Pod, Zeroable)]
    #[repr(C)]
    struct TooBig([u128; 5]);

    /// Oracle: `bytemuck` layout guarantees — a write through the typed view
    /// is read back identically, and the zero repr views as the zero struct.
    #[test]
    fn typed_view_round_trip() {
        let mut repr = PositionExtraRepr::ZERO;
        assert_eq!(*repr.view::<V4Like>().unwrap(), V4Like::zeroed());
        let v = V4Like {
            risk_premium: 7,
            premium_accrued: u128::MAX,
            premium_last_update: 1_700_000_000,
            _pad: [0; 12],
        };
        *repr.view_mut::<V4Like>().unwrap() = v;
        assert_eq!(*repr.view::<V4Like>().unwrap(), v);
        assert_ne!(repr, PositionExtraRepr::ZERO);
    }

    /// Negative: a view wider than the representation is an error, never a
    /// panic or a partial read.
    #[test]
    fn oversized_view_is_refused() {
        let repr = PositionExtraRepr::ZERO;
        assert_eq!(
            repr.view::<TooBig>().map(|_| ()),
            Err(ProtocolError::ExtraLayout)
        );
    }
}
