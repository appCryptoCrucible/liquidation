//! Gearbox V3 adapter (WP 15C-gearbox).
//!
//! Pin: `Gearbox-protocol/core-v3` @ `510fc6541c3767ce825929b4c311826fe81d6fa5`.
//! ProtocolId **11**. MarketIds **4200..=4299** (catalog 4200 reserved;
//! credit managers 4201..=4299).
//!
//! Liquidator entry is **CreditFacadeV3**, never the manager
//! (`creditFacadeOnly`). Partial `encode` emits [`ExecutorAdapter::Gearbox`]
//! (id 7). Full close + MultiCall stays [`ProtocolError::ExecutorUnwired`].
//!
//! Partial quote is repay-and-seize from `_calcPartialLiquidationPayments`.
//! Full close needs on-account adapter `MultiCall` fills: fail closed (no
//! invented swap profits).

#![forbid(unsafe_code)]

pub mod apply;
pub mod config;
pub mod events;
pub mod health;
pub mod layout;
pub mod math;
pub mod quote;
pub mod solve;

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, SolEvent};
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FlashRoute, Health, LegChoice,
    LiquidationLeg, LiquidationPlan, PositionRef, ProbeCall, Protocol, ProtocolError, Quote,
    Result, StateWriter, Timestamp,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId, Ray};

pub use config::{
    AssetConfig, Config, ConfigError, Fees, ManagerConfig, RegistryRpc, TokenConfig,
    CATALOG_MARKET, D15_MANAGER_COUNT, FIRST_MANAGER_MARKET, LAST_MANAGER_MARKET,
    MAX_MANAGER_VERSION, MIN_MANAGER_VERSION, PROTOCOL,
};
/// Token not in the intern: health on an account holding it fails closed.
pub use layout::UNMAPPED_ASSET;

use crate::events::{
    configurator, facade, factory, halt, manager, pool, quota, views::ICreditManagerV3,
    DEBT_COLLATERAL_TASK,
};
use crate::layout::ManagerRow;
use crate::math::hf_ray;

/// One Gearbox V3 ContractsRegister deployment.
#[derive(Clone, Debug)]
pub struct GearboxV3 {
    cfg: Config,
}

impl GearboxV3 {
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
        if !cfg.live_fees_asserted {
            return Err(ConfigError::LiveFeesUnasserted);
        }
        cfg.validate()?;
        Ok(Self { cfg })
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.cfg
    }
}

#[must_use]
pub fn alloc_meter() -> Option<&'static (dyn Fn() -> u64 + Sync)> {
    None
}

fn push_topic(out: &mut Vec<LogFilter>, address: Address, topic0: alloy_primitives::B256) {
    out.push(LogFilter { address, topic0 });
}

fn halt_topics(out: &mut Vec<LogFilter>, address: Address) {
    push_topic(out, address, halt::Upgraded::SIGNATURE_HASH);
    push_topic(out, address, halt::AdminChanged::SIGNATURE_HASH);
    push_topic(out, address, halt::Initialized::SIGNATURE_HASH);
}

fn decode_probe(raw: &[u8]) -> Result<Ray> {
    let cdd = ICreditManagerV3::calcDebtAndCollateralCall::abi_decode_returns(raw)
        .map_err(|_| ProtocolError::ProbeDecode)?;
    if cdd.totalDebtUSD.is_zero() {
        return Ok(Ray::from_raw(U256::MAX));
    }
    hf_ray(cdd.twvUSD, cdd.totalDebtUSD)
}

fn encode_validate(
    cfg: &Config,
    q: &Quote,
    protocol: ProtocolId,
    legs: LegChoice,
    funding: &FlashRoute,
    recipient: Address,
) -> Result<()> {
    if q.key.protocol != protocol {
        return Err(ProtocolError::ProtocolMismatch);
    }
    let repay = q
        .repay_options
        .get(usize::from(legs.repay))
        .ok_or(ProtocolError::LegOutOfRange)?;
    let seize = q
        .seize_options
        .get(usize::from(legs.seize))
        .ok_or(ProtocolError::LegOutOfRange)?;
    if funding.callback.provider() != funding.provider {
        return Err(ProtocolError::CallbackProviderMismatch);
    }
    if recipient == Address::ZERO {
        return Err(ProtocolError::ZeroRecipient);
    }
    if funding.asset != repay.asset {
        return Err(ProtocolError::FundingAssetMismatch);
    }
    if funding.amount < repay.max_repay {
        return Err(ProtocolError::FundingShort);
    }
    let _ = cfg
        .manager_by_market(q.key.market)
        .ok_or(ProtocolError::UnknownMarket(q.key.market))?;
    let _ = cfg
        .underlying_of(repay.asset)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let _ = cfg
        .underlying_of(seize.asset)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let _ = u128::try_from(repay.max_repay).map_err(|_| ProtocolError::AmountTooLarge)?;
    let _ = u128::try_from(funding.amount).map_err(|_| ProtocolError::AmountTooLarge)?;
    Ok(())
}

impl LogSubscriber for GearboxV3 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::new();
        for r in &self.cfg.registers {
            halt_topics(&mut out, *r);
        }
        for m in &self.cfg.managers {
            for t0 in [
                facade::OpenCreditAccount::SIGNATURE_HASH,
                facade::CloseCreditAccount::SIGNATURE_HASH,
                facade::LiquidateCreditAccount::SIGNATURE_HASH,
                facade::PartiallyLiquidateCreditAccount::SIGNATURE_HASH,
                facade::AddCollateral::SIGNATURE_HASH,
                facade::WithdrawCollateral::SIGNATURE_HASH,
                facade::StartMultiCall::SIGNATURE_HASH,
                facade::WithdrawPhantomToken::SIGNATURE_HASH,
                facade::Execute::SIGNATURE_HASH,
                facade::FinishMultiCall::SIGNATURE_HASH,
                facade::Paused::SIGNATURE_HASH,
                facade::Unpaused::SIGNATURE_HASH,
            ] {
                push_topic(&mut out, m.facade, t0);
            }
            halt_topics(&mut out, m.facade);
            push_topic(
                &mut out,
                m.manager,
                manager::SetCreditConfigurator::SIGNATURE_HASH,
            );
            halt_topics(&mut out, m.manager);
            for t0 in [
                configurator::AddCollateralToken::SIGNATURE_HASH,
                configurator::SetTokenLiquidationThreshold::SIGNATURE_HASH,
                configurator::ScheduleTokenLiquidationThresholdRamp::SIGNATURE_HASH,
                configurator::ForbidToken::SIGNATURE_HASH,
                configurator::AllowToken::SIGNATURE_HASH,
                configurator::AllowAdapter::SIGNATURE_HASH,
                configurator::ForbidAdapter::SIGNATURE_HASH,
                configurator::UpdateFees::SIGNATURE_HASH,
                configurator::SetPriceOracle::SIGNATURE_HASH,
                configurator::SetCreditFacade::SIGNATURE_HASH,
                configurator::CreditConfiguratorUpgraded::SIGNATURE_HASH,
                configurator::SetBorrowingLimits::SIGNATURE_HASH,
                configurator::SetMaxDebtPerBlockMultiplier::SIGNATURE_HASH,
                configurator::SetLossPolicy::SIGNATURE_HASH,
                configurator::SetExpirationDate::SIGNATURE_HASH,
            ] {
                push_topic(&mut out, m.configurator, t0);
            }
            halt_topics(&mut out, m.configurator);
            for t0 in [
                pool::Borrow::SIGNATURE_HASH,
                pool::Repay::SIGNATURE_HASH,
                pool::AddCreditManager::SIGNATURE_HASH,
                pool::SetInterestRateModel::SIGNATURE_HASH,
                pool::SetPoolQuotaKeeper::SIGNATURE_HASH,
                pool::SetTotalDebtLimit::SIGNATURE_HASH,
                pool::SetCreditManagerDebtLimit::SIGNATURE_HASH,
                pool::SetWithdrawFee::SIGNATURE_HASH,
                pool::IncurUncoveredLoss::SIGNATURE_HASH,
                pool::Refer::SIGNATURE_HASH,
            ] {
                push_topic(&mut out, m.pool, t0);
            }
            halt_topics(&mut out, m.pool);
            for t0 in [
                factory::DeployCreditAccount::SIGNATURE_HASH,
                factory::TakeCreditAccount::SIGNATURE_HASH,
                factory::ReturnCreditAccount::SIGNATURE_HASH,
                factory::AddCreditManager::SIGNATURE_HASH,
                factory::Rescue::SIGNATURE_HASH,
            ] {
                push_topic(&mut out, m.factory, t0);
            }
            halt_topics(&mut out, m.factory);
            for t0 in [
                quota::UpdateQuota::SIGNATURE_HASH,
                quota::UpdateTokenQuotaRate::SIGNATURE_HASH,
                quota::SetGauge::SIGNATURE_HASH,
                quota::AddCreditManager::SIGNATURE_HASH,
                quota::AddQuotaToken::SIGNATURE_HASH,
                quota::SetTokenLimit::SIGNATURE_HASH,
                quota::SetQuotaIncreaseFee::SIGNATURE_HASH,
            ] {
                push_topic(&mut out, m.quota_keeper, t0);
            }
            halt_topics(&mut out, m.quota_keeper);
        }
        out
    }
}

impl Protocol for GearboxV3 {
    fn id(&self) -> ProtocolId {
        self.cfg.protocol
    }

    fn apply_log(&self, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
        apply::apply_log(&self.cfg, st, log)
    }

    fn backfill(&self, st: &mut dyn StateWriter, src: &dyn Archive, to: BlockNum) -> Result<()> {
        let filters = self.subscriptions();
        src.logs(&filters, 0, to, &mut |log| {
            apply::apply_log(&self.cfg, st, log).map(|_| ())
        })
    }

    fn health(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
        health::health(pos, px)
    }

    fn liquidation_price(
        &self,
        pos: PositionRef<'_>,
        px: &PriceVector,
        asset: AssetId,
    ) -> Result<Option<Price>> {
        solve::liquidation_price(pos, px, asset)
    }

    fn time_to_cross(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
        solve::time_to_cross(pos, px)
    }

    fn quote(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
        quote::quote(pos, px)
    }

    fn encode(
        &self,
        q: &Quote,
        legs: LegChoice,
        funding: &FlashRoute,
        recipient: Address,
    ) -> Result<LiquidationPlan> {
        encode_validate(&self.cfg, q, self.cfg.protocol, legs, funding, recipient)?;
        let repay = q
            .repay_options
            .get(usize::from(legs.repay))
            .ok_or(ProtocolError::LegOutOfRange)?;
        let seize = q
            .seize_options
            .get(usize::from(legs.seize))
            .ok_or(ProtocolError::LegOutOfRange)?;
        let (_, mgr) = self
            .cfg
            .manager_by_market(q.key.market)
            .ok_or(ProtocolError::UnknownMarket(q.key.market))?;
        let seize_token = self
            .cfg
            .underlying_of(seize.asset)
            .ok_or(ProtocolError::OracleSourceMismatch)?;
        if seize.asset == repay.asset || seize_token == mgr.underlying {
            return Err(ProtocolError::ExecutorUnwired);
        }
        let debt_asset = self
            .cfg
            .underlying_of(repay.asset)
            .ok_or(ProtocolError::OracleSourceMismatch)?;
        let wire = |v: U256| u128::try_from(v).map_err(|_| ProtocolError::AmountTooLarge);
        Ok(LiquidationPlan {
            provider: funding.provider,
            flash_source: funding.source,
            debt_asset,
            flash_amount: wire(funding.amount)?,
            leg: LiquidationLeg {
                adapter: ExecutorAdapter::Gearbox,
                market: mgr.facade,
                borrower: q.key.user,
                collateral_asset: seize_token,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, pos: PositionRef<'_>) -> Result<ProbeCall> {
        let row = pos.markets.first().ok_or(ProtocolError::ProbeUnavailable)?;
        let m: &ManagerRow = row.body()?;
        let to = crate::math::addr_from(m.manager);
        if to == Address::ZERO {
            return Err(ProtocolError::ProbeUnavailable);
        }
        Ok(ProbeCall {
            to,
            data: ICreditManagerV3::calcDebtAndCollateralCall {
                creditAccount: pos.key.user,
                task: DEBT_COLLATERAL_TASK,
            }
            .abi_encode()
            .into(),
            decode: decode_probe,
        })
    }
}
