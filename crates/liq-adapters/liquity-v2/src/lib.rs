//! Liquity V2 (BOLD) adapter — WP 15C-liquity.
//! Pin: `liquity/bold` @ `c8a5a4ee2e9dc024905856b6698a77d849c68c7e`.
//!
//! Liquidator PnL is **gas compensation only**. The Stability Pool is the
//! counterparty: the caller does not repay BOLD and does not seize trove
//! collateral. `encode` emits [`ExecutorAdapter::LiquityV2`] (id 5). Tail is
//! the full uint256 trove id (assembled from `TroveExtra`; `PositionKey.user`
//! holds only the low 160 bits). `max_repay` is 0 — the Stability Pool is
//! the counterparty.

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
use alloy_sol_types::SolEvent;
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FlashRoute, Health, LegChoice,
    LiquidationLeg, LiquidationPlan, PositionRef, ProbeCall, Protocol, ProtocolError, Quote,
    Result, StateWriter, Timestamp,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId};

pub use config::{AssetConfig, BranchConfig, Config, ConfigError, Emitter, RegistryRpc};

use crate::events::{self as ev, halt};

/// One Liquity V2 deployment (BOLD + collateral branches).
#[derive(Clone, Debug)]
pub struct LiquityV2 {
    cfg: Config,
}

impl LiquityV2 {
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
        if !cfg.live_registry_asserted {
            return Err(ConfigError::LiveRegistryUnasserted);
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

impl LogSubscriber for LiquityV2 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        const TM: [alloy_primitives::B256; 20] = [
            ev::TroveUpdated::SIGNATURE_HASH,
            ev::TroveOperation::SIGNATURE_HASH,
            ev::BatchedTroveUpdated::SIGNATURE_HASH,
            ev::BatchUpdated::SIGNATURE_HASH,
            ev::Liquidation::SIGNATURE_HASH,
            ev::Redemption::SIGNATURE_HASH,
            ev::RedemptionFeePaidToTrove::SIGNATURE_HASH,
            ev::TroveNFTAddressChanged::SIGNATURE_HASH,
            ev::BorrowerOperationsAddressChanged::SIGNATURE_HASH,
            ev::BoldTokenAddressChanged::SIGNATURE_HASH,
            ev::StabilityPoolAddressChanged::SIGNATURE_HASH,
            ev::GasPoolAddressChanged::SIGNATURE_HASH,
            ev::CollSurplusPoolAddressChanged::SIGNATURE_HASH,
            ev::SortedTrovesAddressChanged::SIGNATURE_HASH,
            ev::CollateralRegistryAddressChanged::SIGNATURE_HASH,
            ev::ActivePoolAddressChanged::SIGNATURE_HASH,
            ev::DefaultPoolAddressChanged::SIGNATURE_HASH,
            ev::PriceFeedAddressChanged::SIGNATURE_HASH,
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
        ];
        const SP: [alloy_primitives::B256; 11] = [
            ev::StabilityPoolBoldBalanceUpdated::SIGNATURE_HASH,
            ev::StabilityPoolCollBalanceUpdated::SIGNATURE_HASH,
            ev::DepositUpdated::SIGNATURE_HASH,
            ev::DepositOperation::SIGNATURE_HASH,
            ev::P_Updated::SIGNATURE_HASH,
            ev::S_Updated::SIGNATURE_HASH,
            ev::B_Updated::SIGNATURE_HASH,
            ev::ScaleUpdated::SIGNATURE_HASH,
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
        ];
        const BO: [alloy_primitives::B256; 5] = [
            ev::ShutDown::SIGNATURE_HASH,
            ev::TroveManagerAddressChanged::SIGNATURE_HASH,
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
        ];
        const PF: [alloy_primitives::B256; 4] = [
            ev::ShutDownFromOracleFailure::SIGNATURE_HASH,
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
        ];
        let n = self.cfg.branches.len().saturating_mul(
            TM.len()
                .saturating_add(1)
                .saturating_add(SP.len())
                .saturating_add(BO.len())
                .saturating_add(PF.len()),
        );
        let mut out = Vec::with_capacity(n);
        for b in &self.cfg.branches {
            for t0 in TM {
                out.push(LogFilter {
                    address: b.trove_manager,
                    topic0: t0,
                });
            }
            out.push(LogFilter {
                address: b.trove_manager,
                topic0: halt::Initialized::SIGNATURE_HASH,
            });
            for t0 in SP {
                out.push(LogFilter {
                    address: b.stability_pool,
                    topic0: t0,
                });
            }
            for t0 in BO {
                out.push(LogFilter {
                    address: b.borrower_operations,
                    topic0: t0,
                });
            }
            for t0 in PF {
                out.push(LogFilter {
                    address: b.price_feed,
                    topic0: t0,
                });
            }
        }
        out
    }
}

impl Protocol for LiquityV2 {
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
        quote::quote(pos, px, self.cfg.weth.asset, self.cfg.weth.decimals)
    }

    /// 10E ABI: `TroveManager.batchLiquidateTroves(uint256[] _troveArray)`.
    /// Empty array reverts `EmptyData`; no liquidatable id reverts
    /// `NothingToLiquidate` (Executor skips the leg).
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
        let branch = self
            .cfg
            .branch_by_market(q.key.market)
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
                adapter: ExecutorAdapter::LiquityV2,
                market: branch.trove_manager,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }
}
