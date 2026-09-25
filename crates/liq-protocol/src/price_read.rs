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

/// Read access to market rows, for adapters whose markets are only known
/// from state (Morpho assigns `MarketId`s from `CreateMarket`).
pub trait MarketRows {
    fn rows(&self, market: MarketId) -> Option<&[MarketRow]>;
}
