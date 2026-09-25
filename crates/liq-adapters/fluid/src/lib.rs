//! Fluid vault adapter (WP 15C-fluid).
//!
//! Pin: `Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`.
//!
//! Quote unit is **(vault, currently liquidatable debt)**. `liquidate` does
//! not take an NFT id. Four vault types = four `liquidate` ABIs (T1–T4).
//!
//! T1 `encode` emits [`ExecutorAdapter::Fluid`] (id 6). T2/T3/T4 stay
//! [`ProtocolError::ExecutorUnwired`] — never call the T1 ABI on them.
//! Order: ProtocolMismatch, LegOutOfRange, CallbackProviderMismatch,
//! ZeroRecipient, FundingAssetMismatch, FundingShort, OracleSourceMismatch,
//! AmountTooLarge, then the plan or Unwired.
//!
//! ProtocolId **10**. MarketIds **4000..=4199** (catalog 4000, vault 1 → 4001).

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
use alloy_sol_types::{SolCall, SolEvent};
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FlashRoute, Health, LegChoice,
    LiquidationLeg, LiquidationPlan, PositionRef, ProbeCall, Protocol, ProtocolError, Quote,
    Result, StateWriter, Timestamp,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, MarketId, Price, PriceVector, ProtocolId, Ray};

pub use config::{AssetConfig, Config, ConfigError, FactoryRpc, VaultPin};
pub use events::liquidate_selector;
pub use layout::{
    CATALOG_MARKET, FIRST_VAULT_MARKET, LAST_VAULT_MARKET, VAULT_T1, VAULT_T2, VAULT_T3, VAULT_T4,
};
pub use math::col_per_unit_debt_1e18;
pub use quote::col_per_unit_debt_1e18_from_quote;

use crate::events::{admin, factory, halt, vault};

/// One Fluid VaultFactory deployment pin.
#[derive(Clone, Debug)]
pub struct Fluid {
    cfg: Config,
}

impl Fluid {
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
        if !cfg.live_factory_asserted {
            return Err(ConfigError::LiveFactoryUnasserted);
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

impl LogSubscriber for Fluid {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::new();
        let f = self.cfg.factory;
        push_topic(&mut out, f, factory::VaultDeployed::SIGNATURE_HASH);
        push_topic(&mut out, f, factory::NewPositionMinted::SIGNATURE_HASH);
        push_topic(&mut out, f, factory::Transfer::SIGNATURE_HASH);
        push_topic(&mut out, f, factory::LogSetDeployer::SIGNATURE_HASH);
        push_topic(&mut out, f, factory::LogSetGlobalAuth::SIGNATURE_HASH);
        push_topic(&mut out, f, factory::LogSetVaultAuth::SIGNATURE_HASH);
        push_topic(
            &mut out,
            f,
            factory::LogSetVaultDeploymentLogic::SIGNATURE_HASH,
        );
        halt_topics(&mut out, f);
        let mut vaults = self.cfg.vaults.clone();
        for p in &self.cfg.vault_pins {
            if !vaults.contains(&p.vault) {
                vaults.push(p.vault);
            }
        }
        for v in vaults {
            push_topic(&mut out, v, vault::LogOperate::SIGNATURE_HASH);
            push_topic(&mut out, v, vault::LogUpdateExchangePrice::SIGNATURE_HASH);
            push_topic(&mut out, v, vault::LogLiquidate::SIGNATURE_HASH);
            push_topic(&mut out, v, vault::LogAbsorb::SIGNATURE_HASH);
            push_topic(&mut out, v, vault::LogRebalance::SIGNATURE_HASH);
            push_topic(
                &mut out,
                v,
                admin::LogUpdateLiquidationThreshold::SIGNATURE_HASH,
            );
            push_topic(
                &mut out,
                v,
                admin::LogUpdateLiquidationMaxLimit::SIGNATURE_HASH,
            );
            push_topic(
                &mut out,
                v,
                admin::LogUpdateLiquidationPenalty::SIGNATURE_HASH,
            );
            push_topic(&mut out, v, admin::LogUpdateOracle::SIGNATURE_HASH);
            push_topic(&mut out, v, admin::LogUpdateCoreSettings::SIGNATURE_HASH);
            push_topic(
                &mut out,
                v,
                admin::LogUpdateSupplyRateMagnifier::SIGNATURE_HASH,
            );
            push_topic(
                &mut out,
                v,
                admin::LogUpdateBorrowRateMagnifier::SIGNATURE_HASH,
            );
            push_topic(
                &mut out,
                v,
                admin::LogUpdateCollateralFactor::SIGNATURE_HASH,
            );
            push_topic(&mut out, v, admin::LogUpdateWithdrawGap::SIGNATURE_HASH);
            push_topic(&mut out, v, admin::LogUpdateBorrowFee::SIGNATURE_HASH);
            push_topic(&mut out, v, admin::LogUpdateRebalancer::SIGNATURE_HASH);
            push_topic(&mut out, v, admin::LogRescueFunds::SIGNATURE_HASH);
            push_topic(&mut out, v, admin::LogAbsorbDustDebt::SIGNATURE_HASH);
            halt_topics(&mut out, v);
        }
        out
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
        let pin = self
            .cfg
            .pin_of(q.key.user)
            .ok_or(ProtocolError::ExecutorUnwired)?;
        if pin.vault_type != VAULT_T1 {
            return Err(ProtocolError::ExecutorUnwired);
        }
        let repay = q
            .repay_options
            .get(usize::from(legs.repay))
            .ok_or(ProtocolError::LegOutOfRange)?;
        let seize = q
            .seize_options
            .get(usize::from(legs.seize))
            .ok_or(ProtocolError::LegOutOfRange)?;
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

    /// T1 vaults only. `getExchangeRateLiquidate` is debt per collateral at
    /// 1e27 (`oracle_debt_per_col_1e27`). The overlay publishes a pair that
    /// reproduces that rate exactly, or nothing.
    fn price_reads(&self, rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::PriceRead> {
        let mut out = Vec::new();
        let mut id = crate::layout::FIRST_VAULT_MARKET.0;
        while id <= crate::layout::LAST_VAULT_MARKET.0 {
            if let Some(market_rows) = rows.rows(MarketId(id)) {
                if let Some(read) = fluid_t1_read(MarketId(id), market_rows) {
                    out.push(read);
                }
            }
            id = match id.checked_add(1) {
                Some(v) => v,
                None => break,
            };
        }
        out
    }

    fn decode_prices(
        &self,
        read: &liq_protocol::PriceRead,
        ret: &[u8],
        out: &mut Vec<(AssetId, Ray)>,
    ) -> Result<()> {
        let rate = crate::events::views::getExchangeRateLiquidateCall::abi_decode_returns(ret)
            .map_err(|_| ProtocolError::ProbeDecode)?;
        let coll_dec = u8::try_from(read.tag & 0xff).map_err(|_| ProtocolError::ProbeDecode)?;
        let hi = read.tag.checked_shr(8).ok_or(ProtocolError::ProbeDecode)?;
        let debt_dec = u8::try_from(hi & 0xff).map_err(|_| ProtocolError::ProbeDecode)?;
        let Some((p_coll, p_debt)) = crate::math::t1_prices_from_rate(rate, coll_dec, debt_dec)
        else {
            return Ok(());
        };
        let (Some(&coll), Some(&debt)) = (read.assets.first(), read.assets.get(1)) else {
            return Err(ProtocolError::ProbeDecode);
        };
        out.push((coll, Ray::from_raw(p_coll)));
        out.push((debt, Ray::from_raw(p_debt)));
        Ok(())
    }
}

fn fluid_t1_read(
    market: MarketId,
    rows: &[liq_protocol::MarketRow],
) -> Option<liq_protocol::PriceRead> {
    use crate::events::views::getExchangeRateLiquidateCall;
    use crate::layout::{VaultRow, UNMAPPED_ASSET};
    let head = rows.first()?;
    let body: &VaultRow = head.body().ok()?;
    if !body.is_t1_token_pair() {
        return None;
    }
    let debt = rows.get(usize::from(body.n_col))?;
    if head.asset == UNMAPPED_ASSET || debt.asset == UNMAPPED_ASSET {
        return None;
    }
    let vault = Address::from(body.vault);
    if vault.is_zero() {
        return None;
    }
    let coll_dec = u32::from(head.decimals);
    let debt_dec = u32::from(debt.decimals);
    let tag = debt_dec.checked_shl(8)?.checked_add(coll_dec)?;
    Some(liq_protocol::PriceRead {
        market,
        target: vault,
        calldata: Bytes::from(getExchangeRateLiquidateCall {}.abi_encode()),
        tag,
        assets: vec![head.asset, debt.asset],
    })
}
