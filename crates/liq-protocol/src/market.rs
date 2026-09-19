//! `MarketRow` — the per-`(protocol, market, slot)` table (GUIDE 02 §3).
//!
//! One flat row per reserve with the hub-level accounting denormalised into it
//! (Aave V4 hub/spoke; `hub_ref` lets a hub accrual fan out to every row that
//! points at it). Two cache lines, every row on a line boundary; line 0 holds
//! what `health()` reads to project balances (indices and rates), line 1 the
//! rest. Field widths mirror the chain's storage widths (`RayU128` for
//! `uint128` RAY indices; bps-scaled `u16`/`u32` for governance parameters —
//! GUIDE 04 confirms each width from source and may widen **only** with that
//! evidence). The `const` asserts below are what stop the row silently growing
//! to a third line (TESTING §4 mutation #16).

use liq_types::{AssetId, MarketId, RayU128};

/// Price feed identity (GUIDE 06). Interned `u16`; the side table is
/// `liq-oracle`'s (WP 06A-1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct FeedId(pub u16);

/// Reserve flags (GUIDE 02 §3): `frozen | paused | siloed | isolated`.
/// Hand-rolled: `bitflags` is not a workspace dependency and four bits do not
/// justify one.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct MarketFlags(pub u8);

impl MarketFlags {
    /// No flag set.
    pub const NONE: Self = Self(0);
    /// Reserve frozen: no new supply/borrow; liquidation still allowed.
    pub const FROZEN: Self = Self(1 << 0);
    /// Reserve paused: every action including liquidation reverts.
    pub const PAUSED: Self = Self(1 << 1);
    /// Siloed borrowing (Aave V3): the only borrowable reserve for the user.
    pub const SILOED: Self = Self(1 << 2);
    /// Isolation mode collateral (Aave V3) with a debt ceiling.
    pub const ISOLATED: Self = Self(1 << 3);

    /// `true` when every bit of `other` is set in `self`.
    #[inline]
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Address of one [`MarketRow`]: a slot inside a protocol-scoped market. This
/// is the unit `DirtySet::MarketAccrual`/`MarketReprice` and `StateWriter`
/// address — one row is one column of balances (GUIDE 02 §2).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarketSlot {
    pub market: MarketId,
    pub slot: u16,
}

/// One reserve of one market with its hub accounting denormalised in.
///
/// Layout is `#[repr(C, align(64))]`, 128 bytes, no implicit padding: the
/// trailing `_pad` is explicit so every byte is a field and the row can become
/// `bytemuck::Pod` for the memory-mapped snapshot (GUIDE 02 §7) by adding the
/// derive alone. That derive is not here yet because it needs `Pod` on
/// `liq_types::{RayU128, AssetId}` — a `liq-types` change outside this WP
/// (flagged in the 01 return note).
///
/// | line | bytes | fields |
/// |---|---|---|
/// | 0 | 0..64 | `supply_index`, `debt_index`, `supply_rate`, `debt_rate` |
/// | 1 | 64..128 | `dust_floor`, `last_update`, `target_hf`, ids, thresholds, liquidation config, flags |
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MarketRow {
    // ---- line 0: read by every health()/time_to_cross() projection ---------
    /// V4 `addExRate`; V3 `liquidityIndex`. RAY, `uint128` on chain.
    pub supply_index: RayU128,
    /// V4 `drawnIndex`; V3 `variableBorrowIndex`. RAY, `uint128` on chain.
    pub debt_index: RayU128,
    /// Current supply rate, RAY per second (V3 `currentLiquidityRate`).
    pub supply_rate: RayU128,
    /// Current borrow rate, RAY per second (V3 `currentVariableBorrowRate`).
    pub debt_rate: RayU128,

    // ---- line 1 -------------------------------------------------------------
    /// Minimum remaining debt after a liquidation (protocol dust rule), in the
    /// reserve's raw underlying units. `0` when the protocol has no dust rule.
    pub dust_floor: u128,
    /// Unix seconds of the last index update (`lastUpdateTimestamp`).
    pub last_update: u32,
    /// V4 target health factor after liquidation, 1e4-scaled. `0` when the
    /// protocol uses a fixed close factor instead.
    pub target_hf: u32,
    /// Hub asset this row denormalises; `u16::MAX` for protocols without a
    /// hub. A hub accrual updates every row sharing this value.
    pub hub_ref: u16,
    /// Liquidation threshold, bps (V4 `collateralRisk`, V3 `liqThreshold`).
    pub liq_threshold: u16,
    /// Loan-to-value, bps.
    pub ltv: u16,
    /// Price feed of `asset` for this protocol (GUIDE 06 §2).
    pub price_feed: FeedId,
    /// **Global** asset id — the `PriceVector` index.
    pub asset: AssetId,
    /// Maximum liquidation bonus, bps.
    pub max_liq_bonus: u16,
    /// V4: health factor at and below which the bonus saturates, 1e4-scaled.
    pub hf_for_max_bonus: u16,
    /// V4: bonus slope/base parameter, 1e4-scaled (GUIDE 04 §4).
    pub liq_bonus_factor: u16,
    /// Underlying decimals.
    pub decimals: u8,
    /// Reserve flags.
    pub flags: MarketFlags,
    /// Explicit tail padding to the 128-byte, two-line row. Always zero.
    pub _pad: [u8; 22],
}

impl MarketRow {
    /// Bytes of real fields — `size_of::<MarketRow>()` minus `_pad`.
    pub const PAYLOAD_BYTES: usize = 106;
}

/// Row size and alignment are load-bearing (GUIDE 02 §3 acceptance;
/// TESTING §4 mutation #16). Widening any field past these fails the build.
const _: () = {
    assert!(core::mem::size_of::<MarketRow>() == 128);
    assert!(core::mem::align_of::<MarketRow>() == 64);
    assert!(MarketRow::PAYLOAD_BYTES < 128);
    // The explicit pad accounts for every byte not covered by a field, so a
    // future derive(Pod) is sound (no implicit padding).
    assert!(
        MarketRow::PAYLOAD_BYTES + 22 == core::mem::size_of::<MarketRow>(),
        "field bytes + explicit pad must equal the row size"
    );
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{FeedId, MarketFlags, MarketRow};
    use core::mem::{offset_of, size_of};

    /// Oracle: GUIDE 02 §3 — indices and rates on cache line 0, everything
    /// else on line 1; the row is exactly two lines.
    #[test]
    fn hot_fields_share_line_zero() {
        assert!(offset_of!(MarketRow, supply_index) < 64);
        assert!(offset_of!(MarketRow, debt_index) < 64);
        assert!(offset_of!(MarketRow, supply_rate) < 64);
        assert!(offset_of!(MarketRow, debt_rate) < 64);
        assert_eq!(offset_of!(MarketRow, dust_floor), 64);
        assert_eq!(size_of::<MarketRow>(), 128);
    }

    /// Oracle: GUIDE 02 §3 field widths (sum = 106 bytes); the assertion is
    /// on the *sum of declared widths*, independent of `size_of`.
    #[test]
    fn payload_bytes_match_declared_widths() {
        // five u128 · two u32 · eight u16 · two u8
        let declared = 16 * 5 + 4 * 2 + 2 * 8 + 2;
        assert_eq!(declared, MarketRow::PAYLOAD_BYTES);
        assert_eq!(size_of::<FeedId>(), 2);
        assert_eq!(size_of::<MarketFlags>(), 1);
    }

    /// Oracle: bit definitions. Negative: `PAUSED` is not implied by `FROZEN`.
    #[test]
    fn flags_contain() {
        let f = MarketFlags(MarketFlags::FROZEN.0 | MarketFlags::SILOED.0);
        assert!(f.contains(MarketFlags::FROZEN));
        assert!(f.contains(MarketFlags::SILOED));
        assert!(!f.contains(MarketFlags::PAUSED));
        assert!(MarketFlags::NONE.contains(MarketFlags::NONE));
    }
}
