//! Public-mempool OCR `transmit()` decoder (GUIDE 06 §5, WP 06D).
//!
//! Consumes 03B's SPSC ring (`rtrb` 4096, drop-oldest on the producer).
//! Median observation → [`SourceKind::PendingPublic`]. Pre-warm only:
//! never writes [`crate::canonical::CanonicalBook`].

use crate::canonical::answer_to_ray;
use crate::feeds::FeedSet;
use crate::{OracleError, Result};
use alloy_primitives::{Address, I256};
use alloy_sol_types::{SolCall, SolValue};
use liq_types::{AssetId, Confidence, PriceTick, SourceKind};

// Seam types live in the cycle-breaker `liq-types` so the 03B producer
// (`liq-node`) and this 06D consumer share them without depending on each
// other (06D review D1). Re-exported here to preserve the crate API.
pub use liq_types::{PendingTx, RING_CAP};

/// Selector `transmit(bytes32[3],bytes,bytes32[],bytes32[],bytes32)`.
pub const TRANSMIT_SELECTOR: [u8; 4] = [0x6f, 0xad, 0xcf, 0x72];

alloy_sol_types::sol! {
    function transmit(
        bytes32[3] reportContext,
        bytes report,
        bytes32[] rs,
        bytes32[] ss,
        bytes32 rawVs
    );
    /// OffchainAggregator `_decodeReport` (`bytes32,bytes32,int192[]`).
    struct OcrReport {
        bytes32 rawReportContext;
        bytes32 rawObservers;
        int192[] observations;
    }
}

/// Watch-list decoder. Linear scan (feed count is tiny).
pub struct MempoolOracle {
    targets: Vec<(Address, AssetId, u8)>,
}

impl MempoolOracle {
    /// Every configured aggregator (push + SVR standard/svr). SVR `transmit`
    /// is not expected here; if it appears it is still decoded, not invented.
    pub fn new(feeds: &FeedSet) -> Result<Self> {
        let mut targets = Vec::new();
        for f in &feeds.specs {
            for a in f.spec.watch()? {
                targets.push((a, f.asset, f.spec.decimals));
            }
        }
        Ok(Self { targets })
    }

    /// Drain until empty. Never blocks. Ticks are [`SourceKind::PendingPublic`].
    pub fn drain_prewarm(&self, rx: &mut rtrb::Consumer<PendingTx>, out: &mut Vec<PriceTick>) {
        while let Ok(tx) = rx.pop() {
            self.decode_into(&tx, out);
        }
    }

    fn decode_into(&self, tx: &PendingTx, out: &mut Vec<PriceTick>) {
        if tx.to.is_zero() {
            return;
        }
        for (agg, asset, decimals) in &self.targets {
            if *agg != tx.to {
                continue;
            }
            match median_answer(&tx.input) {
                Ok(ans) => match answer_to_ray(ans, *decimals) {
                    Ok(price) => out.push(PriceTick {
                        asset: *asset,
                        price,
                        source: SourceKind::PendingPublic {
                            tx: tx.hash,
                            confidence: Confidence::CERTAIN,
                        },
                        block: 0,
                        ts: 0,
                    }),
                    Err(e) => tracing::error!(
                        aggregator = %agg,
                        hash = %tx.hash,
                        error = %e,
                        "oracle: pending transmit answer is not a price"
                    ),
                },
                Err(e) => tracing::error!(
                    aggregator = %agg,
                    hash = %tx.hash,
                    error = %e,
                    "oracle: pending transmit() decode failed"
                ),
            }
        }
    }
}

fn median_answer(input: &[u8]) -> Result<I256> {
    let Some(sel) = input.get(..4) else {
        return Err(OracleError::BadTransmit);
    };
    if sel != TRANSMIT_SELECTOR && sel != transmitCall::SELECTOR {
        return Err(OracleError::BadTransmit);
    }
    let tail = input.get(4..).ok_or(OracleError::BadTransmit)?;
    let decoded = transmitCall::abi_decode_raw(tail).map_err(|_| OracleError::BadTransmit)?;
    median_from_report(decoded.report.as_ref())
}

fn median_from_report(report: &[u8]) -> Result<I256> {
    let decoded = OcrReport::abi_decode(report).map_err(|_| OracleError::BadTransmit)?;
    let mut obs = decoded.observations;
    if obs.is_empty() {
        return Err(OracleError::BadTransmit);
    }
    obs.sort_unstable();
    let mid = obs.len().checked_div(2).ok_or(OracleError::BadTransmit)?;
    let median = obs.get(mid).copied().ok_or(OracleError::BadTransmit)?;
    i192_to_i256(median)
}

fn i192_to_i256(v: alloy_primitives::aliases::I192) -> Result<I256> {
    let src = v.to_be_bytes::<24>();
    let fill = u8::from(v.is_negative()).saturating_mul(0xff);
    let mut wide = [fill; 32];
    let dst = wide.get_mut(8..).ok_or(OracleError::BadTransmit)?;
    if dst.len() != src.len() {
        return Err(OracleError::BadTransmit);
    }
    dst.copy_from_slice(&src);
    Ok(I256::from_be_bytes(wide))
}

#[cfg(test)]
mod tests {
    use super::{
        median_answer, median_from_report, MempoolOracle, OcrReport, PendingTx, RING_CAP,
        TRANSMIT_SELECTOR,
    };
    use crate::canonical::answer_to_ray;
    use crate::feeds::{FeedSet, FeedsConfig};
    use alloy_primitives::{address, b256, Address, Bytes, B256, I256};
    use alloy_sol_types::{SolCall, SolValue};
    use liq_config::{Intern, Registry};
    use liq_types::{Confidence, SourceKind};
    use std::path::PathBuf;

    const WETH_AGG: Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");
    const HASH: B256 = b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn oracle() -> MempoolOracle {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let feeds = FeedsConfig::load(&root.join("config/feeds")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let set = FeedSet::bind(&feeds, &intern, &reg).unwrap();
        MempoolOracle::new(&set).unwrap()
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

    fn transmit_input(answers: &[i64]) -> Bytes {
        let report = report_bytes(answers);
        let call = super::transmitCall {
            reportContext: [B256::ZERO; 3],
            report: report.to_vec().into(),
            rs: vec![],
            ss: vec![],
            rawVs: B256::ZERO,
        };
        // On-chain selector is 6fadcf72; alloy's keccak of `bytes32[3]` may differ.
        // Arg encoding is identical. Prepend the Chainlink selector.
        let mut out = Vec::from(TRANSMIT_SELECTOR);
        call.abi_encode_raw(&mut out);
        Bytes::from(out)
    }

    #[test]
    fn ring_cap_is_guide_4096() {
        assert_eq!(RING_CAP, 4096);
        assert_eq!(TRANSMIT_SELECTOR, [0x6f, 0xad, 0xcf, 0x72]);
    }

    #[test]
    fn ocr_median_is_upper_mid_after_sort() {
        let med = median_from_report(&report_bytes(&[1, 3, 2])).unwrap();
        assert_eq!(med, I256::try_from(2i64).unwrap());
        let even = median_from_report(&report_bytes(&[1, 2, 3, 4])).unwrap();
        assert_eq!(even, I256::try_from(3i64).unwrap());
        assert!(median_from_report(&report_bytes(&[])).is_err());
    }

    #[test]
    fn transmit_calldata_decodes_median() {
        let input = transmit_input(&[100_000_000, 100_000_000, 100_000_000]);
        let ans = median_answer(&input).unwrap();
        assert_eq!(ans, I256::try_from(100_000_000i64).unwrap());
        assert_eq!(answer_to_ray(ans, 8).unwrap(), liq_types::fixed::Ray::ONE);
    }

    #[test]
    fn drain_emits_pending_public_not_canonical() {
        let o = oracle();
        let (mut prod, mut cons) = rtrb::RingBuffer::<PendingTx>::new(RING_CAP);
        prod.push(PendingTx {
            hash: HASH,
            to: WETH_AGG,
            input: transmit_input(&[200_000_000_000]),
        })
        .unwrap();
        prod.push(PendingTx {
            hash: HASH,
            to: address!("0x0000000000000000000000000000000000000001"),
            input: transmit_input(&[1]),
        })
        .unwrap();
        let mut out = Vec::new();
        o.drain_prewarm(&mut cons, &mut out);
        assert!(!out.is_empty(), "WETH aggregator transmit must tick");
        for t in &out {
            match &t.source {
                SourceKind::PendingPublic { tx, confidence } => {
                    assert_eq!(*tx, HASH);
                    assert_eq!(*confidence, Confidence::CERTAIN);
                }
                other => panic!("expected PendingPublic, got {other:?}"),
            }
            assert_eq!(t.block, 0);
            assert_eq!(t.ts, 0, "pending has no inclusion ts");
        }
        assert_eq!(
            out[0].price,
            answer_to_ray(I256::try_from(200_000_000_000i64).unwrap(), 8).unwrap()
        );
    }

    #[test]
    fn known_aggregator_bad_calldata_emits_nothing() {
        let o = oracle();
        let (mut prod, mut cons) = rtrb::RingBuffer::<PendingTx>::new(RING_CAP);
        prod.push(PendingTx {
            hash: HASH,
            to: WETH_AGG,
            input: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
        })
        .unwrap();
        let mut out = Vec::new();
        o.drain_prewarm(&mut cons, &mut out);
        assert!(
            out.is_empty(),
            "oracle: Def — undecodable transmit is not a price"
        );
    }
}
