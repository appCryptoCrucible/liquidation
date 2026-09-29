//! Token-contract logs: aToken `BalanceTransfer` (collateral transfers, and
//! the liquidation protocol fee to the treasury) and `StableDebtToken`
//! `Mint`/`Burn` (stable debt, not modelled: the account is not quoted).
//! Oracle: `LiquidationLogic.executeLiquidationCall` and
//! `StableDebtToken.burn` at the deployed code (see apply.rs).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use alloy_primitives::{uint, Address, U256};
use alloy_sol_types::SolEvent;
use common::*;
use liq_adapters_aave_v3::events::{cfg as ccfg, pool, stable, token};
use liq_protocol::{Protocol, ProtocolError, StateWriter};
use liq_types::fixed::RAY;
use liq_types::{LogSubscriber, PositionId};

const S_WETH: Address = Address::repeat_byte(0xb6);
const TREASURY: Address = Address::repeat_byte(0x7e);

/// The fixture pool, with WETH's stable debt token recorded at listing.
fn listed(d: &Deploy) -> Vec<OwnedLog> {
    let mut logs = listing_logs(d);
    logs[0] = log(
        d.configurator,
        &ccfg::ReserveInitialized {
            asset: d.weth,
            aToken: d.a_weth,
            stableDebtToken: S_WETH,
            variableDebtToken: d.v_weth,
            interestRateStrategyAddress: Address::repeat_byte(0xd1),
        },
        DEPLOY_BLOCK,
        T0,
    );
    logs
}

fn state(d: &Deploy, extra: Vec<OwnedLog>) -> liq_protocol::conformance::JournalStore {
    let p = d.adapter();
    let mut logs = listed(d);
    logs.extend(activity_logs(d));
    logs.extend(extra);
    store_after(&p, &logs)
}

fn transfer(d: &Deploy, from: Address, to: Address, scaled: U256) -> OwnedLog {
    log(
        d.a_weth,
        &token::BalanceTransfer {
            from,
            to,
            value: scaled,
            index: RAY,
        },
        DEPLOY_BLOCK + 2,
        T0,
    )
}

fn weth_supply(st: &liq_protocol::conformance::JournalStore, who: Address) -> u128 {
    let id = (0..st.positions_len())
        .map(PositionId)
        .find(|id| st.position_key(*id).is_ok_and(|k| k.user == who))
        .unwrap();
    st.supply(id, WETH_SLOT).unwrap()
}

/// Moving aWETH moves collateral: the sender's supply drops, the
/// receiver's rises, by the event's scaled amount.
#[test]
fn atoken_transfer_moves_collateral() {
    let d = Deploy::new();
    let tenth = uint!(100_000_000_000_000_000_U256);
    let st = state(&d, vec![transfer(&d, d.alice, d.bob, tenth)]);
    assert_eq!(weth_supply(&st, d.alice), 900_000_000_000_000_000);
    assert_eq!(weth_supply(&st, d.bob), 100_000_000_000_000_000);
}

/// `receiveAToken`: the collateral reaches the liquidator by
/// `BalanceTransfer`, and the fee reaches the treasury the same way; the
/// `LiquidationCall` event after them only repays debt. The borrower is
/// debited once for each.
#[test]
fn receive_atoken_liquidation_debits_collateral_once_and_the_fee() {
    let d = Deploy::new();
    let liquidator = Address::repeat_byte(0xee);
    let seized = uint!(50_000_000_000_000_000_U256);
    let fee = uint!(250_000_000_000_000_U256);
    let st = state(
        &d,
        vec![
            transfer(&d, d.alice, TREASURY, fee),
            transfer(&d, d.alice, liquidator, seized),
            log(
                d.pool,
                &pool::LiquidationCall {
                    collateralAsset: d.weth,
                    debtAsset: d.dai,
                    user: d.alice,
                    debtToCover: uint!(100_000_000_000_000_000_000_U256),
                    liquidatedCollateralAmount: seized,
                    liquidator,
                    receiveAToken: true,
                },
                DEPLOY_BLOCK + 2,
                T0,
            ),
        ],
    );
    let want = 1_000_000_000_000_000_000u128 - 50_000_000_000_000_000 - 250_000_000_000_000;
    assert_eq!(weth_supply(&st, d.alice), want);
    assert_eq!(weth_supply(&st, liquidator), 50_000_000_000_000_000);
    assert_eq!(weth_supply(&st, TREASURY), 250_000_000_000_000);
}

/// Without `receiveAToken` the collateral is burned: only the event debits it.
#[test]
fn burned_collateral_is_debited_by_the_liquidation_event() {
    let d = Deploy::new();
    let seized = uint!(50_000_000_000_000_000_U256);
    let st = state(
        &d,
        vec![log(
            d.pool,
            &pool::LiquidationCall {
                collateralAsset: d.weth,
                debtAsset: d.dai,
                user: d.alice,
                debtToCover: uint!(100_000_000_000_000_000_000_U256),
                liquidatedCollateralAmount: seized,
                liquidator: Address::repeat_byte(0xee),
                receiveAToken: false,
            },
            DEPLOY_BLOCK + 2,
            T0,
        )],
    );
    assert_eq!(weth_supply(&st, d.alice), 950_000_000_000_000_000);
}

fn stable_mint(d: &Deploy, amount: U256) -> OwnedLog {
    let _ = d;
    log(
        S_WETH,
        &stable::Mint {
            user: Address::repeat_byte(0xc1),
            onBehalfOf: Address::repeat_byte(0xc1),
            amount,
            currentBalance: U256::ZERO,
            balanceIncrease: U256::ZERO,
            newRate: U256::ZERO,
            avgStableRate: U256::ZERO,
            newTotalSupply: amount,
        },
        DEPLOY_BLOCK + 2,
        T0,
    )
}

fn stable_burn(amount: U256, current: U256, increase: U256) -> OwnedLog {
    log(
        S_WETH,
        &stable::Burn {
            from: Address::repeat_byte(0xc1),
            amount,
            currentBalance: current,
            balanceIncrease: increase,
            avgStableRate: U256::ZERO,
            newTotalSupply: U256::ZERO,
        },
        DEPLOY_BLOCK + 3,
        T0,
    )
}

/// Stable debt the adapter does not model makes Alice unquotable; repaying
/// all of it (`currentBalance - (amount + balanceIncrease) == 0`) restores
/// her; a partial repay does not.
#[test]
fn stable_debt_blocks_the_account_until_it_is_gone() {
    let d = Deploy::new();
    let p = d.adapter();
    let px = prices(WETH_P8, DAI_P8);
    let one = uint!(1_000_000_000_000_000_000_U256);

    let st = state(&d, vec![]);
    assert!(p.health(st.view(ALICE_ID, T0).unwrap(), &px).is_ok());

    let st = state(&d, vec![stable_mint(&d, one)]);
    assert!(matches!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::UntrackedDebt)
    ));
    assert!(matches!(
        p.quote(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::UntrackedDebt)
    ));

    // Partial: 1.01 owed, 0.5 repaid (event amount = 0.5 - 0.01 accrued).
    let accrued = uint!(10_000_000_000_000_000_U256);
    let st = state(
        &d,
        vec![
            stable_mint(&d, one),
            stable_burn(uint!(490_000_000_000_000_000_U256), one + accrued, accrued),
        ],
    );
    assert!(p.health(st.view(ALICE_ID, T0).unwrap(), &px).is_err());

    // Full: amount + accrued == currentBalance.
    let st = state(
        &d,
        vec![
            stable_mint(&d, one),
            stable_burn(one, one + accrued, accrued),
        ],
    );
    assert!(p.health(st.view(ALICE_ID, T0).unwrap(), &px).is_ok());
}

/// The pool's tokens are subscribed for the three token events, and the
/// stable-borrowing switch on the configurator.
#[test]
fn token_logs_are_subscribed() {
    let d = Deploy::new();
    let subs = d.adapter().subscriptions();
    for (addr, t0) in [
        (d.a_weth, token::BalanceTransfer::SIGNATURE_HASH),
        (d.a_dai, stable::Mint::SIGNATURE_HASH),
        (d.a_dai, stable::Burn::SIGNATURE_HASH),
        (
            d.configurator,
            ccfg::ReserveStableRateBorrowing::SIGNATURE_HASH,
        ),
    ] {
        assert!(
            subs.iter().any(|f| f.address == addr && f.topic0 == t0),
            "{addr} {t0}"
        );
    }
}
