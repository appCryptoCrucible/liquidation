//! Viability band (GUIDE 12 §4b): per `(protocol, coll, debt)` per block,
//! the debt-size interval on which `net(s) ≥ 0`. Gas cost is
//! `(base fee + priority fee) × gas`; the two fees stay separate fields.
//! The bid is a share of net, never a cost here.
//!
//! `net(s) = min(spot, twa)(exact_quote(seized(s))) − s·(1 + flash) − gas`
//! with `seized(s) = s · (1 + bonus) · coll_per_debt`. `exact_quote` is
//! the water-fill of [`crate::exact`], so impact is real, not a model.
//! `net` is concave in `s` (concave quote minus affine), so `{net ≥ 0}`
//! is one interval and each edge is a bracketed bisection. The band is a
//! pre-filter — errors cost an opportunity, never money — so edges are
//! resolved to 1e-3 relative and kept on the viable side of each edge,
//! so the published interval sits inside the true `{net ≥ 0}` set.

use std::collections::HashMap;

use alloy_primitives::U256;
use liq_types::fixed::RAY;
use liq_types::{AssetId, ProtocolId, Ray};

use crate::exact::{solve_pair, GasTerms, SolveBudget};
use crate::solver::{mul_div_512, PoolBook, RouteError};

/// EIP-1559 `ELASTICITY_MULTIPLIER`.
const ELASTICITY: u64 = 2;
/// EIP-1559 `BASE_FEE_MAX_CHANGE_DENOMINATOR`.
const DENOM: u128 = 8;
/// Edge resolution: `1e-3` relative.
const EDGE_TOL: U256 = U256::from_limbs([1_000, 0, 0, 0]);
/// Basis points.
const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);

/// Exact next block base fee from the parent header (EIP-1559 §
/// "Specification", integer order preserved). `None` only when
/// `parent_gas_limit < ELASTICITY` (not a valid header).
#[must_use]
pub fn next_base_fee(
    parent_base_fee: u128,
    parent_gas_used: u64,
    parent_gas_limit: u64,
) -> Option<u128> {
    let target = parent_gas_limit.checked_div(ELASTICITY)?;
    if target == 0 {
        return None;
    }
    let target128 = u128::from(target);
    if parent_gas_used == target {
        return Some(parent_base_fee);
    }
    if parent_gas_used > target {
        let delta = u128::from(parent_gas_used.checked_sub(target)?);
        let raw = parent_base_fee
            .checked_mul(delta)?
            .checked_div(target128)?
            .checked_div(DENOM)?;
        parent_base_fee.checked_add(raw.max(1))
    } else {
        let delta = u128::from(target.checked_sub(parent_gas_used)?);
        let raw = parent_base_fee
            .checked_mul(delta)?
            .checked_div(target128)?
            .checked_div(DENOM)?;
        parent_base_fee.checked_sub(raw)
    }
}

/// The band. `min_size ≤ max_size`, both in **debt raw units**.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ViabilityBand {
    /// Below this, gas dominates.
    pub min_size: U256,
    /// Above this, impact dominates (or route depth ends).
    pub max_size: U256,
    /// The exact next base fee this band was computed for.
    pub base_fee: u128,
    pub block: u64,
}

/// Protocol-side terms for one `(protocol, coll, debt)`. Every field is an
/// input the caller owns (registry, adapter, oracle); nothing is defaulted.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PairTerms {
    /// Liquidation bonus as a RAY fraction (`0.05e27` = 5 %).
    pub bonus: Ray,
    /// Raw `coll` units received per raw `debt` unit repaid, RAY-scaled,
    /// **before** bonus (the protocol's own oracle ratio).
    pub coll_per_debt: Ray,
    pub flash_fee_bps: u16,
    /// Gas outside the swap hops: flash wrap, `liquidationCall`, repay.
    pub fixed_gas: u64,
}

/// Keyed per pair, not per token (GUIDE 12 §4b).
#[derive(Clone, Debug, Default)]
pub struct BandTable {
    pub bands: HashMap<(ProtocolId, AssetId, AssetId), ViabilityBand>,
    pub block: u64,
    pub base_fee: u128,
}

impl BandTable {
    #[inline]
    #[must_use]
    pub fn get(
        &self,
        protocol: ProtocolId,
        coll: AssetId,
        debt: AssetId,
    ) -> Option<&ViabilityBand> {
        self.bands.get(&(protocol, coll, debt))
    }
}

/// `s · (1 + bonus) · coll_per_debt`, floor.
fn seized_for(s: U256, t: &PairTerms) -> Result<U256, RouteError> {
    let one_plus = RAY.checked_add(t.bonus.raw()).ok_or(RouteError::Math)?;
    let eq = mul_div_512(s, one_plus, RAY)?;
    mul_div_512(eq, t.coll_per_debt.raw(), RAY)
}

/// Everything one pair's band is computed from. `haircut(size_in,
/// spot_out) → min(spot, twa) out` is the warm tier's
/// [`crate::warm::RouteTable::min_spot_twa`].
pub struct BandCtx<'a> {
    pub book: &'a PoolBook,
    pub coll: AssetId,
    pub debt: AssetId,
    pub terms: &'a PairTerms,
    /// Output token = `debt`.
    pub gas: &'a GasTerms,
    pub haircut: &'a dyn Fn(U256, U256) -> U256,
    pub budget: &'a SolveBudget,
}

impl BandCtx<'_> {
    /// `net(s) ≥ 0`: `Ok(Some(true))` viable, `Ok(Some(false))` not,
    /// `Ok(None)` when no route absorbs `seized(s)` (depth ends).
    fn viable(&self, s: U256) -> Result<Option<bool>, RouteError> {
        let t = self.terms;
        // Only fails when `seized` does not fit `U256`: unabsorbable.
        let Ok(seized) = seized_for(s, t) else {
            return Ok(None);
        };
        if seized.is_zero() {
            return Ok(Some(false));
        }
        // `Math` here is an overflow inside a pool's own swap arithmetic
        // (`amountIn · 997 · reserveOut` etc.): the size is beyond what
        // that pool can represent, i.e. beyond its depth. Same meaning as
        // `InsufficientLiquidity` for the scan; the chain would revert too.
        let q = match solve_pair(
            self.book,
            self.coll,
            self.debt,
            seized,
            self.gas,
            self.budget,
        ) {
            Ok(q) => q,
            Err(RouteError::InsufficientLiquidity | RouteError::Math) => return Ok(None),
            Err(e) => return Err(e),
        };
        let out = (self.haircut)(seized, q.amount_out);
        let flash = mul_div_512(s, U256::from(t.flash_fee_bps), BPS)?;
        let gas_cost = self
            .gas
            .cost_in_out(t.fixed_gas.checked_add(q.hop_gas).ok_or(RouteError::Math)?)?;
        let cost = s
            .checked_add(flash)
            .and_then(|v| v.checked_add(gas_cost))
            .ok_or(RouteError::Math)?;
        Ok(Some(out >= cost))
    }
}

/// Compute one pair's band at `ctx.gas.base_fee_wei`. `Ok(None)`: no
/// viable size.
pub fn compute_band(ctx: &BandCtx<'_>, block: u64) -> Result<Option<ViabilityBand>, RouteError> {
    let (terms, gas) = (ctx.terms, ctx.gas);
    if terms.bonus.raw() >= RAY {
        return Err(RouteError::BadLeg);
    }
    let v = |s: U256| ctx.viable(s);
    // Below the fixed gas cost in debt units net < 0 (bonus < 100 %), so
    // the geometric grid starts there.
    let mut s = gas.cost_in_out(terms.fixed_gas)?.max(U256::ONE);
    let mut prev_bad = U256::ZERO;
    let mut first_good: Option<U256> = None;
    let mut last_good = U256::ZERO;
    let mut upper_bad: Option<U256> = None;
    for _ in 0..256u32 {
        match v(s)? {
            Some(true) => {
                if first_good.is_none() {
                    first_good = Some(s);
                }
                last_good = s;
            }
            Some(false) if first_good.is_none() => prev_bad = s,
            None if first_good.is_none() => return Ok(None), // depth ends before gas is covered
            Some(false) | None => {
                upper_bad = Some(s);
                break;
            }
        }
        // Past `U256` there is nothing to probe: the scan ends.
        let Some(next) = s.checked_mul(U256::from(2u64)) else {
            break;
        };
        s = next;
    }
    let Some(first_good) = first_good else {
        return Ok(None);
    };
    // Lower edge: bisect (prev_bad, first_good], keep the viable end.
    let (mut lo, mut hi) = (prev_bad, first_good);
    while hi.checked_sub(lo).ok_or(RouteError::Math)?
        > hi.checked_div(EDGE_TOL).unwrap_or(U256::ONE).max(U256::ONE)
    {
        let mid = lo
            .checked_add(hi.checked_sub(lo).ok_or(RouteError::Math)?.wrapping_shr(1))
            .ok_or(RouteError::Math)?;
        if v(mid)? == Some(true) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let min_size = hi;
    // Upper edge: bisect [last_good, upper_bad), keep the viable end.
    let max_size = match upper_bad {
        None => last_good,
        Some(bad) => {
            let (mut lo, mut hi) = (last_good, bad);
            while hi.checked_sub(lo).ok_or(RouteError::Math)?
                > hi.checked_div(EDGE_TOL).unwrap_or(U256::ONE).max(U256::ONE)
            {
                let mid = lo
                    .checked_add(hi.checked_sub(lo).ok_or(RouteError::Math)?.wrapping_shr(1))
                    .ok_or(RouteError::Math)?;
                if v(mid)? == Some(true) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            lo
        }
    };
    Ok(Some(ViabilityBand {
        min_size,
        max_size,
        base_fee: gas.base_fee_wei,
        block,
    }))
}

/// Per-pair inputs the band needs from the registry / oracle side.
/// `None` means "not available this block" → the key is absent from the
/// table (logged), never defaulted.
pub trait BandInputs {
    fn pair_terms(&self, protocol: ProtocolId, coll: AssetId, debt: AssetId) -> Option<PairTerms>;
    /// Raw units of `asset` per `1e18` wei.
    fn per_eth(&self, asset: AssetId) -> Option<U256>;
}

/// Build the table for `keys` at `base_fee` / `block`. Off the hot path
/// (warm tier, inter-block window).
#[allow(clippy::too_many_arguments)] // each input is a distinct required term
pub fn build_table(
    book: &PoolBook,
    keys: &[(ProtocolId, AssetId, AssetId)],
    inputs: &dyn BandInputs,
    haircut: &dyn Fn(AssetId, AssetId, U256, U256) -> U256,
    budget: &SolveBudget,
    base_fee: u128,
    priority_fee: u128,
    block: u64,
) -> BandTable {
    let mut out = BandTable {
        bands: HashMap::with_capacity(keys.len()),
        block,
        base_fee,
    };
    for &(protocol, coll, debt) in keys {
        let (Some(terms), Some(per_eth)) = (
            inputs.pair_terms(protocol, coll, debt),
            inputs.per_eth(debt),
        ) else {
            tracing::warn!(
                ?protocol,
                ?coll,
                ?debt,
                "band: terms or price missing; key skipped"
            );
            continue;
        };
        let gas = GasTerms {
            base_fee_wei: base_fee,
            priority_fee_wei: priority_fee,
            out_per_eth: per_eth,
        };
        let hc = |size_in: U256, spot: U256| haircut(coll, debt, size_in, spot);
        let ctx = BandCtx {
            book,
            coll,
            debt,
            terms: &terms,
            gas: &gas,
            haircut: &hc,
            budget,
        };
        match compute_band(&ctx, block) {
            Ok(Some(b)) => {
                out.bands.insert((protocol, coll, debt), b);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(?protocol, ?coll, ?debt, error = %e, "band: solve refused"),
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::U256;
    use liq_types::fixed::RAY;
    use liq_types::{ProtocolId, Ray};
    use smallvec::SmallVec;

    use super::*;
    use crate::fixtures::*;
    use crate::solver::Pool;
    use crate::warm::{WarmBuilder, WarmConfig, WarmInputs};

    fn cb(
        book: &PoolBook,
        terms: &PairTerms,
        gas: &GasTerms,
        haircut: &dyn Fn(U256, U256) -> U256,
        block: u64,
    ) -> Result<Option<ViabilityBand>, RouteError> {
        let budget = SolveBudget::default();
        compute_band(
            &BandCtx {
                book,
                coll: A0,
                debt: A1,
                terms,
                gas,
                haircut,
                budget: &budget,
            },
            block,
        )
    }

    /// Oracle: EIP-1559 reference arithmetic. Target block → unchanged;
    /// full block → +12.5 %; empty block → −12.5 %; one gas over target
    /// → `+max(1, ·)`; the integer-division order of the spec.
    #[test]
    fn next_base_fee_eip1559_vectors() {
        let gwei = 1_000_000_000u128;
        assert_eq!(
            next_base_fee(1000 * gwei, 15_000_000, 30_000_000),
            Some(1000 * gwei)
        );
        assert_eq!(
            next_base_fee(1000 * gwei, 30_000_000, 30_000_000),
            Some(1125 * gwei)
        );
        assert_eq!(next_base_fee(1000 * gwei, 0, 30_000_000), Some(875 * gwei));
        // floor(7·1/15e6/8) = 0 → max(1, 0)
        assert_eq!(next_base_fee(7, 15_000_001, 30_000_000), Some(8));
        // 100 gwei, used 20M of 30M: delta 5M → 100e9·5e6/15e6/8 = 4_166_666_666
        assert_eq!(
            next_base_fee(100 * gwei, 20_000_000, 30_000_000),
            Some(100 * gwei + 4_166_666_666)
        );
        assert_eq!(
            next_base_fee(100 * gwei, 10_000_000, 30_000_000),
            Some(100 * gwei - 4_166_666_666)
        );
        assert_eq!(next_base_fee(1, 0, 1), None);
    }

    struct In {
        sizes: SmallVec<[U256; 4]>,
        block: u64,
        base_fee: u128,
    }
    impl WarmInputs for In {
        fn bucket_sizes(&self, _: AssetId) -> Option<SmallVec<[U256; 4]>> {
            Some(self.sizes.clone())
        }
        fn per_eth(&self, _: AssetId) -> Option<U256> {
            Some(e18(1))
        }
        fn next_base_fee(&self) -> u128 {
            self.base_fee
        }
        fn priority_fee_wei(&self) -> u128 {
            0
        }
        fn block(&self) -> u64 {
            self.block
        }
    }

    fn terms(bonus_bps: u64, fixed_gas: u64) -> PairTerms {
        PairTerms {
            bonus: Ray::from_raw(RAY * U256::from(bonus_bps) / U256::from(10_000u64)),
            coll_per_debt: Ray::from_raw(RAY), // 1:1 oracle
            flash_fee_bps: 5,
            fixed_gas,
        }
    }

    fn book(pools: Vec<Pool>) -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        let mut b = PoolBook::new(assets, None, HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    /// Oracle: closed form on a V2 exit. With bonus 5 %, flash 5 bps,
    /// 1:1 oracle and a single V2 pool, `net(s) ≥ 0 ⇔ quote(1.05 s) ≥
    /// 1.0005 s + gas`. Both edges are checked against direct evaluation
    /// of that inequality one step either side (1e-3 relative).
    #[test]
    fn band_edges_bracket_the_sign_change() {
        let pool = v2(1, e18(10_000), e18(10_000));
        let bk = book(vec![pool.clone()]);
        let t = terms(500, 400_000);
        let gas = GasTerms {
            base_fee_wei: 30_000_000_000,
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        let spot = |_: U256, o: U256| o;
        let band = cb(&bk, &t, &gas, &spot, 7).unwrap().unwrap();
        assert_eq!((band.block, band.base_fee), (7, gas.base_fee_wei));
        assert!(band.min_size < band.max_size);
        let net_ok = |s: U256| {
            let seized = s * U256::from(105u64) / U256::from(100u64);
            let out = pool.quote_exact_in(0, 1, seized).unwrap();
            let cost = s
                + s * U256::from(5u64) / U256::from(10_000u64)
                + gas.cost_in_out(400_000 + HOP_GAS).unwrap();
            out >= cost
        };
        let step = |x: U256| x / U256::from(500u64);
        assert!(net_ok(band.min_size) && !net_ok(band.min_size - step(band.min_size)));
        assert!(net_ok(band.max_size) && !net_ok(band.max_size + step(band.max_size)));
        // Lower edge sits where gas ≈ margin: 0.015 ETH / (1.05·0.997 − 1.0005) ≈ 0.3236
        assert!(
            band.min_size > e18(31) / U256::from(100u64)
                && band.min_size < e18(34) / U256::from(100u64),
            "{}",
            band.min_size
        );
        // Higher base fee → higher lower edge; upper edge unchanged to 1e-3.
        let gas2 = GasTerms {
            base_fee_wei: 60_000_000_000,
            priority_fee_wei: 0,
            ..gas
        };
        let band2 = cb(&bk, &t, &gas2, &spot, 8).unwrap().unwrap();
        assert!(band2.min_size > band.min_size);
        assert!(band2.max_size.abs_diff(band.max_size) <= band.max_size / U256::from(400u64));
    }

    /// No viable size: a pool too shallow to ever cover gas → `None`;
    /// bonus ≥ 100 % is rejected as an input error.
    #[test]
    fn band_none_when_never_viable() {
        let bk = book(vec![v2(
            1,
            e18(1) / U256::from(100u64),
            e18(1) / U256::from(100u64),
        )]);
        let gas = GasTerms {
            base_fee_wei: 30_000_000_000,
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        let spot = |_: U256, o: U256| o;
        assert_eq!(cb(&bk, &terms(500, 400_000), &gas, &spot, 1), Ok(None));
        assert_eq!(
            cb(&bk, &terms(10_000, 1), &gas, &spot, 1),
            Err(RouteError::BadLeg)
        );
    }

    /// Acceptance: a simulated liquidity collapse shrinks `max_size` on
    /// the **next block** — `min(spot, twa)` respects spot immediately,
    /// while the TWA alone would still report the pre-collapse depth.
    #[test]
    fn collapse_shrinks_max_size_next_block() {
        let mut bk = book(vec![v2(1, e18(10_000), e18(10_000))]);
        let mut warm = WarmBuilder::new(WarmConfig {
            max_impact_bps: 500,
            twa_blocks: 8,
            budget: SolveBudget::default(),
        });
        let sizes: SmallVec<[U256; 4]> =
            SmallVec::from_slice(&[e18(10), e18(100), e18(1_000), e18(5_000)]);
        let gas = GasTerms {
            base_fee_wei: 30_000_000_000,
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        let t = terms(500, 400_000);
        let band_at = |bk: &PoolBook, block: u64, warm: &mut WarmBuilder| {
            let table = warm.rebuild(
                bk,
                &In {
                    sizes: sizes.clone(),
                    block,
                    base_fee: gas.base_fee_wei,
                },
            );
            let hc = |size_in: U256, spot: U256| table.min_spot_twa(A0, A1, size_in, spot);
            cb(bk, &t, &gas, &hc, block).unwrap().unwrap()
        };
        // Seven calm blocks build the TWA history.
        let mut before = None;
        for b in 1..=7 {
            before = Some(band_at(&bk, b, &mut warm));
        }
        let before = before.unwrap();
        // Block 8: 90 % of the pool leaves.
        bk.apply_log(&v2_sync_log(addr(1), e18(1_000), e18(1_000)).decoded());
        let after = band_at(&bk, 8, &mut warm);
        assert!(
            after.max_size < before.max_size / U256::from(5u64),
            "{} vs {}",
            after.max_size,
            before.max_size
        );
        // The TWA alone still remembers depth: mean of 7 big + 1 small
        // outputs is far above spot, so plain-TWA would not have shrunk.
        let table = warm.slot().load_full();
        let e = table.entry(A0, A1).unwrap();
        let b3 = &e.buckets[3];
        let (spot, min) = (b3.out_spot, b3.out_min);
        assert!(spot.is_none() || min.unwrap() <= spot.unwrap());
        let b2 = &e.buckets[2];
        assert!(
            b2.out_min.unwrap() == b2.out_spot.unwrap(),
            "collapse respected at once: min == spot"
        );
        // Inflation is capped the other way: restore depth ×10 on block 9
        // and the published min stays at the average, below spot.
        bk.apply_log(&v2_sync_log(addr(1), e18(100_000), e18(100_000)).decoded());
        let _ = band_at(&bk, 9, &mut warm);
        let table = warm.slot().load_full();
        let b2 = &table.entry(A0, A1).unwrap().buckets[2];
        assert!(
            b2.out_min.unwrap() < b2.out_spot.unwrap(),
            "inflated spot capped by twa"
        );
    }

    /// `build_table` skips keys with missing inputs and logs; present keys
    /// are computed at the given base fee and block.
    #[test]
    fn build_table_skips_missing_inputs() {
        struct BI;
        impl BandInputs for BI {
            fn pair_terms(&self, p: ProtocolId, _: AssetId, _: AssetId) -> Option<PairTerms> {
                (p.0 == 1).then(|| terms(500, 400_000))
            }
            fn per_eth(&self, _: AssetId) -> Option<U256> {
                Some(e18(1))
            }
        }
        let bk = book(vec![v2(1, e18(10_000), e18(10_000))]);
        let keys = [
            (ProtocolId(1), A0, A1),
            (ProtocolId(2), A0, A1),
            (ProtocolId(1), A1, A0),
        ];
        let hc = |_: AssetId, _: AssetId, _: U256, o: U256| o;
        let t = build_table(
            &bk,
            &keys,
            &BI,
            &hc,
            &SolveBudget::default(),
            30_000_000_000,
            0,
            42,
        );
        assert_eq!(t.bands.len(), 2);
        assert!(t.get(ProtocolId(1), A0, A1).is_some());
        assert!(t.get(ProtocolId(2), A0, A1).is_none());
        assert_eq!(t.get(ProtocolId(1), A1, A0).unwrap().block, 42);
    }
}
