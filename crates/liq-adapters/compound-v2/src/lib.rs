//! Compound V2 forks adapter — WP 15C-compound-v2.
//! Pin: `compound-finance/compound-protocol` @
//! `a3214f67b73310d547e00fc578e8355911c9d376`.
//!
//! ProtocolId 3. Intern MarketIds 295..=560 (official Unitroller = 355).
//! `encode` emits [`ExecutorAdapter::CompoundV2`] (id 8). `market` is the
//! debt cToken. CEther vs CErc20 is the config pin (`underlying == 0`), never
//! a symbol or an on-chain `underlying()` guess.

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
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId};

pub use config::{
    AssetConfig, CTokenPin, Config, ConfigError, ForkConfig, RegistryRpc, FAMILY_PROTOCOL,
    INTERN_MARKET_MAX, INTERN_MARKET_MIN, OFFICIAL_MARKET, OFFICIAL_UNITROLLER,
};

use crate::events::{comptroller as cmp, ctoken, halt, pause_global, pause_market};

/// One Compound V2 family (official Unitroller + intern-bound forks).
#[derive(Clone, Debug)]
pub struct CompoundV2 {
    cfg: Config,
}

impl CompoundV2 {
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
        cfg.validate()?;
        if !cfg.live_registry_asserted {
            return Err(ConfigError::LiveRegistryUnasserted);
        }
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

impl LogSubscriber for CompoundV2 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        const CMP: [alloy_primitives::B256; 11] = [
            cmp::MarketListed::SIGNATURE_HASH,
            cmp::MarketEntered::SIGNATURE_HASH,
            cmp::MarketExited::SIGNATURE_HASH,
            cmp::NewCloseFactor::SIGNATURE_HASH,
            cmp::NewCollateralFactor::SIGNATURE_HASH,
            cmp::NewLiquidationIncentive::SIGNATURE_HASH,
            cmp::NewPriceOracle::SIGNATURE_HASH,
            pause_global::ActionPaused::SIGNATURE_HASH,
            pause_market::ActionPaused::SIGNATURE_HASH,
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
        ];
        const CT: [alloy_primitives::B256; 13] = [
            ctoken::AccrueInterest::SIGNATURE_HASH,
            crate::events::ctoken_original::AccrueInterest::SIGNATURE_HASH,
            crate::events::ctoken_reserves::AccrueInterest::SIGNATURE_HASH,
            ctoken::Mint::SIGNATURE_HASH,
            ctoken::Redeem::SIGNATURE_HASH,
            ctoken::Borrow::SIGNATURE_HASH,
            ctoken::RepayBorrow::SIGNATURE_HASH,
            ctoken::LiquidateBorrow::SIGNATURE_HASH,
            ctoken::NewReserveFactor::SIGNATURE_HASH,
            ctoken::Transfer::SIGNATURE_HASH,
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
        ];
        let mut n = self
            .cfg
            .forks
            .len()
            .saturating_mul(CMP.len().saturating_add(1));
        for f in &self.cfg.forks {
            n = n.saturating_add(f.ctokens.len().saturating_mul(CT.len()));
        }
        let mut out = Vec::with_capacity(n);
        for f in &self.cfg.forks {
            for t0 in CMP {
                out.push(LogFilter {
                    address: f.comptroller,
                    topic0: t0,
                });
            }
            out.push(LogFilter {
                address: f.comptroller,
                topic0: halt::Initialized::SIGNATURE_HASH,
            });
            for c in &f.ctokens {
                for t0 in CT {
                    out.push(LogFilter {
                        address: c.ctoken,
                        topic0: t0,
                    });
                }
            }
        }
        out
    }
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
    let _fork = cfg
        .fork_by_market(q.key.market)
        .ok_or(ProtocolError::UnknownMarket(q.key.market))?;
    let _debt = cfg
        .underlying_of(repay.asset)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let _coll = cfg
        .underlying_of(seize.asset)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let _ = u128::try_from(repay.max_repay).map_err(|_| ProtocolError::AmountTooLarge)?;
    let _ = u128::try_from(funding.amount).map_err(|_| ProtocolError::AmountTooLarge)?;
    Ok(())
}

impl Protocol for CompoundV2 {
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
        quote::quote(&self.cfg, pos, px)
    }

    /// 10E: `CErc20.liquidateBorrow(borrower, repayAmount, cTokenCollateral)`
    /// or payable `CEther.liquidateBorrow(borrower, cTokenCollateral)`.
    /// CEther is underlying-absence, not a symbol.
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
        let fork = self
            .cfg
            .fork_by_market(q.key.market)
            .ok_or(ProtocolError::UnknownMarket(q.key.market))?;
        // P6. Prefer the cToken the QUOTE named. `ctoken_for_asset` resolves
        // by underlying, and cWBTC/cWBTC2 share one — so it silently picks
        // the deprecated market. The quote walked the borrower's actual rows
        // and knows which cToken the balance is in; the config lookup stays
        // only as the fallback for a quote that did not name one.
        let debt_ctoken = match repay.slot.contract() {
            Some(a) if !a.is_zero() => a,
            _ => {
                self.cfg
                    .ctoken_for_asset(fork, repay.asset)
                    .ok_or(ProtocolError::OracleSourceMismatch)?
                    .ctoken
            }
        };
        let coll_ctoken = match seize.slot.contract() {
            Some(a) if !a.is_zero() => a,
            _ => {
                self.cfg
                    .ctoken_for_asset(fork, seize.asset)
                    .ok_or(ProtocolError::OracleSourceMismatch)?
                    .ctoken
            }
        };
        // The collateral cToken is what the 21-byte tail carries; it must be
        // a market this deployment actually lists, whichever way it was
        // resolved.
        let coll_pin = self
            .cfg
            .ctoken_seed(coll_ctoken)
            .map(|(_, c)| c)
            .ok_or(ProtocolError::OracleSourceMismatch)?;
        let _ = coll_pin;
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
                adapter: ExecutorAdapter::CompoundV2,
                market: debt_ctoken,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }

    /// `borrowRatePerBlock()` on every listed cToken of every interned
    /// Comptroller, each block: the rate the health projection accrues at.
    /// A Fuse cToken also reads `totalFuseFees`, `totalAdminFees`,
    /// `fuseFeeMantissa` and `adminFeeMantissa`: a fee withdrawal moves the
    /// totals without an event. Tag = `kind << 16 | slot` ([`READ_RATE`]..).
    fn state_reads(&self, rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::StateRead> {
        let mut out = Vec::new();
        for (_, market) in &self.cfg.interned {
            if self.cfg.fork_by_market(*market).is_none() {
                continue;
            }
            let Some(rs) = rows.rows(*market) else {
                continue;
            };
            for (slot, row) in rs.iter().enumerate().skip(1) {
                let Ok(body) = row.body::<layout::CTokenRow>() else {
                    continue;
                };
                // A frozen market cannot accrue: nothing to read.
                if body.flags & layout::CTokenRow::LISTED == 0
                    || body.flags2 & layout::CTokenRow::FROZEN != 0
                {
                    continue;
                }
                let Ok(slot) = u64::try_from(slot) else {
                    continue;
                };
                let target = math::addr_from(body.ctoken);
                let mut read = |kind: u64, calldata: Vec<u8>| {
                    out.push(liq_protocol::StateRead {
                        market: *market,
                        target,
                        calldata: alloy_primitives::Bytes::from(calldata),
                        tag: kind << 16 | slot,
                    });
                };
                // The rate only moves interest, so only borrowed-from
                // markets need it; the fees only move a collateral's
                // exchange rate, so only supplied markets need them. Most
                // Fuse markets are empty.
                if body.total_borrows != 0 {
                    read(READ_RATE, borrowRatePerBlockCall {}.abi_encode());
                }
                if body.total_supply != 0 && body.flags2 & layout::CTokenRow::FUSE != 0 {
                    read(READ_FUSE_FEES, totalFuseFeesCall {}.abi_encode());
                    read(READ_ADMIN_FEES, totalAdminFeesCall {}.abi_encode());
                    read(READ_FUSE_FEE, fuseFeeMantissaCall {}.abi_encode());
                    read(READ_ADMIN_FEE, adminFeeMantissaCall {}.abi_encode());
                }
                // Moma: the same four words under its own names.
                if body.total_supply != 0 && body.flags2 & layout::CTokenRow::MOMA != 0 {
                    read(READ_FUSE_FEES, totalFeesCall {}.abi_encode());
                    read(READ_ADMIN_FEES, totalMomaFeesCall {}.abi_encode());
                    read(READ_FUSE_FEE, feeFactorMantissaCall {}.abi_encode());
                    read(READ_ADMIN_FEE, getMomaFeeFactorCall {}.abi_encode());
                }
            }
        }
        out
    }

    fn apply_state_reads(
        &self,
        st: &mut dyn StateWriter,
        _timestamp: Timestamp,
        answers: &[liq_protocol::StateAnswer<'_>],
    ) -> Result<Vec<DirtySet>> {
        // One cToken's answers together: the four Fuse words are written
        // only when all four read.
        #[derive(Default)]
        struct Read {
            target: alloy_primitives::Address,
            words: [Option<alloy_primitives::U256>; 5],
        }
        let mut by_slot: Vec<(liq_protocol::MarketSlot, Read)> = Vec::new();
        for a in answers {
            if !a.success || a.data.len() < 32 {
                continue;
            }
            let (kind, Ok(slot)) = (a.read.tag >> 16, u16::try_from(a.read.tag & 0xffff)) else {
                continue;
            };
            let Some(i) = usize::try_from(kind).ok().filter(|k| *k < 5) else {
                continue;
            };
            let at = liq_protocol::MarketSlot {
                market: a.read.market,
                slot,
            };
            let Some(head) = a.data.get(..32) else {
                continue;
            };
            let word = alloy_primitives::U256::from_be_slice(head);
            let r = match by_slot.iter().position(|(s, _)| *s == at) {
                Some(p) => by_slot.get_mut(p).map(|(_, r)| r),
                None => {
                    by_slot.push((
                        at,
                        Read {
                            target: a.read.target,
                            ..Read::default()
                        },
                    ));
                    by_slot.last_mut().map(|(_, r)| r)
                }
            };
            if let Some(slot) = r.and_then(|r| r.words.get_mut(i)) {
                *slot = Some(word);
            }
        }
        let mut rows = liq_protocol::DirtyRows::new();
        for (at, r) in by_slot {
            let Ok(cur) = st.market(at) else { continue };
            let mut row = *cur;
            let body: &mut layout::CTokenRow = row.body_mut()?;
            if math::addr_from(body.ctoken) != r.target {
                continue;
            }
            let before = *body;
            let w = |k: u64| {
                usize::try_from(k)
                    .ok()
                    .and_then(|k| r.words.get(k).copied().flatten())
            };
            if let Some(rate) = w(READ_RATE).and_then(|w| u64::try_from(w).ok()) {
                body.borrow_rate_per_block = rate;
                body.flags |= layout::CTokenRow::RATE_KNOWN;
            }
            if body.flags2 & layout::CTokenRow::FEE_ACCUMULATORS != 0 {
                if let (Some(f), Some(a), Some(ff), Some(af)) = (
                    w(READ_FUSE_FEES),
                    w(READ_ADMIN_FEES),
                    w(READ_FUSE_FEE),
                    w(READ_ADMIN_FEE),
                ) {
                    let fees = f.checked_add(a).and_then(|t| u128::try_from(t).ok());
                    let rate = ff.checked_add(af).and_then(|t| u64::try_from(t).ok());
                    if let (Some(fees), Some(rate)) = (fees, rate) {
                        body.total_fees = fees;
                        body.fee_mantissa = rate;
                        body.flags2 |= layout::CTokenRow::FEES_KNOWN;
                        apply::recompute_exrate(body)?;
                    }
                }
            }
            if *body == before {
                continue;
            }
            st.set_market(at, row)?;
            rows.push(at);
        }
        Ok(if rows.is_empty() {
            Vec::new()
        } else {
            vec![DirtySet::MarketAccrual(rows)]
        })
    }

    /// `PriceOracle.getUnderlyingPrice(cToken)` per cToken of each interned
    /// comptroller (no batch getter). Tag = the underlying's decimals.
    fn price_reads(&self, _rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::PriceRead> {
        let mut out = Vec::new();
        for fork in &self.cfg.forks {
            if fork.oracle.is_zero() {
                continue;
            }
            let Some(market) = self
                .cfg
                .interned
                .iter()
                .find(|(a, _)| *a == fork.comptroller)
                .map(|(_, m)| *m)
            else {
                continue;
            };
            for c in &fork.ctokens {
                let asset = if c.underlying.is_zero() {
                    self.cfg.native_asset(fork)
                } else {
                    self.cfg.asset_by_underlying(c.underlying)
                };
                let Some(asset) = asset else { continue };
                out.push(liq_protocol::PriceRead {
                    market,
                    target: fork.oracle,
                    calldata: alloy_primitives::Bytes::from(
                        getUnderlyingPriceCall { cToken: c.ctoken }.abi_encode(),
                    ),
                    tag: u32::from(asset.decimals),
                    assets: vec![asset.asset],
                });
            }
        }
        out
    }

    /// Compound's mantissa is `USD · 10^(36 − decimals)` per whole token;
    /// RAY is `USD · 10^27`, so `ray = m · 10^(decimals − 9)`. (The adapter
    /// re-derives the mantissa keeping 18 USD decimals — a sub-1e-18
    /// relative difference on tokens under 18 decimals.)
    fn decode_prices(
        &self,
        read: &liq_protocol::PriceRead,
        ret: &[u8],
        out: &mut Vec<(AssetId, liq_types::Ray)>,
    ) -> Result<()> {
        let m = getUnderlyingPriceCall::abi_decode_returns(ret)
            .map_err(|_| ProtocolError::ProbeDecode)?;
        let [asset] = read.assets.as_slice() else {
            return Err(ProtocolError::ProbeDecode);
        };
        if m.is_zero() {
            return Ok(());
        }
        let dec = read.tag;
        let ray = compound_ray(m, dec).ok_or(ProtocolError::ProbeDecode)?;
        out.push((*asset, liq_types::Ray::from_raw(ray)));
        Ok(())
    }
}

alloy_sol_types::sol! {
    /// Compound V2 `PriceOracle.getUnderlyingPrice`.
    function getUnderlyingPrice(address cToken) external view returns (uint256);
    /// `CToken.borrowRatePerBlock` @ `a3214f67`: the model's rate on the
    /// stored cash, borrows and reserves.
    function borrowRatePerBlock() external view returns (uint256);
    /// Fuse `CToken` (`0x67db14e7…`) fee accumulators and rates.
    function totalFuseFees() external view returns (uint256);
    function totalAdminFees() external view returns (uint256);
    function fuseFeeMantissa() external view returns (uint256);
    function adminFeeMantissa() external view returns (uint256);
    /// Moma `MToken` (`0x1d0fcc81…`) fee accumulators and rates.
    function totalFees() external view returns (uint256);
    function totalMomaFees() external view returns (uint256);
    function feeFactorMantissa() external view returns (uint256);
    function getMomaFeeFactor() external view returns (uint256);
}

/// [`liq_protocol::StateRead::tag`] kinds (`kind << 16 | slot`).
const READ_RATE: u64 = 0;
const READ_FUSE_FEES: u64 = 1;
const READ_ADMIN_FEES: u64 = 2;
const READ_FUSE_FEE: u64 = 3;
const READ_ADMIN_FEE: u64 = 4;

/// `USD · 10^(36 − decimals)` → RAY (`USD · 10^27`): `m · 10^(decimals − 9)`.
/// Zero is not a price.
pub(crate) fn compound_ray(m: U256, dec: u32) -> Option<U256> {
    if m.is_zero() {
        return None;
    }
    if dec >= 9 {
        m.checked_mul(U256::from(10u64).pow(U256::from(dec.saturating_sub(9))))
    } else {
        m.checked_div(U256::from(10u64).pow(U256::from(9u32.saturating_sub(dec))))
    }
}

#[cfg(test)]
mod ray_scale {
    use super::compound_ray;
    use alloy_primitives::U256;

    #[test]
    fn six_decimals_divides_the_mantissa_by_one_thousand() {
        // cUSDC: mantissa is USD·10^30; RAY is USD·10^27. 10^(6−9) = 1/1000.
        // 1_000 * 1_000_000_000 / 1_000 = 1_000_000_000. Written out, not
        // taken from `compound_ray`.
        assert_eq!(
            compound_ray(U256::from(1_000_000_000_000u64), 6),
            Some(U256::from(1_000_000_000u64))
        );
    }

    #[test]
    fn eighteen_decimals_multiplies_by_one_billion() {
        // 10^(18−9) = 10^9. One mantissa unit is 1_000_000_000 ray.
        assert_eq!(
            compound_ray(U256::from(1u64), 18),
            Some(U256::from(1_000_000_000u64))
        );
    }

    #[test]
    fn zero_mantissa_is_not_a_price() {
        assert_eq!(compound_ray(U256::ZERO, 6), None);
        assert_eq!(compound_ray(U256::ZERO, 18), None);
    }
}
