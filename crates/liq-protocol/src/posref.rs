//! `PositionRef` — the borrowed view `health()`, `quote()` and friends read
//! (GUIDE 01 §7). Always constructed by the store's `StateView` (GUIDE 02 §6),
//! so canonical, pending-overlay and backtest evaluation share one code path.

use liq_types::{PositionId, PositionKey};

use crate::extra::PositionExtraRepr;
use crate::market::MarketRow;
use crate::mask::AssetMask;
use crate::Timestamp;

/// One position, by reference, at one chain time.
///
/// `supply`, `debt` and `markets` are all indexed by **slot** (the position's
/// market's reserve index; see [`AssetMask`]). `config` has a bit set for
/// every slot with a nonzero supply or debt — the first thing `health()`
/// does is iterate those bits and touch nothing else.
#[derive(Copy, Clone, Debug)]
pub struct PositionRef<'a> {
    pub id: PositionId,
    /// Natural key, from the interner's reverse table. Copied into
    /// `Quote::key` so `encode` can address the borrower and market.
    pub key: &'a PositionKey,
    /// Slots with any balance.
    pub config: AssetMask,
    /// Supply shares per slot (V4 `suppliedShares`, V3 scaled balance).
    pub supply: &'a [u128],
    /// Debt shares per slot (V4 `drawnShares`, V3 scaled variable debt).
    pub debt: &'a [u128],
    /// Fixed-size protocol-specific state; never boxed.
    pub extra: &'a PositionExtraRepr,
    /// The position's market's rows, indexed by slot.
    pub markets: &'a [MarketRow],
    /// Chain time the view is evaluated at. Index projection from
    /// `MarketRow::last_update` runs to this instant, which is what makes
    /// `health()` bit-exact against the chain's view function at that
    /// block — and lets the engine evaluate at the *target* block's time
    /// (`+12 s`) rather than the last seen one.
    pub timestamp: Timestamp,
}
