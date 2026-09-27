//! SVR hint matcher.
//!
//! Chainlink's searcher page: selector `6fadcf72` is `forward(address,bytes)`
//! on a per-node forwarder. The aggregator is the decoded `address` argument,
//! and it must be one of the configured SVR aggregators. The inner call is
//! `transmitSecondary` (`ba0cb29e`). The price is
//! `report.observations[len / 2]` — the index the page specifies. The array
//! is not re-sorted here.

use super::{MevShareError, Result};
use crate::canonical::answer_to_ray;
use crate::feeds::{FeedSet, Mechanism};
use alloy_primitives::{Address, Bytes, I256};
use alloy_sol_types::{SolCall, SolValue};
use liq_types::fixed::Ray;
use liq_types::{AssetId, MevShareHint, SourceKind};
use std::time::Instant;

alloy_sol_types::sol! {
    function forward(address to, bytes callData);
    function transmitSecondary(
        bytes32[3] reportContext,
        bytes report,
        bytes32[] rs,
        bytes32[] ss,
        bytes32 rawVs
    );
    struct SvrReport {
        uint32 observationsTimestamp;
        bytes32 observers;
        int192[] observations;
        int192 juelsPerFeeCoin;
    }
}

/// `forward(address,bytes)`.
pub const FORWARD_SELECTOR: [u8; 4] = forwardCall::SELECTOR;

/// `transmitSecondary(bytes32[3],bytes,bytes32[],bytes32[],bytes32)`.
pub const TRANSMIT_SECONDARY_SELECTOR: [u8; 4] = transmitSecondaryCall::SELECTOR;

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
}

/// A hint whose decoded `forward` destination is a configured SVR aggregator
/// and whose inner call is `transmitSecondary`.
#[derive(Clone, Debug)]
pub struct SvrMatch {
    pub target: SvrTarget,
    pub hint: MevShareHint,
    pub price: Ray,
    pub origin: PriceOrigin,
}

/// SVR rows only. The same aggregator on two markets is one target.
/// Two assets on one aggregator is a config error.
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
        if let Some(prev) = out.iter().find(|t: &&SvrTarget| t.aggregator == agg) {
            if prev.asset != f.asset {
                return Err(MevShareError::Oracle(crate::OracleError::Load(format!(
                    "SVR aggregator {agg:#x} bound to two assets"
                ))));
            }
            continue;
        }
        out.push(SvrTarget {
            aggregator: agg,
            asset: f.asset,
            decimals: f.spec.decimals,
        });
    }
    Ok(out)
}

/// Decode `forward`, require the address argument to be a configured SVR
/// aggregator, then require inner `transmitSecondary`. A forward aimed at
/// anything else is not this pipeline (`Ok(None)`). A forward at one of our
/// aggregators that does not decode is an error.
pub fn match_hint(hint: &MevShareHint, targets: &[SvrTarget]) -> Result<Option<SvrMatch>> {
    if let Some(sel) = hint.function_selector {
        if sel != FORWARD_SELECTOR {
            return Ok(None);
        }
    }
    let Some(data) = hint.call_data.as_ref() else {
        return Ok(None);
    };
    let Some((aggregator, inner)) = decode_forward(data)? else {
        return Ok(None);
    };
    let Some(target) = targets.iter().copied().find(|t| t.aggregator == aggregator) else {
        return Ok(None);
    };
    let Some(ans) = decode_secondary(&inner)? else {
        return Ok(None);
    };
    let price = answer_to_ray(ans, target.decimals)?;
    Ok(Some(SvrMatch {
        target,
        hint: hint.clone(),
        price,
        origin: PriceOrigin::Extracted,
    }))
}

fn decode_forward(data: &[u8]) -> Result<Option<(Address, Bytes)>> {
    let Some(sel) = data.get(..4) else {
        return Ok(None);
    };
    if sel != FORWARD_SELECTOR {
        return Ok(None);
    }
    let decoded = forwardCall::abi_decode(data).map_err(|_| MevShareError::BadCallData)?;
    Ok(Some((decoded.to, decoded.callData)))
}

fn decode_secondary(data: &Bytes) -> Result<Option<I256>> {
    let bytes = data.as_ref();
    let Some(sel) = bytes.get(..4) else {
        return Ok(None);
    };
    if sel != TRANSMIT_SECONDARY_SELECTOR {
        return Ok(None);
    }
    let decoded =
        transmitSecondaryCall::abi_decode(bytes).map_err(|_| MevShareError::BadCallData)?;
    Ok(Some(median_from_report(&decoded.report)?))
}

fn median_from_report(report: &[u8]) -> Result<I256> {
    let decoded = SvrReport::abi_decode(report).map_err(|_| MevShareError::BadCallData)?;
    if decoded.observations.is_empty() {
        return Err(MevShareError::BadCallData);
    }
    let mid = decoded
        .observations
        .len()
        .checked_div(2)
        .ok_or(MevShareError::BadCallData)?;
    let median = decoded
        .observations
        .get(mid)
        .copied()
        .ok_or(MevShareError::BadCallData)?;
    median
        .to_string()
        .parse::<I256>()
        .map_err(|_| MevShareError::BadCallData)
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
            from: m.hint.from,
        },
        deadline,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        forwardCall, match_hint, median_from_report, publish_source, transmitSecondaryCall,
        PriceOrigin, SvrReport, SvrTarget, FORWARD_SELECTOR, TRANSMIT_SECONDARY_SELECTOR,
    };
    use alloy_primitives::{address, b256, Bytes, B256, I256};
    use alloy_sol_types::{SolCall, SolValue};
    use liq_types::fixed::Ray;
    use liq_types::{AssetId, MevShareHint, SourceKind};

    const AGG: alloy_primitives::Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");
    const FWD: alloy_primitives::Address = address!("0x45ab36b69e02e59d3c49b863b31f530c991dd554");
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
    ) -> MevShareHint {
        MevShareHint {
            hash: HASH,
            to,
            function_selector: sel,
            call_data: call,
            logs: None,
            from: None,
        }
    }

    fn report_bytes(answers: &[i64]) -> Bytes {
        let observations = answers
            .iter()
            .map(|a| (*a).try_into().expect("i64 fits int192"))
            .collect();
        let r = SvrReport {
            observationsTimestamp: 0,
            observers: B256::ZERO,
            observations,
            juelsPerFeeCoin: 0i64.try_into().expect("zero fits int192"),
        };
        Bytes::from(r.abi_encode())
    }

    fn forward_bytes(agg: alloy_primitives::Address, answers: &[i64]) -> Bytes {
        let report = report_bytes(answers);
        let inner = transmitSecondaryCall {
            reportContext: [B256::ZERO, B256::ZERO, B256::ZERO],
            report,
            rs: vec![],
            ss: vec![],
            rawVs: B256::ZERO,
        };
        Bytes::from(
            forwardCall {
                to: agg,
                callData: Bytes::from(inner.abi_encode()),
            }
            .abi_encode(),
        )
    }

    #[test]
    fn selectors_match_the_searcher_page() {
        assert_eq!(FORWARD_SELECTOR, [0x6f, 0xad, 0xcf, 0x72]);
        assert_eq!(TRANSMIT_SECONDARY_SELECTOR, [0xba, 0x0c, 0xb2, 0x9e]);
    }

    /// The aggregator is the `forward` argument. `hint.to` is the forwarder.
    #[test]
    fn matches_decoded_aggregator_not_tx_to() {
        let t = [target()];
        let cd = forward_bytes(AGG, &[100_000_000]);
        let inner = cd.get(100..104).expect("inner selector offset");
        assert_eq!(inner, TRANSMIT_SECONDARY_SELECTOR);

        assert!(match_hint(&hint(None, None, None), &t).unwrap().is_none());
        assert!(match_hint(&hint(Some(AGG), None, None), &t)
            .unwrap()
            .is_none());
        assert!(match_hint(&hint(Some(FWD), Some([0x11, 0x22, 0x33, 0x44]), None), &t)
            .unwrap()
            .is_none());

        let other = Bytes::from(
            forwardCall {
                to: AGG,
                callData: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            }
            .abi_encode(),
        );
        assert!(
            match_hint(&hint(Some(FWD), Some(FORWARD_SELECTOR), Some(other)), &t)
                .unwrap()
                .is_none(),
            "a forward that is not transmitSecondary is not an SVR update"
        );

        let wrong_agg = forward_bytes(
            address!("0x00000000000000000000000000000000000000aa"),
            &[100_000_000],
        );
        assert!(match_hint(
            &hint(Some(FWD), Some(FORWARD_SELECTOR), Some(wrong_agg)),
            &t
        )
        .unwrap()
        .is_none());

        let extracted = match_hint(&hint(Some(FWD), Some(FORWARD_SELECTOR), Some(cd)), &t)
            .unwrap()
            .expect("forward to configured aggregator");
        assert_eq!(extracted.origin, PriceOrigin::Extracted);
        assert_eq!(extracted.target.aggregator, AGG);
        assert_eq!(extracted.hint.to, Some(FWD));
        assert_eq!(extracted.price, Ray::ONE);

        let src = publish_source(&extracted, std::time::Instant::now());
        match src {
            SourceKind::SvrAnnounced { hint, .. } => {
                assert!(hint.logs.is_none());
                assert_eq!(hint.hash, HASH);
                assert!(hint.call_data.is_some());
            }
            other => panic!("expected SvrAnnounced, got {other:?}"),
        }
    }

    /// Page: `observations[len/2]`. The array is not re-sorted.
    #[test]
    fn median_is_the_documented_index() {
        let sorted = median_from_report(&report_bytes(&[1, 2, 3])).unwrap();
        assert_eq!(sorted, I256::try_from(2i64).unwrap());
        let unsorted = median_from_report(&report_bytes(&[1, 3, 2])).unwrap();
        assert_eq!(unsorted, I256::try_from(3i64).unwrap());
        let even = median_from_report(&report_bytes(&[1, 2, 3, 4])).unwrap();
        assert_eq!(even, I256::try_from(3i64).unwrap());
        assert!(median_from_report(&report_bytes(&[])).is_err());
    }
}
