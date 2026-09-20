//! Registry-driven liquidation decoder. Addresses come from `liq-config`
//! [`Registry`], never from a hand list.

use std::collections::HashMap;
use std::str::FromStr;

use alloy_primitives::{Address, B256, I256, U256};
use alloy_sol_types::SolEvent;
use liq_config::{Intern, OnChainId, ProtocolEntry, Registry};
use liq_types::{AssetId, LogFilter, LogSubscriber, MarketId, ProtocolId};

use crate::abi::{aave_v3, aave_v4, ajna, chainlink, compound_v2, euler, morpho, silo};
use crate::error::{Result, WatchError};
use crate::source::OwnedLog;
use crate::types::{CoverageDims, DecodedLiquidation, TriggerClass};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Family {
    AaveV3,
    Spark,
    AaveV4,
    MorphoBlue,
    CompoundV2,
    EulerV2,
    SiloV2,
    Ajna,
}

impl Family {
    fn parse(s: &str) -> Result<Option<Self>> {
        Ok(Some(match s {
            "aave-v3" => Self::AaveV3,
            "spark" => Self::Spark,
            "aave-v4" => Self::AaveV4,
            "morpho-blue" => Self::MorphoBlue,
            "compound-v2" => Self::CompoundV2,
            "euler-v2" => Self::EulerV2,
            "silo-v2" => Self::SiloV2,
            "ajna" => Self::Ajna,
            "sky-maker" => return Ok(None),
            other => return Err(WatchError::UnknownFamily(other.to_string())),
        }))
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AaveV3 => "aave-v3",
            Self::Spark => "spark",
            Self::AaveV4 => "aave-v4",
            Self::MorphoBlue => "morpho-blue",
            Self::CompoundV2 => "compound-v2",
            Self::EulerV2 => "euler-v2",
            Self::SiloV2 => "silo-v2",
            Self::Ajna => "ajna",
        }
    }
}

#[derive(Clone, Debug)]
struct Bind {
    family: Family,
    protocol: ProtocolId,
    market: MarketId,
    instance: String,
    repay: Address,
    seize: Address,
}

/// One decoder for every tracked family in the committed registry.
pub struct WatchDecoder {
    intern: Intern,
    by_addr: HashMap<(Address, B256), Bind>,
    morpho_by_id: HashMap<B256, Bind>,
    morpho_t0: B256,
    aggregators: HashMap<Address, bool>,
    filters: Vec<LogFilter>,
}

impl WatchDecoder {
    pub fn from_registry(reg: &Registry, intern: Intern) -> Result<Self> {
        let mut by_addr = HashMap::new();
        let mut morpho_by_id = HashMap::new();
        let mut filters = Vec::new();
        let morpho_t0 = morpho::Liquidate::SIGNATURE_HASH;
        let mut morpho_filter_done = false;

        for (key, entry) in &reg.protocols {
            let Some(fam) = Family::parse(&entry.family)? else {
                tracing::error!(family = %entry.family, "sky-maker has no Dog/Clipper in registry; not subscribed");
                continue;
            };
            let protocol = intern
                .protocol(&entry.family)
                .ok_or_else(|| WatchError::UnknownFamily(entry.family.clone()))?;
            let market = intern
                .markets()
                .iter()
                .find(|m| m.protocol == protocol && m.key == entry.market)
                .map(|m| m.id)
                .ok_or(WatchError::UnknownMarket)?;

            match fam {
                Family::AaveV3 | Family::Spark => {
                    let pool = addr_id(entry.market)?;
                    let t0 = aave_v3::LiquidationCall::SIGNATURE_HASH;
                    by_addr.insert(
                        (pool, t0),
                        Bind {
                            family: fam,
                            protocol,
                            market,
                            instance: key.clone(),
                            repay: Address::ZERO,
                            seize: Address::ZERO,
                        },
                    );
                    push_filter(&mut filters, pool, t0);
                }
                Family::AaveV4 => {
                    if extra_str(entry, "kind") != Some("spoke") {
                        continue;
                    }
                    let spoke = addr_id(entry.market)?;
                    let t0 = aave_v4::LiquidationCall::SIGNATURE_HASH;
                    let asset = extra_addr_opt(entry, "asset")?;
                    by_addr.insert(
                        (spoke, t0),
                        Bind {
                            family: fam,
                            protocol,
                            market,
                            instance: key.clone(),
                            repay: asset,
                            seize: asset,
                        },
                    );
                    push_filter(&mut filters, spoke, t0);
                }
                Family::MorphoBlue => {
                    let id = slot_id(entry.market)?;
                    let loan = extra_addr(entry, "loan_token")?;
                    let coll = extra_addr(entry, "collateral_token")?;
                    morpho_by_id.insert(
                        id,
                        Bind {
                            family: fam,
                            protocol,
                            market,
                            instance: key.clone(),
                            repay: loan,
                            seize: coll,
                        },
                    );
                    if !morpho_filter_done {
                        filters.push(LogFilter {
                            address: Address::ZERO,
                            topic0: morpho_t0,
                        });
                        morpho_filter_done = true;
                    }
                }
                Family::CompoundV2 => {
                    let t0 = compound_v2::LiquidateBorrow::SIGNATURE_HASH;
                    for ctoken in &entry.receipt_tokens {
                        by_addr.insert(
                            (*ctoken, t0),
                            Bind {
                                family: fam,
                                protocol,
                                market,
                                instance: key.clone(),
                                repay: Address::ZERO,
                                seize: Address::ZERO,
                            },
                        );
                        push_filter(&mut filters, *ctoken, t0);
                    }
                }
                Family::EulerV2 => {
                    let vault = addr_id(entry.market)?;
                    let t0 = euler::Liquidate::SIGNATURE_HASH;
                    let asset = extra_addr_opt(entry, "asset")?;
                    by_addr.insert(
                        (vault, t0),
                        Bind {
                            family: fam,
                            protocol,
                            market,
                            instance: key.clone(),
                            repay: asset,
                            seize: Address::ZERO,
                        },
                    );
                    push_filter(&mut filters, vault, t0);
                }
                Family::SiloV2 => {
                    let silo_addr = addr_id(entry.market)?;
                    let t0 = silo::LiquidationCall::SIGNATURE_HASH;
                    let asset = extra_addr_opt(entry, "asset")?;
                    by_addr.insert(
                        (silo_addr, t0),
                        Bind {
                            family: fam,
                            protocol,
                            market,
                            instance: key.clone(),
                            repay: asset,
                            seize: Address::ZERO,
                        },
                    );
                    push_filter(&mut filters, silo_addr, t0);
                }
                Family::Ajna => {
                    let pool = addr_id(entry.market)?;
                    let t0 = ajna::Kick::SIGNATURE_HASH;
                    let quote = extra_addr(entry, "quote_token")?;
                    let coll = extra_addr(entry, "collateral_token")?;
                    by_addr.insert(
                        (pool, t0),
                        Bind {
                            family: fam,
                            protocol,
                            market,
                            instance: key.clone(),
                            repay: quote,
                            seize: coll,
                        },
                    );
                    push_filter(&mut filters, pool, t0);
                }
            }
        }

        let mut aggregators = HashMap::new();
        let ans = chainlink::AnswerUpdated::SIGNATURE_HASH;
        for (proxy, o) in &reg.oracles {
            aggregators.insert(o.aggregator, o.svr);
            push_filter(&mut filters, o.aggregator, ans);
            let _ = proxy;
        }

        Ok(Self {
            intern,
            by_addr,
            morpho_by_id,
            morpho_t0,
            aggregators,
            filters,
        })
    }

    #[must_use]
    pub fn intern(&self) -> &Intern {
        &self.intern
    }

    #[must_use]
    pub fn is_aggregator(&self, addr: Address) -> bool {
        self.aggregators.contains_key(&addr)
    }

    #[must_use]
    pub fn aggregator_svr(&self, addr: Address) -> Option<bool> {
        self.aggregators.get(&addr).copied()
    }

    pub fn asset_id(&self, addr: Address) -> Result<AssetId> {
        self.intern
            .asset(addr)
            .ok_or(WatchError::UnknownAsset(addr))
    }

    pub fn decode_log(
        &self,
        log: &OwnedLog,
        trigger: TriggerClass,
        vol_ray: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let t0 = log
            .topics
            .first()
            .copied()
            .ok_or(WatchError::MalformedLog)?;
        if t0 == self.morpho_t0 {
            return self.decode_morpho(log, trigger, vol_ray);
        }
        let Some(bind) = self.by_addr.get(&(log.address, t0)) else {
            return Ok(None);
        };
        match bind.family {
            Family::AaveV3 | Family::Spark => self.decode_v3(log, bind, trigger, vol_ray),
            Family::AaveV4 => self.decode_v4(log, bind, trigger, vol_ray),
            Family::CompoundV2 => self.decode_comp(log, bind, trigger, vol_ray),
            Family::EulerV2 => self.decode_euler(log, bind, trigger, vol_ray),
            Family::SiloV2 => self.decode_silo(log, bind, trigger, vol_ray),
            Family::Ajna => self.decode_ajna(log, bind, trigger, vol_ray),
            Family::MorphoBlue => Ok(None),
        }
    }

    /// Classify trigger + realized vol from same-block aggregator logs (real samples only).
    pub fn coverage_from_oracle_logs(
        &self,
        block_logs: &[OwnedLog],
        liq_tx_index: u32,
    ) -> (TriggerClass, U256) {
        let t0 = chainlink::AnswerUpdated::SIGNATURE_HASH;
        let mut answers: Vec<I256> = Vec::new();
        let mut trigger = TriggerClass::Unobserved;
        for l in block_logs {
            let Some(topic) = l.topics.first() else {
                continue;
            };
            if *topic != t0 {
                continue;
            }
            let Some(&svr) = self.aggregators.get(&l.address) else {
                continue;
            };
            if l.tx_index <= liq_tx_index {
                trigger = if svr {
                    TriggerClass::SvrAuction
                } else {
                    TriggerClass::OraclePublic
                };
            }
            if let Ok(ev) =
                chainlink::AnswerUpdated::decode_raw_log(l.topics.iter().copied(), &l.data)
            {
                answers.push(ev.current);
            }
        }
        let vol = match answers.as_slice() {
            [a, b, ..] => vol_ray(*a, *b).unwrap_or(U256::ZERO),
            _ => U256::ZERO,
        };
        (trigger, vol)
    }

    fn decode_v3(
        &self,
        log: &OwnedLog,
        bind: &Bind,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev = aave_v3::LiquidationCall::decode_raw_log(log.topics.iter().copied(), &log.data)
            .map_err(|_| WatchError::Abi("LiquidationCall"))?;
        Ok(Some(self.finish(
            bind,
            log,
            ev.user,
            ev.liquidator,
            ev.debtAsset,
            ev.debtToCover,
            ev.collateralAsset,
            ev.liquidatedCollateralAmount,
            serde_json::json!({
                "collateralAsset": format!("{:#x}", ev.collateralAsset),
                "debtAsset": format!("{:#x}", ev.debtAsset),
                "user": format!("{:#x}", ev.user),
                "debtToCover": ev.debtToCover.to_string(),
                "liquidatedCollateralAmount": ev.liquidatedCollateralAmount.to_string(),
                "liquidator": format!("{:#x}", ev.liquidator),
                "receiveAToken": ev.receiveAToken,
            }),
            trigger,
            vol,
        )))
    }

    fn decode_v4(
        &self,
        log: &OwnedLog,
        bind: &Bind,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev = aave_v4::LiquidationCall::decode_raw_log(log.topics.iter().copied(), &log.data)
            .map_err(|_| WatchError::Abi("LiquidationCallV4"))?;
        Ok(Some(self.finish(
            bind,
            log,
            ev.user,
            ev.liquidator,
            bind.repay,
            ev.debtAmountRestored,
            bind.seize,
            ev.collateralAmountRemoved,
            serde_json::json!({
                "collateralReserveId": ev.collateralReserveId.to_string(),
                "debtReserveId": ev.debtReserveId.to_string(),
                "user": format!("{:#x}", ev.user),
                "liquidator": format!("{:#x}", ev.liquidator),
                "receiveShares": ev.receiveShares,
                "debtAmountRestored": ev.debtAmountRestored.to_string(),
                "drawnSharesLiquidated": ev.drawnSharesLiquidated.to_string(),
                "collateralAmountRemoved": ev.collateralAmountRemoved.to_string(),
                "collateralSharesLiquidated": ev.collateralSharesLiquidated.to_string(),
                "collateralSharesToLiquidator": ev.collateralSharesToLiquidator.to_string(),
            }),
            trigger,
            vol,
        )))
    }

    fn decode_morpho(
        &self,
        log: &OwnedLog,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev = morpho::Liquidate::decode_raw_log(log.topics.iter().copied(), &log.data)
            .map_err(|_| WatchError::Abi("Liquidate"))?;
        let Some(bind) = self.morpho_by_id.get(&ev.id) else {
            tracing::error!(id = %ev.id, "morpho Liquidate for market not in registry");
            return Err(WatchError::UnknownMarket);
        };
        Ok(Some(self.finish(
            bind,
            log,
            ev.borrower,
            ev.caller,
            bind.repay,
            ev.repaidAssets,
            bind.seize,
            ev.seizedAssets,
            serde_json::json!({
                "id": format!("{:#x}", ev.id),
                "caller": format!("{:#x}", ev.caller),
                "borrower": format!("{:#x}", ev.borrower),
                "repaidAssets": ev.repaidAssets.to_string(),
                "repaidShares": ev.repaidShares.to_string(),
                "seizedAssets": ev.seizedAssets.to_string(),
                "badDebtAssets": ev.badDebtAssets.to_string(),
                "badDebtShares": ev.badDebtShares.to_string(),
            }),
            trigger,
            vol,
        )))
    }

    fn decode_comp(
        &self,
        log: &OwnedLog,
        bind: &Bind,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev =
            compound_v2::LiquidateBorrow::decode_raw_log(log.topics.iter().copied(), &log.data)
                .map_err(|_| WatchError::Abi("LiquidateBorrow"))?;
        Ok(Some(self.finish(
            bind,
            log,
            ev.borrower,
            ev.liquidator,
            log.address,
            ev.repayAmount,
            ev.cTokenCollateral,
            ev.seizeTokens,
            serde_json::json!({
                "liquidator": format!("{:#x}", ev.liquidator),
                "borrower": format!("{:#x}", ev.borrower),
                "repayAmount": ev.repayAmount.to_string(),
                "cTokenCollateral": format!("{:#x}", ev.cTokenCollateral),
                "seizeTokens": ev.seizeTokens.to_string(),
            }),
            trigger,
            vol,
        )))
    }

    fn decode_euler(
        &self,
        log: &OwnedLog,
        bind: &Bind,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev = euler::Liquidate::decode_raw_log(log.topics.iter().copied(), &log.data)
            .map_err(|_| WatchError::Abi("EulerLiquidate"))?;
        Ok(Some(self.finish(
            bind,
            log,
            ev.violator,
            ev.liquidator,
            bind.repay,
            ev.repayAssets,
            ev.collateral,
            ev.yieldBalance,
            serde_json::json!({
                "liquidator": format!("{:#x}", ev.liquidator),
                "violator": format!("{:#x}", ev.violator),
                "collateral": format!("{:#x}", ev.collateral),
                "repayAssets": ev.repayAssets.to_string(),
                "yieldBalance": ev.yieldBalance.to_string(),
            }),
            trigger,
            vol,
        )))
    }

    fn decode_silo(
        &self,
        log: &OwnedLog,
        bind: &Bind,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev = silo::LiquidationCall::decode_raw_log(log.topics.iter().copied(), &log.data)
            .map_err(|_| WatchError::Abi("SiloLiquidationCall"))?;
        Ok(Some(self.finish(
            bind,
            log,
            ev.borrower,
            ev.liquidator,
            bind.repay,
            ev.repayDebtAssets,
            bind.seize,
            ev.withdrawCollateral,
            serde_json::json!({
                "liquidator": format!("{:#x}", ev.liquidator),
                "borrower": format!("{:#x}", ev.borrower),
                "repayDebtAssets": ev.repayDebtAssets.to_string(),
                "withdrawCollateral": ev.withdrawCollateral.to_string(),
            }),
            trigger,
            vol,
        )))
    }

    fn decode_ajna(
        &self,
        log: &OwnedLog,
        bind: &Bind,
        trigger: TriggerClass,
        vol: U256,
    ) -> Result<Option<DecodedLiquidation>> {
        let ev = ajna::Kick::decode_raw_log(log.topics.iter().copied(), &log.data)
            .map_err(|_| WatchError::Abi("Kick"))?;
        Ok(Some(self.finish(
            bind,
            log,
            ev.borrower,
            Address::ZERO,
            bind.repay,
            ev.amount,
            bind.seize,
            ev.locked,
            serde_json::json!({
                "borrower": format!("{:#x}", ev.borrower),
                "index": ev.index.to_string(),
                "amount": ev.amount.to_string(),
                "bond": ev.bond.to_string(),
                "locked": ev.locked.to_string(),
                "kickTime": ev.kickTime.to_string(),
            }),
            trigger,
            vol,
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        bind: &Bind,
        log: &OwnedLog,
        user: Address,
        liquidator: Address,
        repay: Address,
        repay_amt: U256,
        seize: Address,
        seize_amt: U256,
        raw: serde_json::Value,
        trigger: TriggerClass,
        vol: U256,
    ) -> DecodedLiquidation {
        let family_name = match self
            .intern
            .asset(seize)
            .and_then(|id| self.intern.asset_rec(id))
        {
            Some(a) => a.symbol.clone().unwrap_or_else(|| format!("{seize:#x}")),
            None => format!("{seize:#x}"),
        };
        DecodedLiquidation {
            family: bind.family.as_str().into(),
            instance: bind.instance.clone(),
            protocol: bind.protocol.0,
            market: bind.market.0,
            block: log.block,
            block_hash: log.block_hash,
            tx_hash: log.tx_hash,
            tx_index: log.tx_index,
            log_index: log.log_index,
            user,
            liquidator,
            repay_asset: repay,
            repay_amount: repay_amt.to_string(),
            seize_asset: seize,
            seize_amount: seize_amt.to_string(),
            raw,
            coverage: CoverageDims {
                instance: bind.instance.clone(),
                collateral_family: family_name,
                trigger_class: trigger,
                realized_vol_ray: vol.to_string(),
            },
        }
    }
}

impl LogSubscriber for WatchDecoder {
    fn subscriptions(&self) -> Vec<LogFilter> {
        self.filters.clone()
    }
}

fn push_filter(out: &mut Vec<LogFilter>, address: Address, topic0: B256) {
    let f = LogFilter { address, topic0 };
    if !out.contains(&f) {
        out.push(f);
    }
}

fn addr_id(id: OnChainId) -> Result<Address> {
    match id {
        OnChainId::Addr(a) => Ok(a),
        OnChainId::Slot(_) => Err(WatchError::UnknownMarket),
    }
}

fn slot_id(id: OnChainId) -> Result<B256> {
    match id {
        OnChainId::Slot(s) => Ok(s),
        OnChainId::Addr(_) => Err(WatchError::UnknownMarket),
    }
}

fn extra_str<'a>(e: &'a ProtocolEntry, k: &str) -> Option<&'a str> {
    e.extra.get(k).and_then(|v| v.as_str())
}

fn extra_addr(e: &ProtocolEntry, k: &'static str) -> Result<Address> {
    let s = extra_str(e, k).ok_or(WatchError::ExtraAddress(k))?;
    Address::from_str(s).map_err(|_| WatchError::ExtraAddress(k))
}

fn extra_addr_opt(e: &ProtocolEntry, k: &'static str) -> Result<Address> {
    match extra_str(e, k) {
        None => Ok(Address::ZERO),
        Some(s) => Address::from_str(s).map_err(|_| WatchError::ExtraAddress(k)),
    }
}

fn vol_ray(a: I256, b: I256) -> Result<U256> {
    let aa = abs_u256(a)?;
    let bb = abs_u256(b)?;
    if aa.is_zero() {
        return Ok(U256::ZERO);
    }
    let diff = if bb > aa {
        bb.saturating_sub(aa)
    } else {
        aa.saturating_sub(bb)
    };
    liq_types::fixed::mul_div(
        diff,
        liq_types::fixed::RAY,
        aa,
        liq_types::fixed::Rounding::Down,
    )
    .map_err(|e| WatchError::Rpc(e.to_string()))
}

fn abs_u256(v: I256) -> Result<U256> {
    Ok(v.unsigned_abs())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use liq_config::Registry;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn catalog_covers_every_registry_family() {
        let reg = Registry::from_path(&root().join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let dec = WatchDecoder::from_registry(&reg, intern).unwrap();
        let fams: std::collections::BTreeSet<_> =
            reg.protocols.values().map(|p| p.family.as_str()).collect();
        for f in fams {
            if f == "sky-maker" {
                continue;
            }
            Family::parse(f).unwrap().expect(f);
        }
        assert!(!dec.subscriptions().is_empty());
        assert!(dec
            .subscriptions()
            .iter()
            .any(|f| f.topic0 == aave_v3::LiquidationCall::SIGNATURE_HASH));
        assert!(dec
            .subscriptions()
            .iter()
            .any(|f| f.topic0 == aave_v4::LiquidationCall::SIGNATURE_HASH));
        assert!(dec
            .subscriptions()
            .iter()
            .any(|f| f.topic0 == morpho::Liquidate::SIGNATURE_HASH));
        use alloy_primitives::b256;
        assert_eq!(
            aave_v3::LiquidationCall::SIGNATURE_HASH,
            b256!("0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286")
        );
        assert_eq!(
            aave_v4::LiquidationCall::SIGNATURE_HASH,
            b256!("0x2a1f12d996f530f89d8038aa293f9fde81cac44b6dfd6225e3358d09b78a4a37")
        );
        assert_eq!(
            morpho::Liquidate::SIGNATURE_HASH,
            b256!("0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41")
        );
    }

    #[test]
    fn unknown_family_fails_closed() {
        assert!(matches!(
            Family::parse("not-a-protocol"),
            Err(WatchError::UnknownFamily(_))
        ));
    }
}
