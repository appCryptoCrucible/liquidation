//! Derived pricing adapters (GUIDE 06 §7). Quiet-bug factory: each adapter is
//! the protocol formula, subscribed via the ExEx log stream (not polling).

mod abi;
pub mod math;

use crate::canonical::pow10;
use crate::feeds::{asset_source_updated_topic0, IAaveOracle};
use crate::{OracleError, Result};
use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::DecodedLog;
use liq_types::fixed::{Ray, WAD};
use liq_types::{
    AssetId, Confidence, HaltReason, HaltScope, HaltSink, LogFilter, LogSubscriber, Price,
    PriceTick, PriceVector, SourceKind,
};
use smallvec::SmallVec;

pub use math::CrossOrder;

/// Alias of [`Confidence::CERTAIN`] for derived / on-chain rates (WP 06A-2).
pub const DERIVED_CONFIDENCE_CERTAIN: Confidence = Confidence::CERTAIN;

/// Rate units in the protocol contract.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RateScale {
    Wad,
    Ray,
}

/// Adapter formula. Deps are interned [`AssetId`]s — never guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Formula {
    /// `out = dep * rate / 1e18` (wstETH, rETH, LRT `getRate`).
    LstWad { underlying: AssetId },
    /// `out = dep * chi / 1e27` (sDAI `Pot.chi`).
    YieldChi { underlying: AssetId },
    /// Aave Capo: min(live ratio, growth cap) then LstWad-shaped mul.
    CappedLst {
        underlying: AssetId,
        snapshot_ratio: U256,
        snapshot_ts: u64,
        max_yearly_ratio_growth_percent: U256,
        scale: RateScale,
    },
    /// Fair LP, not spot. Needs `ReservesAndSupply` on the rate contract.
    LpFair { token0: AssetId, token1: AssetId },
    /// `X/USD` from `X/ETH` and `ETH/USD` with the protocol's mul/div order.
    Cross {
        base: AssetId,
        quote: AssetId,
        quote_decimals: u8,
        order: CrossOrder,
    },
}

/// One derived asset. `protocol_source` is what the protocol reads; swap ≠ this
/// is fail-closed (same as 06A-1 aggregator migration).
#[derive(Clone, Debug)]
pub struct DerivedSpec {
    pub asset: AssetId,
    pub asset_addr: Address,
    pub protocol_source: Address,
    pub rate_contract: Address,
    pub formula: Formula,
}

struct AdapterState {
    spec: DerivedSpec,
    rate: Option<U256>,
    lp: Option<(U256, U256, U256)>,
}

/// Working derived book. Writes [`SourceKind::Derived`] into a [`PriceVector`]
/// sized like the intern table; never embeds hint logs.
pub struct DerivedBook {
    adapters: Vec<AdapterState>,
    vector: PriceVector,
    by_rate: Vec<(Address, usize)>,
}

impl DerivedBook {
    pub fn new(specs: Vec<DerivedSpec>, n_assets: usize) -> Result<Self> {
        if n_assets > usize::from(u16::MAX) {
            return Err(OracleError::Load("too many assets".into()));
        }
        let mut vector = Vec::with_capacity(n_assets);
        for i in 0..n_assets {
            let id = u16::try_from(i).map_err(|_| OracleError::Load("asset id".into()))?;
            vector.push(Price {
                asset: AssetId(id),
                price: Ray::ZERO,
                source: SourceKind::Canonical,
                block: 0,
                ts: 0,
            });
        }
        let mut by_rate = Vec::with_capacity(specs.len());
        let mut adapters = Vec::with_capacity(specs.len());
        for (i, spec) in specs.into_iter().enumerate() {
            if spec.rate_contract.is_zero() || spec.protocol_source.is_zero() {
                return Err(OracleError::ZeroAggregator {
                    aggregator: spec.rate_contract,
                });
            }
            if usize::from(spec.asset.0) >= n_assets {
                return Err(OracleError::InternAsset {
                    asset: spec.asset_addr,
                });
            }
            by_rate.push((spec.rate_contract, i));
            adapters.push(AdapterState {
                spec,
                rate: None,
                lp: None,
            });
        }
        Ok(Self {
            adapters,
            vector: PriceVector(vector),
            by_rate,
        })
    }

    #[must_use]
    pub fn vector(&self) -> &PriceVector {
        &self.vector
    }

    /// `None` until a rate log has produced a price (`ts != 0`).
    #[must_use]
    pub fn price(&self, asset: AssetId) -> Option<&Price> {
        let p = self.vector.0.get(usize::from(asset.0))?;
        if p.ts == 0 {
            None
        } else {
            Some(p)
        }
    }

    /// Fold a routed log. Emits a [`PriceTick`] with [`SourceKind::Derived`] on
    /// rate change. `deps` is the canonical vector (same intern indices).
    pub fn apply_log(
        &mut self,
        log: &DecodedLog<'_>,
        deps: &PriceVector,
        sink: &dyn HaltSink,
    ) -> Result<Option<PriceTick>> {
        let Some(t0) = log.topics.first() else {
            return Ok(None);
        };
        if *t0 == asset_source_updated_topic0() {
            self.apply_source_swap(log, sink)?;
            return Ok(None);
        }
        let idx = self
            .by_rate
            .iter()
            .find(|(a, _)| *a == log.address)
            .map(|(_, i)| *i);
        let Some(idx) = idx else {
            return Ok(None);
        };
        self.apply_rate(idx, log, deps)
    }

    fn apply_source_swap(&mut self, log: &DecodedLog<'_>, sink: &dyn HaltSink) -> Result<()> {
        let ev =
            IAaveOracle::AssetSourceUpdated::decode_raw_log(log.topics.iter().copied(), log.data)
                .map_err(|_| OracleError::BadRateLog)?;
        for a in &self.adapters {
            if a.spec.asset_addr != ev.asset {
                continue;
            }
            if log.address != a.spec.protocol_source && log.address != a.spec.rate_contract {
                continue;
            }
            // Fail-closed on ANY source change, matching canonical.rs posture.
            // The rate_contract is a rate provider, not a valid replacement for the
            // protocol_source — a migration to a new source halts for re-validation.
            if ev.source != a.spec.protocol_source {
                sink.halt(HaltScope::Asset(a.spec.asset), HaltReason::ProxyUpgrade);
                return Err(OracleError::SourceMigrated {
                    asset: ev.asset,
                    expected: a.spec.protocol_source,
                    found: ev.source,
                });
            }
        }
        Ok(())
    }

    fn apply_rate(
        &mut self,
        idx: usize,
        log: &DecodedLog<'_>,
        deps: &PriceVector,
    ) -> Result<Option<PriceTick>> {
        let st = self
            .adapters
            .get_mut(idx)
            .ok_or_else(|| OracleError::Load("adapter index".into()))?;
        match &st.spec.formula {
            Formula::LpFair { .. } => {
                let ev = abi::ILpOracle::ReservesAndSupply::decode_raw_log(
                    log.topics.iter().copied(),
                    log.data,
                )
                .map_err(|_| OracleError::BadRateLog)?;
                if ev.totalSupply.is_zero() {
                    return Err(OracleError::ZeroLpSupply);
                }
                st.lp = Some((ev.reserve0, ev.reserve1, ev.totalSupply));
            }
            _ => {
                let rate = decode_rate(log)?;
                if rate.is_zero() {
                    return Err(OracleError::UnknownRate {
                        contract: log.address,
                    });
                }
                st.rate = Some(rate);
            }
        }
        self.recompute(idx, deps, log.block, log.timestamp)
    }

    fn recompute(
        &mut self,
        idx: usize,
        deps: &PriceVector,
        block: u64,
        ts: u64,
    ) -> Result<Option<PriceTick>> {
        if ts == 0 {
            return Err(OracleError::BadRateLog);
        }
        let st = self
            .adapters
            .get(idx)
            .ok_or_else(|| OracleError::Load("adapter index".into()))?;
        let (raw, dep_ids) = price_raw(st, deps, ts)?;
        if raw.is_zero() {
            return Err(OracleError::UnknownRate {
                contract: st.spec.rate_contract,
            });
        }
        let asset = st.spec.asset;
        let tick = Price {
            asset,
            price: Ray::from_raw(raw),
            source: SourceKind::Derived { deps: dep_ids },
            block,
            ts,
        };
        let slot = self
            .vector
            .0
            .get_mut(usize::from(asset.0))
            .ok_or(OracleError::InternAsset {
                asset: st.spec.asset_addr,
            })?;
        *slot = tick.clone();
        Ok(Some(tick))
    }
}

fn dep_price(deps: &PriceVector, id: AssetId) -> Result<U256> {
    let p = deps
        .0
        .get(usize::from(id.0))
        .ok_or(OracleError::MissingDep(id.0))?;
    if p.ts == 0 {
        return Err(OracleError::MissingDep(id.0));
    }
    Ok(p.price.raw())
}

fn price_raw(
    st: &AdapterState,
    deps: &PriceVector,
    now: u64,
) -> Result<(U256, SmallVec<[AssetId; 4]>)> {
    match &st.spec.formula {
        Formula::LstWad { underlying } => {
            let rate = st.rate.ok_or(OracleError::UnknownRate {
                contract: st.spec.rate_contract,
            })?;
            let px = dep_price(deps, *underlying)?;
            Ok((
                math::mul_wad_down(px, rate)?,
                SmallVec::from_slice(&[*underlying]),
            ))
        }
        Formula::YieldChi { underlying } => {
            let chi = st.rate.ok_or(OracleError::UnknownRate {
                contract: st.spec.rate_contract,
            })?;
            let px = dep_price(deps, *underlying)?;
            Ok((
                math::mul_ray_down(px, chi)?,
                SmallVec::from_slice(&[*underlying]),
            ))
        }
        Formula::CappedLst {
            underlying,
            snapshot_ratio,
            snapshot_ts,
            max_yearly_ratio_growth_percent,
            scale,
        } => {
            let live = st.rate.ok_or(OracleError::UnknownRate {
                contract: st.spec.rate_contract,
            })?;
            if now < *snapshot_ts {
                return Err(OracleError::BadRateLog);
            }
            let elapsed = U256::from(now.saturating_sub(*snapshot_ts));
            let scale_u = match scale {
                RateScale::Wad => WAD,
                RateScale::Ray => liq_types::fixed::RAY,
            };
            let px = dep_price(deps, *underlying)?;
            Ok((
                math::capped_lst(
                    px,
                    live,
                    *snapshot_ratio,
                    elapsed,
                    *max_yearly_ratio_growth_percent,
                    scale_u,
                )?,
                SmallVec::from_slice(&[*underlying]),
            ))
        }
        Formula::LpFair { token0, token1 } => {
            let (r0, r1, supply) = st.lp.ok_or(OracleError::UnknownRate {
                contract: st.spec.rate_contract,
            })?;
            let p0 = dep_price(deps, *token0)?;
            let p1 = dep_price(deps, *token1)?;
            Ok((
                math::lp_fair(r0, r1, p0, p1, supply)?,
                SmallVec::from_slice(&[*token0, *token1]),
            ))
        }
        Formula::Cross {
            base,
            quote,
            quote_decimals,
            order,
        } => {
            let x = dep_price(deps, *base)?;
            let y = dep_price(deps, *quote)?;
            let denom = pow10(*quote_decimals)?;
            Ok((
                math::cross_rate(x, y, denom, *order)?,
                SmallVec::from_slice(&[*base, *quote]),
            ))
        }
    }
}

fn decode_rate(log: &DecodedLog<'_>) -> Result<U256> {
    let t0 = log.topics.first().copied().ok_or(OracleError::BadRateLog)?;
    if t0 == abi::ILido::TokenRebased::SIGNATURE_HASH {
        let ev = abi::ILido::TokenRebased::decode_raw_log(log.topics.iter().copied(), log.data)
            .map_err(|_| OracleError::BadRateLog)?;
        return math::pooled_by_shares(ev.postTotalEther, ev.postTotalShares, WAD)
            .map_err(Into::into);
    }
    if t0 == abi::IRocketNetworkBalances::BalancesUpdated::SIGNATURE_HASH {
        let ev = abi::IRocketNetworkBalances::BalancesUpdated::decode_raw_log(
            log.topics.iter().copied(),
            log.data,
        )
        .map_err(|_| OracleError::BadRateLog)?;
        return math::pooled_by_shares(ev.totalEth, ev.rethSupply, WAD).map_err(Into::into);
    }
    if t0 == abi::IPot::Drip::SIGNATURE_HASH {
        let ev = abi::IPot::Drip::decode_raw_log(log.topics.iter().copied(), log.data)
            .map_err(|_| OracleError::BadRateLog)?;
        return Ok(ev.chi);
    }
    if t0 == abi::IRateProvider::ExchangeRateUpdated::SIGNATURE_HASH {
        let ev = abi::IRateProvider::ExchangeRateUpdated::decode_raw_log(
            log.topics.iter().copied(),
            log.data,
        )
        .map_err(|_| OracleError::BadRateLog)?;
        return Ok(ev.newRate);
    }
    Err(OracleError::BadRateLog)
}

impl LogSubscriber for DerivedBook {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let t_src = asset_source_updated_topic0();
        let mut out = Vec::with_capacity(self.adapters.len().saturating_mul(2));
        for a in &self.adapters {
            let t_rate = match &a.spec.formula {
                Formula::LstWad { .. } | Formula::CappedLst { .. } => {
                    abi::ILido::TokenRebased::SIGNATURE_HASH
                }
                Formula::YieldChi { .. } => abi::IPot::Drip::SIGNATURE_HASH,
                Formula::LpFair { .. } => abi::ILpOracle::ReservesAndSupply::SIGNATURE_HASH,
                Formula::Cross { .. } => abi::IRateProvider::ExchangeRateUpdated::SIGNATURE_HASH,
            };
            // LRT uses the same WAD mul as LST but BalancesUpdated / ExchangeRateUpdated.
            let extra = match &a.spec.formula {
                Formula::LstWad { .. } | Formula::CappedLst { .. } => {
                    Some(abi::IRateProvider::ExchangeRateUpdated::SIGNATURE_HASH)
                }
                _ => None,
            };
            out.push(LogFilter {
                address: a.spec.rate_contract,
                topic0: t_rate,
            });
            if let Some(t) = extra {
                out.push(LogFilter {
                    address: a.spec.rate_contract,
                    topic0: t,
                });
                out.push(LogFilter {
                    address: a.spec.rate_contract,
                    topic0: abi::IRocketNetworkBalances::BalancesUpdated::SIGNATURE_HASH,
                });
            }
            out.push(LogFilter {
                address: a.spec.protocol_source,
                topic0: t_src,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{abi, DerivedBook, DerivedSpec, Formula, RateScale, DERIVED_CONFIDENCE_CERTAIN};
    use crate::feeds::IAaveOracle;
    use crate::OracleError;
    use alloy_primitives::{address, Address, U256};
    use alloy_sol_types::SolEvent;
    use liq_node::{DecodeArena, LogRouter, OwnedLog, Route};
    use liq_types::fixed::{Ray, RAY, WAD};
    use liq_types::{
        AssetId, Confidence, HaltReason, HaltScope, HaltSink, LogSubscriber, Price, PriceVector,
        SourceKind,
    };
    use smallvec::SmallVec;
    use std::sync::Mutex;

    const RATE: Address = address!("0x1111111111111111111111111111111111111111");
    const SRC: Address = address!("0x2222222222222222222222222222222222222222");
    const OUT: Address = address!("0x3333333333333333333333333333333333333333");

    struct Rec(Mutex<Vec<(HaltScope, HaltReason)>>);
    impl HaltSink for Rec {
        fn halt(&self, scope: HaltScope, reason: HaltReason) {
            self.0.lock().unwrap().push((scope, reason));
        }
    }

    fn px(id: u16, raw: U256, ts: u64) -> Price {
        Price {
            asset: AssetId(id),
            price: Ray::from_raw(raw),
            source: SourceKind::Canonical,
            block: 1,
            ts,
        }
    }

    fn deps_steth() -> PriceVector {
        PriceVector(vec![
            px(0, RAY, 1),
            px(1, U256::ZERO, 0),
            px(2, RAY, 1),
            px(3, RAY, 1),
        ])
    }

    fn owned(
        address: Address,
        ev_topics: Vec<alloy_primitives::B256>,
        data: Vec<u8>,
        block: u64,
        ts: u64,
    ) -> OwnedLog {
        let mut av = arrayvec::ArrayVec::<alloy_primitives::B256, 4>::new();
        for t in ev_topics {
            av.push(t);
        }
        OwnedLog {
            address,
            topics: av,
            data,
            block,
            timestamp: ts,
            tx_index: 0,
            log_index: 0,
        }
    }

    fn route_apply(
        book: &mut DerivedBook,
        log: &OwnedLog,
        deps: &PriceVector,
    ) -> Result<Option<liq_types::PriceTick>, OracleError> {
        let filters = book.subscriptions();
        struct Sub(Vec<liq_types::LogFilter>);
        impl LogSubscriber for Sub {
            fn subscriptions(&self) -> Vec<liq_types::LogFilter> {
                self.0.clone()
            }
        }
        let router = LogRouter::from_subscribers(&[&Sub(filters) as &dyn LogSubscriber]).unwrap();
        let arena = DecodeArena::with_capacity(4096);
        match router.route(&arena, log).unwrap() {
            Route::Hit { decoded, .. } => {
                let rec = Rec(Mutex::new(Vec::new()));
                book.apply_log(&decoded, deps, &rec)
            }
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    /// Oracle: GUIDE 06 §7 — TokenRebased → Derived tick, no Canonical source.
    #[test]
    fn lst_rate_log_emits_derived_tick() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::LstWad {
                underlying: AssetId(0),
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let ev = abi::ILido::TokenRebased {
            reportTimestamp: U256::from(1_700_000_000u64),
            timeElapsed: U256::from(1u64),
            preTotalShares: WAD,
            preTotalEther: WAD,
            postTotalShares: WAD,
            postTotalEther: WAD + WAD / U256::from(10u64),
            sharesMintedAsFees: U256::ZERO,
        };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(RATE, topics, ev.encode_data(), 9, 1_700_000_012);
        let tick = route_apply(&mut book, &log, &deps_steth())
            .unwrap()
            .expect("tick");
        assert_eq!(tick.asset, AssetId(1));
        assert_eq!(tick.ts, 1_700_000_012);
        match &tick.source {
            SourceKind::Derived { deps } => assert_eq!(deps.as_slice(), &[AssetId(0)]),
            other => panic!("expected Derived, got {other:?}"),
        }
        let expect =
            crate::derived::math::mul_wad_down(RAY, WAD + WAD / U256::from(10u64)).unwrap();
        assert_eq!(tick.price, Ray::from_raw(expect));
        assert_eq!(DERIVED_CONFIDENCE_CERTAIN, Confidence::CERTAIN);
        assert_eq!(Confidence::CERTAIN.0, 10_000);
    }

    /// Oracle: unknown rate (no log yet) is an error, not a fabricated 1.0.
    #[test]
    fn unknown_rate_is_error() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::YieldChi {
                underlying: AssetId(2),
            },
        };
        let book = DerivedBook::new(vec![spec], 4).unwrap();
        assert!(book.price(AssetId(1)).is_none());
    }

    /// Oracle: 06A-1 fail-closed — source ≠ registry/protocol_source.
    #[test]
    fn rate_source_swap_fail_closed() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::LstWad {
                underlying: AssetId(0),
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let new_src = address!("0x4444444444444444444444444444444444444444");
        let ev = IAaveOracle::AssetSourceUpdated {
            asset: OUT,
            source: new_src,
        };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(SRC, topics, ev.encode_data(), 1, 1);
        let rec = Rec(Mutex::new(Vec::new()));
        let filters = book.subscriptions();
        struct Sub(Vec<liq_types::LogFilter>);
        impl LogSubscriber for Sub {
            fn subscriptions(&self) -> Vec<liq_types::LogFilter> {
                self.0.clone()
            }
        }
        let router = LogRouter::from_subscribers(&[&Sub(filters) as &dyn LogSubscriber]).unwrap();
        let arena = DecodeArena::with_capacity(4096);
        match router.route(&arena, &log).unwrap() {
            Route::Hit { decoded, .. } => {
                let err = book.apply_log(&decoded, &deps_steth(), &rec).unwrap_err();
                assert!(
                    matches!(err, OracleError::SourceMigrated { found, .. } if found == new_src)
                );
            }
            other => panic!("{other:?}"),
        }
        let hits = rec.0.lock().unwrap();
        assert!(hits
            .iter()
            .any(|(s, r)| *s == HaltScope::Asset(AssetId(1)) && *r == HaltReason::ProxyUpgrade));
    }

    /// Oracle: `Drip(chi)` → sDAI = DAI * chi / RAY.
    #[test]
    fn sdai_drip_emits_derived() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::YieldChi {
                underlying: AssetId(2),
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let chi = RAY + RAY / U256::from(20u64);
        let ev = abi::IPot::Drip { chi };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(RATE, topics, ev.encode_data(), 3, 50);
        let tick = route_apply(&mut book, &log, &deps_steth())
            .unwrap()
            .expect("tick");
        assert_eq!(tick.price.raw(), chi);
        assert!(matches!(tick.source, SourceKind::Derived { .. }));
    }

    /// Oracle: missing dep is error, not last-known or 1.0.
    #[test]
    fn missing_dep_fail_closed() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::LstWad {
                underlying: AssetId(1),
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let ev = abi::IRateProvider::ExchangeRateUpdated {
            oldRate: U256::ZERO,
            newRate: WAD,
        };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(RATE, topics, ev.encode_data(), 1, 1);
        let err = route_apply(&mut book, &log, &deps_steth()).unwrap_err();
        assert!(matches!(err, OracleError::MissingDep(1)));
    }

    /// Oracle: LP `ReservesAndSupply` log → fair value, Derived deps = token0,1.
    #[test]
    fn lp_log_emits_derived() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::LpFair {
                token0: AssetId(0),
                token1: AssetId(2),
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let ev = abi::ILpOracle::ReservesAndSupply {
            reserve0: U256::from(100u64),
            reserve1: U256::from(400u64),
            totalSupply: U256::from(200u64),
        };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(RATE, topics, ev.encode_data(), 4, 8);
        let tick = route_apply(&mut book, &log, &deps_steth())
            .unwrap()
            .expect("tick");
        let expect = crate::derived::math::lp_fair(
            U256::from(100u64),
            U256::from(400u64),
            RAY,
            RAY,
            U256::from(200u64),
        )
        .unwrap();
        assert_eq!(tick.price.raw(), expect);
        match tick.source {
            SourceKind::Derived { deps } => {
                assert_eq!(
                    deps,
                    SmallVec::<[AssetId; 4]>::from_slice(&[AssetId(0), AssetId(2)])
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// Oracle: cross mul-then-div; Cross has no rate body — ExchangeRateUpdated unused.
    /// Cross reprices from dep vector; we still require a rate log to trigger.
    #[test]
    fn cross_rate_trigger_uses_protocol_order() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::Cross {
                base: AssetId(0),
                quote: AssetId(3),
                quote_decimals: 27,
                order: crate::derived::math::CrossOrder::MulThenDivDown,
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let ev = abi::IRateProvider::ExchangeRateUpdated {
            oldRate: U256::from(1u64),
            newRate: U256::from(1u64),
        };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(RATE, topics, ev.encode_data(), 1, 2);
        let tick = route_apply(&mut book, &log, &deps_steth())
            .unwrap()
            .expect("tick");
        assert_eq!(tick.price.raw(), RAY, "RAY * RAY / 10^27 = RAY");
        match tick.source {
            SourceKind::Derived { deps } => {
                assert_eq!(deps.as_slice(), &[AssetId(0), AssetId(3)]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Oracle: Capo min(live, maxRatio) on TokenRebased.
    #[test]
    fn capped_lst_applies_growth_cap() {
        let spec = DerivedSpec {
            asset: AssetId(1),
            asset_addr: OUT,
            protocol_source: SRC,
            rate_contract: RATE,
            formula: Formula::CappedLst {
                underlying: AssetId(0),
                snapshot_ratio: WAD,
                snapshot_ts: 0,
                max_yearly_ratio_growth_percent: U256::from(1_000u64),
                scale: RateScale::Wad,
            },
        };
        let mut book = DerivedBook::new(vec![spec], 4).unwrap();
        let ev = abi::ILido::TokenRebased {
            reportTimestamp: U256::from(1u64),
            timeElapsed: crate::derived::math::SECS_PER_YEAR,
            preTotalShares: WAD,
            preTotalEther: WAD,
            postTotalShares: WAD,
            postTotalEther: WAD * U256::from(2u64),
            sharesMintedAsFees: U256::ZERO,
        };
        let topics: Vec<_> = ev.encode_topics().into_iter().map(Into::into).collect();
        let log = owned(RATE, topics, ev.encode_data(), 1, 31_536_000);
        let tick = route_apply(&mut book, &log, &deps_steth())
            .unwrap()
            .expect("tick");
        let expect = crate::derived::math::capped_lst(
            RAY,
            WAD * U256::from(2u64),
            WAD,
            crate::derived::math::SECS_PER_YEAR,
            U256::from(1_000u64),
            WAD,
        )
        .unwrap();
        assert_eq!(tick.price.raw(), expect);
    }
}
