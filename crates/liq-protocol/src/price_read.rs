//! Protocol-reported prices (GUIDE 06): the `eth_call`s whose answers are a
//! protocol's *own* price for its markets. Health must use what the
//! protocol reads — an Aave CAPO adapter, a Morpho market oracle, a Liquity
//! branch feed — not a shared feed that only approximates it. The bot reads
//! these off the hot path every block and lays them over the canonical
//! vector per market (`liq_engine::ProtocolPrices`).

use alloy_primitives::{Address, Bytes};
use liq_types::{AssetId, MarketId};

use crate::market::MarketRow;

/// One read: `target.call(calldata)` answers prices for `market`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceRead {
    /// The protocol market the answer prices (the overlay key).
    pub market: MarketId,
    pub target: Address,
    pub calldata: Bytes,
    /// Adapter-private: which of its reads this is (e.g. a pool index).
    pub tag: u32,
    /// The asset each priced slot of the answer is, in answer order —
    /// fixed when the read is built (from config or state), so decoding
    /// needs neither.
    pub assets: Vec<AssetId>,
}

/// A chain read whose answer the adapter folds into its own **state**, not
/// the price overlay: a protocol whose liquidatable size is only knowable by
/// asking the protocol (Fluid's vaults simulate their own `liquidate`).
/// Read off the hot path once per block, pinned to that block, and applied
/// on the ingest thread inside that block's undo record — so a reorg
/// unwinds it with the block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateRead {
    /// The market the answer describes.
    pub market: MarketId,
    pub target: Address,
    pub calldata: Bytes,
    /// Adapter-private: which of its reads this is.
    pub tag: u64,
}

/// One [`StateRead`]'s outcome. `success == false` carries the revert data
/// (reads that answer by reverting — a dead-address simulation — are
/// successes of the read and failures of the call).
#[derive(Clone, Copy, Debug)]
pub struct StateAnswer<'a> {
    pub read: &'a StateRead,
    pub success: bool,
    pub data: &'a [u8],
}

/// Read access to market rows, for adapters whose markets are only known
/// from state (Morpho assigns `MarketId`s from `CreateMarket`).
pub trait MarketRows {
    fn rows(&self, market: MarketId) -> Option<&[MarketRow]>;
}
