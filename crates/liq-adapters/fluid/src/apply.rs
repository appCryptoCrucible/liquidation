//! Fluid state: vault rows from the bind-time read, and each vault's
//! liquidation as the vault itself reports it every block.
//!
//! Every block, per vault, two dead-address simulations (`liquidate` on T1,
//! `simulateLiquidate` on T2–T4; with and without `absorb_`) give the
//! liquidation in the vault's own units. A smart side's shares are then
//! priced in each of its two tokens by the DEX's own estimate
//! (`paybackPerfectInOneToken` / `withdrawPerfectInOneToken`), at the same
//! block. Nothing about Fluid's tick tree is reproduced.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError, SolEvent};
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtySet, MarketFlags, MarketRow, MarketSlot, PositionExtraRepr,
    ProtocolError, Result, StateAnswer, StateRead, StateWriter, Timestamp,
};
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::{Config, VaultPin};
use crate::events::{dex, factory, vault};
use crate::layout::{CatalogEntry, VaultExtra, VaultRow, CATALOG_ASSET, UNMAPPED_ASSET, VAULT_T1};

/// Fluid `X128`: `liquidate`'s "everything" debt amount (resolver pin).
const X128: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
/// Dead address: Fluid simulations revert with the result when `to_` is it.
const DEAD: Address = alloy_primitives::address!("0x000000000000000000000000000000000000dEaD");
/// Maximum for a one-token payback estimate (only the estimate is read).
const ESTIMATE_MAX: U256 = U256::from_limbs([0, 0, 0, 1 << 63]);

// Read tags: `pin index << 8 | kind`.
const K_SIM: u64 = 0; // + absorb
const K_PAYBACK: u64 = 2; // + absorb * 4 + token
const K_WITHDRAW: u64 = 4; // + absorb * 4 + token

#[inline]
fn tag(pin: usize, kind: u64) -> u64 {
    (u64::try_from(pin).unwrap_or(u64::MAX >> 8) << 8) | kind
}

/// `base + absorb·4 + token`: which follow-up a tag is.
#[inline]
fn kind_of(base: u64, absorb: u64, token: u64) -> u64 {
    base.saturating_add(absorb.saturating_mul(4))
        .saturating_add(token)
}

#[inline]
fn untag(t: u64) -> (usize, u64) {
    (usize::try_from(t >> 8).unwrap_or(usize::MAX), t & 0xff)
}

fn addr20(a: Address) -> [u8; 20] {
    a.into_array()
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

pub(crate) fn apply_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if log.address != cfg.factory {
        return Err(ProtocolError::UnexpectedLog);
    }
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    if topic0 != factory::VaultDeployed::SIGNATURE_HASH {
        return Err(ProtocolError::UnexpectedLog);
    }
    let ev = factory::VaultDeployed::decode_raw_log(log.topics.iter().copied(), log.data)
        .map_err(|_| ProtocolError::MalformedLog)?;
    match cfg.pin_of(ev.vault) {
        Some(pin) => {
            ensure_vault(cfg, st, pin)?;
        }
        None => tracing::warn!(
            target: "coverage",
            vault = %ev.vault,
            vault_id = %ev.vaultId,
            "fluid vault deployed after bind — not read until the next restart"
        ),
    }
    Ok(DirtySet::None)
}

/// Catalog entry + the vault's market rows, once. Returns its market.
pub(crate) fn ensure_vault(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pin: &VaultPin,
) -> Result<MarketId> {
    let market = pin.market();
    if st.markets(market).is_ok_and(|r| !r.is_empty()) {
        return Ok(market);
    }
    let mut cat = MarketRow::blank(CATALOG_ASSET, 0);
    {
        let e: &mut CatalogEntry = cat.body_mut()?;
        e.vault = addr20(pin.vault);
        e.vault_id = pin.vault_id;
        e.market = market.0;
    }
    st.push_market(cfg.catalog, cat)?;
    let cols = pin.col_tokens();
    let debts = pin.debt_tokens();
    let body = VaultRow {
        vault: addr20(pin.vault),
        supply: addr20(pin.supply),
        borrow: addr20(pin.borrow),
        supply0: addr20(pin.supply0),
        supply1: addr20(pin.supply1),
        borrow0: addr20(pin.borrow0),
        borrow1: addr20(pin.borrow1),
        vault_id: pin.vault_id,
        vault_type: pin.vault_type,
        n_col: u8::try_from(cols.len()).map_err(|_| ProtocolError::Internal)?,
        n_debt: u8::try_from(debts.len()).map_err(|_| ProtocolError::Internal)?,
        position_plus1: 0,
        _pad: [0; 2],
    };
    for (i, (tok, dec)) in cols.iter().chain(debts.iter()).enumerate() {
        let mapped = cfg.asset_of_token(*tok);
        let mut row = MarketRow::blank(
            mapped.map_or(UNMAPPED_ASSET, |a| a.asset),
            mapped.map_or(*dec, |a| a.decimals),
        );
        if mapped.is_none() {
            row.flags = MarketFlags::UNPRICED;
        }
        if i == 0 {
            *row.body_mut::<VaultRow>()? = body;
        }
        st.push_market(market, row)?;
    }
    Ok(market)
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

fn quotable(cfg: &Config, pin: &VaultPin) -> bool {
    pin.col_tokens()
        .iter()
        .any(|(t, _)| cfg.asset_of_token(*t).is_some())
        && pin
            .debt_tokens()
            .iter()
            .any(|(t, _)| cfg.asset_of_token(*t).is_some())
}

/// Two simulations per vault with at least one mapped token on each side.
pub(crate) fn state_reads(cfg: &Config) -> Vec<StateRead> {
    let mut out = Vec::new();
    for (i, pin) in cfg.vault_pins.iter().enumerate() {
        if !quotable(cfg, pin) {
            continue;
        }
        for absorb in [false, true] {
            let calldata = if pin.vault_type == VAULT_T1 {
                vault::liquidateCall {
                    debtAmt_: X128,
                    colPerUnitDebt_: U256::ZERO,
                    to_: DEAD,
                    absorb_: absorb,
                }
                .abi_encode()
            } else {
                vault::simulateLiquidateCall {
                    debtAmt_: U256::ZERO,
                    absorb_: absorb,
                }
                .abi_encode()
            };
            out.push(StateRead {
                market: pin.market(),
                target: pin.vault,
                calldata: Bytes::from(calldata),
                tag: tag(i, kind_of(K_SIM, 0, u64::from(absorb))),
            });
        }
    }
    out
}

/// `FluidLiquidateResult(col, debt)` from a simulation's revert data.
fn sim_result(a: &StateAnswer<'_>) -> Option<(U256, U256)> {
    if a.success {
        return None;
    }
    let e = vault::FluidLiquidateResult::abi_decode(a.data).ok()?;
    Some((e.colLiquidated, e.debtLiquidated))
}

fn one_token(a: &StateAnswer<'_>, withdraw: bool) -> Option<U256> {
    if a.success {
        return None;
    }
    if withdraw {
        dex::FluidDexLiquidityOutput::abi_decode(a.data)
            .ok()
            .map(|e| e.tokenAmt)
    } else {
        dex::FluidDexSingleTokenOutput::abi_decode(a.data)
            .ok()
            .map(|e| e.tokenAmt)
    }
}

/// On a smart side, what the simulated shares are in each of its tokens.
pub(crate) fn state_follow_ups(cfg: &Config, a: StateAnswer<'_>) -> Vec<StateRead> {
    let (pi, kind) = untag(a.read.tag);
    let Some(pin) = cfg.vault_pins.get(pi) else {
        return Vec::new();
    };
    if kind > K_SIM + 1 {
        return Vec::new();
    }
    let Some((col, debt)) = sim_result(&a) else {
        return Vec::new();
    };
    if col.is_zero() || debt.is_zero() {
        return Vec::new();
    }
    let absorb = kind.saturating_sub(K_SIM);
    let mut out = Vec::new();
    let smart_debt = pin.debt_tokens().len() == 2;
    let smart_col = pin.col_tokens().len() == 2;
    for t in 0..2u64 {
        if smart_debt {
            out.push(StateRead {
                market: pin.market(),
                target: pin.borrow,
                calldata: Bytes::from(
                    dex::paybackPerfectInOneTokenCall {
                        shares_: debt,
                        maxToken0_: if t == 0 { ESTIMATE_MAX } else { U256::ZERO },
                        maxToken1_: if t == 1 { ESTIMATE_MAX } else { U256::ZERO },
                        estimate_: true,
                    }
                    .abi_encode(),
                ),
                tag: tag(pi, kind_of(K_PAYBACK, absorb, t)),
            });
        }
        if smart_col {
            out.push(StateRead {
                market: pin.market(),
                target: pin.supply,
                calldata: Bytes::from(
                    dex::withdrawPerfectInOneTokenCall {
                        shares_: col,
                        minToken0_: if t == 0 { U256::from(1u8) } else { U256::ZERO },
                        minToken1_: if t == 1 { U256::from(1u8) } else { U256::ZERO },
                        to_: DEAD,
                    }
                    .abi_encode(),
                ),
                tag: tag(pi, kind_of(K_WITHDRAW, absorb, t)),
            });
        }
    }
    out
}

/// One vault's answers, gathered.
#[derive(Default, Clone, Copy)]
struct VaultRead {
    /// `[no absorb, absorb]` → `(col units, debt units)`.
    sim: [Option<(U256, U256)>; 2],
    /// `[absorb][token]` one-token payback.
    pay: [[Option<U256>; 2]; 2],
    /// `[absorb][token]` one-token withdraw.
    out: [[Option<U256>; 2]; 2],
}

/// Fluid's liquidation resolver: absorb when the plain liquidation is empty,
/// or when absorb adds size at no worse collateral-per-debt.
fn choose_absorb(sim: &[Option<(U256, U256)>; 2]) -> Option<bool> {
    let plain = sim[0].filter(|(c, d)| !c.is_zero() && !d.is_zero());
    let abs = sim[1].filter(|(c, d)| !c.is_zero() && !d.is_zero());
    match (plain, abs) {
        (None, None) => None,
        (Some(_), None) => Some(false),
        (None, Some(_)) => Some(true),
        (Some((c0, d0)), Some((c1, d1))) => {
            let bigger = d1 > d0;
            // c1/d1 >= c0/d0 without division.
            let not_worse = c1
                .checked_mul(d0)
                .zip(c0.checked_mul(d1))
                .is_some_and(|(l, r)| l >= r);
            Some(bigger && not_worse)
        }
    }
}

fn u128_of(v: U256) -> Result<u128> {
    u128::try_from(v).map_err(|_| ProtocolError::AmountTooLarge)
}

/// Fold one block's answers into each vault's position.
pub(crate) fn apply_state_reads(
    cfg: &Config,
    st: &mut dyn StateWriter,
    ts: Timestamp,
    answers: &[StateAnswer<'_>],
) -> Result<DirtySet> {
    let mut reads: Vec<(usize, VaultRead)> = Vec::new();
    for a in answers {
        let (pi, kind) = untag(a.read.tag);
        let slot = match reads.iter().position(|(i, _)| *i == pi) {
            Some(s) => s,
            None => {
                reads.push((pi, VaultRead::default()));
                reads.len().saturating_sub(1)
            }
        };
        let Some((_, r)) = reads.get_mut(slot) else {
            continue;
        };
        let (absorb, t) = if kind >= K_PAYBACK {
            let k = kind.saturating_sub(K_PAYBACK);
            (
                usize::try_from(k / 4).unwrap_or(2),
                usize::try_from(k % 4).unwrap_or(4),
            )
        } else {
            (usize::try_from(kind).unwrap_or(2), 0)
        };
        match kind {
            0 | 1 => {
                if let Some(s) = r.sim.get_mut(absorb) {
                    *s = sim_result(a);
                }
            }
            _ => {
                // t: 0/1 payback token, 2/3 withdraw token.
                let withdraw = t >= 2;
                let token = t % 2;
                let v = one_token(a, withdraw);
                let table = if withdraw { &mut r.out } else { &mut r.pay };
                if let Some(cell) = table.get_mut(absorb).and_then(|x| x.get_mut(token)) {
                    *cell = v;
                }
            }
        }
    }
    let mut dirty = DirtyPositions::new();
    for (pi, r) in reads {
        let Some(pin) = cfg.vault_pins.get(pi) else {
            continue;
        };
        if let Some(id) = fold_vault(cfg, st, pin, &r, ts)? {
            dirty.push(id);
        }
    }
    Ok(if dirty.is_empty() {
        DirtySet::None
    } else {
        DirtySet::Positions(dirty)
    })
}

/// Write one vault's liquidation into its position. `Some(id)` when anything
/// the engine reads changed (or it is liquidatable and was re-read).
fn fold_vault(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pin: &VaultPin,
    r: &VaultRead,
    ts: Timestamp,
) -> Result<Option<PositionId>> {
    let cols = pin.col_tokens();
    let debts = pin.debt_tokens();
    let n_col = u16::try_from(cols.len()).map_err(|_| ProtocolError::Internal)?;
    let mut col_amt = [0u128; 2];
    let mut debt_amt = [0u128; 2];
    let mut extra = VaultExtra {
        debt_units: 0,
        col_units: 0,
        read_ts: ts,
        flags: 0,
        _pad: [0; 7],
    };
    if let Some(absorb) = choose_absorb(&r.sim) {
        let v = usize::from(absorb);
        let (col, debt) = r.sim.get(v).copied().flatten().unwrap_or_default();
        extra.col_units = u128_of(col)?;
        extra.debt_units = u128_of(debt)?;
        if absorb {
            extra.flags |= VaultExtra::ABSORB;
        }
        if debts.len() == 2 {
            for (t, slot) in debt_amt.iter_mut().enumerate() {
                let p = r.pay.get(v).and_then(|x| x.get(t)).copied().flatten();
                *slot = p.map(u128_of).transpose()?.unwrap_or(0);
            }
        } else {
            debt_amt[0] = extra.debt_units;
        }
        if cols.len() == 2 {
            for (t, slot) in col_amt.iter_mut().enumerate() {
                let o = r.out.get(v).and_then(|x| x.get(t)).copied().flatten();
                *slot = o.map(u128_of).transpose()?.unwrap_or(0);
            }
        } else {
            col_amt[0] = extra.col_units;
        }
    }
    let live = debt_amt.iter().any(|d| *d != 0) && col_amt.iter().any(|c| *c != 0);
    let market = pin.market();
    let key = PositionKey {
        protocol: cfg.protocol,
        market,
        user: pin.vault,
    };
    let head = MarketSlot { market, slot: 0 };
    let existing = st
        .market(head)
        .ok()
        .and_then(|row| row.body::<VaultRow>().ok())
        .and_then(|b| b.position_plus1.checked_sub(1))
        .map(PositionId);
    if existing.is_none() && !live {
        return Ok(None);
    }
    ensure_vault(cfg, st, pin)?;
    let id = match existing {
        Some(id) => id,
        None => {
            let id = st.intern(&key)?;
            let mut row = *st.market(head)?;
            row.body_mut::<VaultRow>()?.position_plus1 =
                id.0.checked_add(1).ok_or(ProtocolError::Internal)?;
            st.set_market(head, row)?;
            id
        }
    };
    let mut changed = false;
    for (i, amt) in col_amt.iter().take(cols.len()).enumerate() {
        let slot = u16::try_from(i).map_err(|_| ProtocolError::Internal)?;
        if st.supply(id, slot)? != *amt {
            st.set_supply(id, slot, *amt)?;
            changed = true;
        }
    }
    for (i, amt) in debt_amt.iter().take(debts.len()).enumerate() {
        let slot = n_col.saturating_add(u16::try_from(i).map_err(|_| ProtocolError::Internal)?);
        if st.debt(id, slot)? != *amt {
            st.set_debt(id, slot, *amt)?;
            changed = true;
        }
    }
    let old: VaultExtra = *st.extra(id)?.view::<VaultExtra>()?;
    if !live {
        extra.read_ts = old.read_ts;
    }
    if old != extra {
        let mut repr = PositionExtraRepr::ZERO;
        *repr.view_mut::<VaultExtra>()? = extra;
        st.set_extra(id, repr)?;
        changed = changed || live || old.debt_units != 0;
    }
    Ok(changed.then_some(id))
}
