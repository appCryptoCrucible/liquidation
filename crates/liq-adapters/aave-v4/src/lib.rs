//! Aave V4 adapter (WP 04A) — the first real [`Protocol`], pinned to
//! `aave/aave-v4` @ `40232a0a91150d8ee5cab42bd3ddd0baf4ffff9f` (main,
//! 2026-09-15). The template every later adapter follows.
//!
//! Module map — each answers "what does the chain do?" for one trait method:
//!
//! | module | chain source | trait method |
//! |---|---|---|
//! | [`layout`] | `IHub.Asset`, `ISpoke.{Reserve,UserPosition,…}` | store shape |
//! | [`math`] | `WadRayMath`, `MathUtils`, `SharesMath`, `AssetLogic`, `Premium` | every step |
//! | [`health`] | `Spoke._processUserAccountData` | `health` |
//! | [`solve`] | (rational solve over `health`) | `liquidation_price`, `time_to_cross` |
//! | [`quote`] | `LiquidationLogic._calculateLiquidationAmounts` | `quote` |
//! | [`apply`] | every `emit` in `Hub.sol`/`Spoke.sol`/`AaveOracle.sol` | `apply_log`, `backfill` |
//! | [`events`] | `IHubBase`, `IHub`, `ISpoke`, `IAaveOracle` | `subscriptions` |
//! | this file | `Executor.sol` `A_AAVE_V4`, `Spoke.getUserAccountData` | `encode`, `health_probe` |
//!
//! Every arithmetic step's rounding direction is tabulated in
//! `docs/coverage/aave-v4-rounding.md`; `math` is that table as code.
//!
//! **Allocation-free hot path (D57).** `health`, `liquidation_price` and
//! `time_to_cross` are stack arithmetic over borrowed slices — no heap type
//! is named anywhere on their paths (`health.rs`, `solve.rs`), and their
//! return types are `Copy`/inline. That is the structural proof. The
//! measured proof runs through the conformance harness's `AllocMeter`
//! seam: [`alloc_meter`] hands the harness the process allocation counter
//! when the sanctioned counting allocator (`liq-bot`'s `PanicOnAlloc`, WP
//! 16A) is installed, and `None` — reported as `Report::alloc_metered ==
//! false` — until it is. No substitute counter is invented here.

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
    Result, StateWriter, Timestamp,
};
use liq_types::{AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId, Ray};

pub use config::{AssetConfig, Config, ConfigError, HubConfig, SourcePin, SpokeConfig};

use crate::events::{halt, hub, oracle, spoke};
use crate::math::hf_wad_to_ray;

sol! {
    /// `ISpoke.getUserAccountData` — the drift detector's ground truth.
    struct UserAccountData {
        uint256 riskPremium;
        uint256 avgCollateralFactor;
        uint256 healthFactor;
        uint256 totalCollateralValue;
        uint256 totalDebtValueRay;
        uint256 activeCollateralCount;
        uint256 borrowCount;
    }
    function getUserAccountData(address user) external view returns (UserAccountData memory);
}

/// The adapter: one deployment ([`Config`]), immutable after construction.
#[derive(Clone, Debug)]
pub struct AaveV4 {
    cfg: Config,
}

impl AaveV4 {
    /// Validates `cfg` ([`Config::validate`]).
    pub fn new(cfg: Config) -> core::result::Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self { cfg })
    }

    /// The deployment this instance tracks.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.cfg
    }
}

/// The conformance harness's allocation meter (`liq_protocol::conformance::
/// AllocMeter`) for this process. `Some` only when the sanctioned counting
/// allocator is installed — WP 16A's `PanicOnAlloc` in `liq-bot` — and it
/// exposes its counter; `None` here until then, which the harness reports as
/// `alloc_metered == false` rather than asserting a vacuous zero.
///
/// The seam is a plain `fn() -> u64` so 16A wires it without this crate
/// depending on `liq-bot` (dep-lint).
#[must_use]
pub fn alloc_meter() -> Option<&'static (dyn Fn() -> u64 + Sync)> {
    None
}

/// Decode `getUserAccountData`'s return into the normalised health factor.
fn decode_probe(raw: &[u8]) -> Result<Ray> {
    let out =
        getUserAccountDataCall::abi_decode_returns(raw).map_err(|_| ProtocolError::ProbeDecode)?;
    hf_wad_to_ray(out.healthFactor)
}

impl LogSubscriber for AaveV4 {
    fn subscriptions(&self) -> Vec<LogFilter> {
        const HUB: [alloy_primitives::B256; 16] = [
            hub::Add::SIGNATURE_HASH,
            hub::Remove::SIGNATURE_HASH,
            hub::Draw::SIGNATURE_HASH,
            hub::Restore::SIGNATURE_HASH,
            hub::RefreshPremium::SIGNATURE_HASH,
            hub::ReportDeficit::SIGNATURE_HASH,
            hub::TransferShares::SIGNATURE_HASH,
            hub::Sweep::SIGNATURE_HASH,
            hub::Reclaim::SIGNATURE_HASH,
            hub::EliminateDeficit::SIGNATURE_HASH,
            hub::MintFeeShares::SIGNATURE_HASH,
            hub::AddAsset::SIGNATURE_HASH,
            hub::UpdateAsset::SIGNATURE_HASH,
            hub::UpdateAssetConfig::SIGNATURE_HASH,
            hub::AddSpoke::SIGNATURE_HASH,
            hub::UpdateSpokeConfig::SIGNATURE_HASH,
        ];
        const SPOKE: [alloy_primitives::B256; 21] = [
            spoke::Supply::SIGNATURE_HASH,
            spoke::Withdraw::SIGNATURE_HASH,
            spoke::Borrow::SIGNATURE_HASH,
            spoke::Repay::SIGNATURE_HASH,
            spoke::LiquidationCall::SIGNATURE_HASH,
            spoke::ReportDeficit::SIGNATURE_HASH,
            spoke::SetUsingAsCollateral::SIGNATURE_HASH,
            spoke::UpdateUserRiskPremium::SIGNATURE_HASH,
            spoke::RefreshPremiumDebt::SIGNATURE_HASH,
            spoke::RefreshAllUserDynamicConfig::SIGNATURE_HASH,
            spoke::RefreshSingleUserDynamicConfig::SIGNATURE_HASH,
            spoke::SetUserPositionManager::SIGNATURE_HASH,
            spoke::SetSpokeImmutables::SIGNATURE_HASH,
            spoke::UpdateLiquidationConfig::SIGNATURE_HASH,
            spoke::AddReserve::SIGNATURE_HASH,
            spoke::UpdateReserveConfig::SIGNATURE_HASH,
            spoke::UpdateReservePriceSource::SIGNATURE_HASH,
            spoke::AddDynamicReserveConfig::SIGNATURE_HASH,
            spoke::UpdateDynamicReserveConfig::SIGNATURE_HASH,
            spoke::UpdatePositionManager::SIGNATURE_HASH,
            halt::AuthorityUpdated::SIGNATURE_HASH,
        ];
        const ORACLE: [alloy_primitives::B256; 2] = [
            oracle::UpdateReserveSource::SIGNATURE_HASH,
            oracle::SetSpoke::SIGNATURE_HASH,
        ];
        const PROXY: [alloy_primitives::B256; 3] = [
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
        ];
        let mut out = Vec::new();
        for h in &self.cfg.hubs {
            for t0 in HUB
                .iter()
                .chain(&PROXY)
                .chain([&halt::AuthorityUpdated::SIGNATURE_HASH])
            {
                out.push(LogFilter {
                    address: h.address,
                    topic0: *t0,
                });
            }
        }
        for s in &self.cfg.spokes {
            for t0 in SPOKE.iter().chain(&PROXY) {
                out.push(LogFilter {
                    address: s.address,
                    topic0: *t0,
                });
            }
            for t0 in ORACLE.iter().chain(&PROXY) {
                out.push(LogFilter {
                    address: s.oracle,
                    topic0: *t0,
                });
            }
        }
        out
    }
}

impl Protocol for AaveV4 {
    fn id(&self) -> ProtocolId {
        self.cfg.protocol
    }

    fn apply_log(&self, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
        apply::apply_log(&self.cfg, st, log)
    }

    /// Folds every subscribed log from the deployment's first block through
    /// `to`. Halt-class logs at or before `pinned_through` are the audited
    /// deployment; a later one aborts the backfill with `HaltSignal`.
    fn backfill(&self, st: &mut dyn StateWriter, src: &dyn Archive, to: BlockNum) -> Result<()> {
        let filters = self.subscriptions();
        src.logs(&filters, 0, to, &mut |log| {
            apply::apply_log(&self.cfg, st, log).map(|_| ())
        })
    }

    /// `Spoke._processUserAccountData(user, false)` at `pos.timestamp` and
    /// `px`, plus the liquidation-state classification.
    ///
    /// Errors: `MissingPrice(asset)` — `px` has no (or a zero) entry for a
    /// held asset; `OracleSourceMismatch` — a held slot is `UNPRICED` (its
    /// source is not the registry pin) or its underlying is unmapped;
    /// `SlotOutOfRange` — a set `config` bit past the market's rows;
    /// `BodyLayout`/`ExtraLayout` — a row or extra not shaped by this
    /// adapter; `TimestampBeforeUpdate` — `pos.timestamp` precedes a row's
    /// `last_update`; `Fixed(Overflow | Underflow)` — an intermediate the
    /// chain would also revert on (a negative premium, a `uint256` product
    /// overflow).
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
        let spoke = self
            .cfg
            .spoke_by_market(q.key.market)
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
                adapter: ExecutorAdapter::AaveV4,
                market: spoke.address,
                borrower: q.key.user,
                collateral_asset,
                repay_amount: wire(repay.max_repay)?,
            },
        })
    }

    /// `Spoke.getUserAccountData(user)` on the position's spoke; the decoder
    /// lifts `healthFactor` (WAD) to RAY. `ProbeUnavailable` when the
    /// position's market is not one of this adapter's spokes.
    fn health_probe(&self, pos: PositionRef<'_>) -> Result<ProbeCall> {
        let spoke = self
            .cfg
            .spoke_by_market(pos.key.market)
            .ok_or(ProtocolError::ProbeUnavailable)?;
        let call = getUserAccountDataCall { user: pos.key.user };
        Ok(ProbeCall {
            to: spoke.address,
            data: Bytes::from(call.abi_encode()),
            decode: decode_probe,
        })
    }
}
