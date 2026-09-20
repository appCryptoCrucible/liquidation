//! `StateWriter` — the only surface through which an adapter mutates state
//! (D46). `liq-state`'s `StateStore` implements it; `Protocol::apply_log`
//! takes `&mut dyn StateWriter`, which is what removes the 01 → 02 crate
//! dependency.
//!
//! **Contract for implementors** (GUIDE 02 §5): every setter records its
//! inverse in the undo ring before writing, so `undo(apply(log))` is
//! byte-identical (conformance check 6). Because adapters cannot reach the
//! columns any other way, no adapter "fast path" can skip an undo record — the
//! failure mode GUIDE 02 warns about is structurally excluded.
//!
//! `config` (the [`AssetMask`](crate::AssetMask) of slots with a balance) is
//! **maintained by the store** inside `set_supply`/`set_debt`, never set by an
//! adapter; a forgotten mask bit would silently drop a reserve from
//! `health()`.
//!
//! It is a `&mut` on one thread — no lock behind it (RUST-CONVENTIONS §1).

use liq_types::{MarketId, PositionId, PositionKey};

use crate::error::Result;
use crate::extra::PositionExtraRepr;
use crate::market::{MarketRow, MarketSlot};

/// Mutable state surface for `Protocol::apply_log` / `backfill`.
pub trait StateWriter {
    /// Dense id for `key`, assigning one on first sight (`UndoOp::Created`).
    /// Ids are never reused or removed (GUIDE 02 §1).
    fn intern(&mut self, key: &PositionKey) -> Result<PositionId>;

    /// Supply shares of `pos` in `slot`.
    fn supply(&self, pos: PositionId, slot: u16) -> Result<u128>;
    /// Debt shares of `pos` in `slot`.
    fn debt(&self, pos: PositionId, slot: u16) -> Result<u128>;
    /// Set supply shares; updates `config` and pushes the inverse.
    fn set_supply(&mut self, pos: PositionId, slot: u16, shares: u128) -> Result<()>;
    /// Set debt shares; updates `config` and pushes the inverse.
    fn set_debt(&mut self, pos: PositionId, slot: u16, shares: u128) -> Result<()>;

    /// Protocol-specific per-position state.
    fn extra(&self, pos: PositionId) -> Result<&PositionExtraRepr>;
    /// Replace it wholesale (64-byte copy; the inverse is the previous copy).
    fn set_extra(&mut self, pos: PositionId, extra: PositionExtraRepr) -> Result<()>;

    /// Protocol-specific per-position **per-slot** state
    /// (`PositionRef::slot_extra`). Zero for a slot never written.
    fn slot_extra(&self, pos: PositionId, slot: u16) -> Result<&PositionExtraRepr>;
    /// Replace one slot's extra wholesale (64-byte copy; the inverse is the
    /// previous copy). Does **not** touch `config`: a slot with only extra
    /// state and no balance is not a held slot.
    fn set_slot_extra(
        &mut self,
        pos: PositionId,
        slot: u16,
        extra: PositionExtraRepr,
    ) -> Result<()>;

    /// Number of interned positions; ids are dense `0..len`. For the rare
    /// governance fan-out that must visit every position of a market
    /// (a dynamic-config value change).
    fn positions_len(&self) -> u32;
    /// Natural key of an interned position.
    fn position_key(&self, pos: PositionId) -> Result<&PositionKey>;

    /// One market row.
    fn market(&self, at: MarketSlot) -> Result<&MarketRow>;
    /// All rows of `market`, indexed by slot — the hub fan-out scan reads
    /// `hub_market`/`hub_slot` across these.
    fn markets(&self, market: MarketId) -> Result<&[MarketRow]>;
    /// Replace a row wholesale (256-byte copy; the inverse is the previous
    /// row). Adapters read-modify-write.
    fn set_market(&mut self, at: MarketSlot, row: MarketRow) -> Result<()>;
    /// Append a reserve to `market` (reserve initialisation, config load).
    /// Returns its slot. Fails with `SlotOutOfRange` past
    /// `AssetMask::MAX_SLOTS`.
    fn push_market(&mut self, market: MarketId, row: MarketRow) -> Result<MarketSlot>;
}
