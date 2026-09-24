//! Euler V2 (EVK) adapter — WP 15C-euler / 10E.
//! Pin: `euler-xyz/euler-vault-kit` @ `bfb325a6e6ca09613d940b46f72ccfe017353933`.
//! `encode` emits [`ExecutorAdapter::EulerV2`] (id 3). Tail is
//! `uint256 minYieldBalance ‖ address collateralVault` (yield from the
//! quote, vault from [`SeizeOption::call_target`]).

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
use liq_types::fixed::WAD;
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId, Ray};

pub use config::{
    AssetConfig, Config, ConfigError, SourcePin, CATALOG_MARKET, FIRST_DISCOVERED_MARKET,
    FOREIGN_MARKET_MAX, FOREIGN_MARKET_MIN,
};

use crate::events as ev;
use crate::events::accountLiquidityCall;
use crate::layout::{VaultRow, DEBT_SLOT};
use crate::math::hf_wad_to_ray;

/// One Euler GenericFactory + EVC instance.
#[derive(Clone, Debug)]
pub struct EulerV2 {
    cfg: Config,
}

impl EulerV2 {
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

fn vault_topics() -> [alloy_primitives::B256; 25] {
    [
        ev::Transfer::SIGNATURE_HASH,
        ev::Approval::SIGNATURE_HASH,
        ev::Deposit::SIGNATURE_HASH,
        ev::Withdraw::SIGNATURE_HASH,
        ev::EVaultCreated::SIGNATURE_HASH,
        ev::VaultStatus::SIGNATURE_HASH,
        ev::Borrow::SIGNATURE_HASH,
        ev::Repay::SIGNATURE_HASH,
        ev::InterestAccrued::SIGNATURE_HASH,
        ev::Liquidate::SIGNATURE_HASH,
        ev::PullDebt::SIGNATURE_HASH,
        ev::DebtSocialized::SIGNATURE_HASH,
        ev::ConvertFees::SIGNATURE_HASH,
        ev::BalanceForwarderStatus::SIGNATURE_HASH,
        ev::GovSetFeeReceiver::SIGNATURE_HASH,
        ev::GovSetLTV::SIGNATURE_HASH,
        ev::GovSetInterestRateModel::SIGNATURE_HASH,
        ev::GovSetMaxLiquidationDiscount::SIGNATURE_HASH,
        ev::GovSetLiquidationCoolOffTime::SIGNATURE_HASH,
        ev::GovSetHookConfig::SIGNATURE_HASH,
        ev::GovSetConfigFlags::SIGNATURE_HASH,
        ev::GovSetCaps::SIGNATURE_HASH,
        ev::GovSetInterestFee::SIGNATURE_HASH,
        ev::halt::Upgraded::SIGNATURE_HASH,
        ev::GovSetGovernorAdmin::SIGNATURE_HASH,
    ]
}

impl LogSubscriber for EulerV2 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::new();
        for t0 in [
            ev::Genesis::SIGNATURE_HASH,
            ev::ProxyCreated::SIGNATURE_HASH,
            ev::SetImplementation::SIGNATURE_HASH,
            ev::SetUpgradeAdmin::SIGNATURE_HASH,
            ev::halt::Upgraded::SIGNATURE_HASH,
            ev::halt::AdminChanged::SIGNATURE_HASH,
            ev::halt::Initialized::SIGNATURE_HASH,
        ] {
            out.push(LogFilter {
                address: self.cfg.factory,
                topic0: t0,
            });
        }
        for t0 in [
            ev::evc::CollateralStatus::SIGNATURE_HASH,
            ev::evc::ControllerStatus::SIGNATURE_HASH,
            ev::evc::AccountStatusCheck::SIGNATURE_HASH,
            ev::evc::OwnerRegistered::SIGNATURE_HASH,
            ev::evc::LockdownModeStatus::SIGNATURE_HASH,
            ev::evc::VaultStatusCheck::SIGNATURE_HASH,
            ev::halt::Upgraded::SIGNATURE_HASH,
            ev::halt::AdminChanged::SIGNATURE_HASH,
            ev::halt::Initialized::SIGNATURE_HASH,
        ] {
            out.push(LogFilter {
                address: self.cfg.evc,
                topic0: t0,
            });
        }
        for vault in &self.cfg.vaults {
            for t0 in vault_topics() {
                out.push(LogFilter {
                    address: *vault,
                    topic0: t0,
                });
            }
            out.push(LogFilter {
                address: *vault,
                topic0: ev::halt::AdminChanged::SIGNATURE_HASH,
            });
            out.push(LogFilter {
                address: *vault,
                topic0: ev::halt::Initialized::SIGNATURE_HASH,
            });
        }
        out
    }
}

fn decode_probe(raw: &[u8]) -> Result<Ray> {
    let out =
        accountLiquidityCall::abi_decode_returns(raw).map_err(|_| ProtocolError::ProbeDecode)?;
    if out.liabilityValue.is_zero() {
        return Ok(Ray::from_raw(U256::MAX));
    }
    hf_wad_to_ray(crate::math::mul_div_down(
        out.collateralValue,
        WAD,
        out.liabilityValue,
    )?)
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
        .underlying_of(repay.asset)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let _ = cfg
        .underlying_of(seize.asset)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let _ = u128::try_from(repay.max_repay).map_err(|_| ProtocolError::AmountTooLarge)?;
    let _ = u128::try_from(funding.amount).map_err(|_| ProtocolError::AmountTooLarge)?;
    Ok(())
}

impl Protocol for EulerV2 {
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
        let market = self
            .cfg
            .vault_of(q.key.market)
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
                adapter: ExecutorAdapter::EulerV2,
                market,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, pos: PositionRef<'_>) -> Result<ProbeCall> {
        let row = pos
            .markets
            .get(usize::from(DEBT_SLOT))
            .ok_or(ProtocolError::ProbeUnavailable)?;
        let v: &VaultRow = row.body()?;
        let to = crate::math::addr_from(v.vault);
        if to == Address::ZERO {
            return Err(ProtocolError::ProbeUnavailable);
        }
        Ok(ProbeCall {
            to,
            data: accountLiquidityCall {
                account: pos.key.user,
                liquidation: true,
            }
            .abi_encode()
            .into(),
            decode: decode_probe,
        })
    }
}
