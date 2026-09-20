//! `RecallReport` fold. Does not invent rates when fork facts are missing.

use super::classifier::{
    classify_decline, classify_miss, ClassifyError, DeclaredConfig, DeclineReason, EventFields,
    ForkFacts, MissClass,
};
use super::coverage::{in_scope_gate, InScopeGate, SliceRate};
use super::timing::{detected_at_or_before, is_late, timing, DetectionHit, TimingSample};
use alloy_primitives::U256;
use liq_types::{AssetId, ProtocolId};
use liq_watch::ActualLiquidation;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct RecallReport {
    pub total_actual: usize,
    pub detected_before: usize,
    pub detected_late: usize,
    pub never_detected: Vec<(EventFields, MissClass)>,
    pub declined: Vec<(EventFields, DeclineReason)>,
    pub timings: Vec<TimingSample>,
    pub slices: Vec<SliceRate>,
}

impl RecallReport {
    #[must_use]
    pub fn in_scope_misses(&self) -> usize {
        self.never_detected
            .iter()
            .filter(|(_, c)| *c == MissClass::InScope)
            .count()
    }

    #[must_use]
    pub fn in_scope_n(&self) -> usize {
        self.in_scope_misses().saturating_add(self.detected_before)
    }

    #[must_use]
    pub fn gate(&self) -> InScopeGate {
        in_scope_gate(self.in_scope_misses(), self.in_scope_n())
    }
}

#[derive(Clone, Debug)]
pub struct Observed {
    pub event: EventFields,
    pub instance: String,
    pub collateral_family: String,
    pub trigger: String,
    pub fork: Option<ForkFacts>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecallError {
    #[error(transparent)]
    Classify(#[from] ClassifyError),
    #[error("repay/seize amount is not a decimal integer: {0}")]
    Amount(String),
    #[error("observation count overflow")]
    Overflow,
}

pub fn event_from_actual(row: &ActualLiquidation) -> Result<EventFields, RecallError> {
    Ok(EventFields {
        protocol: ProtocolId(row.position.protocol),
        user: row.position.user,
        liquidator: row.liquidator,
        repay_asset: AssetId(row.repay_asset),
        repay_amount: parse_u256(&row.repay_amount)?,
        seize_asset: AssetId(row.seize_asset),
        seize_amount: parse_u256(&row.seize_amount)?,
        block: row.block,
        tx_index: row.tx_index,
    })
}

fn parse_u256(s: &str) -> Result<U256, RecallError> {
    s.parse::<U256>().map_err(|_| RecallError::Amount(s.into()))
}

fn hit_for(ev: &EventFields, hits: &[DetectionHit]) -> Option<DetectionHit> {
    hits.iter()
        .copied()
        .find(|h| h.protocol == ev.protocol && h.user == ev.user)
}

pub fn build_report(
    observed: &[Observed],
    hits: &[DetectionHit],
    cfg: &DeclaredConfig<'_>,
) -> Result<RecallReport, RecallError> {
    let mut detected_before = 0usize;
    let mut detected_late = 0usize;
    let mut never_detected = Vec::new();
    let mut declined = Vec::new();
    let mut timings = Vec::new();
    let mut slices: Vec<SliceRate> = Vec::new();

    for o in observed {
        let ev = o.event;
        ensure_slice(&mut slices, &o.instance);
        ensure_slice(&mut slices, &o.collateral_family);
        ensure_slice(&mut slices, &o.trigger);

        let hit = hit_for(&ev, hits);
        if let Some(h) = hit {
            timings.push(timing(&ev, &h).map_err(|_| RecallError::Overflow)?);
        }

        if hit.filter(|h| detected_at_or_before(&ev, h)).is_some() {
            match classify_decline(&ev, cfg, o.fork)? {
                Some(reason) => declined.push((ev, reason)),
                None => {
                    detected_before = detected_before
                        .checked_add(1)
                        .ok_or(RecallError::Overflow)?;
                    mark_in_scope_hit(&mut slices, o);
                }
            }
            continue;
        }
        if hit.filter(|h| is_late(&ev, h)).is_some() {
            detected_late = detected_late.checked_add(1).ok_or(RecallError::Overflow)?;
            continue;
        }
        let class = classify_miss(&ev, cfg, o.fork)?;
        never_detected.push((ev, class));
        if class == MissClass::InScope {
            mark_in_scope_miss(&mut slices, o);
        }
    }

    Ok(RecallReport {
        total_actual: observed.len(),
        detected_before,
        detected_late,
        never_detected,
        declined,
        timings,
        slices,
    })
}

fn ensure_slice(slices: &mut Vec<SliceRate>, key: &str) {
    if slices.iter().any(|s| s.key == key) {
        return;
    }
    slices.push(SliceRate {
        key: key.into(),
        in_scope: 0,
        in_scope_misses: 0,
    });
}

fn slice_mut<'a>(slices: &'a mut [SliceRate], key: &str) -> Option<&'a mut SliceRate> {
    slices.iter_mut().find(|s| s.key == key)
}

fn mark_in_scope_hit(slices: &mut [SliceRate], o: &Observed) {
    for k in [&o.instance, &o.collateral_family, &o.trigger] {
        if let Some(s) = slice_mut(slices, k) {
            s.in_scope = s.in_scope.saturating_add(1);
        }
    }
}

fn mark_in_scope_miss(slices: &mut [SliceRate], o: &Observed) {
    for k in [&o.instance, &o.collateral_family, &o.trigger] {
        if let Some(s) = slice_mut(slices, k) {
            s.in_scope = s.in_scope.saturating_add(1);
            s.in_scope_misses = s.in_scope_misses.saturating_add(1);
        }
    }
}

/// Receipt/fork fetch is not possible without A3. Callers must not invent facts.
pub fn require_fork(facts: Option<ForkFacts>) -> Result<ForkFacts, ClassifyError> {
    facts.ok_or(ClassifyError::ForkUnavailable)
}
