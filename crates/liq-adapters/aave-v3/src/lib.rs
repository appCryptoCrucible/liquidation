//! Aave V3 adapter (WP 04B) — pinned to `aave-dao/aave-v3-origin` @
//! `8305565ae342f1773c42cd2e4593f175fe5968a0` (v3.7.0+1 / ≥3.5).

#![forbid(unsafe_code)]

pub mod apply;
pub mod config;
pub mod events;
pub mod health;
pub mod layout;
pub mod math;
pub mod quote;
pub mod solve;

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall, SolEvent};
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FlashRoute, Health, LegChoice,
    LiquidationLeg, LiquidationPlan, PositionRef, ProbeCall, Protocol, ProtocolError, Quote,
    Result, StateWriter,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId, Ray};

pub use config::{AssetConfig, Config, ConfigError, LiquidationParams, PoolConfig, SourcePin};

use crate::events::{cfg as ccfg, halt, oracle, pool, provider, sentinel, token};
use crate::math::hf_wad_to_ray;

sol! {
    function getUserAccountData(address user) external view returns (
        uint256 totalCollateralBase,
        uint256 totalDebtBase,
        uint256 availableBorrowsBase,
        uint256 currentLiquidationThreshold,
        uint256 ltv,
        uint256 healthFactor
    );
}

#[derive(Clone, Debug)]
pub struct AaveV3 {
    cfg: Config,
}

impl AaveV3 {
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
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

fn decode_probe(raw: &[u8]) -> Result<Ray> {
    let out =
        getUserAccountDataCall::abi_decode_returns(raw).map_err(|_| ProtocolError::ProbeDecode)?;
    hf_wad_to_ray(out.healthFactor)
}

impl LogSubscriber for AaveV3 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::new();
        for p in &self.cfg.pools {
            for t0 in [
                pool::Supply::SIGNATURE_HASH,
                pool::Withdraw::SIGNATURE_HASH,
                pool::Borrow::SIGNATURE_HASH,
                pool::Repay::SIGNATURE_HASH,
                pool::UserEModeSet::SIGNATURE_HASH,
                pool::ReserveUsedAsCollateralEnabled::SIGNATURE_HASH,
                pool::ReserveUsedAsCollateralDisabled::SIGNATURE_HASH,
                pool::FlashLoan::SIGNATURE_HASH,
                pool::LiquidationCall::SIGNATURE_HASH,
                pool::ReserveDataUpdated::SIGNATURE_HASH,
                pool::DeficitCovered::SIGNATURE_HASH,
                pool::MintedToTreasury::SIGNATURE_HASH,
                pool::DeficitCreated::SIGNATURE_HASH,
                pool::PositionManagerApproved::SIGNATURE_HASH,
                pool::PositionManagerRevoked::SIGNATURE_HASH,
                halt::Upgraded::SIGNATURE_HASH,
                halt::AdminChanged::SIGNATURE_HASH,
                halt::Initialized::SIGNATURE_HASH,
            ] {
                out.push(LogFilter {
                    address: p.address,
                    topic0: t0,
                });
            }
            for t0 in [
                ccfg::ReserveInitialized::SIGNATURE_HASH,
                ccfg::ReserveBorrowing::SIGNATURE_HASH,
                ccfg::ReserveFlashLoaning::SIGNATURE_HASH,
                ccfg::CollateralConfigurationChanged::SIGNATURE_HASH,
                ccfg::ReserveActive::SIGNATURE_HASH,
                ccfg::ReserveFrozen::SIGNATURE_HASH,
                ccfg::ReservePaused::SIGNATURE_HASH,
                ccfg::ReserveFactorChanged::SIGNATURE_HASH,
                ccfg::BorrowCapChanged::SIGNATURE_HASH,
                ccfg::SupplyCapChanged::SIGNATURE_HASH,
                ccfg::LiquidationProtocolFeeChanged::SIGNATURE_HASH,
                ccfg::LiquidationGracePeriodChanged::SIGNATURE_HASH,
                ccfg::LiquidationGracePeriodDisabled::SIGNATURE_HASH,
                ccfg::AssetCollateralInEModeChanged::SIGNATURE_HASH,
                ccfg::AssetBorrowableInEModeChanged::SIGNATURE_HASH,
                ccfg::AssetLtvzeroInEModeChanged::SIGNATURE_HASH,
                ccfg::EModeCategoryAdded::SIGNATURE_HASH,
                ccfg::EModeCategoryIsolationChanged::SIGNATURE_HASH,
                ccfg::ReserveInterestRateDataChanged::SIGNATURE_HASH,
                ccfg::PendingLtvChanged::SIGNATURE_HASH,
                ccfg::ATokenUpgraded::SIGNATURE_HASH,
                ccfg::VariableDebtTokenUpgraded::SIGNATURE_HASH,
                ccfg::FlashloanPremiumTotalUpdated::SIGNATURE_HASH,
                halt::Upgraded::SIGNATURE_HASH,
                halt::AdminChanged::SIGNATURE_HASH,
                halt::Initialized::SIGNATURE_HASH,
            ] {
                out.push(LogFilter {
                    address: p.configurator,
                    topic0: t0,
                });
            }
            for t0 in [
                oracle::AssetSourceUpdated::SIGNATURE_HASH,
                oracle::FallbackOracleUpdated::SIGNATURE_HASH,
                oracle::BaseCurrencySet::SIGNATURE_HASH,
                halt::Upgraded::SIGNATURE_HASH,
            ] {
                out.push(LogFilter {
                    address: p.oracle,
                    topic0: t0,
                });
            }
            for t0 in [
                provider::PoolUpdated::SIGNATURE_HASH,
                provider::PoolConfiguratorUpdated::SIGNATURE_HASH,
                provider::PriceOracleUpdated::SIGNATURE_HASH,
                provider::ACLManagerUpdated::SIGNATURE_HASH,
                provider::ACLAdminUpdated::SIGNATURE_HASH,
                provider::PriceOracleSentinelUpdated::SIGNATURE_HASH,
                provider::ProxyCreated::SIGNATURE_HASH,
                provider::AddressSet::SIGNATURE_HASH,
                provider::AddressSetAsProxy::SIGNATURE_HASH,
                halt::Upgraded::SIGNATURE_HASH,
                halt::AdminChanged::SIGNATURE_HASH,
            ] {
                out.push(LogFilter {
                    address: p.provider,
                    topic0: t0,
                });
            }
            if p.sentinel != Address::ZERO {
                out.push(LogFilter {
                    address: p.sentinel,
                    topic0: sentinel::GracePeriodUpdated::SIGNATURE_HASH,
                });
                out.push(LogFilter {
                    address: p.sentinel,
                    topic0: sentinel::SequencerOracleUpdated::SIGNATURE_HASH,
                });
            }
            if p.sequencer_oracle != Address::ZERO {
                out.push(LogFilter {
                    address: p.sequencer_oracle,
                    topic0: sentinel::AnswerUpdated::SIGNATURE_HASH,
                });
            }
        }
        let _ = token::BorrowAllowanceDelegated::SIGNATURE_HASH;
        out
    }
}

impl Protocol for AaveV3 {
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
        health::health_with(&self.cfg, pos, px)
    }

    fn liquidation_price(
        &self,
        pos: PositionRef<'_>,
        px: &PriceVector,
        asset: AssetId,
    ) -> Result<Option<Price>> {
        solve::liquidation_price(&self.cfg, pos, px, asset)
    }

    fn time_to_cross(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
        solve::time_to_cross(&self.cfg, pos, px)
    }

    fn quote(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
        quote::quote(&self.cfg, pos, px)
    }

    fn encode(
        &self,
        q: &Quote,
        legs: LegChoice,
        funding: &FlashRoute,
        recipient: Address,
    ) -> Result<LiquidationPlan> {
        if q.key.protocol != self.cfg.protocol {
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
        let pool = self
            .cfg
            .pool_by_market(q.key.market)
            .ok_or(ProtocolError::UnknownMarket(q.key.market))?;
        let debt_asset = self
            .cfg
            .underlying_of(repay.asset)
            .ok_or(ProtocolError::OracleSourceMismatch)?;
        let collateral_asset = self
            .cfg
            .underlying_of(seize.asset)
            .ok_or(ProtocolError::OracleSourceMismatch)?;
        let wire = |v: U256| u128::try_from(v).map_err(|_| ProtocolError::AmountTooLarge);
        Ok(LiquidationPlan {
            provider: funding.provider,
            flash_source: funding.source,
            debt_asset,
            flash_amount: wire(funding.amount)?,
            leg: LiquidationLeg {
                adapter: ExecutorAdapter::AaveV3,
                market: pool.address,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, pos: PositionRef<'_>) -> Result<ProbeCall> {
        let pool = self
            .cfg
            .pool_by_market(pos.key.market)
            .ok_or(ProtocolError::ProbeUnavailable)?;
        let call = getUserAccountDataCall { user: pos.key.user };
        Ok(ProbeCall {
            to: pool.address,
            data: Bytes::from(call.abi_encode()),
            decode: decode_probe,
        })
    }
}

use liq_protocol::Timestamp;
