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
//! The solvency oracle quotes in the pair's quote token, and `debt_value`
//! is what the quote sizes. That is not USD unless the debt token is the
//! quote token, which the rows do not prove. No protocol-price overlay:
//! health and sizing stay on the canonical vector. A zero-address oracle
//! (1:1 in the quote token) is not turned into a guessed USD price.
//!
//! `liq-watch` `silo::LiquidationCall(liquidator, borrower, repay, withdraw)` is
//! a different topic0. This adapter decodes the pin ABI. 10E calls the hook.
//!
//! # Isolated pair
//!
//! One `SiloConfig` = one interned [`liq_types::MarketId`] (the admitted debt
//! silo). Slot 0 is `getSilos().0`, slot 1 is `getSilos().1`. One collateral
//! token backs one debt token. Bonus = `collateralConfig.liquidationFee` (WAD).
//! Dust: hook `require(repay <= maxCover)` → `FullLiquidationRequired`.
//! Solvent borrower → `UserIsSolvent` (`debtConfig.silo == 0` / `isSolvent`).
//!
//! # Health state (pin `PartialLiquidation` / `PartialLiquidationLib`)
//!
//! | condition | state |
//! |---|---|
//! | no debt, or `ltv <= collateralConfig.lt` (`isSolvent`) | Healthy |
//! | debt and **zero** coll+protected assets (`NoCollateralToLiquidate`) | BadDebt |
//! | paused silo | Blocked |
//! | else, including `ltv >= 1e18` with coll remaining | Liquidatable |
//!
//! `_BAD_DEBT = 1e18` only widens `liquidationPreview` cover (any amount).
//! `maxLiquidation` still returns amounts when `ltv > lt`.
//!
//! # 10E wire ABI
//!
//! `encode` emits [`ExecutorAdapter::SiloV2`] (id 4). `market` = hook
//! receiver. Tail is empty; `receiveSToken = false` is hardcoded on-chain.
//! Order: ProtocolMismatch, LegOutOfRange, CallbackProviderMismatch,
//! ZeroRecipient, FundingAssetMismatch, FundingShort, AmountTooLarge, then
//! the plan.
//! Calldata for the later WP:
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

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, SolEvent};
use liq_protocol::{
    Archive, BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FlashRoute, Health, LegChoice,
    LiquidationLeg, LiquidationPlan, PositionRef, ProbeCall, Protocol, ProtocolError, Quote,
    Result, StateWriter, Timestamp,
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

alloy_sol_types::sol! {
    /// The silo's own totals (`Silo.sol` @ pin): `getDebtAssets` and
    /// `getCollateralAssets` include interest accrued to `block.timestamp`;
    /// the storage pair and `utilizationData` are as of the last accrual.
    interface ISiloTotals {
        function getDebtAssets() external view returns (uint256);
        function getCollateralAssets() external view returns (uint256);
        function getCollateralAndDebtTotalsStorage() external view returns (uint256, uint256);
        function utilizationData() external view returns (uint256, uint256, uint64);
    }
    /// `ISiloOracle.quote`: `baseAmount` of `baseToken` in the oracle's
    /// quote token.
    interface ISiloOracle {
        function quote(uint256 baseAmount, address baseToken) external view returns (uint256);
    }
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }
    struct Call3Result {
        bool success;
        bytes returnData;
    }
    interface IMulticall3 {
        function aggregate3(Call3[] calldata calls) external payable returns (Call3Result[] memory);
    }
}

/// Multicall3, the same address on every chain.
const MULTICALL3: Address = alloy_primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Whole tokens each side's oracle is asked to quote: enough that an oracle
/// answering in a 6-decimal unit still resolves the ratio to a unit in
/// 10^12. Every live Silo oracle quotes linearly over this range (to 1e-6,
/// survey at block 26132977), as `isSolvent` needs of it at any size.
const QUOTE_WHOLE_TOKENS: u64 = 1_000_000;

/// State-read kinds, `tag = kind << 16 | slot`.
const READ_DEBT_WITH_INTEREST: u64 = 0;
const READ_COLL_WITH_INTEREST: u64 = 1;
const READ_TOTALS_STORAGE: u64 = 2;
const READ_UTILIZATION: u64 = 3;

/// The amount a side's oracle is asked to quote.
fn quote_base(decimals: u8) -> Option<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))?
        .checked_mul(U256::from(QUOTE_WHOLE_TOKENS))
}

/// The pair's price read: one `aggregate3` of both sides' solvency-oracle
/// quotes, so the two answers are of one block, as `isSolvent` reads them.
/// A side without an oracle (`address(0)`) is valued at its raw amount, as
/// `SiloSolvencyLib.getPositionValues` does, and needs no call. The
/// numeraire published first is the side without an oracle (the quote
/// token itself) when there is one, else slot 1.
fn pair_price_read(cfg: &Config, pair: &PairConfig) -> Option<liq_protocol::PriceRead> {
    let mut calls = Vec::new();
    let mut assets = Vec::with_capacity(2);
    for side in [&pair.silo0, &pair.silo1] {
        let a = cfg.asset_by_underlying(side.token)?;
        assets.push(a.asset);
        if side.solvency_oracle != Address::ZERO {
            calls.push(Call3 {
                target: side.solvency_oracle,
                allowFailure: false,
                callData: ISiloOracle::quoteCall {
                    baseAmount: quote_base(a.decimals)?,
                    baseToken: side.token,
                }
                .abi_encode()
                .into(),
            });
        }
    }
    // Slot order unless slot 0 is the oracle-less side.
    let swapped = !(pair.silo0.solvency_oracle == Address::ZERO
        && pair.silo1.solvency_oracle != Address::ZERO);
    if swapped {
        assets.swap(0, 1);
    }
    Some(liq_protocol::PriceRead {
        market: pair.market,
        target: MULTICALL3,
        calldata: IMulticall3::aggregate3Call { calls }.abi_encode().into(),
        tag: u32::from(swapped),
        assets,
    })
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
        let (_, pair) = self
            .cfg
            .pair_by_market(q.key.market)
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
                adapter: ExecutorAdapter::SiloV2,
                market: pair.hook_receiver,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }

    /// One read per listed pair: both sides' solvency-oracle quotes
    /// ([`pair_price_read`]), a ratio the bot restates in USD.
    fn price_reads(&self, rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::PriceRead> {
        self.cfg
            .pairs
            .iter()
            .filter(|p| rows.rows(p.market).is_some_and(|r| !r.is_empty()))
            .filter_map(|p| pair_price_read(&self.cfg, p))
            .collect()
    }

    /// Each side's value of [`QUOTE_WHOLE_TOKENS`] whole tokens in the
    /// pair's quote unit (RAY-scaled by `1e18`; only the ratio is used),
    /// in `read.assets` order.
    fn decode_prices(
        &self,
        read: &liq_protocol::PriceRead,
        ret: &[u8],
        out: &mut Vec<(AssetId, liq_types::Ray)>,
    ) -> Result<()> {
        let (_, pair) = self
            .cfg
            .pair_by_market(read.market)
            .ok_or(ProtocolError::UnknownMarket(read.market))?;
        let answers = IMulticall3::aggregate3Call::abi_decode_returns(ret)
            .map_err(|_| ProtocolError::ProbeDecode)?;
        let mut answers = answers.into_iter();
        let mut values = Vec::with_capacity(2);
        for side in [&pair.silo0, &pair.silo1] {
            let a = self
                .cfg
                .asset_by_underlying(side.token)
                .ok_or(ProtocolError::OracleSourceMismatch)?;
            let v = if side.solvency_oracle == Address::ZERO {
                quote_base(a.decimals).ok_or(ProtocolError::AmountTooLarge)?
            } else {
                let r = answers.next().ok_or(ProtocolError::ProbeDecode)?;
                if !r.success {
                    return Ok(());
                }
                ISiloOracle::quoteCall::abi_decode_returns(&r.returnData)
                    .map_err(|_| ProtocolError::ProbeDecode)?
            };
            if v.is_zero() {
                return Ok(());
            }
            let ray = v
                .checked_mul(liq_types::fixed::WAD)
                .ok_or(ProtocolError::AmountTooLarge)?;
            values.push((a.asset, liq_types::Ray::from_raw(ray)));
        }
        if read.tag == 1 {
            values.swap(0, 1);
        }
        out.extend(values);
        Ok(())
    }

    /// Per borrowed-from silo: its totals with interest and as stored, and
    /// the last accrual's time — the growth the store projects between
    /// reads ([`layout::SiloRow::debt_rate_ray`]). The storage collateral
    /// total is read with them but only the debt's measures the rate.
    fn state_reads(&self, rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::StateRead> {
        let mut out = Vec::new();
        for p in &self.cfg.pairs {
            let Some(rs) = rows.rows(p.market) else {
                continue;
            };
            for (slot, row) in rs.iter().enumerate() {
                let Ok(body) = row.body::<layout::SiloRow>() else {
                    continue;
                };
                // Interest accrues only on debt.
                if body.flags & layout::SiloRow::VIEWED == 0 || body.total_debt_assets == 0 {
                    continue;
                }
                let Ok(slot) = u64::try_from(slot) else {
                    continue;
                };
                let target = Address::from(body.silo);
                let mut read = |kind: u64, calldata: Vec<u8>| {
                    out.push(liq_protocol::StateRead {
                        market: p.market,
                        target,
                        calldata: calldata.into(),
                        tag: kind << 16 | slot,
                    });
                };
                read(
                    READ_DEBT_WITH_INTEREST,
                    ISiloTotals::getDebtAssetsCall {}.abi_encode(),
                );
                read(
                    READ_COLL_WITH_INTEREST,
                    ISiloTotals::getCollateralAssetsCall {}.abi_encode(),
                );
                read(
                    READ_TOTALS_STORAGE,
                    ISiloTotals::getCollateralAndDebtTotalsStorageCall {}.abi_encode(),
                );
                read(
                    READ_UTILIZATION,
                    ISiloTotals::utilizationDataCall {}.abi_encode(),
                );
            }
        }
        out
    }

    fn apply_state_reads(
        &self,
        st: &mut dyn StateWriter,
        timestamp: Timestamp,
        answers: &[liq_protocol::StateAnswer<'_>],
    ) -> Result<Vec<DirtySet>> {
        apply::apply_totals(st, timestamp, answers)
    }
}
