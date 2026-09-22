//! Gas-compensation economics only. The liquidator does not repay BOLD and
//! does not seize trove collateral; `BonusCurve::Static { bonus: 0 }` — never
//! a flash-repay-seize incentive mantissa.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, Quote, RepayOption, Result, SeizeOption,
};
use liq_types::{AssetId, PriceVector, Ray};
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::math::{coll_gas_from_offset, value_wad, ETH_GAS_COMPENSATION};

pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
    weth: AssetId,
    weth_decimals: u8,
) -> Result<Option<Quote>> {
    let t0 = terms(pos)?;
    let (mut t, health) = finish(&t0, px)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    t.p_weth = crate::health::price_ray(px, weth)?;
    let coll_gas = coll_gas_from_offset(
        t.entire_coll,
        t.entire_debt,
        U256::from(t.branch.sp_bold_deposits),
    )?;
    let eth_gas = ETH_GAS_COMPENSATION;
    let eth_notional = value_wad(eth_gas, t.p_weth, weth_decimals)?;
    let coll_notional = if coll_gas.is_zero() {
        U256::ZERO
    } else {
        value_wad(coll_gas, t.p_coll, t.coll_row.decimals)?
    };
    let notional = eth_notional
        .checked_add(coll_notional)
        .ok_or(liq_types::fixed::FixedError::Overflow)?;
    if notional.is_zero() || notional > cons.per_liquidation_notional_cap.raw() {
        return Ok(None);
    }

    let bonus = Ray::ZERO;
    let curve = BonusCurve::Static { bonus };
    let mut seize_options = SmallVec::new();
    if t.coll_row.asset == weth {
        let max_seize = eth_gas
            .checked_add(coll_gas)
            .ok_or(liq_types::fixed::FixedError::Overflow)?;
        seize_options.push(SeizeOption {
            asset: weth,
            max_seize,
            bonus,
            curve,
            call_target: alloy_primitives::Address::ZERO,
        });
    } else {
        let weth_first = eth_notional >= coll_notional;
        let weth_opt = SeizeOption {
            asset: weth,
            max_seize: eth_gas,
            bonus,
            curve,
            call_target: alloy_primitives::Address::ZERO,
        };
        let coll_opt = SeizeOption {
            asset: t.coll_row.asset,
            max_seize: coll_gas,
            bonus,
            curve,
            call_target: alloy_primitives::Address::ZERO,
        };
        if coll_gas.is_zero() {
            seize_options.push(weth_opt);
        } else if weth_first {
            seize_options.push(weth_opt);
            seize_options.push(coll_opt);
        } else {
            seize_options.push(coll_opt);
            seize_options.push(weth_opt);
        }
    }

    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        asset: t.loan_row.asset,
        // Liquidator does not repay BOLD. SP is the counterparty. Zero is the
        // protocol truth — not a flash size.
        max_repay: U256::ZERO,
    });
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}
