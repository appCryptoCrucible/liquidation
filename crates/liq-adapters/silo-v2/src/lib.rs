//! Silo V2 adapter (WP 15C-silo).
//!
//! Pin: `silo-finance/silo-contracts-v2` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7`
//! (GitHub API may redirect to `silo-contracts-v3`; same SHA, package still
//! `silo-contracts-v2`).
//!
//! # Hook receiver vs Silo ERC-4626
//!
//! `IPartialLiquidation.liquidationCall(_collateralAsset, _debtAsset, _borrower,
//! _maxDebtToCover, _receiveSToken)` is implemented on the pair's **hook
//! receiver** (`PartialLiquidation.sol`), not on the Silo ERC-4626. The Silo
//! exposes `isSolvent` and share accounting; it does **not** expose
//! `liquidationCall`. `maxLiquidation` is also on the hook.
//!
//! Pin event (hook):
//! `LiquidationCall(address indexed liquidator, address indexed silo,
//! address indexed borrower, uint256 repayDebtAssets, uint256 withdrawCollateral,
//! bool receiveSToken)`.
//!
//! `liq-watch` `silo::LiquidationCall(liquidator, borrower, repay, withdraw)` is
//! a different topic0. This adapter decodes the pin ABI. 10R must call the hook.
//!
//! # Isolated pair
//!
//! One `SiloConfig` = one interned [`liq_types::MarketId`] (the admitted debt
//! silo). Slot 0 is `getSilos().0`, slot 1 is `getSilos().1`. One collateral
//! token backs one debt token. Bonus = `collateralConfig.liquidationFee` (WAD).
//! Dust: hook `require(repay <= maxCover)` → `FullLiquidationRequired`.
//! Solvent borrower → `UserIsSolvent` (`debtConfig.silo == 0` / `isSolvent`).
//!
//! # 10R wire ABI (not encoded here)
//!
//! `encode` returns [`ProtocolError::ExecutorUnwired`] — no
//! `ExecutorAdapter` discriminant yet (D48). Calldata for the later WP:
//!
//! ```text
//! hook.liquidationCall(address _collateralAsset, address _debtAsset,
//!     address _borrower, uint256 _maxDebtToCover, bool _receiveSToken)
//!     returns (uint256 withdrawCollateral, uint256 repayDebtAssets)
//! hook.maxLiquidation(address _borrower)
//!     view returns (uint256 collateralToLiquidate, uint256 debtToRepay, bool sTokenRequired)
//! silo.isSolvent(address _borrower) view returns (bool)
//! ```

#![forbid(unsafe_code)]

pub mod apply;
pub mod config;
pub mod events;
pub mod health;
pub mod layout;
pub mod math;
pub mod quote;
pub mod solve;

use alloy_primitives::Address;
use alloy_sol_types::SolEvent;
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, FlashRoute, Health, LegChoice, LiquidationPlan,
    PositionRef, ProbeCall, Protocol, ProtocolError, Quote, Result, StateWriter, Timestamp,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId};

pub use config::{AssetConfig, Config, ConfigError, PairConfig, SideConfig};

use crate::events::{factory, halt, hook, silo};

/// One Silo V2 deployment pin (factories + admitted isolated pairs).
#[derive(Clone, Debug)]
pub struct SiloV2 {
    cfg: Config,
}

impl SiloV2 {
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

fn push_topic(out: &mut Vec<LogFilter>, address: Address, topic0: alloy_primitives::B256) {
    out.push(LogFilter { address, topic0 });
}

fn halt_topics(out: &mut Vec<LogFilter>, address: Address) {
    push_topic(out, address, halt::Upgraded::SIGNATURE_HASH);
    push_topic(out, address, halt::AdminChanged::SIGNATURE_HASH);
    push_topic(out, address, halt::Initialized::SIGNATURE_HASH);
}

impl LogSubscriber for SiloV2 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::new();
        for f in &self.cfg.factories {
            push_topic(&mut out, *f, factory::NewSilo::SIGNATURE_HASH);
            push_topic(&mut out, *f, factory::NewSiloShareTokens::SIGNATURE_HASH);
            push_topic(&mut out, *f, factory::NewSiloHook::SIGNATURE_HASH);
            halt_topics(&mut out, *f);
        }
        for p in &self.cfg.pairs {
            push_topic(
                &mut out,
                p.hook_receiver,
                hook::LiquidationCall::SIGNATURE_HASH,
            );
            push_topic(
                &mut out,
                p.hook_receiver,
                hook::LiquidationStart::SIGNATURE_HASH,
            );
            halt_topics(&mut out, p.hook_receiver);
            for side in [&p.silo0, &p.silo1] {
                let s = side.silo;
                push_topic(&mut out, s, silo::Deposit::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::DepositProtected::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::Withdraw::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::WithdrawProtected::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::Borrow::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::Repay::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::CollateralTypeChanged::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::AccruedInterest::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::FlashLoan::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::HooksUpdated::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::WithdrawnFees::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::DeployerFeesRedirected::SIGNATURE_HASH);
                push_topic(&mut out, s, silo::Transfer::SIGNATURE_HASH);
                halt_topics(&mut out, s);
                if side.protected_share != s {
                    push_topic(
                        &mut out,
                        side.protected_share,
                        silo::Transfer::SIGNATURE_HASH,
                    );
                }
                if side.debt_share != s {
                    push_topic(&mut out, side.debt_share, silo::Transfer::SIGNATURE_HASH);
                }
            }
        }
        out
    }
}

impl Protocol for SiloV2 {
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

    fn quote(
        &self,
        pos: PositionRef<'_>,
        px: &PriceVector,
        cons: &liq_protocol::Constraints,
    ) -> Result<Option<Quote>> {
        quote::quote(pos, px, cons)
    }

    fn encode(
        &self,
        q: &Quote,
        _legs: LegChoice,
        _funding: &FlashRoute,
        _recipient: Address,
    ) -> Result<LiquidationPlan> {
        if q.key.protocol != self.cfg.protocol {
            return Err(ProtocolError::ProtocolMismatch);
        }
        // Hook `liquidationCall` is not an ExecutorAdapter discriminant. 10R.
        Err(ProtocolError::ExecutorUnwired)
    }

    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }
}
