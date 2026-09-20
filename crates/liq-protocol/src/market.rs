//! `MarketRow` — the per-`(protocol, market, slot)` table (GUIDE 02 §3).
//!
//! One flat row per reserve. The row is a **protocol-neutral header** the
//! store, the router and the conformance harness read, followed by an
//! **opaque body** only the owning adapter interprets through a `Pod` view
//! ([`MarketRow::body`] / [`MarketRow::body_mut`]). Shape fixed by WP 04A
//! at the first real adapter: GUIDE 02's named accounting fields
//! (`supply_index`, `addExRate`, …) do not exist on Aave V4 — collateral
//! value there is a share conversion over eleven hub-asset accounting words
//! (`AssetLogic.totalAddedAssets`), and no generic reader ever consumed them
//! (`docs/coverage/aave-v4-rounding.md` §1, STATE.md 04A).
//!
//! Four cache lines, every row on a line boundary. The header is the first
//! 16 bytes of line 0; an adapter puts what its `health()` reads for a
//! debt-only reserve in the rest of line 0 so that side costs one line.
//! Widening the row past 256 bytes fails the const asserts below (TESTING
//! §4 mutation #16) and the WAL frame cap in `liq-state`.

use bytemuck::{Pod, Zeroable};
use liq_types::{AssetId, MarketId};

use crate::error::ProtocolError;

/// Price feed identity (GUIDE 06). Interned `u16`; the side table is
/// `liq-oracle`'s (WP 06A-1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub struct FeedId(pub u16);

/// Reserve flags (GUIDE 02 §3): `frozen | paused | siloed | isolated |
/// unpriced`. Hand-rolled: `bitflags` is not a workspace dependency and five
/// bits do not justify one.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Pod, Zeroable)]
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
    /// The reserve's price source is not the one the feed registry pins
    /// (GUIDE 06 §2, 06A-1 carry-forward), or its underlying is not in the
    /// registry at all. Fail closed: `health()` on a position holding the
    /// slot is `Err(OracleSourceMismatch)`, never a number.
    pub const UNPRICED: Self = Self(1 << 4);

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

/// One reserve of one market: neutral header plus adapter-owned body.
///
/// Layout is `#[repr(C, align(64))]`, 256 bytes, no implicit padding.
/// `Pod`/`Zeroable` (WP 02B) so the snapshot zero-copies the column via
/// `bytemuck::from_bytes` after `fs::read` (D59).
///
/// | line | bytes | fields |
/// |---|---|---|
/// | 0 | 0..16 | header: `asset`, `price_feed`, `decimals`, `flags`, `hub_slot`, `hub_market`, `last_update` |
/// | 0 | 16..64 | `body[0..3]` |
/// | 1–3 | 64..256 | `body[3..15]` |
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C, align(64))]
pub struct MarketRow {
    /// Global asset (GUIDE 00: one id per token across protocols).
    pub asset: AssetId,
    /// Price feed the engine reads for `asset` (GUIDE 06).
    pub price_feed: FeedId,
    /// Token decimals, from the protocol's own listing event — never a
    /// registry guess.
    pub decimals: u8,
    /// Neutral flags every reader may consult (`PAUSED` blocks a quote).
    /// The adapter recomputes them from its body whenever a source bit
    /// changes, so they are derived, never the only copy.
    pub flags: MarketFlags,
    /// Slot of the row this one denormalises from (Aave V4: the hub asset
    /// id) inside `hub_market`; meaningless when `hub_market == NO_HUB`.
    pub hub_slot: u16,
    /// `MarketId.0` of the upstream market whose accrual fans out into this
    /// row, or [`MarketRow::NO_HUB`]. Raw `u32` (not `MarketId`) so the row
    /// stays `Pod`.
    pub hub_market: u32,
    /// Chain time (seconds) of the last index write. `u32` is exact until
    /// 2106; V4 stores `uint40` — the adapter errors past `u32::MAX`.
    pub last_update: u32,
    /// Adapter-owned bytes, read through [`MarketRow::body`].
    pub body: [u128; MarketRow::BODY_CELLS],
}

impl MarketRow {
    /// Body length in `u128` cells: 256 − 16 header bytes.
    pub const BODY_CELLS: usize = 15;
    /// `hub_market` value of a row that is its own source of truth.
    pub const NO_HUB: u32 = u32::MAX;

    /// A row with only the neutral header set and a zero body. What every
    /// non-adapter fixture needs; adapters fill the body afterwards.
    #[must_use]
    pub const fn blank(asset: AssetId, decimals: u8) -> Self {
        Self {
            asset,
            price_feed: FeedId(0),
            decimals,
            flags: MarketFlags::NONE,
            hub_slot: 0,
            hub_market: Self::NO_HUB,
            last_update: 0,
            body: [0; Self::BODY_CELLS],
        }
    }

    /// The body as the adapter's `Pod` struct. `T` must be at most 240 bytes
    /// and at most 16-aligned; otherwise [`ProtocolError::BodyLayout`].
    #[inline]
    pub fn body<T: Pod>(&self) -> Result<&T, ProtocolError> {
        let bytes = bytemuck::bytes_of(&self.body);
        bytes
            .get(..core::mem::size_of::<T>())
            .and_then(|b| bytemuck::try_from_bytes(b).ok())
            .ok_or(ProtocolError::BodyLayout)
    }

    /// Mutable typed view of the body; same bounds as [`MarketRow::body`].
    #[inline]
    pub fn body_mut<T: Pod>(&mut self) -> Result<&mut T, ProtocolError> {
        let bytes = bytemuck::bytes_of_mut(&mut self.body);
        bytes
            .get_mut(..core::mem::size_of::<T>())
            .and_then(|b| bytemuck::try_from_bytes_mut(b).ok())
            .ok_or(ProtocolError::BodyLayout)
    }
}

// Four cache lines, on a line boundary, no implicit padding.
const _: () = assert!(core::mem::size_of::<MarketRow>() == 256);
const _: () = assert!(core::mem::align_of::<MarketRow>() == 64);
const _: () = assert!(core::mem::offset_of!(MarketRow, body) == 16);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::{FeedId, MarketFlags, MarketRow};
    use bytemuck::{Pod, Zeroable};
    use liq_types::AssetId;

    /// Oracle: GUIDE 02 §3's contract restated for the 04A shape — the row
    /// is four whole lines, starts on a line boundary, and the header ends
    /// exactly where the body starts (mutation #16: grow a field → fails).
    #[test]
    fn row_is_four_lines_and_header_is_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<MarketRow>(), 256);
        assert_eq!(core::mem::align_of::<MarketRow>(), 64);
        assert_eq!(core::mem::offset_of!(MarketRow, asset), 0);
        assert_eq!(core::mem::offset_of!(MarketRow, price_feed), 2);
        assert_eq!(core::mem::offset_of!(MarketRow, decimals), 4);
        assert_eq!(core::mem::offset_of!(MarketRow, flags), 5);
        assert_eq!(core::mem::offset_of!(MarketRow, hub_slot), 6);
        assert_eq!(core::mem::offset_of!(MarketRow, hub_market), 8);
        assert_eq!(core::mem::offset_of!(MarketRow, last_update), 12);
        assert_eq!(core::mem::offset_of!(MarketRow, body), 16);
        assert_eq!(
            core::mem::size_of::<[u128; MarketRow::BODY_CELLS]>(),
            256 - 16
        );
    }

    #[derive(Copy, Clone, Pod, Zeroable)]
    #[repr(C)]
    struct Body {
        a: u128,
        b: u64,
        c: u32,
        d: u32,
    }

    #[derive(Copy, Clone, Pod, Zeroable)]
    #[repr(C)]
    struct TooWide([u128; MarketRow::BODY_CELLS + 1]);

    /// Oracle: `bytemuck` round trip — a write through the typed view is
    /// visible in the raw cells, and a view wider than the body is refused
    /// with the named variant rather than truncated.
    #[test]
    fn body_view_round_trips_and_refuses_oversize() {
        let mut r = MarketRow::blank(AssetId(7), 18);
        assert_eq!(r.hub_market, MarketRow::NO_HUB);
        assert_eq!(r.price_feed, FeedId(0));
        assert_eq!(r.flags, MarketFlags::NONE);
        {
            let b: &mut Body = r.body_mut().unwrap();
            b.a = u128::MAX - 1;
            b.b = 5;
            b.c = 6;
            b.d = 7;
        }
        assert_eq!(r.body[0], u128::MAX - 1);
        let b: &Body = r.body().unwrap();
        assert_eq!((b.b, b.c, b.d), (5, 6, 7));
        assert_eq!(
            r.body::<TooWide>().err(),
            Some(crate::ProtocolError::BodyLayout)
        );
        assert_eq!(
            r.body_mut::<TooWide>().err(),
            Some(crate::ProtocolError::BodyLayout)
        );
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
