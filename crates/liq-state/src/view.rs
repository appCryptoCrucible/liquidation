//! Read views (GUIDE 02 §6).
//!
//! [`StateView`] borrows the store immutably and builds `PositionRef`s at one
//! chain time — the *target* block's time, so index projection inside
//! `health()` runs forward to the instant the liquidation would land. While a
//! view exists the borrow checker forbids any `&mut StateStore`: a reader on
//! the hot thread is consistent by construction, and it needs no lock because
//! nothing else can write.
//!
//! [`Overlay`] is a pending delta — mempool evaluation, simulation — authored
//! through the same [`StateWriter`] an adapter's `apply_log` drives, layered
//! on the store without touching it: the first write to a position or market
//! copies that position's rows / that market's rows into the overlay's own
//! pooled buffers, and every later read of it comes from the copy. Its
//! "undo" is [`Overlay::clear`]: the whole delta is discarded, so no journal
//! is kept (a mempool transaction is never reorged, it is dropped). Buffers
//! are pooled, so a reused overlay allocates only while it grows past its
//! previous high-water mark.

use std::collections::HashMap;

use liq_protocol::{
    AssetMask, BlockNum, MarketRow, MarketSlot, PositionExtraRepr, PositionRef, ProtocolError,
    StateWriter, Timestamp,
};
use liq_types::{MarketId, PositionId, PositionKey};

use crate::error::StateError;
use crate::store::StateStore;

/// Consistent read view: canonical state, optionally with a pending overlay.
#[derive(Copy, Clone)]
pub struct StateView<'a> {
    base: &'a StateStore,
    overlay: Option<&'a Overlay>,
    timestamp: Timestamp,
}

impl<'a> StateView<'a> {
    #[inline]
    pub(crate) fn new(
        base: &'a StateStore,
        overlay: Option<&'a Overlay>,
        timestamp: Timestamp,
    ) -> Self {
        Self {
            base,
            overlay,
            timestamp,
        }
    }

    /// Chain time every `PositionRef` from this view is evaluated at.
    #[inline]
    #[must_use]
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Block the view reflects (before the overlay's pending delta).
    #[inline]
    #[must_use]
    pub fn tip(&self) -> BlockNum {
        self.base.tip()
    }

    /// Positions addressable through this view.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.overlay.map_or(self.base.len(), |ov| {
            self.base.len().saturating_add(ov.new_keys.len())
        })
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The position, with the overlay's version of it and of its market's
    /// rows when present. Allocation-free.
    #[inline]
    pub fn position(&self, id: PositionId) -> Result<PositionRef<'a>, StateError> {
        let Some(ov) = self.overlay else {
            return self.base.position_ref(id, self.timestamp);
        };
        if let Some(p) = ov.position(id) {
            let markets = ov
                .rows(p.key.market)
                .or_else(|| self.base.rows(p.key.market))
                .ok_or(StateError::UnknownMarket(p.key.market))?;
            return Ok(PositionRef {
                id,
                key: &p.key,
                config: p.config,
                supply: &p.supply,
                debt: &p.debt,
                extra: &p.extra,
                slot_extra: &p.slot_extra,
                markets,
                timestamp: self.timestamp,
            });
        }
        let mut r = self.base.position_ref(id, self.timestamp)?;
        if let Some(rows) = ov.rows(r.key.market) {
            // Asymmetry (STATE.md 04A carry-forward 7): when the overlay
            // listed a reserve (`push_market`) this position's base columns
            // are one cell shorter than `markets`. That is sound: a listing
            // never sets a position bit, so `config` — the only index an
            // adapter iterates — is bounded by the base column length, and
            // `PositionRef::slot` refuses the extra slot with
            // `SlotOutOfRange` rather than reading past the column.
            r.markets = rows;
        }
        Ok(r)
    }

    /// Rows of `market`, overlay first.
    #[inline]
    pub fn markets(&self, market: MarketId) -> Result<&'a [MarketRow], StateError> {
        self.overlay
            .and_then(|ov| ov.rows(market))
            .or_else(|| self.base.rows(market))
            .ok_or(StateError::UnknownMarket(market))
    }
}

/// Overlay copy of one position.
struct OvPos {
    key: PositionKey,
    config: AssetMask,
    extra: PositionExtraRepr,
    supply: Vec<u128>,
    debt: Vec<u128>,
    slot_extra: Vec<PositionExtraRepr>,
}

impl OvPos {
    const fn empty() -> Self {
        Self {
            key: PositionKey {
                protocol: liq_types::ProtocolId(0),
                market: MarketId(0),
                user: alloy_primitives::Address::ZERO,
            },
            config: AssetMask::EMPTY,
            extra: PositionExtraRepr::ZERO,
            supply: Vec::new(),
            debt: Vec::new(),
            slot_extra: Vec::new(),
        }
    }

    /// Reset every column to `n` blank cells.
    fn blank(&mut self, n: usize) {
        self.supply.clear();
        self.supply.resize(n, 0);
        self.debt.clear();
        self.debt.resize(n, 0);
        self.slot_extra.clear();
        self.slot_extra.resize(n, PositionExtraRepr::ZERO);
    }
}

/// Pending delta over a [`StateStore`]. Build it with [`Overlay::writer`],
/// read it with [`StateStore::view_with`], reuse it with [`Overlay::clear`].
#[derive(Default)]
pub struct Overlay {
    /// Store state the delta was built on; a view over any other is refused.
    /// `tip` alone is not that state: every setter of a block runs at one
    /// tip, so a copy taken mid-block and a copy taken at its end share it.
    tip: BlockNum,
    base_len: usize,
    base_writes: u64,
    pos_index: HashMap<PositionId, usize>,
    /// Pool; the live entries are `pos[..pos_len]`.
    pos: Vec<OvPos>,
    pos_len: usize,
    /// Positions interned here (not in the base): id = `base_len + i`.
    new_keys: HashMap<PositionKey, PositionId>,
    mkt_index: HashMap<MarketId, usize>,
    /// Pool; live entries are `mkt[..mkt_len]`.
    mkt: Vec<Vec<MarketRow>>,
    mkt_len: usize,
}

impl Overlay {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Discard the delta, keeping every buffer for reuse.
    pub fn clear(&mut self) {
        self.pos_index.clear();
        self.pos_len = 0;
        self.new_keys.clear();
        self.mkt_index.clear();
        self.mkt_len = 0;
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos_len == 0 && self.mkt_len == 0
    }

    /// A writer that reads through `base` and writes here. An overlay with
    /// content built on another store state is refused, not extended.
    pub fn writer<'a>(&'a mut self, base: &'a StateStore) -> Result<OverlayWriter<'a>, StateError> {
        if self.is_empty() {
            self.tip = base.tip();
            self.base_len = base.len();
            self.base_writes = base.writes();
        } else {
            self.check_base(base)?;
        }
        Ok(OverlayWriter { base, ov: self })
    }

    pub(crate) fn check_base(&self, base: &StateStore) -> Result<(), StateError> {
        if self.tip != base.tip()
            || self.base_len != base.len()
            || self.base_writes != base.writes()
        {
            return Err(StateError::OverlayStale {
                overlay: self.tip,
                overlay_len: self.base_len,
                overlay_writes: self.base_writes,
                store: base.tip(),
                store_len: base.len(),
                store_writes: base.writes(),
            });
        }
        Ok(())
    }

    #[inline]
    fn position(&self, id: PositionId) -> Option<&OvPos> {
        self.pos.get(*self.pos_index.get(&id)?)
    }

    #[inline]
    fn rows(&self, market: MarketId) -> Option<&[MarketRow]> {
        self.mkt
            .get(*self.mkt_index.get(&market)?)
            .map(Vec::as_slice)
    }

    /// Next free pool slot for a position (reused or freshly pushed).
    fn alloc_pos(&mut self) -> usize {
        let i = self.pos_len;
        if i == self.pos.len() {
            self.pos.push(OvPos::empty());
        }
        self.pos_len = i.saturating_add(1);
        i
    }

    fn alloc_mkt(&mut self) -> usize {
        let i = self.mkt_len;
        if i == self.mkt.len() {
            self.mkt.push(Vec::new());
        }
        self.mkt_len = i.saturating_add(1);
        i
    }
}

/// [`StateWriter`] over an [`Overlay`]: reads fall through to the base store
/// until the overlay holds its own copy; writes go to the copy.
pub struct OverlayWriter<'a> {
    base: &'a StateStore,
    ov: &'a mut Overlay,
}

impl OverlayWriter<'_> {
    /// Slot count of `market` as this writer sees it.
    fn rows_len(&self, market: MarketId) -> Result<usize, ProtocolError> {
        self.ov
            .rows(market)
            .or_else(|| self.base.rows(market))
            .map(<[MarketRow]>::len)
            .ok_or(ProtocolError::UnknownMarket(market))
    }

    /// Pool index of `pos`'s overlay copy, made from the base on first touch.
    fn materialise(&mut self, pos: PositionId) -> Result<usize, ProtocolError> {
        if let Some(&i) = self.ov.pos_index.get(&pos) {
            return Ok(i);
        }
        let r = self
            .base
            .position_ref(pos, 0)
            .map_err(|_| ProtocolError::UnknownPosition(pos))?;
        // The overlay's copy of the market may have more slots than the base.
        let n = self.rows_len(r.key.market)?;
        let i = self.ov.alloc_pos();
        let p = self
            .ov
            .pos
            .get_mut(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        p.key = *r.key;
        p.config = r.config;
        p.extra = *r.extra;
        p.supply.clear();
        p.supply.extend_from_slice(r.supply);
        p.supply.resize(n, 0);
        p.debt.clear();
        p.debt.extend_from_slice(r.debt);
        p.debt.resize(n, 0);
        p.slot_extra.clear();
        p.slot_extra.extend_from_slice(r.slot_extra);
        p.slot_extra.resize(n, PositionExtraRepr::ZERO);
        self.ov.pos_index.insert(pos, i);
        Ok(i)
    }

    /// Pool index of `market`'s overlay rows, copied from the base on first
    /// touch; `create` admits a market the base does not have.
    fn materialise_market(
        &mut self,
        market: MarketId,
        create: bool,
    ) -> Result<usize, ProtocolError> {
        if let Some(&i) = self.ov.mkt_index.get(&market) {
            return Ok(i);
        }
        let base_rows = match (self.base.rows(market), create) {
            (Some(rows), _) => rows,
            (None, true) => &[],
            (None, false) => return Err(ProtocolError::UnknownMarket(market)),
        };
        let i = self.ov.alloc_mkt();
        let rows = self
            .ov
            .mkt
            .get_mut(i)
            .ok_or(ProtocolError::UnknownMarket(market))?;
        rows.clear();
        rows.extend_from_slice(base_rows);
        self.ov.mkt_index.insert(market, i);
        Ok(i)
    }

    fn set_column(
        &mut self,
        supply: bool,
        pos: PositionId,
        slot: u16,
        shares: u128,
    ) -> Result<(), ProtocolError> {
        let i = self.materialise(pos)?;
        let p = self
            .ov
            .pos
            .get_mut(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let at = MarketSlot {
            market: p.key.market,
            slot,
        };
        let (col, other) = if supply {
            (&mut p.supply, &p.debt)
        } else {
            (&mut p.debt, &p.supply)
        };
        let other_nonzero = *other
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(at))?
            != 0;
        let cell = col
            .get_mut(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(at))?;
        *cell = shares;
        p.config = if shares != 0 || other_nonzero {
            p.config
                .with(slot)
                .ok_or(ProtocolError::SlotOutOfRange(at))?
        } else {
            p.config.without(slot)
        };
        Ok(())
    }

    fn column(&self, supply: bool, pos: PositionId, slot: u16) -> Result<u128, ProtocolError> {
        let Some(p) = self.ov.position(pos) else {
            return if supply {
                self.base.supply(pos, slot)
            } else {
                self.base.debt(pos, slot)
            };
        };
        let col = if supply { &p.supply } else { &p.debt };
        col.get(usize::from(slot))
            .copied()
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: p.key.market,
                slot,
            }))
    }
}

impl StateWriter for OverlayWriter<'_> {
    fn intern(&mut self, key: &PositionKey) -> Result<PositionId, ProtocolError> {
        if let Some(id) = self.base.position_id(key) {
            return Ok(id);
        }
        if let Some(id) = self.ov.new_keys.get(key) {
            return Ok(*id);
        }
        let n = self.rows_len(key.market)?;
        let id = self
            .ov
            .base_len
            .checked_add(self.ov.new_keys.len())
            .and_then(|i| u32::try_from(i).ok())
            .map(PositionId)
            .ok_or(ProtocolError::Internal)?;
        let i = self.ov.alloc_pos();
        let p = self
            .ov
            .pos
            .get_mut(i)
            .ok_or(ProtocolError::UnknownPosition(id))?;
        p.key = *key;
        p.config = AssetMask::EMPTY;
        p.extra = PositionExtraRepr::ZERO;
        p.blank(n);
        self.ov.pos_index.insert(id, i);
        self.ov.new_keys.insert(*key, id);
        Ok(id)
    }

    fn supply(&self, pos: PositionId, slot: u16) -> Result<u128, ProtocolError> {
        self.column(true, pos, slot)
    }

    fn debt(&self, pos: PositionId, slot: u16) -> Result<u128, ProtocolError> {
        self.column(false, pos, slot)
    }

    fn set_supply(
        &mut self,
        pos: PositionId,
        slot: u16,
        shares: u128,
    ) -> Result<(), ProtocolError> {
        self.set_column(true, pos, slot, shares)
    }

    fn set_debt(&mut self, pos: PositionId, slot: u16, shares: u128) -> Result<(), ProtocolError> {
        self.set_column(false, pos, slot, shares)
    }

    fn extra(&self, pos: PositionId) -> Result<&PositionExtraRepr, ProtocolError> {
        match self.ov.position(pos) {
            Some(p) => Ok(&p.extra),
            None => self.base.extra(pos),
        }
    }

    fn set_extra(
        &mut self,
        pos: PositionId,
        extra: PositionExtraRepr,
    ) -> Result<(), ProtocolError> {
        let i = self.materialise(pos)?;
        self.ov
            .pos
            .get_mut(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?
            .extra = extra;
        Ok(())
    }

    fn market(&self, at: MarketSlot) -> Result<&MarketRow, ProtocolError> {
        self.markets(at.market)?
            .get(usize::from(at.slot))
            .ok_or(ProtocolError::SlotOutOfRange(at))
    }

    fn markets(&self, market: MarketId) -> Result<&[MarketRow], ProtocolError> {
        self.ov
            .rows(market)
            .or_else(|| self.base.rows(market))
            .ok_or(ProtocolError::UnknownMarket(market))
    }

    fn set_market(&mut self, at: MarketSlot, row: MarketRow) -> Result<(), ProtocolError> {
        let i = self.materialise_market(at.market, false)?;
        *self
            .ov
            .mkt
            .get_mut(i)
            .and_then(|rows| rows.get_mut(usize::from(at.slot)))
            .ok_or(ProtocolError::SlotOutOfRange(at))? = row;
        Ok(())
    }

    fn push_market(
        &mut self,
        market: MarketId,
        row: MarketRow,
    ) -> Result<MarketSlot, ProtocolError> {
        let i = self.materialise_market(market, true)?;
        let rows = self
            .ov
            .mkt
            .get_mut(i)
            .ok_or(ProtocolError::UnknownMarket(market))?;
        let slot = u16::try_from(rows.len()).map_err(|_| {
            ProtocolError::SlotOutOfRange(MarketSlot {
                market,
                slot: u16::MAX,
            })
        })?;
        let at = MarketSlot { market, slot };
        if slot >= AssetMask::MAX_SLOTS {
            return Err(ProtocolError::SlotOutOfRange(at));
        }
        rows.push(row);
        // Every overlay copy of a position in this market gains the cell;
        // base positions gain it when (if) they are materialised.
        let n = rows.len();
        for p in self.ov.pos.iter_mut().take(self.ov.pos_len) {
            if p.key.market == market {
                p.supply.resize(n, 0);
                p.debt.resize(n, 0);
                p.slot_extra.resize(n, PositionExtraRepr::ZERO);
            }
        }
        Ok(at)
    }

    fn slot_extra(&self, pos: PositionId, slot: u16) -> Result<&PositionExtraRepr, ProtocolError> {
        let Some(p) = self.ov.position(pos) else {
            return self.base.slot_extra(pos, slot);
        };
        p.slot_extra
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: p.key.market,
                slot,
            }))
    }

    fn set_slot_extra(
        &mut self,
        pos: PositionId,
        slot: u16,
        extra: PositionExtraRepr,
    ) -> Result<(), ProtocolError> {
        let i = self.materialise(pos)?;
        let p = self
            .ov
            .pos
            .get_mut(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let at = MarketSlot {
            market: p.key.market,
            slot,
        };
        *p.slot_extra
            .get_mut(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(at))? = extra;
        Ok(())
    }

    fn positions_len(&self) -> u32 {
        u32::try_from(self.ov.base_len.saturating_add(self.ov.new_keys.len())).unwrap_or(u32::MAX)
    }

    fn position_key(&self, pos: PositionId) -> Result<&PositionKey, ProtocolError> {
        match self.ov.position(pos) {
            Some(p) => Ok(&p.key),
            None => self.base.position_key(pos),
        }
    }
}
