//! Rebuild 04A store slices from a Foundry `DiffCase` and run `health()`.

use alloy_primitives::{Address, U256};
use liq_adapters_aave_v4::layout::{
    HubAsset, Reserve, ReserveCfg, SpokeMeta, UserExtra, UserReserve, META_ASSET,
};
use liq_adapters_aave_v4::math::{split, P8_TO_RAY};
use liq_adapters_aave_v4::{AaveV4, AssetConfig, Config, HubConfig, SourcePin, SpokeConfig};
use liq_protocol::{
    AssetMask, FeedId, Health, MarketFlags, MarketRow, PositionExtraRepr, PositionRef, Protocol,
    ProtocolError,
};
use liq_types::fixed::WAD_RAY_RATIO;
use liq_types::{
    AssetId, MarketId, PositionId, PositionKey, Price, PriceVector, ProtocolId, Ray, SourceKind,
};
use thiserror::Error;

use super::abi::{DiffCase, DiffSlot};

const USING: u8 = 1 << 0;
const PAUSED: u8 = 1 << 1;
const ACTIVE: u8 = 1 << 2;
const HALTED: u8 = 1 << 3;

/// Adapter `health()` expressed in the on-chain view's units (WAD HF, Value, RAY·Value).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AdapterView {
    pub health_factor_wad: U256,
    pub total_collateral_value: U256,
    pub total_debt_value_ray: U256,
}

#[derive(Debug, Error)]
pub enum CaseError {
    #[error("adapter: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("config: {0}")]
    Config(#[from] liq_adapters_aave_v4::ConfigError),
    #[error("lastUpdate does not fit MarketRow::last_update")]
    LastUpdate,
    #[error("decimals > 18")]
    Decimals,
}

/// Run 04A `health()` on `case`. Expected values come from Foundry `HealthOracle`, not here.
pub fn adapter_view(case: &DiffCase) -> Result<AdapterView, CaseError> {
    let proto = adapter()?;
    let built = Built::from_case(case)?;
    let health = proto.health(built.pos(), &built.px)?;
    view_from_health(health)
}

fn view_from_health(h: Health) -> Result<AdapterView, CaseError> {
    let health_factor_wad = if h.hf.raw() == U256::MAX {
        U256::MAX
    } else {
        h.hf.raw()
            .checked_div(WAD_RAY_RATIO)
            .ok_or(ProtocolError::Fixed(
                liq_types::fixed::FixedError::DivisionByZero,
            ))?
    };
    Ok(AdapterView {
        health_factor_wad,
        total_collateral_value: h.collateral_value.raw(),
        total_debt_value_ray: h.debt_value.raw(),
    })
}

fn adapter() -> Result<AaveV4, CaseError> {
    let spoke = Address::repeat_byte(0x22);
    Ok(AaveV4::new(Config {
        protocol: ProtocolId(4),
        hubs: vec![HubConfig {
            address: Address::repeat_byte(0x11),
            market: MarketId(1),
        }],
        spokes: vec![SpokeConfig {
            address: spoke,
            market: MarketId(2),
            oracle: Address::repeat_byte(0x33),
        }],
        assets: vec![
            AssetConfig {
                underlying: Address::repeat_byte(0xa0),
                asset: AssetId(0),
                feed: FeedId(0),
            },
            AssetConfig {
                underlying: Address::repeat_byte(0xa1),
                asset: AssetId(1),
                feed: FeedId(1),
            },
        ],
        price_sources: vec![
            SourcePin {
                spoke,
                reserve_id: 0,
                source: Address::repeat_byte(0xb0),
            },
            SourcePin {
                spoke,
                reserve_id: 1,
                source: Address::repeat_byte(0xb1),
            },
        ],
        pinned_through: 0,
    })?)
}

struct Built {
    key: PositionKey,
    config: AssetMask,
    supply: [u128; 3],
    debt: [u128; 3],
    extra: PositionExtraRepr,
    slot_extra: [PositionExtraRepr; 3],
    markets: [MarketRow; 3],
    timestamp: u64,
    px: PriceVector,
}

impl Built {
    fn from_case(c: &DiffCase) -> Result<Self, CaseError> {
        let mut config = AssetMask::EMPTY;
        if c.coll.suppliedShares != 0 || c.coll.drawnShares != 0 {
            config =
                config
                    .with(1)
                    .ok_or(ProtocolError::SlotOutOfRange(liq_protocol::MarketSlot {
                        market: MarketId(2),
                        slot: 1,
                    }))?;
        }
        if c.debt.suppliedShares != 0 || c.debt.drawnShares != 0 {
            config =
                config
                    .with(2)
                    .ok_or(ProtocolError::SlotOutOfRange(liq_protocol::MarketSlot {
                        market: MarketId(2),
                        slot: 2,
                    }))?;
        }
        let mut extra = PositionExtraRepr::ZERO;
        *extra.view_mut::<UserExtra>()? = UserExtra {
            risk_premium: 0,
            _pad: [0; 12],
        };
        Ok(Self {
            key: PositionKey {
                protocol: ProtocolId(4),
                market: MarketId(2),
                user: Address::repeat_byte(0xc1),
            },
            config,
            supply: [0, c.coll.suppliedShares, c.debt.suppliedShares],
            debt: [0, c.coll.drawnShares, c.debt.drawnShares],
            extra,
            slot_extra: [
                PositionExtraRepr::ZERO,
                user_extra(&c.coll)?,
                user_extra(&c.debt)?,
            ],
            markets: [
                meta_row(c.spokeKind)?,
                reserve_row(AssetId(0), &c.coll, c.isolation)?,
                reserve_row(AssetId(1), &c.debt, false)?,
            ],
            timestamp: c.timestamp,
            px: prices(c)?,
        })
    }

    fn pos(&self) -> PositionRef<'_> {
        PositionRef {
            id: PositionId(0),
            key: &self.key,
            config: self.config,
            supply: &self.supply,
            debt: &self.debt,
            extra: &self.extra,
            slot_extra: &self.slot_extra,
            markets: &self.markets,
            timestamp: self.timestamp,
        }
    }
}

fn user_extra(s: &DiffSlot) -> Result<PositionExtraRepr, CaseError> {
    let (lo, hi) = split(s.premiumOffsetRay.into_raw());
    let mut e = PositionExtraRepr::ZERO;
    *e.view_mut::<UserReserve>()? = UserReserve {
        premium_shares: s.premiumShares,
        premium_offset_lo: lo,
        premium_offset_hi: hi,
        collateral_factor: s.collateralFactor,
        liquidation_fee: 0,
        max_liquidation_bonus: 0,
        dyn_key: 0,
        flags: if s.flags & USING != 0 {
            UserReserve::USING_AS_COLLATERAL
        } else {
            0
        },
        _pad: [0; 3],
    };
    Ok(e)
}

fn meta_row(spoke_kind: u8) -> Result<MarketRow, CaseError> {
    let (target_hf, hf_for_max, bonus): (u128, u64, u16) = match spoke_kind {
        0 => (1_050_000_000_000_000_000, 700_000_000_000_000_000, 20_00),
        1 => (1_010_000_000_000_000_000, 800_000_000_000_000_000, 5_00),
        2 => (1_250_000_000_000_000_000, 500_000_000_000_000_000, 50_00),
        _ => (1_000_000_000_000_000_000, 900_000_000_000_000_000, 1_00),
    };
    let mut row = MarketRow::blank(META_ASSET, 0);
    *row.body_mut::<SpokeMeta>()? = SpokeMeta {
        target_hf,
        hf_for_max_bonus: hf_for_max,
        bonus_factor: bonus,
        _pad: [0; 6],
        priced: 0b11,
    };
    Ok(row)
}

fn reserve_row(asset: AssetId, s: &DiffSlot, isolated: bool) -> Result<MarketRow, CaseError> {
    if s.decimals > 18 {
        return Err(CaseError::Decimals);
    }
    let last = u32::try_from(s.lastUpdate.to::<u64>()).map_err(|_| CaseError::LastUpdate)?;
    let (plo, phi) = split(s.poolPremiumOffsetRay.into_raw());
    let (dlo, dhi) = split(s.deficitRay);
    let mut flags = MarketFlags::NONE;
    if s.flags & PAUSED != 0 {
        flags = MarketFlags(flags.0 | MarketFlags::PAUSED.0);
    }
    if isolated {
        flags = MarketFlags(flags.0 | MarketFlags::ISOLATED.0);
    }
    let mut cfg_flags = 0u8;
    if s.flags & PAUSED != 0 {
        cfg_flags |= ReserveCfg::PAUSED;
    }
    if s.flags & ACTIVE != 0 {
        cfg_flags |= ReserveCfg::SPOKE_ACTIVE;
    }
    if s.flags & HALTED != 0 {
        cfg_flags |= ReserveCfg::SPOKE_HALTED;
    }
    let mut row = MarketRow::blank(asset, s.decimals);
    row.price_feed = FeedId(asset.0);
    row.flags = flags;
    row.hub_slot = asset.0;
    row.hub_market = 1;
    row.last_update = last;
    *row.body_mut::<Reserve>()? = Reserve {
        hub: HubAsset {
            drawn_index: s.drawnIndex,
            drawn_rate: s.drawnRate,
            drawn_shares: s.poolDrawnShares,
            premium_shares: s.poolPremiumShares,
            premium_offset_lo: plo,
            premium_offset_hi: phi,
            liquidity: s.liquidity,
            swept: s.swept,
            realized_fees: s.realizedFees,
            added_shares: s.addedShares,
            deficit_ray_lo: dlo,
            deficit_ray_hi: dhi,
            liquidity_fee: s.liquidityFee,
            _pad: [0; 14],
        },
        cfg: ReserveCfg {
            collateral_factor: s.collateralFactor,
            liquidation_fee: 0,
            max_liquidation_bonus: 0,
            dyn_key: 0,
            collateral_risk: 0,
            flags: cfg_flags,
            _pad: [0; 15],
        },
    };
    Ok(row)
}

fn prices(c: &DiffCase) -> Result<PriceVector, CaseError> {
    let p = |asset: AssetId, p8: U256, ts: u64| -> Result<Price, CaseError> {
        let raw = p8
            .checked_mul(P8_TO_RAY)
            .ok_or(ProtocolError::Fixed(liq_types::fixed::FixedError::Overflow))?;
        Ok(Price {
            asset,
            price: Ray::from_raw(raw),
            source: SourceKind::Canonical,
            block: 1,
            ts,
        })
    };
    Ok(PriceVector(vec![
        p(AssetId(0), c.coll.priceP8, c.timestamp)?,
        p(AssetId(1), c.debt.priceP8, c.timestamp)?,
    ]))
}
