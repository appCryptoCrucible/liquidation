//! Warm tier (GUIDE 12 §3): pool **sets** per `(coll, debt)` pair at a
//! bucket ladder, re-solved after each block, published through
//! `ArcSwap`. The hot path does one `load` and one indexed read.
//!
//! **No thread is spawned here.** GUIDE 12 acceptance forbids
//! `thread::spawn` / `rayon` in `liq-router`; the wiring layer (17A) owns
//! the builder thread and calls [`WarmBuilder::rebuild`] once per block
//! after the block's logs are folded into the [`PoolBook`].
//!
//! **`min(spot, twa)`.** Each bucket keeps a ring of the last
//! `twa_blocks` spot outputs; the published `out_min` is
//! `min(spot, mean(ring))`. A collapse is respected at once (spot is
//! lower); an inflated spot is capped by the average (GUIDE 12 §4b).

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::U256;
use arc_swap::ArcSwap;
use liq_types::AssetId;
use smallvec::SmallVec;

use crate::exact::{solve_pair, GasTerms, SolveBudget};
use crate::solver::{mul_div_512, Leg, PoolBook, RouteError, Q96};

/// Bucket ladder length (GUIDE 12 §3: `$10k / $100k / $1M / $5M`).
pub const BUCKETS: usize = 4;
/// Longest TWA window supported.
pub const MAX_TWA_BLOCKS: usize = 32;
const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);

/// Per-block inputs from the oracle side. `None` → the pair is skipped
/// this block and logged; nothing is invented.
pub trait WarmInputs {
    /// Bucket ladder in **`coll` raw units** (notional buckets through the
    /// oracle's `min(spot, twa)` price).
    fn bucket_sizes(&self, coll: AssetId) -> Option<SmallVec<[U256; BUCKETS]>>;
    /// Raw units of `asset` per `1e18` wei.
    fn per_eth(&self, asset: AssetId) -> Option<U256>;
    /// Exact next base fee (wei / gas). Not mixed with priority.
    fn next_base_fee(&self) -> u128;
    /// Priority fee (wei / gas). Kept separate from [`Self::next_base_fee`].
    fn priority_fee_wei(&self) -> u128;
    /// Block the folded state corresponds to.
    fn block(&self) -> u64;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WarmConfig {
    /// Impact bound for `has_exit` in bps (over the zero-size marginal,
    /// which already includes fee).
    pub max_impact_bps: u16,
    /// TWA window, blocks (`1` = spot only).
    pub twa_blocks: u8,
    pub budget: SolveBudget,
}

/// One bucket of one pair.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub size_in: U256,
    /// Exact water-fill output at the block's state; `None` when no pool
    /// set absorbs `size_in`.
    pub out_spot: Option<U256>,
    /// `min(out_spot, mean over the TWA ring)`; `None` when unroutable.
    pub out_min: Option<U256>,
    /// Pools with a non-zero allocation.
    pub pools: u8,
    pub hop_gas: u64,
    /// Within `max_impact_bps` of the zero-size marginal.
    pub viable: bool,
}

/// Pool set and bucket outputs for one pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    /// Every live directed leg for the pair (the exact tier's candidate
    /// set), best `ρ₀` first.
    pub legs: SmallVec<[Leg; 8]>,
    /// Best zero-size marginal, Q96 sqrt.
    pub rho0: U256,
    pub buckets: SmallVec<[Bucket; BUCKETS]>,
}

/// Published table. Immutable once built.
#[derive(Clone, Debug, Default)]
pub struct RouteTable {
    pairs: HashMap<(AssetId, AssetId), RouteEntry>,
    /// Indexed by `AssetId.0`: largest viable bucket size for the asset
    /// over every debt pair. Zero = no exit.
    exit_cap: Vec<U256>,
    pub block: u64,
    pub base_fee: u128,
    /// `PoolBook::generation` the table was built from.
    pub generation: u64,
}

impl RouteTable {
    #[inline]
    #[must_use]
    pub fn entry(&self, coll: AssetId, debt: AssetId) -> Option<&RouteEntry> {
        self.pairs.get(&(coll, debt))
    }

    /// Hot path: one indexed read, no hashing, no allocation.
    #[inline]
    #[must_use]
    pub fn exit_cap(&self, coll: AssetId) -> U256 {
        self.exit_cap
            .get(usize::from(coll.0))
            .copied()
            .unwrap_or(U256::ZERO)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// `min(spot, twa)` haircut for an arbitrary size: scale `spot_out`
    /// by the TWA ratio of the smallest bucket ≥ `size_in` (or the largest
    /// bucket when above the ladder). No history / no bucket → spot.
    #[must_use]
    pub fn min_spot_twa(
        &self,
        coll: AssetId,
        debt: AssetId,
        size_in: U256,
        spot_out: U256,
    ) -> U256 {
        let Some(e) = self.entry(coll, debt) else {
            return spot_out;
        };
        let b = e
            .buckets
            .iter()
            .find(|b| b.size_in >= size_in)
            .or_else(|| e.buckets.last());
        match b {
            Some(Bucket {
                out_spot: Some(s),
                out_min: Some(m),
                ..
            }) if m < s && !s.is_zero() => mul_div_512(spot_out, *m, *s).unwrap_or(spot_out),
            _ => spot_out,
        }
    }
}

/// Fixed ring of the last `n` spot outputs for one bucket.
#[derive(Clone, Debug, Default)]
struct Ring {
    vals: SmallVec<[U256; MAX_TWA_BLOCKS]>,
    next: usize,
}

impl Ring {
    fn push(&mut self, v: U256, n: usize) {
        if self.vals.len() < n {
            self.vals.push(v);
        } else if let Some(slot) = self.vals.get_mut(self.next) {
            *slot = v;
        }
        self.next = self
            .next
            .checked_add(1)
            .map_or(0, |k| k.checked_rem(n.max(1)).unwrap_or(0));
    }

    fn mean(&self) -> Option<U256> {
        if self.vals.is_empty() {
            return None;
        }
        let sum = self
            .vals
            .iter()
            .try_fold(U256::ZERO, |a, v| a.checked_add(*v))?;
        sum.checked_div(U256::from(self.vals.len()))
    }
}

/// Owns TWA history and the publish slot. One instance, one caller.
pub struct WarmBuilder {
    cfg: WarmConfig,
    history: HashMap<(AssetId, AssetId), SmallVec<[Ring; BUCKETS]>>,
    slot: Arc<ArcSwap<RouteTable>>,
}

impl WarmBuilder {
    #[must_use]
    pub fn new(cfg: WarmConfig) -> Self {
        Self {
            cfg,
            history: HashMap::new(),
            slot: Arc::new(ArcSwap::from_pointee(RouteTable::default())),
        }
    }

    /// The slot readers subscribe to ([`crate::cache::WarmRouteCache`]).
    #[must_use]
    pub fn slot(&self) -> Arc<ArcSwap<RouteTable>> {
        Arc::clone(&self.slot)
    }

    /// Re-solve every pair at every bucket against `book` and publish.
    /// Returns the table just published.
    pub fn rebuild(&mut self, book: &PoolBook, inputs: &dyn WarmInputs) -> Arc<RouteTable> {
        let n_twa = usize::from(self.cfg.twa_blocks).clamp(1, MAX_TWA_BLOCKS);
        let base_fee = inputs.next_base_fee();
        let priority_fee = inputs.priority_fee_wei();
        let mut table = RouteTable {
            pairs: HashMap::new(),
            exit_cap: Vec::new(),
            block: inputs.block(),
            base_fee,
            generation: book.generation(),
        };
        for (coll, debt) in book.pairs() {
            let (Some(sizes), Some(per_eth)) = (inputs.bucket_sizes(coll), inputs.per_eth(debt))
            else {
                tracing::warn!(
                    ?coll,
                    ?debt,
                    "warm: bucket ladder or price missing; pair skipped"
                );
                continue;
            };
            let gas = GasTerms {
                base_fee_wei: base_fee,
                priority_fee_wei: priority_fee,
                out_per_eth: per_eth,
            };
            let mut legs: SmallVec<[(U256, Leg); 8]> = book
                .legs(coll, debt)
                .iter()
                .filter_map(|l| {
                    let p = book.get(l.pool)?;
                    p.is_live()
                        .then(|| p.rho_at_zero(l.i, l.j).ok().map(|r| (r, *l)))?
                })
                .collect();
            if legs.is_empty() {
                continue;
            }
            legs.sort_by_key(|l| std::cmp::Reverse(l.0));
            let rho0 = legs.first().map_or(U256::ZERO, |l| l.0);
            let rings = self
                .history
                .entry((coll, debt))
                .or_insert_with(|| SmallVec::from_elem(Ring::default(), BUCKETS));
            let mut buckets: SmallVec<[Bucket; BUCKETS]> = SmallVec::new();
            let mut cap = U256::ZERO;
            for (k, &size_in) in sizes.iter().enumerate().take(BUCKETS) {
                let solved = match solve_pair(book, coll, debt, size_in, &gas, &self.cfg.budget) {
                    Ok(q) => Some(q),
                    Err(RouteError::InsufficientLiquidity | RouteError::StalePool) => None,
                    Err(e) => {
                        tracing::warn!(?coll, ?debt, %size_in, error = %e, "warm: solve refused");
                        None
                    }
                };
                let out_spot = solved.as_ref().map(|q| q.amount_out);
                if let Some(r) = rings.get_mut(k) {
                    // Unroutable blocks count as zero output: the average
                    // remembers a collapse, never smooths one away.
                    r.push(out_spot.unwrap_or(U256::ZERO), n_twa);
                }
                let twa = rings.get(k).and_then(Ring::mean);
                let out_min = match (out_spot, twa) {
                    (Some(s), Some(t)) => Some(s.min(t)),
                    (Some(s), None) => Some(s),
                    (None, _) => None,
                };
                let viable = match out_min {
                    Some(m) => within_impact(size_in, rho0, m, self.cfg.max_impact_bps),
                    None => false,
                };
                if viable {
                    cap = cap.max(size_in);
                }
                buckets.push(Bucket {
                    size_in,
                    out_spot,
                    out_min,
                    pools: solved.as_ref().map_or(0, |q| {
                        u8::try_from(q.allocs.iter().filter(|a| !a.amount_in.is_zero()).count())
                            .unwrap_or(u8::MAX)
                    }),
                    hop_gas: solved.as_ref().map_or(0, |q| q.hop_gas),
                    viable,
                });
            }
            let idx = usize::from(coll.0);
            if table.exit_cap.len() <= idx {
                table.exit_cap.resize(idx.saturating_add(1), U256::ZERO);
            }
            if let Some(slot) = table.exit_cap.get_mut(idx) {
                *slot = (*slot).max(cap);
            }
            table.pairs.insert(
                (coll, debt),
                RouteEntry {
                    legs: legs.into_iter().map(|(_, l)| l).collect(),
                    rho0,
                    buckets,
                },
            );
        }
        let table = Arc::new(table);
        self.slot.store(Arc::clone(&table));
        table
    }
}

/// `out ≥ size · ρ₀² / 2^192 · (1 − max_impact)`: realised output within
/// the impact bound of the zero-size (post-fee) marginal.
fn within_impact(size_in: U256, rho0: U256, out: U256, max_impact_bps: u16) -> bool {
    let Ok(zero_impact) = mul_div_512(size_in, rho0, Q96).and_then(|v| mul_div_512(v, rho0, Q96))
    else {
        return false;
    };
    let Some(keep) = BPS.checked_sub(U256::from(max_impact_bps)) else {
        return false;
    };
    let (Some(lhs), Some(rhs)) = (out.checked_mul(BPS), zero_impact.checked_mul(keep)) else {
        return false;
    };
    lhs >= rhs
}
