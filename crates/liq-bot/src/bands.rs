//! Viability-band plumbing (GUIDE 12 §4b): the band is the sole source of
//! truth for which sizes are worth taking.
//!
//! * The hot thread owns the inputs (live prices → `per_eth`, candidate
//!   quotes → `pair_terms`, header → fees) and writes them in place into
//!   [`BandShared::inputs`] — brief lock, no allocation for existing keys.
//! * The warm thread ([`rebuild`]) copies the inputs once per new block and
//!   builds the whole [`BandTable`] off the hot path, then publishes it
//!   through an [`ArcSwap`]. The hot path only ever does a lock-free load.
//! * A pair the table has never evaluated (first candidate on that pair) is
//!   computed once on the hot path by the drain, so the first opportunity
//!   on a new pair is not lost to a one-block lag.
//!
//! A pair with no measured fixed gas is never banded: a band that ignores
//! the liquidation's own gas would pass sizes that lose money.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::U256;
use arc_swap::ArcSwap;
use liq_router::{
    build_table, compute_band, BandCtx, BandInputs, BandTable, GasTerms, PairTerms, PoolBook,
    RouteTable, SolveBudget, ViabilityBand,
};
use liq_types::{AssetId, ProtocolId};
use parking_lot::Mutex;

pub type BandKey = (ProtocolId, AssetId, AssetId);

/// Everything one block's bands are computed from. Written by the hot
/// thread, copied by the warm thread.
#[derive(Clone, Debug, Default)]
pub struct BandInputsSnapshot {
    pub terms: HashMap<BandKey, PairTerms>,
    pub per_eth: HashMap<AssetId, U256>,
    pub base_fee: u128,
    pub priority_fee: u128,
    pub block: u64,
}

impl BandInputs for BandInputsSnapshot {
    fn pair_terms(&self, protocol: ProtocolId, coll: AssetId, debt: AssetId) -> Option<PairTerms> {
        self.terms.get(&(protocol, coll, debt)).copied()
    }
    fn per_eth(&self, asset: AssetId) -> Option<U256> {
        self.per_eth.get(&asset).copied()
    }
}

/// Shared between the hot thread (writer of inputs, reader of the table)
/// and the warm thread (reader of inputs, writer of the table).
#[derive(Debug)]
pub struct BandShared {
    pub inputs: Mutex<BandInputsSnapshot>,
    pub table: ArcSwap<BandTable>,
}

impl BandShared {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inputs: Mutex::new(BandInputsSnapshot::default()),
            table: ArcSwap::from_pointee(BandTable::default()),
        })
    }
}

/// Keys the band may be computed for: measured fixed gas only.
fn bandable(terms: &PairTerms) -> bool {
    terms.fixed_gas != 0
}

/// Warm thread: rebuild and publish the whole table when the inputs have
/// moved to a newer block. Returns whether a table was published.
pub fn rebuild(
    shared: &BandShared,
    book: &PoolBook,
    routes: &RouteTable,
    budget: &SolveBudget,
    last_block: &mut u64,
) -> bool {
    let snap = {
        let g = shared.inputs.lock();
        if g.block == 0 || g.block <= *last_block {
            return false;
        }
        g.clone()
    };
    let mut unmeasured = 0usize;
    let keys: Vec<BandKey> = snap
        .terms
        .iter()
        .filter_map(|(k, t)| {
            if bandable(t) {
                Some(*k)
            } else {
                unmeasured = unmeasured.saturating_add(1);
                None
            }
        })
        .collect();
    if unmeasured != 0 {
        tracing::warn!(
            unmeasured,
            "pairs without measured fixed gas — not banded, not sized"
        );
    }
    let hc = |c: AssetId, d: AssetId, size_in: U256, spot: U256| {
        routes.min_spot_twa(c, d, size_in, spot)
    };
    let table = build_table(
        book,
        &keys,
        &snap,
        &hc,
        budget,
        snap.base_fee,
        snap.priority_fee,
        snap.block,
    );
    shared.table.store(Arc::new(table));
    *last_block = snap.block;
    true
}

/// Warm-tier inputs derived from the band (GUIDE 12 §3): each collateral's
/// size ladder spans the collateral seized at `min_size` … `max_size` across
/// every banded pair on that collateral — the sizes worth taking, as exit
/// liquidity and costs set them — in four near-geometric steps.
pub struct BandWarmInputs<'a> {
    snap: &'a BandInputsSnapshot,
    ladders: HashMap<AssetId, smallvec::SmallVec<[U256; liq_router::warm::BUCKETS]>>,
}

/// `a^(3/4) · b^(1/4)` and `a^(1/4) · b^(3/4)` via integer square roots;
/// linear thirds when the product does not fit.
fn inner_points(lo: U256, hi: U256) -> (U256, U256) {
    if let Some(p) = lo.checked_mul(hi) {
        let mid = p.root(2);
        if let (Some(a), Some(b)) = (lo.checked_mul(mid), mid.checked_mul(hi)) {
            return (a.root(2), b.root(2));
        }
    }
    let third = hi
        .saturating_sub(lo)
        .checked_div(U256::from(3u64))
        .unwrap_or(U256::ZERO);
    (lo.saturating_add(third), hi.saturating_sub(third))
}

impl<'a> BandWarmInputs<'a> {
    #[must_use]
    pub fn new(snap: &'a BandInputsSnapshot, table: &BandTable) -> Self {
        let mut span: HashMap<AssetId, (U256, U256)> = HashMap::new();
        for (&(p, c, d), band) in &table.bands {
            let Some(terms) = snap.terms.get(&(p, c, d)) else {
                continue;
            };
            let (Ok(lo), Ok(hi)) = (
                liq_router::seized_for(band.min_size, terms),
                liq_router::seized_for(band.max_size, terms),
            ) else {
                continue;
            };
            if lo.is_zero() || hi < lo {
                continue;
            }
            let e = span.entry(c).or_insert((lo, hi));
            e.0 = e.0.min(lo);
            e.1 = e.1.max(hi);
        }
        let ladders = span
            .into_iter()
            .map(|(c, (lo, hi))| {
                let (a, b) = inner_points(lo, hi);
                (c, smallvec::SmallVec::from_slice(&[lo, a, b, hi]))
            })
            .collect();
        Self { snap, ladders }
    }
}

impl liq_router::WarmInputs for BandWarmInputs<'_> {
    fn bucket_sizes(
        &self,
        coll: AssetId,
    ) -> Option<smallvec::SmallVec<[U256; liq_router::warm::BUCKETS]>> {
        self.ladders.get(&coll).cloned()
    }
    fn per_eth(&self, asset: AssetId) -> Option<U256> {
        self.snap.per_eth.get(&asset).copied()
    }
    fn next_base_fee(&self) -> u128 {
        self.snap.base_fee
    }
    fn priority_fee_wei(&self) -> u128 {
        self.snap.priority_fee
    }
    fn block(&self) -> u64 {
        self.snap.block
    }
}

/// Hot path, first sighting of a pair: compute its band now from the same
/// inputs the warm thread will use. `None` when the pair has no viable
/// size, lacks inputs, or has no measured fixed gas.
#[allow(clippy::too_many_arguments)] // each input is a distinct band term
#[must_use]
pub fn compute_one(
    key: BandKey,
    terms: &PairTerms,
    per_eth_debt: U256,
    base_fee: u128,
    priority_fee: u128,
    block: u64,
    book: &PoolBook,
    routes: &RouteTable,
    budget: &SolveBudget,
) -> Option<ViabilityBand> {
    if !bandable(terms) || per_eth_debt.is_zero() {
        return None;
    }
    let (_, coll, debt) = key;
    let gas = GasTerms {
        base_fee_wei: base_fee,
        priority_fee_wei: priority_fee,
        out_per_eth: per_eth_debt,
    };
    let hc = |size_in: U256, spot: U256| routes.min_spot_twa(coll, debt, size_in, spot);
    let ctx = BandCtx {
        book,
        coll,
        debt,
        terms,
        gas: &gas,
        haircut: &hc,
        budget,
    };
    match compute_band(&ctx, block) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(?key, error = %e, "band: solve refused");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use liq_router::{Pool, PoolState, V2State};
    use liq_types::fixed::RAY;
    use liq_types::Ray;
    use smallvec::SmallVec;

    const P: ProtocolId = ProtocolId(1);
    const C: AssetId = AssetId(0);
    const D: AssetId = AssetId(1);

    fn e18(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
    }

    fn book() -> PoolBook {
        let (t0, t1) = (Address::repeat_byte(0x10), Address::repeat_byte(0x11));
        let mut assets = HashMap::new();
        assets.insert(t0, C);
        assets.insert(t1, D);
        let mut b = PoolBook::new(assets, None, 0);
        b.add(Pool {
            address: Address::repeat_byte(0x01),
            assets: SmallVec::from_slice(&[C, D]),
            tokens: SmallVec::from_slice(&[t0, t1]),
            hop_gas: 100_000,
            state: PoolState::V2(V2State {
                reserve0: e18(10_000),
                reserve1: e18(10_000),
                factory: 0,
            }),
        })
        .unwrap();
        b
    }

    fn terms(fixed_gas: u64) -> PairTerms {
        PairTerms {
            bonus: Ray::from_raw(RAY / U256::from(20u64)),
            coll_per_debt: Ray::from_raw(RAY),
            flash_fee_bps: 5,
            fixed_gas,
        }
    }

    fn inputs(shared: &BandShared, fixed_gas: u64, block: u64) {
        let mut i = shared.inputs.lock();
        i.terms.insert((P, C, D), terms(fixed_gas));
        i.per_eth.insert(D, e18(1));
        i.base_fee = 30_000_000_000;
        i.block = block;
    }

    #[test]
    fn rebuild_publishes_once_per_block_and_only_measured_pairs() {
        let shared = BandShared::new();
        let (bk, routes, budget) = (book(), RouteTable::default(), SolveBudget::default());
        let mut last = 0;
        assert!(
            !rebuild(&shared, &bk, &routes, &budget, &mut last),
            "no block yet"
        );

        inputs(&shared, 400_000, 7);
        assert!(rebuild(&shared, &bk, &routes, &budget, &mut last));
        let t = shared.table.load();
        let b = t.get(P, C, D).copied().unwrap();
        assert_eq!((t.block, b.block), (7, 7));
        assert!(b.min_size < b.max_size);
        assert!(
            !rebuild(&shared, &bk, &routes, &budget, &mut last),
            "same block: no rebuild"
        );

        inputs(&shared, 0, 8);
        assert!(rebuild(&shared, &bk, &routes, &budget, &mut last));
        assert!(
            shared.table.load().get(P, C, D).is_none(),
            "unmeasured fixed gas is never banded"
        );
    }

    #[test]
    fn warm_ladder_spans_the_band_in_seized_coll_units() {
        use liq_router::WarmInputs;
        let shared = BandShared::new();
        let (bk, routes, budget) = (book(), RouteTable::default(), SolveBudget::default());
        inputs(&shared, 400_000, 7);
        let mut last = 0;
        rebuild(&shared, &bk, &routes, &budget, &mut last);
        let snap = shared.inputs.lock().clone();
        let table = shared.table.load_full();
        let band = table.get(P, C, D).copied().unwrap();
        let w = BandWarmInputs::new(&snap, &table);
        let ladder = w.bucket_sizes(C).unwrap();
        assert_eq!(ladder.len(), 4);
        assert!(ladder.windows(2).all(|x| x[0] < x[1]), "{ladder:?}");
        let t = terms(400_000);
        assert_eq!(
            ladder[0],
            liq_router::seized_for(band.min_size, &t).unwrap()
        );
        assert_eq!(
            ladder[3],
            liq_router::seized_for(band.max_size, &t).unwrap()
        );
        assert!(w.bucket_sizes(D).is_none(), "debt-only asset has no ladder");
        assert_eq!(w.block(), 7);
    }

    #[test]
    fn compute_one_matches_the_table() {
        let shared = BandShared::new();
        let (bk, routes, budget) = (book(), RouteTable::default(), SolveBudget::default());
        inputs(&shared, 400_000, 9);
        let mut last = 0;
        rebuild(&shared, &bk, &routes, &budget, &mut last);
        let from_table = shared.table.load().get(P, C, D).copied();
        let one = compute_one(
            (P, C, D),
            &terms(400_000),
            e18(1),
            30_000_000_000,
            0,
            9,
            &bk,
            &routes,
            &budget,
        );
        assert_eq!(one, from_table, "hot-path first sighting == warm table");
        assert!(compute_one(
            (P, C, D),
            &terms(0),
            e18(1),
            30_000_000_000,
            0,
            9,
            &bk,
            &routes,
            &budget
        )
        .is_none());
    }
}
