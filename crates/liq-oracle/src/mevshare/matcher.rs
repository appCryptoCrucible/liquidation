//! Partial-hint matcher. `to` first; selector when present; price from
//! callData/logs else predicted. No searcher allowlist (GUIDE 06 §4).

use super::{MevShareError, Result};
use crate::canonical::answer_to_ray;
use crate::feeds::{FeedSet, IAggregator, Mechanism, ANSWER_UPDATED_TOPIC0};
use alloy_primitives::{Address, Bytes, Log, I256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use liq_types::fixed::Ray;
use liq_types::{AssetId, MevShareHint, SourceKind};
use std::time::Instant;

/// `transmit(bytes32[3],bytes32[],bytes32[],bytes32)` — GUIDE 06 §4.
pub const TRANSMIT_SELECTOR: [u8; 4] = [0x6f, 0xad, 0xcf, 0x72];

/// One configured SVR aggregator the matcher may bind a hint to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SvrTarget {
    pub aggregator: Address,
    pub asset: AssetId,
    pub decimals: u8,
}

/// Where the announced price came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PriceOrigin {
    Extracted,
    Predicted,
}

/// A hint that is an SVR `transmit` for a configured aggregator.
#[derive(Clone, Debug)]
pub struct SvrMatch {
    pub target: SvrTarget,
    pub hint: MevShareHint,
    pub price: Ray,
    pub origin: PriceOrigin,
}

/// SVR rows only. Push-feed aggregators are not this path.
pub fn svr_targets(feeds: &FeedSet) -> Result<Vec<SvrTarget>> {
    let mut out = Vec::new();
    for f in &feeds.specs {
        if !matches!(f.spec.mechanism, Mechanism::ChainlinkSvr) {
            continue;
        }
        let Some(agg) = f.spec.svr_aggregator else {
            return Err(MevShareError::Oracle(crate::OracleError::NoAggregator {
                proxy: f.spec.proxy,
            }));
        };
        out.push(SvrTarget {
            aggregator: agg,
            asset: f.asset,
            decimals: f.spec.decimals,
        });
    }
    Ok(out)
}

/// Match on `to` == `svr_aggregator`. Selector `6fadcf72` when present.
/// Price: callData → logs → predicted. Present-but-undecodable is an error.
pub fn match_hint(
    hint: &MevShareHint,
    targets: &[SvrTarget],
    predicted: Option<Ray>,
) -> Result<Option<SvrMatch>> {
    let Some(to) = hint.to else {
        return Ok(None);
    };
    let Some(target) = targets.iter().copied().find(|t| t.aggregator == to) else {
        return Ok(None);
    };
    if let Some(sel) = hint.function_selector {
        if sel != TRANSMIT_SELECTOR {
            return Ok(None);
        }
    }
    let (price, origin) = resolve_price(hint, target, predicted)?;
    Ok(Some(SvrMatch {
        target,
        hint: hint.clone(),
        price,
        origin,
    }))
}

fn resolve_price(
    hint: &MevShareHint,
    target: SvrTarget,
    predicted: Option<Ray>,
) -> Result<(Ray, PriceOrigin)> {
    if let Some(data) = hint.call_data.as_ref() {
        let ans = answer_from_calldata(data)?;
        return Ok((answer_to_ray(ans, target.decimals)?, PriceOrigin::Extracted));
    }
    if let Some(logs) = hint.logs.as_ref() {
        let ans = answer_from_logs(logs, target.aggregator)?;
        return Ok((answer_to_ray(ans, target.decimals)?, PriceOrigin::Extracted));
    }
    let px = predicted.ok_or(MevShareError::MissingPredicted)?;
    Ok((px, PriceOrigin::Predicted))
}

alloy_sol_types::sol! {
    function transmit(
        bytes32[3] reportContext,
        bytes report,
        bytes32[] rs,
        bytes32[] ss,
        bytes32 rawVs
    );
    struct OcrReport {
        bytes32 rawReportContext;
        bytes32 rawObservers;
        int192[] observations;
    }
}

fn answer_from_calldata(data: &Bytes) -> Result<I256> {
    let bytes = data.as_ref();
    if let Some(sel) = bytes.get(..4) {
        if sel == TRANSMIT_SELECTOR || sel == transmitCall::SELECTOR {
            if let Some(tail) = bytes.get(4..) {
                if let Ok(decoded) = transmitCall::abi_decode(tail) {
                    return median_from_report(&decoded.report);
                }
            }
        }
    }
    if let Ok(ans) = median_from_report(bytes) {
        return Ok(ans);
    }
    if let Some(tail) = bytes.get(4..) {
        if let Ok(ans) = median_from_report(tail) {
            return Ok(ans);
        }
    }
    Err(MevShareError::BadCallData)
}

fn median_from_report(report: &[u8]) -> Result<I256> {
    let decoded = OcrReport::abi_decode(report).map_err(|_| MevShareError::BadCallData)?;
    let mut obs = decoded.observations;
    if obs.is_empty() {
        return Err(MevShareError::BadCallData);
    }
    obs.sort_unstable();
    let mid = obs.len().checked_div(2).ok_or(MevShareError::BadCallData)?;
    let median = obs.get(mid).copied().ok_or(MevShareError::BadCallData)?;
    // Single conversion for the median (was per-observation in the original: ~31 string allocs → 1)
    median
        .to_string()
        .parse::<I256>()
        .map_err(|_| MevShareError::BadCallData)
}

fn answer_from_logs(logs: &[Log], aggregator: Address) -> Result<I256> {
    for log in logs {
        if log.address != aggregator {
            continue;
        }
        let Some(t0) = log.topics().first() else {
            continue;
        };
        if *t0 != ANSWER_UPDATED_TOPIC0 {
            continue;
        }
        let ev = IAggregator::AnswerUpdated::decode_log(log)
            .map_err(|_| MevShareError::NoAnswerUpdated(aggregator))?;
        return Ok(ev.current);
    }
    Err(MevShareError::NoAnswerUpdated(aggregator))
}

/// `SourceKind` for the published vector: hint **without** logs (06A-1/06B).
#[must_use]
pub fn publish_source(m: &SvrMatch, deadline: Instant) -> SourceKind {
    SourceKind::SvrAnnounced {
        hint: MevShareHint {
            hash: m.hint.hash,
            to: m.hint.to,
            function_selector: m.hint.function_selector,
            call_data: m.hint.call_data.clone(),
            logs: None,
        },
        deadline,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        match_hint, median_from_report, publish_source, OcrReport, PriceOrigin, SvrTarget,
        TRANSMIT_SELECTOR,
    };
    use alloy_primitives::{address, b256, Bytes, Log, B256, I256};
    use alloy_sol_types::SolValue;
    use liq_types::fixed::Ray;
    use liq_types::{AssetId, MevShareHint, SourceKind};

    const AGG: alloy_primitives::Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");
    const HASH: B256 = b256!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    fn target() -> SvrTarget {
        SvrTarget {
            aggregator: AGG,
            asset: AssetId(0),
            decimals: 8,
        }
    }

    fn hint(
        to: Option<alloy_primitives::Address>,
        sel: Option<[u8; 4]>,
        call: Option<Bytes>,
        logs: Option<Vec<Log>>,
    ) -> MevShareHint {
        MevShareHint {
            hash: HASH,
            to,
            function_selector: sel,
            call_data: call,
            logs,
        }
    }

    fn report_bytes(answers: &[i64]) -> Bytes {
        let observations = answers
            .iter()
            .map(|a| (*a).try_into().expect("i64 fits int192"))
            .collect();
        let r = OcrReport {
            rawReportContext: B256::ZERO,
            rawObservers: B256::ZERO,
            observations,
        };
        Bytes::from(r.abi_encode())
    }

    /// Oracle: GUIDE 06 §4 — to-only, to+selector, to+callData (partial hints).
    #[test]
    fn matcher_partial_hints() {
        let t = [target()];
        let pred = Ray::from_raw(alloy_primitives::U256::from(2u64));

        assert!(
            match_hint(&hint(None, None, None, None), &t, Some(pred))
                .unwrap()
                .is_none(),
            "oracle: GUIDE-06 match on `to` first — absent to is not a match"
        );

        let to_only = match_hint(&hint(Some(AGG), None, None, None), &t, Some(pred))
            .unwrap()
            .expect("to-only");
        assert_eq!(to_only.origin, PriceOrigin::Predicted);
        assert_eq!(to_only.price, pred);

        let other_sel = match_hint(
            &hint(Some(AGG), Some([0x11, 0x22, 0x33, 0x44]), None, None),
            &t,
            Some(pred),
        )
        .unwrap();
        assert!(
            other_sel.is_none(),
            "oracle: selector present must be 6fadcf72"
        );

        let with_sel = match_hint(
            &hint(Some(AGG), Some(TRANSMIT_SELECTOR), None, None),
            &t,
            Some(pred),
        )
        .unwrap()
        .expect("to+selector");
        assert_eq!(with_sel.origin, PriceOrigin::Predicted);

        assert!(
            match_hint(&hint(Some(AGG), None, None, None), &t, None).is_err(),
            "oracle: Def — predicted required when callData/logs absent"
        );

        let cd = report_bytes(&[100_000_000]);
        let extracted = match_hint(
            &hint(Some(AGG), Some(TRANSMIT_SELECTOR), Some(cd), None),
            &t,
            None,
        )
        .unwrap()
        .expect("to+callData");
        assert_eq!(extracted.origin, PriceOrigin::Extracted);
        assert_eq!(extracted.price, Ray::ONE);

        let src = publish_source(&extracted, std::time::Instant::now());
        match src {
            SourceKind::SvrAnnounced { hint, .. } => {
                assert!(
                    hint.logs.is_none(),
                    "oracle: 06A-1/06B — published Price must not embed hint logs"
                );
                assert_eq!(hint.hash, HASH);
            }
            other => panic!("expected SvrAnnounced, got {other:?}"),
        }

        let med = median_from_report(&report_bytes(&[1, 3, 2])).unwrap();
        assert_eq!(med, I256::try_from(2i64).unwrap());
    }
}
