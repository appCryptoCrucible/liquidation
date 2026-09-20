//! GUIDE 05 §4 — detection vs winner position.

use super::classifier::EventFields;
use thiserror::Error;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DetectionHit {
    pub protocol: liq_types::ProtocolId,
    pub user: alloy_primitives::Address,
    pub first_block: u64,
    pub first_tx_index: u16,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimingSample {
    /// Winner block − our first HF<1 block. Positive ⇒ we were early.
    pub block_delta: i64,
    /// Winner tx_index − our tx_index on the same block. `None` if different blocks.
    pub intra_block_delta: Option<i32>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TimingError {
    #[error("block delta overflow")]
    Overflow,
}

pub fn timing(ev: &EventFields, hit: &DetectionHit) -> Result<TimingSample, TimingError> {
    let actual = i64::try_from(ev.block).map_err(|_| TimingError::Overflow)?;
    let det = i64::try_from(hit.first_block).map_err(|_| TimingError::Overflow)?;
    let block_delta = actual.checked_sub(det).ok_or(TimingError::Overflow)?;
    let intra_block_delta = if ev.block == hit.first_block {
        let w = i32::from(ev.tx_index);
        let o = i32::from(hit.first_tx_index);
        Some(w.checked_sub(o).ok_or(TimingError::Overflow)?)
    } else {
        None
    };
    Ok(TimingSample {
        block_delta,
        intra_block_delta,
    })
}

#[must_use]
pub(super) fn detected_at_or_before(ev: &EventFields, hit: &DetectionHit) -> bool {
    hit.protocol == ev.protocol
        && hit.user == ev.user
        && (hit.first_block < ev.block
            || (hit.first_block == ev.block && hit.first_tx_index <= ev.tx_index))
}

#[must_use]
pub(super) fn is_late(ev: &EventFields, hit: &DetectionHit) -> bool {
    hit.protocol == ev.protocol && hit.user == ev.user && !detected_at_or_before(ev, hit)
}
