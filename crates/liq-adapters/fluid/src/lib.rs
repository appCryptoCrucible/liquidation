//! Fluid vault adapter.
//!
//! Pin: `Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`.
//!
//! Fluid liquidates a vault's whole underwater range at once; `liquidate`
//! takes no position id. So the unit here is **the vault**: one position per
//! vault, holding the liquidation the vault itself reports each block
//! (dead-address `liquidate` / `simulateLiquidate`, then the DEX's one-token
//! estimate on a smart side). Nothing of Fluid's tick tree is reproduced.
//!
//! All four vault types are quoted and encoded (id 6): T1 normal/normal, T2
//! smart collateral, T3 smart debt, T4 both, each with one debt token in and
//! one collateral token out. Native ETH is priced and routed as WETH; the
//! Executor unwraps and wraps around the call.
//!
//! ProtocolId **10**. MarketIds **4000..=4999** (catalog 4000, vault 1 → 4001).

#![forbid(unsafe_code)]

pub mod apply;
pub mod config;
pub mod events;
pub mod health;
pub mod layout;
pub mod quote;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FlashRoute, Health, LegChoice,
    LiquidationLeg, LiquidationPlan, MarketRows, PositionRef, ProbeCall, Protocol, ProtocolError,
    Quote, Result, StateAnswer, StateRead, StateWriter, Timestamp,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId};

pub use config::{AssetConfig, Config, ConfigError, FactoryRpc, VaultPin};
pub use layout::{
    VaultExtra, VaultRow, CATALOG_MARKET, FIRST_VAULT_MARKET, LAST_VAULT_MARKET, NATIVE_TOKEN,
    UNMAPPED_ASSET, VAULT_T1, VAULT_T2, VAULT_T3, VAULT_T4,
};

use crate::events::factory;

/// One Fluid VaultFactory deployment, bound live.
#[derive(Clone, Debug)]
pub struct Fluid {
    cfg: Config,
}

impl Fluid {
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
        if !cfg.live_bound {
            return Err(ConfigError::LiveUnbound);
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

impl LogSubscriber for Fluid {
    /// The factory's `VaultDeployed` only: every vault's state is read from
    /// the vault each block, so no vault log is needed.
    fn subscriptions(&self) -> Vec<LogFilter> {
        vec![LogFilter {
            address: self.cfg.factory,
            topic0: factory::VaultDeployed::SIGNATURE_HASH,
        }]
    }
}

impl Protocol for Fluid {
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

    /// No price crossing: the vault is re-read every block.
    fn liquidation_price(
        &self,
        _pos: PositionRef<'_>,
        _px: &PriceVector,
        _asset: AssetId,
    ) -> Result<Option<Price>> {
        Ok(None)
    }

    fn time_to_cross(&self, _pos: PositionRef<'_>, _px: &PriceVector) -> Result<Option<Timestamp>> {
        Ok(None)
    }

    fn quote(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
        quote::quote(pos, px)
    }

    /// The fixed 77 leg bytes. The tail (vault type, one-token choices,
    /// slippage floors) is filled at assembly from the vault's row and
    /// position (`liq_router::FluidPins`).
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
                adapter: ExecutorAdapter::Fluid,
                market: q.key.user,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }

    fn state_reads(&self, _rows: &dyn MarketRows) -> Vec<StateRead> {
        apply::state_reads(&self.cfg)
    }

    fn state_follow_ups(&self, answer: StateAnswer<'_>) -> Vec<StateRead> {
        apply::state_follow_ups(&self.cfg, answer)
    }

    fn apply_state_reads(
        &self,
        st: &mut dyn StateWriter,
        timestamp: Timestamp,
        answers: &[StateAnswer<'_>],
    ) -> Result<Vec<DirtySet>> {
        apply::apply_state_reads(&self.cfg, st, timestamp, answers).map(|s| vec![s])
    }
}
