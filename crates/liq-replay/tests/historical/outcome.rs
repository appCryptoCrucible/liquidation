//! What one transaction captured, run on a copy of the pre-state: every
//! party's ETH and token changes, priced at the tip, plus what the block's
//! builder received from it. Captured value is measured before the builder
//! payment (after the base fee, which nobody keeps), so a bid and a bribe
//! compare as what each side chose to give away.

use super::seed::Views;
use super::state::{run, Mode, PreState};
use alloy_primitives::{address, b256, Address, B256, I256, U256};
use alloy_sol_types::sol;
use liq_sim::SimError;
use revm::context::TxEnv;
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use revm::state::Bytecode;
use std::collections::BTreeMap;

const TRANSFER: B256 = b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const WETH_DEPOSIT: B256 =
    b256!("e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c");
const WETH_WITHDRAWAL: B256 =
    b256!("7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65");
pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const AAVE_ORACLE: Address = address!("54586bE62E3c3580375aE3723C145253060Ca0C2");
/// Aave V2's oracle (`config/protocols/aave-v2.toml`): prices in ETH wei per
/// whole token, and lists tokens V3's does not (stETH).
const AAVE_V2_ORACLE: Address = address!("A50ba011c48153De246E5192C8f9258A2ba79Ca9");

sol! {
    interface IPriceSnap {
        function getAssetPrice(address asset) external view returns (uint256);
        function decimals() external view returns (uint8);
        function UNDERLYING_ASSET_ADDRESS() external view returns (address);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub success: bool,
    pub gas_used: u64,
    /// Value captured, in wei: the parties' gains priced at the tip, plus
    /// what the builder got from this transaction.
    pub captured: i128,
    /// What the builder got: priority fee plus any direct payment.
    pub to_builder: i128,
    /// Tokens that moved but have no Aave oracle price (left out of `captured`).
    pub unpriced: Vec<Address>,
}

fn signed(after: U256, before: U256) -> i128 {
    i128::try_from(I256::from_raw(after).saturating_sub(I256::from_raw(before)))
        .unwrap_or(i128::MAX)
}

/// The value of `amount` of `token` in wei of ETH, by Aave V3's oracle at
/// the tip, an aToken as its underlying, or else Aave V2's oracle (in ETH);
/// `None` when none of them prices it.
pub fn eth_value<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    token: Address,
    amount: i128,
) -> Option<i128> {
    if token == WETH {
        return Some(amount);
    }
    let listed = v
        .call(AAVE_ORACLE, IPriceSnap::getAssetPriceCall { asset: token })
        .ok()
        .filter(|p| !p.is_zero());
    if listed.is_none() {
        // An aToken: one per unit of its underlying.
        let Ok(under) = v.call(token, IPriceSnap::UNDERLYING_ASSET_ADDRESSCall {}) else {
            return eth_value_v2(v, token, amount);
        };
        if under != token {
            if let Some(x) = eth_value(v, under, amount) {
                return Some(x);
            }
        }
        return eth_value_v2(v, token, amount);
    }
    let p = v
        .call(AAVE_ORACLE, IPriceSnap::getAssetPriceCall { asset: token })
        .ok()?;
    let pe = v
        .call(AAVE_ORACLE, IPriceSnap::getAssetPriceCall { asset: WETH })
        .ok()?;
    let d = v.call(token, IPriceSnap::decimalsCall {}).ok()?;
    if p.is_zero() || pe.is_zero() {
        return None;
    }
    let mag = U256::from(amount.unsigned_abs()) * p * U256::from(10u8).pow(U256::from(18u8))
        / (pe * U256::from(10u8).pow(U256::from(d)));
    let mag = i128::try_from(mag).ok()?;
    Some(if amount < 0 { -mag } else { mag })
}

/// `amount` of `token` at Aave V2's oracle price (ETH wei per whole token).
fn eth_value_v2<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    token: Address,
    amount: i128,
) -> Option<i128> {
    let p = v
        .call(
            AAVE_V2_ORACLE,
            IPriceSnap::getAssetPriceCall { asset: token },
        )
        .ok()
        .filter(|p| !p.is_zero())?;
    let d = v.call(token, IPriceSnap::decimalsCall {}).ok()?;
    let mag = U256::from(amount.unsigned_abs()) * p / U256::from(10u8).pow(U256::from(d));
    let mag = i128::try_from(mag).ok()?;
    Some(if amount < 0 { -mag } else { mag })
}

/// Run `env` on a copy of the pre-state (with `code` placed and `fund`
/// given ETH for gas) and value what `parties` and the builder came away
/// with.
pub fn measure(
    pre: &PreState,
    env: TxEnv,
    parties: &[Address],
    code: &[(Address, Bytecode)],
    fund: Option<Address>,
) -> Result<Outcome, String> {
    let mut db = pre.db.clone();
    for (at, c) in code {
        liq_sim::place_code(&mut db, *at, c.clone()).map_err(|e| e.to_string())?;
    }
    if let Some(f) = fund {
        let mut info = db
            .basic_ref(f)
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        info.balance = U256::from(10u128.pow(21));
        db.insert_account_info(f, info);
    }
    let coinbase = pre.block.miner;
    let watched: Vec<Address> = parties.iter().copied().chain([coinbase]).collect();
    let before: Vec<U256> = watched
        .iter()
        .map(|a| {
            db.basic_ref(*a)
                .map(|i| i.map_or(U256::ZERO, |i| i.balance))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    let ran = run(&mut db, &pre.block, env, Mode::Chain)?;
    let after = |a: &Address, b: U256| ran.state.get(a).map_or(b, |acct| acct.info.balance);
    let mut eth: i128 = 0;
    let mut to_builder: i128 = 0;
    for (i, a) in watched.iter().enumerate() {
        let d = signed(after(a, before[i]), before[i]);
        if *a == coinbase {
            to_builder = d;
        } else {
            eth += d;
        }
    }
    // Token changes for the parties, from the logs.
    let mut tokens: BTreeMap<Address, i128> = BTreeMap::new();
    let is_party = |w: &B256| {
        parties
            .iter()
            .any(|p| B256::left_padding_from(p.as_slice()) == *w)
    };
    for l in &ran.logs {
        let t = l.topics();
        let amount = || i128::try_from(U256::from_be_slice(&l.data.data)).unwrap_or(i128::MAX);
        match t.first() {
            Some(&TRANSFER) if t.len() == 3 && l.data.data.len() == 32 => {
                if is_party(&t[1]) {
                    *tokens.entry(l.address).or_default() -= amount();
                }
                if is_party(&t[2]) {
                    *tokens.entry(l.address).or_default() += amount();
                }
            }
            Some(&WETH_DEPOSIT) if l.address == WETH && t.len() == 2 && is_party(&t[1]) => {
                *tokens.entry(WETH).or_default() += amount();
            }
            Some(&WETH_WITHDRAWAL) if l.address == WETH && t.len() == 2 && is_party(&t[1]) => {
                *tokens.entry(WETH).or_default() -= amount();
            }
            _ => {}
        }
    }
    let views = Views {
        db: &db,
        at: &pre.parent,
    };
    let mut captured = eth + to_builder;
    let mut unpriced = Vec::new();
    for (token, amount) in tokens {
        if amount == 0 {
            continue;
        }
        match eth_value(&views, token, amount) {
            Some(v) => captured += v,
            None => unpriced.push(token),
        }
    }
    Ok(Outcome {
        success: ran.success,
        gas_used: ran.gas_used,
        captured,
        to_builder,
        unpriced,
    })
}

/// Give `who` ETH on a database, for a run that must pay gas.
pub fn funded<D: DatabaseRef>(db: &mut CacheDB<D>, who: Address) {
    let mut info = db.basic_ref(who).ok().flatten().unwrap_or_default();
    info.balance = U256::from(10u128.pow(21));
    db.insert_account_info(who, info);
}
