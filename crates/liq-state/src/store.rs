//! `StateStore` — the single-writer in-memory mirror (GUIDE 02 §2, §5, §7b).
//!
//! **Layout.** Per position, three dense columns indexed by `PositionId`:
//! `config` (the [`AssetMask`], the hottest field — `health()` reads it first
//! and touches nothing for unset slots), the interner's `key + local` entry,
//! and `extra`. Balances live per market as one flat `Vec<u128>` per column
//! with a fixed **stride** (cells per position, rounded up to a multiple of
//! four — a whole number of cache lines, so every position's block sits at
//! the same phase inside a line and no position's row straddles more lines
//! than another; the allocation itself is `u128`-aligned, not line-aligned,
//! so that phase is not generally zero): position `local`'s supply is
//! `supply[local·stride .. local·stride + rows.len()]`. That slice *is*
//! `PositionRef::supply` — zero copy, no pointer chase — which is what the
//! `PositionRef` contract (WP 01) requires and what a `[slot][position]`
//! column cannot provide without a per-call gather. A market's balances are
//! still one contiguous allocation made once, so a market sweep streams.
//!
//! **Journal before write.** Every setter resolves and validates everything it
//! will touch, pushes the inverse into the undo ring, and only then writes;
//! nothing fallible sits between the push and the write, so a failing setter
//! has either done nothing or journaled what it did.
//!
//! **Single writer.** Mutated through `&mut` on the hot thread only. There is
//! no shared-ownership pointer, no lock and no interlocked word in this
//! definition (`tests/no_sync.rs` grep-asserts it); the borrow checker is the
//! concurrency proof (RUST-CONVENTIONS §1). Off-thread readers get a published
//! snapshot (WP 02B), never a reference in here.

use liq_protocol::{
    AssetMask, BlockNum, MarketRow, MarketSlot, PositionExtraRepr, PositionRef, ProtocolError,
    StateWriter, Timestamp,
};
use liq_types::{MarketId, PositionId, PositionKey};

use crate::error::StateError;
use crate::interner::PositionTable;
use crate::undo::{BlockUndo, UndoCapacity, UndoOp, UndoRing};
use crate::view::{Overlay, StateView};

/// Sentinel in `market_index`: no market at that id.
pub(crate) const NO_MARKET: u16 = u16::MAX;
/// Addressable market ids and market count: the index table is `u16`.
const MAX_MARKETS: usize = NO_MARKET as usize;
/// `u128` cells per cache line; strides are rounded up to this.
pub(crate) const LINE_CELLS: usize = 4;

/// Startup sizing. Everything here is reserved once in [`StateStore::new`];
/// the hot path allocates nothing while it stays within these numbers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StoreConfig {
    /// Block the (empty) store represents. The first `begin_block` is
    /// `base + 1`; the store can never be unwound below `base`.
    pub base: BlockNum,
    /// Position capacity of the per-position columns.
    pub positions: usize,
    /// Market capacity.
    pub markets: usize,
    /// Undo ring reservation per block.
    pub undo: UndoCapacity,
}

/// One market: its reserve rows and both balance columns.
pub(crate) struct Market {
    pub(crate) rows: Vec<MarketRow>,
    /// Cells per position in `supply`/`debt`; `>= rows.len()`, multiple of
    /// [`LINE_CELLS`]. Cells at or past `rows.len()` are always zero.
    pub(crate) stride: usize,
    /// Positions in this market (`PosEntry::local` space).
    pub(crate) n_pos: u32,
    pub(crate) supply: Vec<u128>,
    pub(crate) debt: Vec<u128>,
}

impl Market {
    const fn new() -> Self {
        Self {
            rows: Vec::new(),
            stride: 0,
            n_pos: 0,
            supply: Vec::new(),
            debt: Vec::new(),
        }
    }

    /// Cell index of `(local, slot)`.
    #[inline]
    fn cell(&self, local: u32, slot: u16) -> Option<usize> {
        (local as usize)
            .checked_mul(self.stride)?
            .checked_add(usize::from(slot))
    }

    /// Cell range of position `local`'s row (`rows.len()` cells).
    #[inline]
    fn row_range(&self, local: u32) -> Option<core::ops::Range<usize>> {
        let start = (local as usize).checked_mul(self.stride)?;
        Some(start..start.checked_add(self.rows.len())?)
    }

    /// Grow `stride` so `slots` cells fit per position, re-laying both
    /// columns. Only a reserve listing past the current headroom pays this;
    /// with no positions yet (config load) it is a `resize` to zero cells.
    fn ensure_stride(&mut self, slots: usize) -> Result<(), StateError> {
        if slots <= self.stride {
            return Ok(());
        }
        let new = slots
            .checked_next_multiple_of(LINE_CELLS)
            .ok_or(StateError::Inconsistent)?;
        let n = self.n_pos as usize;
        restride(&mut self.supply, n, self.stride, new)?;
        restride(&mut self.debt, n, self.stride, new)?;
        self.stride = new;
        Ok(())
    }
}

/// Re-lay `col` from `old` to `new` cells per position, back to front so no
/// unmoved row is overwritten; the cells opened up in each row are zeroed.
fn restride(col: &mut Vec<u128>, n_pos: usize, old: usize, new: usize) -> Result<(), StateError> {
    let total = n_pos.checked_mul(new).ok_or(StateError::Inconsistent)?;
    col.resize(total, 0);
    for l in (0..n_pos).rev() {
        let src = l.checked_mul(old).ok_or(StateError::Inconsistent)?;
        let src_end = src.checked_add(old).ok_or(StateError::Inconsistent)?;
        let dst = l.checked_mul(new).ok_or(StateError::Inconsistent)?;
        let dst_mid = dst.checked_add(old).ok_or(StateError::Inconsistent)?;
        let dst_end = dst.checked_add(new).ok_or(StateError::Inconsistent)?;
        if src_end > total || dst_end > total {
            return Err(StateError::Inconsistent);
        }
        col.copy_within(src..src_end, dst);
        if let Some(tail) = col.get_mut(dst_mid..dst_end) {
            tail.fill(0);
        }
    }
    Ok(())
}

/// Which balance column a setter targets.
#[derive(Copy, Clone)]
enum Col {
    Supply,
    Debt,
}

/// The store. See the module docs for the layout.
pub struct StateStore {
    /// Slots with a nonzero supply or debt, by `PositionId`. First: hottest.
    pub(crate) config: Vec<AssetMask>,
    pub(crate) positions: PositionTable,
    pub(crate) extra: Vec<PositionExtraRepr>,
    pub(crate) markets: Vec<Market>,
    /// `MarketId.0 → index into markets`, [`NO_MARKET`] when absent. A direct
    /// table, not a hash: `MarketId` is dense by contract (GUIDE 00 §3).
    pub(crate) market_index: Vec<u16>,
    undo: UndoRing,
    /// Mutations applied, ever. Bumped by every setter next to its journal
    /// push, so `(tip, len, writes)` identifies the state an [`Overlay`]'s
    /// copies were taken from — `tip` alone does not, because a block's
    /// setters all run at one tip. Wrapping, not saturating: a counter that
    /// stopped counting would start admitting stale overlays.
    pub(crate) writes: u64,
}

impl core::fmt::Debug for StateStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StateStore")
            .field("positions", &self.positions.len())
            .field("markets", &self.markets.len())
            .field("tip", &self.undo.tip())
            .field("floor", &self.undo.floor())
            .finish_non_exhaustive()
    }
}

/// `markets[market_index[at.market]].rows[at.slot]`, as a free function over
/// the two fields so a caller can hold the row and the undo ring at once.
#[inline]
fn row_mut<'m>(
    markets: &'m mut [Market],
    index: &[u16],
    at: MarketSlot,
) -> Option<&'m mut MarketRow> {
    let mi = market_idx(index, at.market)?;
    markets.get_mut(mi)?.rows.get_mut(usize::from(at.slot))
}

#[inline]
pub(crate) fn market_idx(index: &[u16], id: MarketId) -> Option<usize> {
    let i = *index.get(id.0 as usize)?;
    (i != NO_MARKET).then_some(usize::from(i))
}

impl StateStore {
    /// Reserve everything up front (GUIDE 02 §7b).
    #[must_use]
    pub fn new(cfg: StoreConfig) -> Self {
        Self {
            config: Vec::with_capacity(cfg.positions),
            positions: PositionTable::with_capacity(cfg.positions),
            extra: Vec::with_capacity(cfg.positions),
            markets: Vec::with_capacity(cfg.markets),
            market_index: Vec::new(),
            undo: UndoRing::new(cfg.base, cfg.undo),
            writes: 0,
        }
    }

    /// Reserve room for `n` more positions in `market`'s balance columns so
    /// `intern` does not grow them. Call after the market's reserves are
    /// pushed (the reservation is `n · stride` cells).
    pub fn reserve_positions(&mut self, market: MarketId, n: usize) -> Result<(), StateError> {
        let m = self
            .market_mut(market)
            .ok_or(StateError::UnknownMarket(market))?;
        let cells = n.checked_mul(m.stride).ok_or(StateError::Inconsistent)?;
        m.supply.reserve(cells);
        m.debt.reserve(cells);
        Ok(())
    }

    /// Positions interned.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.len() == 0
    }

    /// Dense id of `key`, if interned.
    #[inline]
    #[must_use]
    pub fn position_id(&self, key: &PositionKey) -> Option<PositionId> {
        self.positions.get(key)
    }

    /// Last block applied (or the base block).
    #[inline]
    #[must_use]
    pub fn tip(&self) -> BlockNum {
        self.undo.tip()
    }

    /// Lowest block `unwind_to` can reach.
    #[inline]
    #[must_use]
    pub fn floor(&self) -> BlockNum {
        self.undo.floor()
    }

    /// Times a block exceeded its undo reservation and the ring allocated.
    /// Nonzero means [`StoreConfig::undo`] is undersized (telemetry, WP 09A).
    #[inline]
    #[must_use]
    pub fn undo_overflows(&self) -> u64 {
        self.undo.overflows()
    }

    /// Open block `block` (`= tip + 1`) for mutation. Every setter until the
    /// next `begin_block` journals into this block's record.
    pub fn begin_block(&mut self, block: BlockNum) -> Result<(), StateError> {
        self.undo.begin(block)
    }

    /// Revert every block above `target`, newest first, restoring the state
    /// as of `target` byte for byte. Fails **before touching anything** with
    /// [`StateError::ReorgTooDeep`] when the ring does not reach `target`.
    /// [`StateError::Inconsistent`] mid-way means a record did not match the
    /// state — the store is then partially unwound and the caller must halt.
    pub fn unwind_to(&mut self, target: BlockNum) -> Result<(), StateError> {
        let tip = self.undo.tip();
        let floor = self.undo.floor();
        if target > tip {
            return Err(StateError::TargetAboveTip { target, tip });
        }
        if target < floor {
            return Err(StateError::ReorgTooDeep {
                depth: tip.saturating_sub(target),
                cap: tip.saturating_sub(floor),
            });
        }
        let mut block = tip;
        while block > target {
            let slot = self.undo.slot_of(block).ok_or(StateError::Inconsistent)?;
            let BlockUndo {
                ops,
                mut extras,
                mut rows,
            } = self.undo.take(slot).ok_or(StateError::Inconsistent)?;
            let mut result = Ok(());
            for op in ops.iter().rev() {
                result = self.revert(*op, &mut extras, &mut rows);
                if result.is_err() {
                    break;
                }
            }
            self.undo.put(slot, BlockUndo { ops, extras, rows });
            result?;
            block = block.saturating_sub(1);
        }
        self.undo.rewind(target)
    }

    /// Canonical read view at chain time `timestamp` (the *target* block's
    /// time for forward projection — GUIDE 02 §6).
    #[inline]
    #[must_use]
    pub fn view(&self, timestamp: Timestamp) -> StateView<'_> {
        StateView::new(self, None, timestamp)
    }

    /// Read view with `overlay`'s pending delta on top. Refused when the
    /// overlay was built against another store state.
    pub fn view_with<'a>(
        &'a self,
        overlay: &'a Overlay,
        timestamp: Timestamp,
    ) -> Result<StateView<'a>, StateError> {
        overlay.check_base(self)?;
        Ok(StateView::new(self, Some(overlay), timestamp))
    }

    // ---- crate-internal read path ------------------------------------------

    /// Mutations applied, ever — an [`Overlay`]'s base generation.
    #[inline]
    pub(crate) fn writes(&self) -> u64 {
        self.writes
    }

    #[inline]
    fn market(&self, id: MarketId) -> Option<&Market> {
        self.markets.get(market_idx(&self.market_index, id)?)
    }

    #[inline]
    fn market_mut(&mut self, id: MarketId) -> Option<&mut Market> {
        let mi = market_idx(&self.market_index, id)?;
        self.markets.get_mut(mi)
    }

    /// Rows of `market`, if known.
    #[inline]
    pub(crate) fn rows(&self, market: MarketId) -> Option<&[MarketRow]> {
        self.market(market).map(|m| m.rows.as_slice())
    }

    /// Borrowed view of `id` at `timestamp` (GUIDE 02 §6). Allocation-free;
    /// every access is checked and the `None` case is an error, never a panic.
    #[inline]
    pub(crate) fn position_ref(
        &self,
        id: PositionId,
        timestamp: Timestamp,
    ) -> Result<PositionRef<'_>, StateError> {
        let i = id.0 as usize;
        let config = *self.config.get(i).ok_or(StateError::UnknownPosition(id))?;
        let entry = self
            .positions
            .entry(i)
            .ok_or(StateError::UnknownPosition(id))?;
        let extra = self.extra.get(i).ok_or(StateError::UnknownPosition(id))?;
        let market = entry.key.market;
        let m = self
            .market(market)
            .ok_or(StateError::UnknownMarket(market))?;
        let range = m.row_range(entry.local).ok_or(StateError::Inconsistent)?;
        let supply = m
            .supply
            .get(range.clone())
            .ok_or(StateError::Inconsistent)?;
        let debt = m.debt.get(range).ok_or(StateError::Inconsistent)?;
        Ok(PositionRef {
            id,
            key: &entry.key,
            config,
            supply,
            debt,
            extra,
            markets: &m.rows,
            timestamp,
        })
    }

    // ---- setters -----------------------------------------------------------

    /// `set_supply` / `set_debt`: journal the previous cell and mask bit,
    /// then write both. `config` is maintained here and nowhere else.
    fn set_column(
        &mut self,
        col: Col,
        pos: PositionId,
        slot: u16,
        shares: u128,
    ) -> Result<(), ProtocolError> {
        let i = pos.0 as usize;
        let entry = self
            .positions
            .entry(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let (market, local) = (entry.key.market, entry.local);
        let at = MarketSlot { market, slot };
        let mi =
            market_idx(&self.market_index, market).ok_or(ProtocolError::UnknownMarket(market))?;
        let m = self
            .markets
            .get_mut(mi)
            .ok_or(ProtocolError::UnknownMarket(market))?;
        if usize::from(slot) >= m.rows.len() {
            return Err(ProtocolError::SlotOutOfRange(at));
        }
        let ci = m
            .cell(local, slot)
            .ok_or(ProtocolError::SlotOutOfRange(at))?;
        let config = self
            .config
            .get_mut(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let prev_set = config.contains(slot);
        let (column, other) = match col {
            Col::Supply => (&mut m.supply, &m.debt),
            Col::Debt => (&mut m.debt, &m.supply),
        };
        let other_nonzero = *other.get(ci).ok_or(ProtocolError::SlotOutOfRange(at))? != 0;
        let cell = column
            .get_mut(ci)
            .ok_or(ProtocolError::SlotOutOfRange(at))?;
        let prev = *cell;
        let new_config = if shares != 0 || other_nonzero {
            config.with(slot).ok_or(ProtocolError::SlotOutOfRange(at))?
        } else {
            config.without(slot)
        };
        // ---- journal, then write. Nothing below can fail. ----
        self.undo.push(match col {
            Col::Supply => UndoOp::Supply {
                pos,
                slot,
                prev_set,
                prev,
            },
            Col::Debt => UndoOp::Debt {
                pos,
                slot,
                prev_set,
                prev,
            },
        });
        *cell = shares;
        *config = new_config;
        self.writes = self.writes.wrapping_add(1);
        Ok(())
    }

    /// Rebuild from a snapshot (WP 02B). `base` is the snapshot tip: the
    /// ring starts at `depth == 0` so mutations until the next `begin_block`
    /// are unjournaled (carry-forward 02B). Capacities from `cfg` are
    /// reserved on top of the captured lengths.
    pub(crate) fn from_captured(
        cfg: StoreConfig,
        writes: u64,
        mut config: Vec<AssetMask>,
        positions: PositionTable,
        mut extra: Vec<PositionExtraRepr>,
        mut markets: Vec<Market>,
        mut market_index: Vec<u16>,
    ) -> Result<Self, StateError> {
        if config.len() != extra.len() || config.len() != positions.len() {
            return Err(StateError::Inconsistent);
        }
        config.reserve(cfg.positions.saturating_sub(config.len()));
        extra.reserve(cfg.positions.saturating_sub(extra.len()));
        markets.reserve(cfg.markets.saturating_sub(markets.len()));
        market_index.reserve(cfg.markets.saturating_sub(market_index.len()));
        Ok(Self {
            config,
            positions,
            extra,
            markets,
            market_index,
            undo: UndoRing::new(cfg.base, cfg.undo),
            writes,
        })
    }

    /// Inverse of [`Self::set_column`].
    fn restore_cell(
        &mut self,
        col: Col,
        pos: PositionId,
        slot: u16,
        prev_set: bool,
        prev: u128,
    ) -> Result<(), StateError> {
        let i = pos.0 as usize;
        let entry = self.positions.entry(i).ok_or(StateError::Inconsistent)?;
        let (market, local) = (entry.key.market, entry.local);
        let m = self.market_mut(market).ok_or(StateError::Inconsistent)?;
        let ci = m.cell(local, slot).ok_or(StateError::Inconsistent)?;
        let column = match col {
            Col::Supply => &mut m.supply,
            Col::Debt => &mut m.debt,
        };
        *column.get_mut(ci).ok_or(StateError::Inconsistent)? = prev;
        let config = self.config.get_mut(i).ok_or(StateError::Inconsistent)?;
        *config = if prev_set {
            config.with(slot).ok_or(StateError::Inconsistent)?
        } else {
            config.without(slot)
        };
        Ok(())
    }

    /// Apply one inverse. `extras`/`rows` are the block's side tables.
    fn revert(
        &mut self,
        op: UndoOp,
        extras: &mut Vec<PositionExtraRepr>,
        rows: &mut Vec<MarketRow>,
    ) -> Result<(), StateError> {
        match op {
            UndoOp::Supply {
                pos,
                slot,
                prev_set,
                prev,
            } => self.restore_cell(Col::Supply, pos, slot, prev_set, prev),
            UndoOp::Debt {
                pos,
                slot,
                prev_set,
                prev,
            } => self.restore_cell(Col::Debt, pos, slot, prev_set, prev),
            UndoOp::Extra { pos } => {
                let prev = extras.pop().ok_or(StateError::Inconsistent)?;
                *self
                    .extra
                    .get_mut(pos.0 as usize)
                    .ok_or(StateError::Inconsistent)? = prev;
                Ok(())
            }
            UndoOp::Market { at } => {
                let prev = rows.pop().ok_or(StateError::Inconsistent)?;
                *row_mut(&mut self.markets, &self.market_index, at)
                    .ok_or(StateError::Inconsistent)? = prev;
                Ok(())
            }
            UndoOp::Created { pos } => {
                // The created position is the newest one at this point of the
                // unwind: later creations were undone before it.
                let last = self
                    .positions
                    .len()
                    .checked_sub(1)
                    .ok_or(StateError::Inconsistent)?;
                if last != pos.0 as usize {
                    return Err(StateError::Inconsistent);
                }
                let entry = self.positions.pop().ok_or(StateError::Inconsistent)?;
                self.config.pop();
                self.extra.pop();
                let m = self
                    .market_mut(entry.key.market)
                    .ok_or(StateError::Inconsistent)?;
                let n = m.n_pos.checked_sub(1).ok_or(StateError::Inconsistent)?;
                if entry.local != n {
                    return Err(StateError::Inconsistent);
                }
                m.n_pos = n;
                let len = (n as usize)
                    .checked_mul(m.stride)
                    .ok_or(StateError::Inconsistent)?;
                m.supply.truncate(len);
                m.debt.truncate(len);
                Ok(())
            }
            UndoOp::Pushed { market, created } => {
                let mi = market_idx(&self.market_index, market).ok_or(StateError::Inconsistent)?;
                let last = self
                    .markets
                    .len()
                    .checked_sub(1)
                    .ok_or(StateError::Inconsistent)?;
                let m = self.markets.get_mut(mi).ok_or(StateError::Inconsistent)?;
                m.rows.pop().ok_or(StateError::Inconsistent)?;
                if created {
                    if mi != last || !m.rows.is_empty() || m.n_pos != 0 {
                        return Err(StateError::Inconsistent);
                    }
                    self.markets.pop();
                    *self
                        .market_index
                        .get_mut(market.0 as usize)
                        .ok_or(StateError::Inconsistent)? = NO_MARKET;
                }
                Ok(())
            }
        }
    }
}

impl StateWriter for StateStore {
    fn intern(&mut self, key: &PositionKey) -> Result<PositionId, ProtocolError> {
        if let Some(id) = self.positions.get(key) {
            return Ok(id);
        }
        let market = key.market;
        let mi =
            market_idx(&self.market_index, market).ok_or(ProtocolError::UnknownMarket(market))?;
        let m = self
            .markets
            .get_mut(mi)
            .ok_or(ProtocolError::UnknownMarket(market))?;
        // Exhausting the id space is a wrong parameter, not a missing market.
        let id = self.positions.next_id().ok_or(ProtocolError::Internal)?;
        let local = m.n_pos;
        let n_pos = local.checked_add(1).ok_or(ProtocolError::Internal)?;
        let cells = m
            .supply
            .len()
            .checked_add(m.stride)
            .ok_or(ProtocolError::Internal)?;
        // ---- journal, then write. Nothing below can fail. ----
        self.undo.push(UndoOp::Created { pos: id });
        m.n_pos = n_pos;
        m.supply.resize(cells, 0);
        m.debt.resize(cells, 0);
        self.positions.push(*key, id, local);
        self.config.push(AssetMask::EMPTY);
        self.extra.push(PositionExtraRepr::ZERO);
        self.writes = self.writes.wrapping_add(1);
        Ok(id)
    }

    fn supply(&self, pos: PositionId, slot: u16) -> Result<u128, ProtocolError> {
        let entry = self
            .positions
            .entry(pos.0 as usize)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let market = entry.key.market;
        let at = MarketSlot { market, slot };
        let m = self
            .market(market)
            .ok_or(ProtocolError::UnknownMarket(market))?;
        if usize::from(slot) >= m.rows.len() {
            return Err(ProtocolError::SlotOutOfRange(at));
        }
        m.cell(entry.local, slot)
            .and_then(|ci| m.supply.get(ci))
            .copied()
            .ok_or(ProtocolError::SlotOutOfRange(at))
    }

    fn debt(&self, pos: PositionId, slot: u16) -> Result<u128, ProtocolError> {
        let entry = self
            .positions
            .entry(pos.0 as usize)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let market = entry.key.market;
        let at = MarketSlot { market, slot };
        let m = self
            .market(market)
            .ok_or(ProtocolError::UnknownMarket(market))?;
        if usize::from(slot) >= m.rows.len() {
            return Err(ProtocolError::SlotOutOfRange(at));
        }
        m.cell(entry.local, slot)
            .and_then(|ci| m.debt.get(ci))
            .copied()
            .ok_or(ProtocolError::SlotOutOfRange(at))
    }

    fn set_supply(
        &mut self,
        pos: PositionId,
        slot: u16,
        shares: u128,
    ) -> Result<(), ProtocolError> {
        self.set_column(Col::Supply, pos, slot, shares)
    }

    fn set_debt(&mut self, pos: PositionId, slot: u16, shares: u128) -> Result<(), ProtocolError> {
        self.set_column(Col::Debt, pos, slot, shares)
    }

    fn extra(&self, pos: PositionId) -> Result<&PositionExtraRepr, ProtocolError> {
        self.extra
            .get(pos.0 as usize)
            .ok_or(ProtocolError::UnknownPosition(pos))
    }

    fn set_extra(
        &mut self,
        pos: PositionId,
        extra: PositionExtraRepr,
    ) -> Result<(), ProtocolError> {
        let e = self
            .extra
            .get_mut(pos.0 as usize)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let prev = *e;
        // ---- journal, then write ----
        self.undo.push_extra(pos, prev);
        *e = extra;
        self.writes = self.writes.wrapping_add(1);
        Ok(())
    }

    fn market(&self, at: MarketSlot) -> Result<&MarketRow, ProtocolError> {
        self.rows(at.market)
            .ok_or(ProtocolError::UnknownMarket(at.market))?
            .get(usize::from(at.slot))
            .ok_or(ProtocolError::SlotOutOfRange(at))
    }

    fn markets(&self, market: MarketId) -> Result<&[MarketRow], ProtocolError> {
        self.rows(market)
            .ok_or(ProtocolError::UnknownMarket(market))
    }

    fn set_market(&mut self, at: MarketSlot, row: MarketRow) -> Result<(), ProtocolError> {
        let r = row_mut(&mut self.markets, &self.market_index, at)
            .ok_or(ProtocolError::SlotOutOfRange(at))?;
        let prev = *r;
        // ---- journal, then write ----
        self.undo.push_market(at, prev);
        *r = row;
        self.writes = self.writes.wrapping_add(1);
        Ok(())
    }

    fn push_market(
        &mut self,
        market: MarketId,
        row: MarketRow,
    ) -> Result<MarketSlot, ProtocolError> {
        let (mi, created) = match market_idx(&self.market_index, market) {
            Some(i) => (i, false),
            None => (self.markets.len(), true),
        };
        let id_ix = market.0 as usize;
        // The direct index table is `u16`-addressed. A dense `MarketId`
        // (GUIDE 00 §3) never reaches this; one that does was read wrong.
        if created && (id_ix >= MAX_MARKETS || mi >= MAX_MARKETS) {
            return Err(ProtocolError::Internal);
        }
        let n = if created {
            0
        } else {
            self.markets
                .get(mi)
                .ok_or(ProtocolError::UnknownMarket(market))?
                .rows
                .len()
        };
        let slot = u16::try_from(n).map_err(|_| {
            ProtocolError::SlotOutOfRange(MarketSlot {
                market,
                slot: u16::MAX,
            })
        })?;
        let at = MarketSlot { market, slot };
        if slot >= AssetMask::MAX_SLOTS {
            return Err(ProtocolError::SlotOutOfRange(at));
        }
        let slots = n.checked_add(1).ok_or(ProtocolError::Internal)?;
        // Physical room first (no logical change): the index table entry and
        // the stride. Only a listing past the headroom re-lays a column. A
        // created market is built and strided off to the side for the same
        // reason the resolution above happens first — everything that can
        // fail runs before the journal push, so the ring never holds the
        // inverse of a mutation that did not happen.
        if created {
            if id_ix >= self.market_index.len() {
                let len = id_ix.checked_add(1).ok_or(ProtocolError::Internal)?;
                self.market_index.resize(len, NO_MARKET);
            }
            // `mi < MAX_MARKETS` was checked above.
            let mi16 = u16::try_from(mi).map_err(|_| ProtocolError::Internal)?;
            let mut m = Market::new();
            m.ensure_stride(slots)
                .map_err(|_| ProtocolError::Internal)?;
            m.rows.push(row);
            let ix = self
                .market_index
                .get_mut(id_ix)
                .ok_or(ProtocolError::Internal)?;
            // ---- journal, then write. Nothing below can fail. ----
            self.undo.push(UndoOp::Pushed {
                market,
                created: true,
            });
            self.markets.push(m);
            *ix = mi16;
        } else {
            let m = self
                .markets
                .get_mut(mi)
                .ok_or(ProtocolError::UnknownMarket(market))?;
            m.ensure_stride(slots)
                .map_err(|_| ProtocolError::Internal)?;
            // ---- journal, then write. Nothing below can fail. ----
            self.undo.push(UndoOp::Pushed {
                market,
                created: false,
            });
            m.rows.push(row);
        }
        self.writes = self.writes.wrapping_add(1);
        Ok(at)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{StateStore, StoreConfig, LINE_CELLS};
    use crate::undo::UndoCapacity;
    use alloy_primitives::Address;
    use liq_protocol::{FeedId, MarketFlags, MarketRow, StateWriter};
    use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId, RayU128};
    use std::collections::BTreeSet;

    fn row(asset: u16) -> MarketRow {
        MarketRow {
            supply_index: RayU128::from_raw(1),
            debt_index: RayU128::from_raw(1),
            supply_rate: RayU128::from_raw(0),
            debt_rate: RayU128::from_raw(0),
            dust_floor: 0,
            last_update: 0,
            target_hf: 0,
            hub_ref: u16::MAX,
            liq_threshold: 0,
            ltv: 0,
            price_feed: FeedId(0),
            asset: AssetId(asset),
            max_liq_bonus: 0,
            hf_for_max_bonus: 0,
            liq_bonus_factor: 0,
            decimals: 18,
            flags: MarketFlags::NONE,
            _pad: [0; 22],
        }
    }

    fn key(market: MarketId, user: u8) -> PositionKey {
        PositionKey {
            protocol: ProtocolId(1),
            market,
            user: Address::repeat_byte(user),
        }
    }

    fn store(positions: usize, ops: usize) -> StateStore {
        StateStore::new(StoreConfig {
            base: 100,
            positions,
            markets: 4,
            undo: UndoCapacity {
                ops,
                extras: ops / 8,
                rows: ops / 8,
            },
        })
    }

    fn line(p: *const u8) -> usize {
        (p as usize) >> 6
    }

    /// Cache lines the store's read path touches for a three-slot position,
    /// **measured from the addresses `position_ref` dereferences** (not
    /// estimated). Re-derived budget (carry-forward from the 01 review):
    /// `health()` needs indices *and* rates, which fill `MarketRow` line 0
    /// exactly, plus `dust_floor`/`liq_threshold`/`asset`/`decimals` on line 1
    /// — two lines per market, so three markets are 6, not 3. Data lines:
    /// mask 1 + key/local entry 1 + extra 1 + supply ≤ 3 + debt ≤ 3 + rows 6
    /// = 15 worst case (slots on distinct lines), 11 best (adjacent slots),
    /// plus the `PriceVector` (≤ 3, not this crate's) → 14–18. Header lines
    /// (`market_index`, the `Market` struct) are L1-resident and reported
    /// separately. Oracle: arithmetic on the layout; GUIDE 02 acceptance.
    #[test]
    fn three_slot_position_touches_at_most_fifteen_data_lines() {
        let mut st = store(16, 64);
        let m = MarketId(3);
        for a in 0..40u16 {
            st.push_market(m, row(a)).unwrap();
        }
        for u in 0..10u8 {
            st.intern(&key(m, u)).unwrap();
        }
        let p = PositionId(7);
        // Slots 0, 5 and 12: three distinct lines in a 40-cell row — the
        // worst case for the balance columns.
        for s in [0u16, 5, 12] {
            st.set_supply(p, s, 1_000).unwrap();
            st.set_debt(p, s, 500).unwrap();
        }
        let r = st.position_ref(p, 0).unwrap();
        assert_eq!(r.config.len(), 3);

        let mut data = BTreeSet::new();
        data.insert(line(st.config.as_ptr().wrapping_add(7).cast()));
        data.insert(line(st.positions.entries.as_ptr().wrapping_add(7).cast()));
        data.insert(line((r.extra as *const _ as *const u8).cast()));
        for s in r.config.iter() {
            data.insert(line((&r.supply[usize::from(s)] as *const u128).cast()));
            data.insert(line((&r.debt[usize::from(s)] as *const u128).cast()));
            let row0 = (&r.markets[usize::from(s)] as *const MarketRow).cast::<u8>();
            data.insert(line(row0));
            data.insert(line(row0.wrapping_add(64)));
        }
        let mut headers = BTreeSet::new();
        headers.insert(line(st.market_index.as_ptr().wrapping_add(3).cast()));
        let mk = (&st.markets[0] as *const super::Market).cast::<u8>();
        for off in (0..core::mem::size_of::<super::Market>()).step_by(64) {
            headers.insert(line(mk.wrapping_add(off)));
        }
        headers.insert(line((&st as *const StateStore).cast()));
        eprintln!(
            "cache lines: data = {} (mask 1, entry 1, extra 1, supply/debt {}, rows 6), headers = {}, total = {}",
            data.len(),
            data.len() - 9,
            headers.len(),
            data.len() + headers.len()
        );
        assert!(data.len() <= 15, "data lines {}", data.len());
        assert!(
            data.len() + headers.len() <= 19,
            "total lines {}",
            data.len() + headers.len()
        );
        // Each MarketRow contributes exactly its two lines and nothing else.
        assert_eq!(core::mem::size_of::<MarketRow>(), 128);
        // Strides are line-granular, so the slot → line map is the same for
        // every position (the base is `u128`-aligned, so the shared phase is
        // not necessarily zero; the count above is measured, not assumed).
        assert_eq!(st.markets[0].stride % LINE_CELLS, 0);
        assert!(st.markets[0].stride >= 40);
    }

    /// No allocation inside a block that stays within the reservation: the
    /// column and ring buffers keep their addresses across a 10k-mutation
    /// block, and the overflow counter stays zero. Oracle: `Vec` never moves
    /// without reallocating, so a stable `as_ptr()` is proof of none.
    #[test]
    fn ten_thousand_mutations_reallocate_nothing() {
        let mut st = store(1_024, 10_000);
        let m = MarketId(0);
        for a in 0..8u16 {
            st.push_market(m, row(a)).unwrap();
        }
        st.reserve_positions(m, 1_024).unwrap();
        let cfg_p = st.config.as_ptr();
        let ent_p = st.positions.entries.as_ptr();
        let ext_p = st.extra.as_ptr();
        let sup_p = st.markets[0].supply.as_ptr();
        let dbt_p = st.markets[0].debt.as_ptr();
        st.begin_block(101).unwrap();
        for u in 0..1_000u32 {
            let k = PositionKey {
                protocol: ProtocolId(1),
                market: m,
                user: Address::from_word(alloy_primitives::B256::from(
                    alloy_primitives::U256::from(u),
                )),
            };
            let p = st.intern(&k).unwrap();
            for s in 0..4u16 {
                st.set_supply(p, s, u128::from(u) + 1).unwrap();
                st.set_debt(p, s, u128::from(u)).unwrap();
            }
        }
        assert_eq!(st.len(), 1_000);
        assert_eq!(st.undo_overflows(), 0, "ring reservation held");
        assert_eq!(st.config.as_ptr(), cfg_p);
        assert_eq!(st.positions.entries.as_ptr(), ent_p);
        assert_eq!(st.extra.as_ptr(), ext_p);
        assert_eq!(st.markets[0].supply.as_ptr(), sup_p);
        assert_eq!(st.markets[0].debt.as_ptr(), dbt_p);
        // Negative: a reservation that is too small is counted, not hidden.
        // Six ops into a 4-op reservation: the fifth push reallocates (once,
        // the `Vec` doubles to 8), the sixth fits — one counted allocation.
        let mut small = store(8, 4);
        small.push_market(m, row(0)).unwrap();
        let p = small.intern(&key(m, 1)).unwrap();
        small.begin_block(101).unwrap();
        for i in 0..6u128 {
            small.set_supply(p, 0, i + 1).unwrap();
        }
        assert_eq!(small.undo_overflows(), 1);
    }

    /// A listing past the stride headroom re-lays both columns without
    /// moving any balance to another `(position, slot)`. Oracle: the values
    /// written before the re-stride, read back through the public reader.
    #[test]
    fn restride_preserves_every_cell() {
        let mut st = store(8, 64);
        let m = MarketId(1);
        for a in 0..4u16 {
            st.push_market(m, row(a)).unwrap();
        }
        assert_eq!(st.markets[0].stride, 4, "exactly one line, no headroom");
        let ids: Vec<_> = (0..3u8).map(|u| st.intern(&key(m, u)).unwrap()).collect();
        for (n, &p) in ids.iter().enumerate() {
            for s in 0..4u16 {
                st.set_supply(p, s, (n as u128 + 1) * 100 + u128::from(s))
                    .unwrap();
                st.set_debt(p, s, (n as u128 + 1) * 1_000 + u128::from(s))
                    .unwrap();
            }
        }
        let slot = st.push_market(m, row(4)).unwrap();
        assert_eq!(slot.slot, 4);
        assert_eq!(st.markets[0].stride, 8);
        for (n, &p) in ids.iter().enumerate() {
            for s in 0..4u16 {
                assert_eq!(
                    st.supply(p, s).unwrap(),
                    (n as u128 + 1) * 100 + u128::from(s)
                );
                assert_eq!(
                    st.debt(p, s).unwrap(),
                    (n as u128 + 1) * 1_000 + u128::from(s)
                );
            }
            assert_eq!(st.supply(p, 4).unwrap(), 0, "new slot is zero");
            let r = st.position_ref(p, 0).unwrap();
            assert_eq!(r.supply.len(), 5);
            assert_eq!(r.config.len(), 4);
        }
    }
}
