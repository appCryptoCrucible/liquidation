//! Morpho Blue adapter (WP 15A-1) — pinned to `morpho-org/morpho-blue` @
//! `8e26ca6a8dbc5089edcd67fb576248810fd2870a` (main, 2026-09-09).

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

pub use config::{AssetConfig, Config, ConfigError, SourcePin};

use crate::events as ev;

/// One Morpho Blue singleton.
#[derive(Clone, Debug)]
pub struct MorphoBlue {
    cfg: Config,
}

impl MorphoBlue {
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

impl LogSubscriber for MorphoBlue {
    fn subscriptions(&self) -> Vec<LogFilter> {
        const T: [alloy_primitives::B256; 19] = [
            ev::SetOwner::SIGNATURE_HASH,
            ev::SetFee::SIGNATURE_HASH,
            ev::SetFeeRecipient::SIGNATURE_HASH,
            ev::EnableIrm::SIGNATURE_HASH,
            ev::EnableLltv::SIGNATURE_HASH,
            ev::CreateMarket::SIGNATURE_HASH,
            ev::Supply::SIGNATURE_HASH,
            ev::Withdraw::SIGNATURE_HASH,
            ev::Borrow::SIGNATURE_HASH,
            ev::Repay::SIGNATURE_HASH,
            ev::SupplyCollateral::SIGNATURE_HASH,
            ev::WithdrawCollateral::SIGNATURE_HASH,
            ev::Liquidate::SIGNATURE_HASH,
            ev::FlashLoan::SIGNATURE_HASH,
            ev::SetAuthorization::SIGNATURE_HASH,
            ev::IncrementNonce::SIGNATURE_HASH,
            ev::AccrueInterest::SIGNATURE_HASH,
            ev::halt::Upgraded::SIGNATURE_HASH,
            ev::halt::AdminChanged::SIGNATURE_HASH,
        ];
        let mut out = Vec::with_capacity(T.len().saturating_add(1));
        for t0 in T {
            out.push(LogFilter {
                address: self.cfg.morpho,
                topic0: t0,
            });
        }
        out.push(LogFilter {
            address: self.cfg.morpho,
            topic0: ev::halt::Initialized::SIGNATURE_HASH,
        });
        out
    }
}

impl Protocol for MorphoBlue {
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
                adapter: ExecutorAdapter::MorphoBlue,
                market: self.cfg.morpho,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }

    /// One `IOracle.price()` per created market. Markets exist only in
    /// state: walk from `first_market` until a gap.
    fn price_reads(&self, rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::PriceRead> {
        let mut out = Vec::new();
        let mut n = self.cfg.first_market.0;
        while let Some(market_rows) = rows.rows(MarketId(n)) {
            if let Some(read) = morpho_price_read(MarketId(n), market_rows) {
                out.push(read);
            }
            n = match n.checked_add(1) {
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
        let price = IOraclePrice::priceCall::abi_decode_returns(ret)
            .map_err(|_| ProtocolError::ProbeDecode)?;
        if price.is_zero() {
            return Ok(());
        }
        let hi = read.tag.checked_shr(8).ok_or(ProtocolError::ProbeDecode)?;
        let loan_dec = u8::try_from(hi & 0xff).map_err(|_| ProtocolError::ProbeDecode)?;
        let coll_dec = u8::try_from(read.tag & 0xff).map_err(|_| ProtocolError::ProbeDecode)?;
        let (p_loan, p_coll) = crate::health::prices_matching_oracle(price, loan_dec, coll_dec)
            .map_err(|_| ProtocolError::ProbeDecode)?;
        let (Some(&loan), Some(&coll)) = (read.assets.first(), read.assets.get(1)) else {
            return Err(ProtocolError::ProbeDecode);
        };
        out.push((loan, Ray::from_raw(p_loan)));
        out.push((coll, Ray::from_raw(p_coll)));
        Ok(())
    }
}

fn morpho_price_read(
    market: MarketId,
    rows: &[liq_protocol::MarketRow],
) -> Option<liq_protocol::PriceRead> {
    use crate::layout::{LoanRow, COLL_SLOT, LOAN_SLOT, UNMAPPED_ASSET};
    let loan_row = rows.get(usize::from(LOAN_SLOT))?;
    let coll_row = rows.get(usize::from(COLL_SLOT))?;
    if loan_row.flags.contains(liq_protocol::MarketFlags::UNPRICED)
        || coll_row.flags.contains(liq_protocol::MarketFlags::UNPRICED)
        || loan_row.asset == UNMAPPED_ASSET
        || coll_row.asset == UNMAPPED_ASSET
    {
        return None;
    }
    let loan: &LoanRow = loan_row.body().ok()?;
    if loan.flags & LoanRow::PRICED == 0 {
        return None;
    }
    let oracle = Address::from(loan.oracle);
    if oracle.is_zero() {
        return None;
    }
    let loan_dec = u32::from(loan_row.decimals);
    let coll_dec = u32::from(loan.coll_decimals);
    let tag = loan_dec.checked_shl(8)?.checked_add(coll_dec)?;
    Some(liq_protocol::PriceRead {
        market,
        target: oracle,
        calldata: Bytes::from(IOraclePrice::priceCall {}.abi_encode()),
        tag,
        assets: vec![loan_row.asset, coll_row.asset],
    })
}

alloy_sol_types::sol! {
    interface IOraclePrice {
        function price() external view returns (uint256);
    }
}
