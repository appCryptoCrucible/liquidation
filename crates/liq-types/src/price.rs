//! Price vector, ticks, and source kinds (GUIDE 06 §1, §3). `ScheduledParamChange`
//! is the second event `liq-oracle` emits into the hot path (GUIDE 08 §5).

use crate::fixed::Ray;
use crate::ids::{AssetId, MarketId, PositionId, ProtocolId};
use crate::trace::TraceId;
use alloy_primitives::{Address, Bytes, Log, TxHash, B256};
use smallvec::SmallVec;
use std::time::Instant;

/// Confidence of a forward-looking price (GUIDE 06 §1). Raw value; scale is
/// owned by WP 06C.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Confidence(pub u16);

/// Partial MEV-Share hint (GUIDE 06 §4). Matchers must work with missing fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MevShareHint {
    pub hash: B256,
    pub to: Option<Address>,
    pub function_selector: Option<[u8; 4]>,
    pub call_data: Option<Bytes>,
    pub logs: Option<Vec<Log>>,
}

/// How a price was obtained. The executor branches on this (GUIDE 06 §1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// What the chain believes now. Ground truth, zero lead.
    Canonical,
    /// Announced privately via MEV-Share; you bid for the right to backrun.
    SvrAnnounced {
        hint: MevShareHint,
        deadline: Instant,
    },
    /// Visible in the public mempool; bundle behind it. Non-SVR feeds.
    PendingPublic { tx: TxHash, confidence: Confidence },
    /// Inferred from the aggregator's own inputs before it publishes.
    /// Pre-warm only — never fire on this alone.
    Predicted {
        eta: Option<Instant>,
        confidence: Confidence,
    },
    /// Computed from other prices + on-chain state (LST rates, LP fair value).
    Derived { deps: SmallVec<[AssetId; 4]> },
}

/// One slot in [`PriceVector`], indexed by global [`AssetId`] (GUIDE 06 §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Price {
    pub asset: AssetId,
    pub price: Ray,
    pub source: SourceKind,
    pub block: u64,
    pub ts: u64,
}

/// Event `liq-oracle` emits into the hot path (GUIDE 06, WP 00C). Same layout
/// as [`Price`].
pub type PriceTick = Price;

/// Flat `Vec<Price>` indexed by global [`AssetId`]. Cheap [`Clone`]: no map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceVector(pub Vec<Price>);

impl PriceVector {
    /// Empty vector for triple-buffer init (GUIDE 06 §7b). Never filled with
    /// fabricated prices; sized at startup from the feed registry (WP 06A-1).
    #[must_use]
    pub const fn zeroed() -> Self {
        Self(Vec::new())
    }
}

/// Governance / timelock parameter change scheduled at an execution block
/// (GUIDE 08 §5, WP 08B). The second hot-path event type from `liq-oracle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledParamChange {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub execution_block: u64,
    pub crossing: Vec<PositionId>,
    pub trace: TraceId,
}

#[cfg(test)]
mod tests {
    use super::{Price, PriceVector};
    use std::mem::size_of;

    /// Oracle: GUIDE 06 §3 — `PriceVector` is a flat `Vec<Price>`, not a map.
    #[test]
    fn price_vector_is_flat_vec() {
        assert_eq!(size_of::<PriceVector>(), size_of::<Vec<Price>>());
        let px = PriceVector::zeroed();
        let cloned = px.clone();
        assert_eq!(px, cloned, "oracle: Def — Clone of an empty Vec is empty");
        assert!(
            cloned.0.is_empty(),
            "oracle: Def — zeroed is empty, never a fabricated price"
        );
    }
}
