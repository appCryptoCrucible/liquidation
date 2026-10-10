//! Phase 4 of the coverage plan: the token graph, the zero-size best-rate
//! table, and the per-candidate route search.
//!
//! * [`TokenGraph`] (4A): tokens are nodes; each direction through a pool,
//!   and each unwrap (one way), is an edge. Compressed adjacency, sorted by
//!   source. It holds no pool state: rates and amounts come from the
//!   [`PoolBook`], the single copy. Rebuilt when pools are added.
//! * [`ZeroTable`] (4B): for every target token, the best zero-size rate
//!   from every token within `K` hops, as a Q32 log2, and the first hop.
//!   Hop-limited Bellman-Ford, run backward from each target: log rates
//!   can be of either sign (raw units differ by decimals) and arbitrage
//!   cycles exist, so Dijkstra does not apply and the hop cap is what
//!   bounds the answer. Each edge's rate is its marginal at zero size,
//!   rounded up, so the table is an upper bound on what any size can get
//!   along any route of at most `K` hops: the search prunes with it.
//! * [`search`] (4E, exact): depth-first branch and bound from the seized
//!   token, carrying the exact amount through each hop with the pool's own
//!   math ([`HopQuoter`]), children tried best bound first. A branch is cut
//!   when even its optimistic finish (amount so far × the table's best
//!   rate onward) cannot beat the `n`-th best net route already found. It
//!   returns the top `n` routes by output net of hop gas.
//!
//! Integer arithmetic only (the workspace denies float arithmetic): logs
//! are fixed point, Q32.

use std::collections::HashMap;

use alloy_primitives::U256;
use liq_types::AssetId;
use smallvec::SmallVec;

use crate::exact::GasTerms;
use crate::solver::{Leg, PoolBook, PoolId, RouteError};

/// `log2` in Q32: `1 << 32` is a factor of two.
pub type Log2 = i64;

/// One unit of [`Log2`] (2^-32).
const ULP: Log2 = 1;
/// Margin added to every edge's log so truncation in [`log2_q32`] and in
/// the doubling of a `ρ` can only raise the table, never lower it.
const EDGE_MARGIN: Log2 = 8 * ULP;
/// `log2(2^96)` in Q32: the Q96 scale of a `ρ`.
const Q96_LOG: Log2 = 96 << 32;

/// `log2(x)` in Q32, rounded down (to within a few units of 2^-32). `None`
/// for zero.
#[must_use]
pub fn log2_q32(x: U256) -> Option<Log2> {
    if x.is_zero() {
        return None;
    }
    let msb = 255usize.checked_sub(x.leading_zeros())?;
    // The top 64 bits, MSB at bit 63: a mantissa in [2^63, 2^64).
    let mut m: u128 = if msb >= 63 {
        u128::try_from(x.wrapping_shr(msb.checked_sub(63)?)).ok()?
    } else {
        u128::try_from(x.wrapping_shl(63usize.checked_sub(msb)?)).ok()?
    };
    let mut frac: i64 = 0;
    for _ in 0..32 {
        // m < 2^64, so m² < 2^128.
        m = m.checked_mul(m)?.wrapping_shr(63);
        frac = frac.wrapping_shl(1);
        if m >= 1u128 << 64 {
            frac |= 1;
            m = m.wrapping_shr(1);
        }
    }
    let int = i64::try_from(msb).ok()?.checked_shl(32)?;
    int.checked_add(frac)
}

/// The zero-size rate (`out` per `in`, raw units) of a `ρ` (Q96 square root
/// of it), as a Q32 log2 rounded up.
fn rate_of_rho(rho: U256) -> Option<Log2> {
    let l = log2_q32(rho)?;
    l.checked_sub(Q96_LOG)?
        .checked_mul(2)?
        .checked_add(EDGE_MARGIN)
}

/// What one edge goes through.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// A swap through one pool: coin `i` in, coin `j` out.
    Pool(Leg),
    /// The unwrap of this wrapper into what it pays (one way).
    Unwrap(AssetId),
}

/// One directed edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: AssetId,
    pub to: AssetId,
    pub via: Via,
    /// Gas of this hop inside the Executor.
    pub hop_gas: u64,
}

/// Index of an edge in [`TokenGraph::edges`].
pub type EdgeId = u32;

/// The token graph: compressed adjacency by source token.
#[derive(Clone, Debug, Default)]
pub struct TokenGraph {
    /// Node index → token.
    nodes: Vec<AssetId>,
    /// Token → node index.
    index: HashMap<AssetId, u32>,
    /// Node index → first edge; `offsets[n]..offsets[n + 1]` are its edges.
    offsets: Vec<u32>,
    /// Sorted by source node.
    edges: Vec<Edge>,
}

impl TokenGraph {
    /// Every direction through every pool of the book whose two coins
    /// differ, and every unwrap. Liveness is not a structural property:
    /// [`ZeroTable::build`] and [`search`] skip what cannot be quoted now.
    pub fn build(book: &PoolBook) -> Result<Self, RouteError> {
        Self::build_with(book, |_| true)
    }

    /// [`Self::build`] over the pools `keep` admits (by index into
    /// [`PoolBook::pools`]); unwraps always enter.
    pub fn build_with(book: &PoolBook, keep: impl Fn(PoolId) -> bool) -> Result<Self, RouteError> {
        let mut edges: Vec<Edge> = Vec::new();
        for (p, pool) in book.pools().iter().enumerate() {
            let id = PoolId(u32::try_from(p).map_err(|_| RouteError::Math)?);
            if !keep(id) {
                continue;
            }
            for (i, &a) in pool.assets.iter().enumerate() {
                for (j, &b) in pool.assets.iter().enumerate() {
                    if i == j || a == b {
                        continue;
                    }
                    let (Ok(i), Ok(j)) = (u8::try_from(i), u8::try_from(j)) else {
                        return Err(RouteError::BadLeg);
                    };
                    edges.push(Edge {
                        from: a,
                        to: b,
                        via: Via::Pool(Leg { pool: id, i, j }),
                        hop_gas: pool.hop_gas,
                    });
                }
            }
        }
        for u in book.unwraps() {
            if u.wrapper != u.into {
                edges.push(Edge {
                    from: u.wrapper,
                    to: u.into,
                    via: Via::Unwrap(u.wrapper),
                    hop_gas: u.gas,
                });
            }
        }
        let mut nodes: Vec<AssetId> = edges.iter().flat_map(|e| [e.from, e.to]).collect();
        nodes.sort_unstable_by_key(|a| a.0);
        nodes.dedup();
        let mut index = HashMap::with_capacity(nodes.len());
        for (n, a) in nodes.iter().enumerate() {
            index.insert(*a, u32::try_from(n).map_err(|_| RouteError::Math)?);
        }
        let node_of = |a: AssetId| index.get(&a).copied().ok_or(RouteError::Math);
        let mut keyed = Vec::with_capacity(edges.len());
        for e in edges {
            keyed.push((node_of(e.from)?, e));
        }
        keyed.sort_by_key(|(n, _)| *n);
        let mut offsets = vec![0u32; nodes.len().checked_add(1).ok_or(RouteError::Math)?];
        for (n, _) in &keyed {
            let at = usize::try_from(*n)
                .ok()
                .and_then(|n| n.checked_add(1))
                .ok_or(RouteError::Math)?;
            let c = offsets.get_mut(at).ok_or(RouteError::Math)?;
            *c = c.checked_add(1).ok_or(RouteError::Math)?;
        }
        for n in 1..offsets.len() {
            let prev = *offsets.get(n.wrapping_sub(1)).ok_or(RouteError::Math)?;
            let c = offsets.get_mut(n).ok_or(RouteError::Math)?;
            *c = c.checked_add(prev).ok_or(RouteError::Math)?;
        }
        Ok(Self {
            nodes,
            index,
            offsets,
            edges: keyed.into_iter().map(|(_, e)| e).collect(),
        })
    }

    #[inline]
    #[must_use]
    pub fn nodes(&self) -> &[AssetId] {
        &self.nodes
    }

    #[inline]
    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    #[inline]
    #[must_use]
    pub fn edge(&self, id: EdgeId) -> Option<&Edge> {
        self.edges.get(usize::try_from(id).ok()?)
    }

    #[inline]
    #[must_use]
    pub fn node(&self, a: AssetId) -> Option<u32> {
        self.index.get(&a).copied()
    }

    /// The edge ids leaving `a`.
    #[must_use]
    pub fn out_edges(&self, a: AssetId) -> core::ops::Range<EdgeId> {
        let Some(n) = self.node(a).and_then(|n| usize::try_from(n).ok()) else {
            return 0..0;
        };
        let lo = self.offsets.get(n).copied().unwrap_or(0);
        let hi = n
            .checked_add(1)
            .and_then(|m| self.offsets.get(m).copied())
            .unwrap_or(lo);
        lo..hi
    }
}

/// Value of a token for the depth filter: USD (WAD) of `10^18` raw units.
pub type RawValue = U256;

/// [`RawValue`] from a USD price in RAY per whole token and the token's
/// decimals: `price · 10^18 / 10^decimals / 10^9`.
#[must_use]
pub fn raw_value(price_ray: U256, decimals: u8) -> Option<RawValue> {
    let d = u32::from(decimals);
    if d <= 9 {
        price_ray.checked_mul(U256::from(10u8).checked_pow(U256::from(9u32.checked_sub(d)?))?)
    } else {
        price_ray.checked_div(U256::from(10u8).checked_pow(U256::from(d.checked_sub(9)?))?)
    }
}

/// USD (WAD) of `amount` raw units at `v`.
fn usd_of(amount: U256, v: RawValue) -> Option<U256> {
    crate::solver::mul_div_512(amount, v, crate::solver::WAD).ok()
}

/// Raw units of a token worth `usd` (WAD) at `v`.
fn raw_of(usd: U256, v: RawValue) -> Option<U256> {
    crate::solver::mul_div_512(usd, crate::solver::WAD, v).ok()
}

/// The slippage the depth is measured at: 2 %, in basis points.
const DEPTH_SLIP_BPS: u64 = 200;
/// A constant-product pool trades within 2 % of its marginal rate up to
/// `1/49` of its input reserve (`R / (R + x) ≥ 0.98`), so 49 times the
/// 2 % size is the reserve a V2 pool would need to trade as well: the
/// depth reported, comparable across venues and equal to a V2 pair's own
/// input reserve (over its fee).
const CP_RESERVE_PER_DEPTH: u64 = 49;
/// The size ladder starts here (USD) and doubles; then a bisection.
const LADDER_START_USD: u64 = 100;
const LADDER_STEPS: u32 = 48;
const BISECT_STEPS: u32 = 14;

/// The largest input of coin `i` (USD, WAD, at `value_in`) that `pool`
/// sells into coin `j` within 2 % of its zero-size rate, by exact quotes
/// on the book's state. Zero when not even the ladder's first rung does.
/// A V3 / V4 pool is quoted on its seeded tick window, past which it
/// refuses: what is not known is not counted.
pub fn slip_depth(pool: &crate::solver::Pool, i: u8, j: u8, value_in: RawValue) -> Option<U256> {
    use crate::solver::{mul_div_512, Q96};
    if !pool.is_live() || value_in.is_zero() {
        return None;
    }
    let rho = pool.rho_at_zero(i, j).ok().filter(|r| !r.is_zero())?;
    let within = |usd: U256| -> bool {
        let Some(amount) = raw_of(usd, value_in).filter(|a| !a.is_zero()) else {
            return true;
        };
        let Ok(out) = pool.quote_exact_in(i, j, amount) else {
            return false;
        };
        let ideal = mul_div_512(amount, rho, Q96).and_then(|x| mul_div_512(x, rho, Q96));
        let Ok(ideal) = ideal else { return false };
        let keep = U256::from(10_000u64.saturating_sub(DEPTH_SLIP_BPS));
        match (
            out.checked_mul(U256::from(10_000u64)),
            ideal.checked_mul(keep),
        ) {
            (Some(o), Some(i)) => o >= i,
            _ => false,
        }
    };
    let wad = crate::solver::WAD;
    let mut lo = U256::ZERO;
    let mut hi = U256::from(LADDER_START_USD).checked_mul(wad)?;
    let mut bounded = false;
    for _ in 0..LADDER_STEPS {
        if !within(hi) {
            bounded = true;
            break;
        }
        lo = hi;
        hi = hi.checked_mul(U256::from(2u64))?;
    }
    if !bounded {
        return Some(lo);
    }
    for _ in 0..BISECT_STEPS {
        let mid = lo.checked_add(hi.checked_sub(lo)?.wrapping_shr(1))?;
        if within(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// One pool's offer of a price to a coin.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PriceOffer {
    /// The lesser depth of the two ways (USD, WAD): the offer's weight.
    pub depth: U256,
    pub price: RawValue,
    pub pool: PoolId,
    /// The priced coin the offer was made from.
    pub from: AssetId,
    /// The pass (hops from a seed, from 1) it was made in.
    pub pass: u8,
}

/// The offer at which half the offering depth is below and half above;
/// the lower of two on a tie.
fn weighted_median(offers: &mut [PriceOffer]) -> Option<PriceOffer> {
    offers.sort_unstable_by_key(|o| o.price);
    let total = offers
        .iter()
        .try_fold(U256::ZERO, |t, o| t.checked_add(o.depth))?;
    let half = total.wrapping_shr(1);
    let mut acc = U256::ZERO;
    for o in offers.iter() {
        acc = acc.checked_add(o.depth)?;
        if acc >= half {
            return Some(*o);
        }
    }
    offers.last().copied()
}

/// A second pool's offer this close (basis points) trusts a price.
pub const CORROBORATE_BPS: u64 = 1_000;
/// A pool paying more than this (basis points) over the fair rate in some
/// direction is off the market: a stranded position or a honeypot shows
/// as a direction that pays more than the market, the lure. A direction
/// paying less than fair is the pool's fee or spread, a cost the exact
/// quote prices (block 25,791,740: the two crvUSD/WETH pools, which the
/// winner sold into, paid 5.3 % under fair the other way and were refused
/// by a band on both sides).
pub const FAIR_BAND_BPS: u64 = 500;

/// `pool`'s spot rate from coin `i` into `j` as value out per value in at
/// the margin, in bps of the fair rate `pa / pb` (10,000 = fair); `None`
/// when it cannot be quoted.
fn fair_bps(pool: &crate::solver::Pool, i: u8, j: u8, pa: RawValue, pb: RawValue) -> Option<u32> {
    use crate::solver::{mul_div_512, Q96};
    let rho = pool.rho_at_zero(i, j).ok()?;
    // The value out per unit of value in, at the margin: ρ² · pb / 2^192,
    // against pa.
    let out = mul_div_512(pb, rho, Q96)
        .and_then(|x| mul_div_512(x, rho, Q96))
        .ok()?;
    let bps = out.checked_mul(U256::from(10_000u64))?.checked_div(pa)?;
    Some(u32::try_from(bps).unwrap_or(u32::MAX))
}

/// Whether `pool`'s spot rate from coin `i` into `j` pays no more than
/// [`FAIR_BAND_BPS`] over the fair rate `pa / pb` (raw values). A rate that
/// cannot be quoted, or pays nothing, is not off the market: it is a pool
/// with nothing at spot, whose depth comes out under the floor.
fn near_fair(pool: &crate::solver::Pool, i: u8, j: u8, pa: RawValue, pb: RawValue) -> bool {
    fair_bps(pool, i, j, pa, pb)
        .is_none_or(|b| u64::from(b) <= 10_000u64.saturating_add(FAIR_BAND_BPS))
}

/// [`slip_depth`] as a constant-product-equivalent reserve (USD, WAD).
fn cp_depth(pool: &crate::solver::Pool, i: u8, j: u8, value_in: RawValue) -> Option<U256> {
    slip_depth(pool, i, j, value_in)?.checked_mul(U256::from(CP_RESERVE_PER_DEPTH))
}

/// Each pool's depth in USD (WAD): over every direction whose input coin
/// has a trusted price and whose output coin has a price, the least
/// constant-product-equivalent reserve (49 × what it sells within 2 %,
/// [`slip_depth`]). A pool with a direction paying more than
/// [`FAIR_BAND_BPS`] over the fair rate the two prices imply is off the
/// market (a honeypot, a stranded position: only deep around its own wrong
/// price, which pays over the market one way) and is disregarded: `None`,
/// as when no direction counts.
///
/// Prices start from `seed` (trusted) and spread through the book, a hop
/// per pass (`passes` bounds it). A pool offers coin `b` a price from a
/// trusted coin `a` at its spot rate when it is at least `floor` deep both
/// ways: selling `a` into `b`, and `b` back into `a` at that candidate
/// price (a position stranded at an absurd price holds one coin only and
/// fails the way back; a dust pool fails both). The coin's price is the
/// depth-weighted median of its offers, and it is **trusted** once another
/// pool's offer is within [`CORROBORATE_BPS`] of it. Only trusted prices
/// price other coins, so one mispriced pool cannot spread its price. A coin
/// with one pool's offer only keeps it, untrusted, for its own pools'
/// depths; a coin whose pools' offers never agree is contested (its price
/// unknown) and every pool holding it is disregarded. A token priced only
/// through dust has no price.
pub fn pool_depths(
    book: &PoolBook,
    seed: &HashMap<AssetId, RawValue>,
    floor: U256,
    passes: u8,
) -> (Vec<Option<U256>>, HashMap<AssetId, RawValue>) {
    let t = pool_depths_traced(book, seed, floor, passes, None);
    (t.depths, t.prices)
}

/// Why a pool has no depth.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DepthWhy {
    /// A coin of its is contested.
    Contested,
    /// No direction sells a coin with a trusted price.
    NoTrustedInput,
    /// A trusted coin sells into one with no price at all.
    Unpriced,
    /// A direction pays more than the fair band over its coins' prices:
    /// the value out per value in at the margin, in bps of fair (10,000),
    /// for the first direction that did.
    OffMarket(u32),
    /// The quote itself failed at every size.
    NoQuote,
}

/// [`pool_depths`], with where each derived price came from.
#[derive(Clone, Debug, Default)]
pub struct DepthTrace {
    pub depths: Vec<Option<U256>>,
    /// Per pool, why its depth is `None` (`None` when it has one).
    pub why: Vec<Option<DepthWhy>>,
    pub prices: HashMap<AssetId, RawValue>,
    /// The offer each derived price took (seeded prices have none).
    pub chosen: HashMap<AssetId, PriceOffer>,
    /// Every offer each derived coin had, in its last pass.
    pub offers: HashMap<AssetId, Vec<PriceOffer>>,
    /// Seeded coins and coins whose price a second pool corroborated.
    pub trusted: std::collections::HashSet<AssetId>,
    /// Coins whose pools offered prices that never agreed: their price is
    /// unknown, and every pool holding one is disregarded.
    pub contested: std::collections::HashSet<AssetId>,
}

/// The most a token's market cap may be (USD, WAD) for a price only one
/// pool offers to stand: above it the pool is a honeypot.
pub const MAX_MARKET_CAP_USD: u64 = 1_000_000_000_000;

/// [`pool_depths`], traced. With `supply` (raw total supply per token), a
/// price only one pool offers must put the token's market cap under
/// [`MAX_MARKET_CAP_USD`], or the token is contested: a single honeypot
/// pool cannot be outvoted, but it cannot price a token beyond every
/// asset there is.
pub fn pool_depths_traced(
    book: &PoolBook,
    seed: &HashMap<AssetId, RawValue>,
    floor: U256,
    passes: u8,
    supply: Option<&HashMap<AssetId, U256>>,
) -> DepthTrace {
    let mut prices = seed.clone();
    let mut trusted: std::collections::HashSet<AssetId> = seed.keys().copied().collect();
    let mut chosen: HashMap<AssetId, PriceOffer> = HashMap::new();
    let mut all_offers: HashMap<AssetId, Vec<PriceOffer>> = HashMap::new();
    // Coins with offers that do not yet agree: their latest median.
    let mut pending: HashMap<AssetId, PriceOffer> = HashMap::new();
    // (pool, i, j) → depth, measured once, when coin `i` first has a price.
    let mut measured: HashMap<(usize, u8, u8), Option<U256>> = HashMap::new();
    let mut depth_of = |prices: &HashMap<AssetId, RawValue>, p: usize, i: u8, j: u8| {
        *measured.entry((p, i, j)).or_insert_with(|| {
            let pool = book.pools().get(p)?;
            let a = pool.assets.get(usize::from(i))?;
            cp_depth(pool, i, j, *prices.get(a)?)
        })
    };
    let dirs =
        |n: usize| (0..n).flat_map(move |i| (0..n).filter(move |&j| j != i).map(move |j| (i, j)));
    for pass in 1..=passes {
        // token → every offer a qualifying pool makes it
        let mut found: HashMap<AssetId, Vec<PriceOffer>> = HashMap::new();
        for (p, pool) in book.pools().iter().enumerate() {
            for (i, j) in dirs(pool.assets.len()) {
                let (Some(&a), Some(&b)) = (pool.assets.get(i), pool.assets.get(j)) else {
                    continue;
                };
                if a == b || prices.contains_key(&b) || !trusted.contains(&a) {
                    continue;
                }
                let Some(&pa) = prices.get(&a) else { continue };
                let (Ok(i), Ok(j)) = (u8::try_from(i), u8::try_from(j)) else {
                    continue;
                };
                let Some(depth) = depth_of(&prices, p, i, j).filter(|d| *d >= floor) else {
                    continue;
                };
                // `b` per `a` at the margin: ρ² / 2^192; a unit of `b` is
                // worth `pa / rate`.
                let Ok(rho) = pool.rho_at_zero(i, j) else {
                    continue;
                };
                let Some(sq) = rho.checked_mul(rho) else {
                    continue;
                };
                let Ok(pb) = crate::solver::mul_div_512(pa, crate::solver::Q192, sq) else {
                    continue;
                };
                // The way back, at the price this pool offers `b`.
                let back = cp_depth(pool, j, i, pb).unwrap_or(U256::ZERO);
                if back < floor {
                    continue;
                }
                found.entry(b).or_default().push(PriceOffer {
                    depth: depth.min(back),
                    price: pb,
                    pool: PoolId(u32::try_from(p).unwrap_or(u32::MAX)),
                    from: a,
                    pass,
                });
            }
        }
        let mut new_trust = false;
        for (b, mut offers) in found {
            if let Some(m) = weighted_median(&mut offers) {
                let band = m
                    .price
                    .checked_mul(U256::from(CORROBORATE_BPS))
                    .map(|x| x.wrapping_div(U256::from(10_000u64)))
                    .unwrap_or(U256::MAX);
                let agreed = offers
                    .iter()
                    .any(|o| o.pool != m.pool && o.price.abs_diff(m.price) <= band);
                if agreed {
                    prices.insert(b, m.price);
                    chosen.insert(b, m);
                    trusted.insert(b);
                    pending.remove(&b);
                    new_trust = true;
                } else {
                    pending.insert(b, m);
                }
            }
            all_offers.insert(b, offers);
        }
        if !new_trust {
            break;
        }
    }
    // Never corroborated. One pool's offer is all there is to know: it
    // stands, untrusted. Offers from several pools that never agreed leave
    // the price unknown: contested.
    let mut contested = std::collections::HashSet::new();
    for (b, m) in pending {
        let pools: std::collections::HashSet<u32> = all_offers
            .get(&b)
            .map(|o| o.iter().map(|o| o.pool.0).collect())
            .unwrap_or_default();
        let cap = U256::from(MAX_MARKET_CAP_USD).saturating_mul(crate::solver::WAD);
        let too_big = supply
            .and_then(|s| s.get(&b))
            .and_then(|&n| usd_of(n, m.price))
            .is_some_and(|mc| mc > cap);
        if pools.len() > 1 || too_big {
            contested.insert(b);
        } else {
            prices.insert(b, m.price);
            chosen.insert(b, m);
        }
    }
    let mut why: Vec<Option<DepthWhy>> = Vec::with_capacity(book.pools().len());
    let depths = book
        .pools()
        .iter()
        .enumerate()
        .map(|(p, pool)| {
            if pool.assets.iter().any(|a| contested.contains(a)) {
                why.push(Some(DepthWhy::Contested));
                return None;
            }
            let mut least: Option<U256> = None;
            let mut seen_trusted = false;
            let mut seen_priced = false;
            for (i, j) in dirs(pool.assets.len()) {
                let (Some(a), Some(b)) = (pool.assets.get(i), pool.assets.get(j)) else {
                    continue;
                };
                if !trusted.contains(a) {
                    continue;
                }
                seen_trusted = true;
                let (Some(&pa), Some(&pb)) = (prices.get(a), prices.get(b)) else {
                    continue;
                };
                seen_priced = true;
                let (Ok(i), Ok(j)) = (u8::try_from(i), u8::try_from(j)) else {
                    continue;
                };
                // Off the market (a honeypot, a stranded position): the
                // pool is disregarded.
                if !near_fair(pool, i, j, pa, pb) {
                    why.push(Some(DepthWhy::OffMarket(
                        fair_bps(pool, i, j, pa, pb).unwrap_or(0),
                    )));
                    return None;
                }
                if let Some(d) = depth_of(&prices, p, i, j) {
                    least = Some(least.map_or(d, |l| l.min(d)));
                }
            }
            why.push(match least {
                Some(_) => None,
                None if !seen_trusted => Some(DepthWhy::NoTrustedInput),
                None if !seen_priced => Some(DepthWhy::Unpriced),
                None => Some(DepthWhy::NoQuote),
            });
            least
        })
        .collect();
    DepthTrace {
        depths,
        why,
        prices,
        chosen,
        offers: all_offers,
        trusted,
        contested,
    }
}

/// The exit providers the router quotes, one per venue and deployer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    UniswapV2,
    Sushi,
    UniswapV3,
    UniswapV4,
    CurveStable,
    CurveNg,
    CurveCrypto,
    /// SushiSwap V3 and PancakeSwap V3 pools: V3 math on another factory.
    SushiV3,
    PancakeV3,
    Balancer,
    /// Fluid DEX (Instadapp) T1 pools.
    Fluid,
}

impl Provider {
    pub const ALL: [Self; 11] = [
        Self::UniswapV2,
        Self::Sushi,
        Self::UniswapV3,
        Self::UniswapV4,
        Self::CurveStable,
        Self::CurveNg,
        Self::CurveCrypto,
        Self::SushiV3,
        Self::PancakeV3,
        Self::Balancer,
        Self::Fluid,
    ];

    /// The provider of `pool`; `None` for a V2 pair of another factory.
    #[must_use]
    pub fn of(pool: &crate::solver::Pool) -> Option<Self> {
        use crate::solver::PoolState;
        Some(match &pool.state {
            PoolState::V2(s) if s.factory == liq_wire::wire::V2_FACTORY_UNISWAP => Self::UniswapV2,
            PoolState::V2(s) if s.factory == liq_wire::wire::V2_FACTORY_SUSHI => Self::Sushi,
            PoolState::V2(_) => return None,
            PoolState::V3(s) if s.v4.is_some() => Self::UniswapV4,
            PoolState::V3(s) if s.factory == liq_wire::wire::V3_FACTORY_SUSHI => Self::SushiV3,
            PoolState::V3(s) if s.factory == liq_wire::wire::V3_FACTORY_PANCAKE => Self::PancakeV3,
            PoolState::V3(_) => Self::UniswapV3,
            PoolState::Curve(s) if s.ng => Self::CurveNg,
            PoolState::Curve(_) => Self::CurveStable,
            PoolState::Crypto(_) => Self::CurveCrypto,
            PoolState::Balancer(_) => Self::Balancer,
            PoolState::Fluid(_) => Self::Fluid,
        })
    }
}

/// One pool of the curated set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Curated {
    pub pool: PoolId,
    pub provider: Provider,
    /// The pool's token set, sorted: its pairing.
    pub pairing: SmallVec<[AssetId; crate::solver::MAX_COINS]>,
    /// USD (WAD), as [`pool_depths`] measures it.
    pub depth: U256,
}

/// How many pools a provider may have in the curated set, and the least
/// depth (USD, WAD) one needs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Quota {
    pub max: usize,
    pub min_depth: U256,
}

/// The deepest pools of each provider, up to its [`Quota`], each pairing
/// (token set) taken once. All pools are walked deepest first: a pool is
/// taken when it meets its provider's minimum depth, the provider has room
/// left, and no deeper pool already took its pairing. So when providers
/// share a pairing, the deepest of them gets it, and the others go on down
/// their own lists to their next unique pairing. Pools without a measured
/// depth are not ranked.
#[must_use]
pub fn curated_pools(
    book: &PoolBook,
    depths: &[Option<U256>],
    quota: impl Fn(Provider) -> Quota,
) -> Vec<Curated> {
    let mut all: Vec<Curated> = book
        .pools()
        .iter()
        .zip(depths)
        .enumerate()
        .filter_map(|(i, (pool, d))| {
            let depth = (*d)?;
            let mut pairing: SmallVec<[AssetId; crate::solver::MAX_COINS]> =
                pool.assets.iter().copied().collect();
            pairing.sort_unstable_by_key(|a| a.0);
            pairing.dedup();
            Some(Curated {
                pool: PoolId(u32::try_from(i).ok()?),
                provider: Provider::of(pool)?,
                pairing,
                depth,
            })
        })
        .collect();
    // Deepest first; ties by pool index, so the choice is stable.
    all.sort_by(|x, y| y.depth.cmp(&x.depth).then(x.pool.0.cmp(&y.pool.0)));
    let mut taken: HashMap<SmallVec<[AssetId; crate::solver::MAX_COINS]>, Provider> =
        HashMap::new();
    let mut count: HashMap<Provider, usize> = HashMap::new();
    let mut out = Vec::new();
    for c in all {
        let q = quota(c.provider);
        let n = count.entry(c.provider).or_insert(0);
        if *n >= q.max || c.depth < q.min_depth || taken.contains_key(&c.pairing) {
            continue;
        }
        *n = n.saturating_add(1);
        taken.insert(c.pairing.clone(), c.provider);
        out.push(c);
    }
    out
}

/// Each token's `per_token` deepest pools: the spokes that join a token
/// to the curated backbone ([`curated_pools`]). Pools without a measured
/// depth (disregarded, unpriced) are not spokes.
#[must_use]
pub fn spoke_pools(book: &PoolBook, depths: &[Option<U256>], per_token: usize) -> Vec<PoolId> {
    let mut by_token: HashMap<AssetId, Vec<(U256, PoolId)>> = HashMap::new();
    for (i, (pool, d)) in book.pools().iter().zip(depths).enumerate() {
        let (Some(d), Ok(i)) = (*d, u32::try_from(i)) else {
            continue;
        };
        for a in &pool.assets {
            by_token.entry(*a).or_default().push((d, PoolId(i)));
        }
    }
    let mut out: Vec<PoolId> = Vec::new();
    for (_, mut pools) in by_token {
        pools.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.1 .0.cmp(&y.1 .0)));
        out.extend(pools.into_iter().take(per_token).map(|(_, id)| id));
    }
    out.sort_unstable_by_key(|id| id.0);
    out.dedup();
    out
}

/// The graph exits route through: its structure, and the zero-size table
/// as of its build (the search's bound). Rates and amounts always come
/// from the book.
#[derive(Clone, Debug, Default)]
pub struct GraphRoutes {
    pub graph: TokenGraph,
    pub table: ZeroTable,
    /// The same pools, those a chain can run through (venue
    /// 10: Uniswap V3, Uniswap V2 and SushiSwap), and their table.
    pub chain_graph: TokenGraph,
    pub chain_table: ZeroTable,
}

/// Whether a chain (venue 10) can run through `pool`: every pool the book
/// holds (Uniswap V2 / SushiSwap, V3, V4 and every Curve venue) has a chain
/// hop kind; V4 and Curve hops sell an exact input only.
#[must_use]
pub fn chainable(pool: &crate::solver::Pool) -> bool {
    Provider::of(pool).is_some()
}

/// The venue-10 data of a path of `hops` (edges of `graph`): hop count,
/// each hop's kind and param, the intermediate tokens, then the extras of
/// its V4 and Curve hops (each one's param is its extra's offset). With it,
/// whether every hop can buy an exact output (V3 and V2 only). `None` when
/// a hop is not a pool, or its data cannot be written.
#[must_use]
pub fn chain_data(
    book: &PoolBook,
    graph: &TokenGraph,
    hops: &[EdgeId],
) -> Option<(SmallVec<[Leg; 4]>, Vec<u8>, bool)> {
    use crate::solver::PoolState;
    use liq_wire::wire::{
        CHAIN_HOP_BALANCER, CHAIN_HOP_CURVE, CHAIN_HOP_CURVE_CRYPTO, CHAIN_HOP_V2, CHAIN_HOP_V3,
        CHAIN_HOP_V3_PANCAKE, CHAIN_HOP_V3_SUSHI, CHAIN_HOP_V4, V3_FACTORY_PANCAKE,
        V3_FACTORY_SUSHI,
    };
    let n = u8::try_from(hops.len()).ok()?;
    let base = 1usize
        .checked_add(hops.len().checked_mul(4)?)?
        .checked_add(hops.len().saturating_sub(1).checked_mul(20)?)?;
    let mut head = vec![n];
    let mut mids = Vec::new();
    let mut extras: Vec<u8> = Vec::new();
    let mut legs = SmallVec::new();
    let mut exact_out = true;
    for (k, id) in hops.iter().enumerate() {
        let e = graph.edge(*id)?;
        let Via::Pool(l) = e.via else { return None };
        let pool = book.get(l.pool)?;
        let extra_at = |extras: &Vec<u8>| -> Option<[u8; 3]> {
            let off = u32::try_from(base.checked_add(extras.len())?).ok()?;
            let b = off.to_be_bytes();
            (b[0] == 0).then_some([b[1], b[2], b[3]])
        };
        match &pool.state {
            PoolState::V3(s) => match &s.v4 {
                Some(key) => {
                    head.push(CHAIN_HOP_V4);
                    head.extend_from_slice(&extra_at(&extras)?);
                    extras.extend_from_slice(&key.leg_data());
                    exact_out = false;
                }
                None => {
                    // The fee tier, on the factory that deployed the pool.
                    head.push(match s.factory {
                        V3_FACTORY_SUSHI => CHAIN_HOP_V3_SUSHI,
                        V3_FACTORY_PANCAKE => CHAIN_HOP_V3_PANCAKE,
                        _ => CHAIN_HOP_V3,
                    });
                    head.extend_from_slice(&s.fee_pips.to_be_bytes()[1..]);
                }
            },
            PoolState::V2(s) => {
                head.push(CHAIN_HOP_V2);
                head.extend_from_slice(&[0, 0, s.factory]);
            }
            PoolState::Curve(c) => {
                head.push(CHAIN_HOP_CURVE);
                head.extend_from_slice(&extra_at(&extras)?);
                extras.extend_from_slice(pool.address.as_slice());
                extras.extend_from_slice(&[l.i, l.j, c.handler]);
                exact_out = false;
            }
            PoolState::Crypto(c) => {
                head.push(CHAIN_HOP_CURVE_CRYPTO);
                head.extend_from_slice(&extra_at(&extras)?);
                extras.extend_from_slice(pool.address.as_slice());
                extras.extend_from_slice(&[l.i, l.j, c.handler]);
                exact_out = false;
            }
            PoolState::Balancer(b) => {
                head.push(CHAIN_HOP_BALANCER);
                head.extend_from_slice(&extra_at(&extras)?);
                extras.extend_from_slice(b.pool_id.as_slice());
                exact_out = false;
            }
            // The pool and the direction (coin 0 is its token0); exact input only.
            PoolState::Fluid(_) => {
                head.push(liq_wire::wire::CHAIN_HOP_FLUID);
                head.extend_from_slice(&extra_at(&extras)?);
                extras.extend_from_slice(pool.address.as_slice());
                extras.push(u8::from(l.i == 0 && l.j == 1));
                exact_out = false;
            }
        }
        if k.saturating_add(1) < hops.len() {
            mids.extend_from_slice(pool.tokens.get(usize::from(l.j))?.as_slice());
        }
        legs.push(l);
    }
    head.extend_from_slice(&mids);
    head.extend_from_slice(&extras);
    Some((legs, head, exact_out))
}

/// A chain the search found: its route, hops, venue-10 data, and whether
/// every hop can buy an exact output.
pub type ChainFound = (Route, SmallVec<[Leg; 4]>, Vec<u8>, bool);

/// The best chain (venue 10) of at most four hops selling `amount` of
/// `from` into `to` on the book's graph: its route (output and hop gas, the
/// chain leg's own [`CHAIN_LEG_GAS`] included), hops and venue-10 data.
/// `None` without a graph or a route.
#[must_use]
pub fn best_chain(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    gas: &GasTerms,
    min_hops: usize,
) -> Option<ChainFound> {
    best_chains(book, from, to, amount, gas, min_hops)
        .into_iter()
        .next()
}

/// [`best_chain`]'s candidates: the search's top routes of at least
/// `min_hops` hops that can run as a chain, best first.
#[must_use]
pub fn best_chains(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    gas: &GasTerms,
    min_hops: usize,
) -> Vec<ChainFound> {
    best_chains_n(book, from, to, amount, gas, min_hops, CHAIN_TOP_N)
}

/// Routes found for `(from, to, min_hops, top_n)`.
type ReusedRoutes = ((AssetId, AssetId, usize, usize), Vec<ChainFound>);

thread_local! {
    /// Routes found while a [`reuse_routes`] scope is open, by
    /// `(from, to, min_hops, top_n)`: later calls re-quote them at their
    /// amount instead of searching again.
    static ROUTE_REUSE: std::cell::RefCell<Option<Vec<ReusedRoutes>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with chain searches reused: the first search for a pair finds
/// its routes, every later one re-quotes those at the new amount. For
/// sizing, which quotes one exit at many sizes and only needs the routes
/// once. Not nested; a scope inside a scope keeps the outer one.
pub fn reuse_routes<T>(f: impl FnOnce() -> T) -> T {
    let outer = ROUTE_REUSE.with(|c| c.borrow().is_some());
    if !outer {
        ROUTE_REUSE.with(|c| *c.borrow_mut() = Some(Vec::new()));
    }
    let out = f();
    if !outer {
        ROUTE_REUSE.with(|c| *c.borrow_mut() = None);
    }
    out
}

/// [`best_chains`] keeping the search's `top_n` routes.
#[must_use]
pub fn best_chains_n(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    gas: &GasTerms,
    min_hops: usize,
    top_n: usize,
) -> Vec<ChainFound> {
    best_chains_budget(
        book,
        from,
        to,
        amount,
        gas,
        min_hops,
        top_n,
        CHAIN_SEARCH_QUOTES,
    )
}

/// [`best_chains_n`] with its own quote budget: the flow's one search per
/// large sale affords more than an exit solve's many.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn best_chains_budget(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    gas: &GasTerms,
    min_hops: usize,
    top_n: usize,
    max_quotes: u32,
) -> Vec<ChainFound> {
    let key = (from, to, min_hops, top_n);
    let cached = ROUTE_REUSE.with(|c| {
        c.borrow().as_ref().and_then(|v| {
            v.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, routes)| routes.clone())
        })
    });
    if let Some(routes) = cached {
        // The same routes at this amount: each hop quoted exactly, and the
        // ones that cannot take it dropped, best output first.
        let mut out: Vec<ChainFound> = routes
            .into_iter()
            .filter_map(|(mut route, hops, data, exact_out)| {
                let mut x = amount;
                for l in &hops {
                    x = book.get(l.pool)?.quote_exact_in(l.i, l.j, x).ok()?;
                }
                route.amount_out = x;
                Some((route, hops, data, exact_out))
            })
            .collect();
        out.sort_by_key(|(r, _, _, _)| core::cmp::Reverse(r.amount_out));
        return out;
    }
    let found = best_chains_search(book, from, to, amount, gas, min_hops, top_n, max_quotes);
    ROUTE_REUSE.with(|c| {
        if let Some(v) = c.borrow_mut().as_mut() {
            v.push((key, found.clone()));
        }
    });
    found
}

#[allow(clippy::too_many_arguments)]
fn best_chains_search(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    gas: &GasTerms,
    min_hops: usize,
    top_n: usize,
    max_quotes: u32,
) -> Vec<ChainFound> {
    let Some(g) = book.graph() else {
        return Vec::new();
    };
    let max_hops = g.chain_table.hops().min(4);
    if usize::from(max_hops) < min_hops {
        return Vec::new();
    }
    let Ok(found) = search(
        &g.chain_graph,
        &g.chain_table,
        book,
        gas,
        from,
        to,
        amount,
        SearchBudget {
            max_hops,
            // The best route may be shorter than `min_hops` (a direct pool):
            // the next few are kept too.
            top_n,
            max_quotes,
            min_share_bps: 0,
        },
    ) else {
        return Vec::new();
    };
    if found.exhausted {
        tracing::debug!(
            quotes = found.quotes,
            routes = found.routes.len(),
            "chain search hit its quote cap"
        );
    }
    found
        .routes
        .into_iter()
        .filter(|r| r.edges.len() >= min_hops)
        .filter_map(|mut route| {
            route.hop_gas = route.hop_gas.saturating_add(CHAIN_LEG_GAS);
            let (hops, data, exact_out) = chain_data(book, &g.chain_graph, &route.edges)?;
            Some((route, hops, data, exact_out))
        })
        .collect()
}

/// Intermediate tokens tried per two-hop exit.
pub const GRAPH_HUBS: usize = 3;
/// Gas a chain leg (venue 10) costs beyond its pools' `[swap]` figures:
/// pool derivation, decoding and the hop in flight held in transient
/// storage. Fork, cold (`ForkChain` `test_gas_chain*`, 2026-10-07): one V3
/// hop 113,309, two 180,672, three 258,184 along WBTC → WETH → USDC → USDT,
/// so a chained hop costs 67k–78k (under `[swap].univ3`, 82,100) and the
/// first one 31,209 more than that figure. Charged once per chain, it
/// leaves every measured length a little over its cost.
pub const CHAIN_LEG_GAS: u64 = 31_209;

/// Routes a chain search keeps, so one of at least two hops survives a
/// better direct pool.
pub const CHAIN_TOP_N: usize = 4;
/// Hops quoted per chain search: a latency cap on one exit.
pub const CHAIN_SEARCH_QUOTES: u32 = 5_000;

impl GraphRoutes {
    /// The graph over the pools `keep` admits, and its table at `k` hops.
    pub fn build(
        book: &PoolBook,
        keep: impl Fn(PoolId) -> bool,
        k: u8,
    ) -> Result<Self, RouteError> {
        let graph = TokenGraph::build_with(book, &keep)?;
        let table = ZeroTable::build(&graph, book, k)?;
        let chain_graph =
            TokenGraph::build_with(book, |id| keep(id) && book.get(id).is_some_and(chainable))?;
        let chain_table = ZeroTable::build(&chain_graph, book, k)?;
        Ok(Self {
            graph,
            table,
            chain_graph,
            chain_table,
        })
    }

    /// Up to `n` tokens `x` for a two-hop exit `from → x → to` through the
    /// graph's pools, best zero-size rate (on the book's state now) first.
    #[must_use]
    pub fn hubs(
        &self,
        book: &PoolBook,
        from: AssetId,
        to: AssetId,
        n: usize,
    ) -> SmallVec<[AssetId; 4]> {
        let best_into = |x: AssetId| -> Option<Log2> {
            self.graph
                .out_edges(x)
                .filter_map(|id| self.graph.edge(id))
                .filter(|e| e.to == to && matches!(e.via, Via::Pool(_)))
                .filter_map(|e| edge_rate(book, e))
                .max()
        };
        let mut best: HashMap<AssetId, Log2> = HashMap::new();
        for id in self.graph.out_edges(from) {
            let Some(e) = self.graph.edge(id) else {
                continue;
            };
            if e.to == to || e.to == from || !matches!(e.via, Via::Pool(_)) {
                continue;
            }
            let (Some(r1), Some(r2)) = (edge_rate(book, e), best_into(e.to)) else {
                continue;
            };
            let r = r1.saturating_add(r2);
            let cell = best.entry(e.to).or_insert(r);
            *cell = (*cell).max(r);
        }
        let mut ranked: Vec<(Log2, AssetId)> = best.into_iter().map(|(a, r)| (r, a)).collect();
        ranked.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.1 .0.cmp(&y.1 .0)));
        ranked.into_iter().take(n).map(|(_, a)| a).collect()
    }
}

/// The zero-size rate of one edge now, or `None` when it cannot be quoted
/// (a pool without state, an unread unwrap).
fn edge_rate(book: &PoolBook, e: &Edge) -> Option<Log2> {
    match e.via {
        Via::Pool(l) => {
            let pool = book.get(l.pool).filter(|p| p.is_live())?;
            rate_of_rho(pool.rho_at_zero(l.i, l.j).ok()?)
        }
        Via::Unwrap(w) => {
            let u = book.unwrap_of(w).filter(|u| u.is_live())?;
            rate_of_rho(u.rho(book).ok()?)
        }
    }
}

/// Marks an unreachable cell.
const UNREACHED: Log2 = i64::MIN;
const NO_EDGE: EdgeId = EdgeId::MAX;

/// For every `(source, target)`, the best zero-size rate within `K` hops
/// (Q32 log2, an upper bound on any size and any route of at most `K`
/// hops) and the first hop of a route that attains it.
#[derive(Clone, Debug, Default)]
pub struct ZeroTable {
    k: u8,
    n: usize,
    /// Row `target · n + source`.
    best: Vec<Log2>,
    next: Vec<EdgeId>,
}

impl ZeroTable {
    /// Hard cap on `K`.
    pub const MAX_HOPS: u8 = 10;

    /// Hop-limited Bellman-Ford backward from each target: round `r`
    /// relaxes every edge `u → v` against round `r − 1`'s best from `v`, so
    /// after `k` rounds a cell holds the best walk of at most `k` hops.
    /// About targets × `k` × edges additions.
    pub fn build(graph: &TokenGraph, book: &PoolBook, k: u8) -> Result<Self, RouteError> {
        if k == 0 || k > Self::MAX_HOPS {
            return Err(RouteError::BadLeg);
        }
        let n = graph.nodes.len();
        let rated: Vec<(usize, usize, Log2, EdgeId)> = graph
            .edges
            .iter()
            .enumerate()
            .filter_map(|(id, e)| {
                let rate = edge_rate(book, e)?;
                let u = usize::try_from(graph.node(e.from)?).ok()?;
                let v = usize::try_from(graph.node(e.to)?).ok()?;
                Some((u, v, rate, EdgeId::try_from(id).ok()?))
            })
            .collect();
        let cells = n.checked_mul(n).ok_or(RouteError::Math)?;
        let mut best = vec![UNREACHED; cells];
        let mut next = vec![NO_EDGE; cells];
        let mut prev = vec![UNREACHED; n];
        let mut cur = vec![UNREACHED; n];
        let mut hop = vec![NO_EDGE; n];
        for t in 0..n {
            prev.fill(UNREACHED);
            hop.fill(NO_EDGE);
            *prev.get_mut(t).ok_or(RouteError::Math)? = 0;
            for _ in 0..k {
                cur.copy_from_slice(&prev);
                let mut moved = false;
                for &(u, v, rate, id) in &rated {
                    let to_t = *prev.get(v).ok_or(RouteError::Math)?;
                    if to_t == UNREACHED || u == t {
                        continue;
                    }
                    let cand = rate.saturating_add(to_t);
                    let cell = cur.get_mut(u).ok_or(RouteError::Math)?;
                    if cand > *cell {
                        *cell = cand;
                        *hop.get_mut(u).ok_or(RouteError::Math)? = id;
                        moved = true;
                    }
                }
                core::mem::swap(&mut prev, &mut cur);
                if !moved {
                    break;
                }
            }
            let row = t.checked_mul(n).ok_or(RouteError::Math)?;
            let end = row.checked_add(n).ok_or(RouteError::Math)?;
            best.get_mut(row..end)
                .ok_or(RouteError::Math)?
                .copy_from_slice(&prev);
            next.get_mut(row..end)
                .ok_or(RouteError::Math)?
                .copy_from_slice(&hop);
        }
        Ok(Self { k, n, best, next })
    }

    #[inline]
    #[must_use]
    pub fn hops(&self) -> u8 {
        self.k
    }

    fn cell(&self, graph: &TokenGraph, from: AssetId, to: AssetId) -> Option<usize> {
        let s = usize::try_from(graph.node(from)?).ok()?;
        let t = usize::try_from(graph.node(to)?).ok()?;
        t.checked_mul(self.n)?.checked_add(s)
    }

    /// The best zero-size rate from `from` to `to` within [`Self::hops`]
    /// hops, as a Q32 log2 (`0` from a token to itself). `None` when no
    /// such route exists now.
    #[must_use]
    pub fn best(&self, graph: &TokenGraph, from: AssetId, to: AssetId) -> Option<Log2> {
        if from == to {
            return Some(0);
        }
        let v = *self.best.get(self.cell(graph, from, to)?)?;
        (v != UNREACHED).then_some(v)
    }

    /// The first hop of a route attaining [`Self::best`]. Following it hop
    /// by hop gives a route, though not always one of exactly that rate
    /// (round `k`'s choice of first hop was made against round `k − 1`'s
    /// best onward): the table prunes, the search routes.
    #[must_use]
    pub fn first_hop(&self, graph: &TokenGraph, from: AssetId, to: AssetId) -> Option<EdgeId> {
        let e = *self.next.get(self.cell(graph, from, to)?)?;
        (e != NO_EDGE).then_some(e)
    }
}

/// Exact output of one hop for an input amount.
pub trait HopQuoter {
    fn quote(&self, edge: &Edge, amount_in: U256) -> Result<U256, RouteError>;
}

/// The pool's own math on the book's state, and the unwrap's own
/// conversion: what the chain would pay at this state.
impl HopQuoter for PoolBook {
    fn quote(&self, edge: &Edge, amount_in: U256) -> Result<U256, RouteError> {
        match edge.via {
            Via::Pool(l) => {
                let pool = self.get(l.pool).ok_or(RouteError::BadLeg)?;
                if !pool.is_live() {
                    return Err(RouteError::StalePool);
                }
                pool.quote_exact_in(l.i, l.j, amount_in)
            }
            Via::Unwrap(w) => self
                .unwrap_of(w)
                .ok_or(RouteError::BadLeg)?
                .convert(amount_in, self),
        }
    }
}

/// Most hops a route may take.
pub const MAX_ROUTE_HOPS: usize = ZeroTable::MAX_HOPS as usize;

/// One route the search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub edges: SmallVec<[EdgeId; MAX_ROUTE_HOPS]>,
    pub amount_out: U256,
    pub hop_gas: u64,
    /// `amount_out` less the hop gas in output units (floored at zero).
    pub net: U256,
}

/// Search limits.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SearchBudget {
    /// Most hops per route (at most the table's).
    pub max_hops: u8,
    /// Routes kept.
    pub top_n: usize,
    /// Most hops quoted in one search: a latency cap, reported when hit.
    pub max_quotes: u32,
    /// Routes netting less than this share of the best (basis points) are
    /// not kept, and branches that cannot reach it are cut. Without it a
    /// token with fewer than `top_n` routes is never pruned at all.
    pub min_share_bps: u16,
}

/// What a search found, and what it cost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// Best net first.
    pub routes: Vec<Route>,
    /// Hops quoted.
    pub quotes: u32,
    /// Branches cut by the table's bound.
    pub pruned: u32,
    /// The quote budget ran out: the result may miss routes.
    pub exhausted: bool,
}

struct Ctx<'a, Q: HopQuoter> {
    graph: &'a TokenGraph,
    table: &'a ZeroTable,
    quoter: &'a Q,
    gas: &'a GasTerms,
    to: AssetId,
    budget: SearchBudget,
    out: SearchResult,
    path: SmallVec<[EdgeId; MAX_ROUTE_HOPS]>,
    visited: Vec<bool>,
}

impl<Q: HopQuoter> Ctx<'_, Q> {
    /// The net the `top_n`-th route must beat; `None` until there are
    /// `top_n` routes.
    fn bar(&self) -> Option<U256> {
        let nth = (self.out.routes.len() >= self.budget.top_n)
            .then(|| self.out.routes.last().map(|r| r.net))
            .flatten();
        nth.max(self.share_bar())
    }

    /// `min_share_bps` of the best net found so far. The final best can
    /// only be higher, so a route under this bar now is under it at the end.
    fn share_bar(&self) -> Option<U256> {
        let best = self.out.routes.first()?.net;
        crate::solver::mul_div_512(
            best,
            U256::from(self.budget.min_share_bps),
            U256::from(10_000u64),
        )
        .ok()
        .filter(|b| !b.is_zero())
    }

    fn record(&mut self, amount_out: U256, hop_gas: u64) -> Result<(), RouteError> {
        let net = amount_out.saturating_sub(self.gas.cost_in_out(hop_gas)?);
        let route = Route {
            edges: self.path.clone(),
            amount_out,
            hop_gas,
            net,
        };
        let at = self
            .out
            .routes
            .iter()
            .position(|r| r.net < net)
            .unwrap_or(self.out.routes.len());
        self.out.routes.insert(at, route);
        self.out.routes.truncate(self.budget.top_n);
        if let Some(bar) = self.share_bar() {
            self.out.routes.retain(|r| r.net >= bar);
        }
        Ok(())
    }

    fn visit(&mut self, at: AssetId, amount: U256, hop_gas: u64) -> Result<(), RouteError> {
        if at == self.to {
            return self.record(amount, hop_gas);
        }
        if self.path.len() >= usize::from(self.budget.max_hops) {
            return Ok(());
        }
        // Quote every live child, then descend best bound first.
        let mut children: SmallVec<[(Log2, EdgeId, U256); 16]> = SmallVec::new();
        for id in self.graph.out_edges(at) {
            let Some(e) = self.graph.edge(id).copied() else {
                continue;
            };
            let Some(n) = self.graph.node(e.to).and_then(|n| usize::try_from(n).ok()) else {
                continue;
            };
            if self.visited.get(n).copied().unwrap_or(true) {
                continue;
            }
            // No pool twice: a second hop through a pool would be quoted
            // on the state before the first (a three-coin pool).
            if let Via::Pool(l) = e.via {
                if self.path.iter().any(|p| {
                    matches!(self.graph.edge(*p).map(|q| q.via), Some(Via::Pool(m)) if m.pool == l.pool)
                }) {
                    continue;
                }
            }
            let Some(onward) = self.table.best(self.graph, e.to, self.to) else {
                continue;
            };
            if self.out.quotes >= self.budget.max_quotes {
                self.out.exhausted = true;
                return Ok(());
            }
            self.out.quotes = self.out.quotes.saturating_add(1);
            let Ok(got) = self.quoter.quote(&e, amount) else {
                continue;
            };
            let Some(lg) = log2_q32(got) else {
                continue;
            };
            // Optimistic finish: everything onward at the zero-size rate,
            // no more gas. `log2_q32` rounds down by a few units: the margin
            // keeps it optimistic.
            let bound = lg.saturating_add(onward).saturating_add(EDGE_MARGIN);
            children.push((bound, id, got));
        }
        children.sort_unstable_by_key(|c| core::cmp::Reverse(c.0));
        for (bound, id, got) in children {
            // The bar only rises as routes are found: re-check each child.
            if let Some(bar) = self.bar() {
                if log2_q32(bar).is_some_and(|b| bound < b) {
                    self.out.pruned = self.out.pruned.saturating_add(1);
                    continue;
                }
            }
            let Some(e) = self.graph.edge(id).copied() else {
                continue;
            };
            let Some(n) = self.graph.node(e.to).and_then(|n| usize::try_from(n).ok()) else {
                continue;
            };
            if let Some(v) = self.visited.get_mut(n) {
                *v = true;
            }
            self.path.push(id);
            let gas = hop_gas.saturating_add(e.hop_gas);
            let r = self.visit(e.to, got, gas);
            self.path.pop();
            if let Some(v) = self.visited.get_mut(n) {
                *v = false;
            }
            r?;
            if self.out.exhausted {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// The best `budget.top_n` routes from `amount` of `from` into `to`, by
/// exact output net of hop gas: a depth-first branch and bound over simple
/// paths (no token twice) of at most `budget.max_hops` hops, each hop
/// quoted by `quoter` on the amount that reaches it. A branch is cut when
/// its amount so far times the table's best rate onward cannot beat the
/// `top_n`-th net already found: the table is an upper bound, so no route
/// that would have made the list is cut. Routes are quoted independently:
/// two that share a pool are not yet quoted against each other.
#[allow(clippy::too_many_arguments)]
pub fn search<Q: HopQuoter>(
    graph: &TokenGraph,
    table: &ZeroTable,
    quoter: &Q,
    gas: &GasTerms,
    from: AssetId,
    to: AssetId,
    amount: U256,
    budget: SearchBudget,
) -> Result<SearchResult, RouteError> {
    if budget.top_n == 0
        || budget.max_hops == 0
        || budget.max_hops > table.hops()
        || budget.min_share_bps > 10_000
    {
        return Err(RouteError::BadLeg);
    }
    let Some(start) = graph.node(from).and_then(|n| usize::try_from(n).ok()) else {
        return Ok(SearchResult::default());
    };
    let mut ctx = Ctx {
        graph,
        table,
        quoter,
        gas,
        to,
        budget,
        out: SearchResult::default(),
        path: SmallVec::new(),
        visited: vec![false; graph.nodes.len()],
    };
    if let Some(v) = ctx.visited.get_mut(start) {
        *v = true;
    }
    if from != to {
        ctx.visit(from, amount, 0)?;
    }
    Ok(ctx.out)
}

/// The route paying most for `amount` of `from` into `to` in
/// `min_hops..=max_hops` hops, by output alone, and the hops it quoted. A
/// label search by hop count: layer `h` holds, per token, the most of it
/// any simple path of `h` hops delivers (no token and no pool twice); each
/// token in a layer is left through every live edge, quoted on what
/// reaches it, unless even the table's best rate from it cannot beat the
/// output already found. Output grows with input, so a token's largest
/// label at a depth stands for all of them: about edges × hops quotes at
/// most, far fewer once a route is found. Exact up to three hops. A
/// four-hop route that reaches its second token with less than the
/// largest label there and goes on to a token on that label's path is not
/// seen: that label blocks the continuation and the smaller one was
/// dropped. The per-slice step of a flow (`exact::flow_split`), where
/// [`search`]'s top routes for one slice are too alike to spread a sale.
#[allow(clippy::too_many_arguments)]
pub fn best_path<Q: HopQuoter>(
    graph: &TokenGraph,
    table: &ZeroTable,
    quoter: &Q,
    from: AssetId,
    to: AssetId,
    amount: U256,
    min_hops: usize,
    max_hops: u8,
) -> (Option<Route>, u32) {
    let (layers, best, quotes) =
        label_layers(graph, table, quoter, from, to, amount, min_hops, max_hops);
    let Some((_, hops)) = best else {
        return (None, quotes);
    };
    (route_from(graph, &layers, to, hops), quotes)
}

/// [`best_path`]'s search, returning the best route of every hop count in
/// `min_hops..=max_hops` that reaches `to`, best output first. The one
/// search finds them all: each layer's label at the target is the most any
/// path of that many hops delivers, as far as the search looked (the bound
/// cuts branches that cannot beat the best found, so a shorter or longer
/// route than the best may be missing or beaten by one it did not
/// quote). The best of them is [`best_path`]'s route exactly. For a flow's
/// candidates: routes worth re-quoting on later slices without another
/// search.
#[allow(clippy::too_many_arguments)]
pub fn best_paths_by_hops<Q: HopQuoter>(
    graph: &TokenGraph,
    table: &ZeroTable,
    quoter: &Q,
    from: AssetId,
    to: AssetId,
    amount: U256,
    min_hops: usize,
    max_hops: u8,
) -> (SmallVec<[Route; MAX_ROUTE_HOPS]>, u32) {
    let (layers, _, quotes) =
        label_layers(graph, table, quoter, from, to, amount, min_hops, max_hops);
    let mut out: SmallVec<[Route; MAX_ROUTE_HOPS]> = SmallVec::new();
    for hops in 1..=max_hops {
        if usize::from(hops) < min_hops {
            continue;
        }
        if let Some(r) = route_from(graph, &layers, to, hops) {
            out.push(r);
        }
    }
    // Best first; equal outputs keep the shorter route first, as
    // `best_path` does.
    out.sort_by_key(|r| core::cmp::Reverse(r.amount_out));
    (out, quotes)
}

/// One layer's cells: the amount at a token, the edge that brought it and
/// the token index it came from (`u32::MAX` for the start).
type Layer = Vec<(U256, EdgeId, u32)>;

/// The route the label at `to` in layer `hops` records, walked back
/// through the layers; `None` when nothing reached `to` in that many hops.
fn route_from(graph: &TokenGraph, layers: &[Layer], to: AssetId, hops: u8) -> Option<Route> {
    let mut k = graph.node(to).and_then(|n| usize::try_from(n).ok())?;
    let amount_out = layers.get(usize::from(hops))?.get(k)?.0;
    if amount_out.is_zero() {
        return None;
    }
    let mut edges: SmallVec<[EdgeId; MAX_ROUTE_HOPS]> = SmallVec::new();
    let mut hop_gas = 0u64;
    let mut layer = usize::from(hops);
    while layer > 0 {
        let &(_, id, prev) = layers.get(layer)?.get(k)?;
        let e = graph.edge(id)?;
        hop_gas = hop_gas.saturating_add(e.hop_gas);
        edges.push(id);
        k = usize::try_from(prev).ok()?;
        layer = layer.saturating_sub(1);
    }
    edges.reverse();
    Some(Route {
        edges,
        amount_out,
        hop_gas,
        net: amount_out,
    })
}

/// The label search of [`best_path`]: its layers, the best `(output,
/// hops)` at `to` within `min_hops..=max_hops`, and the hops quoted.
#[allow(clippy::too_many_arguments)]
fn label_layers<Q: HopQuoter>(
    graph: &TokenGraph,
    table: &ZeroTable,
    quoter: &Q,
    from: AssetId,
    to: AssetId,
    amount: U256,
    min_hops: usize,
    max_hops: u8,
) -> (Vec<Layer>, Option<(U256, u8)>, u32) {
    let n = graph.nodes.len();
    let Some(start) = graph.node(from).and_then(|n| usize::try_from(n).ok()) else {
        return (Vec::new(), None, 0);
    };
    if from == to || amount.is_zero() || max_hops == 0 || max_hops > table.hops() {
        return (Vec::new(), None, 0);
    }
    // Per layer and token: the amount there, the edge that brought it and
    // the token index it came from (`u32::MAX` for the start).
    let mut layers: Vec<Layer> =
        vec![vec![(U256::ZERO, NO_EDGE, u32::MAX); n]; usize::from(max_hops).saturating_add(1)];
    if let Some(cell) = layers.first_mut().and_then(|l| l.get_mut(start)) {
        cell.0 = amount;
    }
    let mut best: Option<(U256, u8)> = None;
    let mut quotes = 0u32;
    for h in 0..usize::from(max_hops) {
        let (done, rest) = layers.split_at_mut(h.saturating_add(1));
        let Some(cur) = done.get(h) else { break };
        let Some(next) = rest.first_mut() else { break };
        for (u, &(have, _, _)) in cur.iter().enumerate() {
            if have.is_zero() {
                continue;
            }
            let Some(&at) = graph.nodes.get(u) else {
                continue;
            };
            if at == to {
                continue;
            }
            // Tokens and pools on this label's path: neither twice.
            let mut on_path: SmallVec<[u32; MAX_ROUTE_HOPS]> = SmallVec::new();
            let mut on_pools: SmallVec<[PoolId; MAX_ROUTE_HOPS]> = SmallVec::new();
            let (mut k, mut layer) = (u, h);
            while let Ok(ku) = u32::try_from(k) {
                on_path.push(ku);
                let Some(&(_, id, prev)) = done.get(layer).and_then(|l| l.get(k)) else {
                    break;
                };
                if prev == u32::MAX || layer == 0 {
                    break;
                }
                if let Some(Via::Pool(l)) = graph.edge(id).map(|e| e.via) {
                    on_pools.push(l.pool);
                }
                let Ok(p) = usize::try_from(prev) else { break };
                k = p;
                layer = layer.saturating_sub(1);
            }
            // Worth leaving at all only while the best found so far could
            // still be beaten at the table's rate from here (an upper
            // bound at zero size). `log2_q32` rounds down by a few units:
            // the margin keeps the bound optimistic.
            let Some(from_here) = table.best(graph, at, to) else {
                continue;
            };
            let cut = match (best, log2_q32(have)) {
                (Some((b, _)), Some(lh)) => log2_q32(b).is_some_and(|lb| {
                    lh.saturating_add(from_here).saturating_add(EDGE_MARGIN) < lb
                }),
                _ => false,
            };
            if cut {
                continue;
            }
            for id in graph.out_edges(at) {
                let Some(e) = graph.edge(id).copied() else {
                    continue;
                };
                let Some(v) = graph.node(e.to) else {
                    continue;
                };
                if on_path.contains(&v) {
                    continue;
                }
                if let Via::Pool(l) = e.via {
                    if on_pools.contains(&l.pool) {
                        continue;
                    }
                }
                if table.best(graph, e.to, to).is_none() {
                    continue;
                }
                quotes = quotes.saturating_add(1);
                let Ok(got) = quoter.quote(&e, have) else {
                    continue;
                };
                let Ok(vi) = usize::try_from(v) else { continue };
                let Some(cell) = next.get_mut(vi) else {
                    continue;
                };
                if got > cell.0 {
                    let Ok(ui) = u32::try_from(u) else { continue };
                    *cell = (got, id, ui);
                    if e.to == to
                        && h.saturating_add(1) >= min_hops
                        && best.is_none_or(|(b, _)| got > b)
                    {
                        best = Some((got, u8::try_from(h.saturating_add(1)).unwrap_or(max_hops)));
                    }
                }
            }
        }
    }
    (layers, best, quotes)
}

/// [`best_path`] on the book's chain graph as a chain leg: the route, its
/// hops, the leg's data and whether it can run exact-output. `None` when
/// no route of at least `min_hops` pays anything.
#[must_use]
pub fn best_slice_chain(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    min_hops: usize,
) -> (Option<ChainFound>, u32) {
    let (found, quotes) = best_slice_chains(book, from, to, amount, min_hops);
    (found.into_iter().next(), quotes)
}

/// [`best_paths_by_hops`] on the book's chain graph as chain legs, best
/// first: one search, a candidate per hop count. The first is
/// [`best_slice_chain`]'s.
#[must_use]
pub fn best_slice_chains(
    book: &PoolBook,
    from: AssetId,
    to: AssetId,
    amount: U256,
    min_hops: usize,
) -> (Vec<ChainFound>, u32) {
    let Some(g) = book.graph() else {
        return (Vec::new(), 0);
    };
    let max_hops = g.chain_table.hops().min(4);
    let (routes, quotes) = best_paths_by_hops(
        &g.chain_graph,
        &g.chain_table,
        book,
        from,
        to,
        amount,
        min_hops,
        max_hops,
    );
    let found = routes
        .into_iter()
        .filter_map(|mut route| {
            route.hop_gas = route.hop_gas.saturating_add(CHAIN_LEG_GAS);
            let (hops, data, exact_out) = chain_data(book, &g.chain_graph, &route.edges)?;
            Some((route, hops, data, exact_out))
        })
        .collect();
    (found, quotes)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::too_many_arguments
)]
mod tests {
    use super::*;
    use crate::fixtures::{e18, tok, v2, HOP_GAS};
    use crate::solver::{Pool, Unwrap, UnwrapKind, UnwrapRate};
    use alloy_primitives::Address;

    const GAS: GasTerms = GasTerms {
        base_fee_wei: 10_000_000_000,
        priority_fee_wei: 0,
        out_per_eth: U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]),
    };

    fn a(n: u16) -> AssetId {
        AssetId(n)
    }

    /// A chain over V3 pools names each hop's kind by the factory that
    /// deployed the pool, with the fee tier as the param: 0 Uniswap, 5
    /// SushiSwap, 6 PancakeSwap (`liq_wire::wire`); all three buy an exact
    /// output. Oracle: the venue-10 layout written out byte by byte —
    /// hops, then per hop kind and 3-byte fee, then the intermediate tokens.
    #[test]
    fn a_chain_names_each_v3_hop_by_its_factory() {
        use crate::fixtures::{v3, SQRT_ONE};
        use crate::solver::PoolState;
        let mk = |n: u64, a0: u16, a1: u16, fee: u32, spacing: i32, factory: u8| {
            // Ticks are multiples of the pool's spacing.
            let edge = 887_220 / spacing * spacing;
            let mut p = v3(
                n,
                fee,
                spacing,
                SQRT_ONE,
                &[(-edge, edge, 1_000_000_000_000_000_000)],
            );
            p.assets = smallvec::SmallVec::from_slice(&[a(a0), a(a1)]);
            p.tokens = smallvec::SmallVec::from_slice(&[tok(u64::from(a0)), tok(u64::from(a1))]);
            if let PoolState::V3(s) = &mut p.state {
                s.factory = factory;
            }
            p
        };
        // t0 -> t1 on Uniswap 0.30 %, t1 -> t2 on Sushi 0.30 %, t2 -> t3 on
        // Pancake 0.25 %.
        let book = book_of(
            4,
            vec![
                mk(1, 0, 1, 3000, 60, 0),
                mk(2, 1, 2, 3000, 60, 1),
                mk(3, 2, 3, 2500, 50, 2),
            ],
        );
        let g = TokenGraph::build(&book).unwrap();
        let edge = |from: u16, to: u16| {
            g.edges()
                .iter()
                .position(|e| e.from == a(from) && e.to == a(to))
                .map(|i| EdgeId::try_from(i).unwrap())
                .unwrap()
        };
        let (_, data, exact_out) =
            chain_data(&book, &g, &[edge(0, 1), edge(1, 2), edge(2, 3)]).unwrap();
        let mut want = vec![3u8];
        want.extend_from_slice(&[0, 0x00, 0x0b, 0xb8]); // Uniswap, 3000
        want.extend_from_slice(&[5, 0x00, 0x0b, 0xb8]); // Sushi, 3000
        want.extend_from_slice(&[6, 0x00, 0x09, 0xc4]); // Pancake, 2500
        want.extend_from_slice(tok(1).as_slice());
        want.extend_from_slice(tok(2).as_slice());
        assert_eq!(data, want);
        assert!(exact_out, "V3 forks buy an exact output");
    }

    fn book_of(n_tokens: u16, pools: Vec<Pool>) -> PoolBook {
        let assets: HashMap<Address, AssetId> =
            (0..n_tokens).map(|n| (tok(u64::from(n)), a(n))).collect();
        let mut b = PoolBook::new(assets, None, HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    /// A V2 pair between tokens `x` and `y` with these reserves.
    fn pair(id: u64, x: u16, y: u16, rx: U256, ry: U256) -> Pool {
        let mut p = v2(id, rx, ry);
        p.assets = SmallVec::from_slice(&[a(x), a(y)]);
        p.tokens = SmallVec::from_slice(&[tok(u64::from(x)), tok(u64::from(y))]);
        p
    }

    /// Oracle: `log2` by 60-digit decimal arithmetic (Python `decimal`),
    /// floored; ours is within a few units of 2^-32 below.
    #[test]
    fn log2_q32_matches_a_high_precision_log() {
        for k in [0usize, 1, 63, 64, 96, 200, 255] {
            assert_eq!(
                log2_q32(U256::ONE << k),
                Some(i64::try_from(k).unwrap() << 32),
                "2^{k}"
            );
        }
        for (x, want) in [
            (U256::from(3u64), 6_807_362_105i64),
            (U256::from(1_000_000_000_000_000_000u64), 256_816_305_489),
            (U256::from(123_456_789u64), 115_446_276_791),
            (
                (U256::ONE << 200usize) + U256::from(12_345u64),
                858_993_459_200,
            ),
            (
                (U256::ONE << 96usize) * U256::from(3u64) / U256::from(2u64),
                414_829_255_225,
            ),
        ] {
            let got = log2_q32(x).unwrap();
            assert!(got <= want && want - got <= 4, "{x}: {got} vs {want}");
        }
        assert_eq!(log2_q32(U256::ZERO), None);
    }

    /// Each two-coin pool is two edges; an unwrap is one, one way.
    #[test]
    fn the_graph_has_both_directions_of_a_pool_and_one_of_an_unwrap() {
        let mut b = book_of(3, vec![pair(1, 0, 1, e18(10), e18(20))]);
        b.add_unwrap(Unwrap {
            kind: UnwrapKind::Erc4626,
            wrapper: a(2),
            wrapper_token: tok(2),
            into: a(0),
            into_token: tok(0),
            rate: UnwrapRate::Linear {
                assets_per_scale: e18(11) / U256::from(10u64),
                max_into: None,
            },
            scale: e18(1),
            read_block: 1,
            gas: 60_000,
            expiry_gas: 0,
            cash_capped: false,
        });
        let g = TokenGraph::build(&b).unwrap();
        assert_eq!(g.edges().len(), 3);
        let out = |x| -> Vec<AssetId> { g.out_edges(x).map(|e| g.edge(e).unwrap().to).collect() };
        assert_eq!(out(a(0)), [a(1)]);
        assert_eq!(out(a(1)), [a(0)]);
        assert_eq!(out(a(2)), [a(0)]);
        // The unwrap's rate (1.1) is its edge's zero-size rate:
        // log2(1.1) · 2^32 = 590_573_137.59… (Python `decimal`). The table
        // is never below it (it is an upper bound) and above it only by
        // the margin, less rounding.
        let t = ZeroTable::build(&g, &b, 2).unwrap();
        let unwrap = t.best(&g, a(2), a(0)).unwrap();
        assert!(
            (590_573_138..=590_573_138 + 16).contains(&unwrap),
            "{unwrap}"
        );
        // Two hops: unwrap, then the pair at 2 per 1 less 0.3 %.
        assert!(t.best(&g, a(2), a(1)).unwrap() > unwrap);
        assert_eq!(t.best(&g, a(1), a(2)), None, "an unwrap is one way");
    }

    /// Deterministic pseudo-random books: pairs between random tokens
    /// (parallel pairs included) at reserves across six decades.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, m: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % m
        }
    }

    fn random_book(seed: u64, tokens: u16, pairs: u64) -> PoolBook {
        let mut r = Lcg(seed);
        let mut pools = Vec::new();
        for id in 0..pairs {
            let x = u16::try_from(r.next(u64::from(tokens))).unwrap();
            let mut y = u16::try_from(r.next(u64::from(tokens))).unwrap();
            if y == x {
                y = (x + 1) % tokens;
            }
            let rx = e18(1 + r.next(9)) * U256::from(10u64).pow(U256::from(r.next(6)));
            let ry = e18(1 + r.next(9)) * U256::from(10u64).pow(U256::from(r.next(6)));
            pools.push(pair(100 + id, x, y, rx, ry));
        }
        book_of(tokens, pools)
    }

    /// Oracle for the table: every walk of at most `k` hops from `s` that
    /// ends on reaching `t`, enumerated; the best sum of edge rates.
    fn brute_best(g: &TokenGraph, b: &PoolBook, s: AssetId, t: AssetId, k: u8) -> Option<Log2> {
        fn go(
            g: &TokenGraph,
            b: &PoolBook,
            at: AssetId,
            t: AssetId,
            left: u8,
            acc: Log2,
            best: &mut Option<Log2>,
        ) {
            if at == t {
                *best = Some(best.map_or(acc, |x| x.max(acc)));
                return;
            }
            if left == 0 {
                return;
            }
            for id in g.out_edges(at) {
                let e = g.edge(id).unwrap();
                if let Some(r) = edge_rate(b, e) {
                    go(g, b, e.to, t, left - 1, acc + r, best);
                }
            }
        }
        let mut best = None;
        go(g, b, s, t, k, 0, &mut best);
        best
    }

    #[test]
    fn the_table_is_the_best_walk_of_at_most_k_hops() {
        for seed in 1..25u64 {
            let b = random_book(seed, 6, 12);
            let g = TokenGraph::build(&b).unwrap();
            for k in [1u8, 2, 4] {
                let t = ZeroTable::build(&g, &b, k).unwrap();
                for &s in g.nodes() {
                    for &d in g.nodes() {
                        let want = if s == d {
                            Some(0)
                        } else {
                            brute_best(&g, &b, s, d, k)
                        };
                        assert_eq!(t.best(&g, s, d), want, "seed {seed} k {k} {s:?}->{d:?}");
                    }
                }
            }
        }
    }

    /// Oracle for the search: every simple path of at most `max_hops` hops,
    /// quoted hop by hop on the pools' own math, ranked by net.
    fn brute_routes(
        g: &TokenGraph,
        b: &PoolBook,
        from: AssetId,
        to: AssetId,
        amount: U256,
        max_hops: u8,
    ) -> Vec<U256> {
        fn go(
            g: &TokenGraph,
            b: &PoolBook,
            at: AssetId,
            to: AssetId,
            amount: U256,
            left: u8,
            gas: u64,
            seen: &mut Vec<AssetId>,
            out: &mut Vec<U256>,
        ) {
            if at == to {
                out.push(amount.saturating_sub(GAS.cost_in_out(gas).unwrap()));
                return;
            }
            if left == 0 {
                return;
            }
            for id in g.out_edges(at) {
                let e = *g.edge(id).unwrap();
                if seen.contains(&e.to) {
                    continue;
                }
                let Ok(got) = b.quote(&e, amount) else {
                    continue;
                };
                if got.is_zero() {
                    continue;
                }
                seen.push(e.to);
                go(g, b, e.to, to, got, left - 1, gas + e.hop_gas, seen, out);
                seen.pop();
            }
        }
        let mut out = Vec::new();
        go(
            g,
            b,
            from,
            to,
            amount,
            max_hops,
            0,
            &mut vec![from],
            &mut out,
        );
        out.sort_unstable_by(|x, y| y.cmp(x));
        out
    }

    /// Every simple path's output by itself (no gas), with its hop count.
    fn brute_outputs(
        g: &TokenGraph,
        b: &PoolBook,
        from: AssetId,
        to: AssetId,
        amount: U256,
        max_hops: u8,
    ) -> Vec<(U256, usize)> {
        fn go(
            g: &TokenGraph,
            b: &PoolBook,
            at: AssetId,
            to: AssetId,
            amount: U256,
            left: u8,
            seen: &mut Vec<AssetId>,
            out: &mut Vec<(U256, usize)>,
        ) {
            if at == to {
                out.push((amount, seen.len() - 1));
                return;
            }
            if left == 0 {
                return;
            }
            for id in g.out_edges(at) {
                let e = *g.edge(id).unwrap();
                if seen.contains(&e.to) {
                    continue;
                }
                let Ok(got) = b.quote(&e, amount) else {
                    continue;
                };
                if got.is_zero() {
                    continue;
                }
                seen.push(e.to);
                go(g, b, e.to, to, got, left - 1, seen, out);
                seen.pop();
            }
        }
        let mut out = Vec::new();
        go(g, b, from, to, amount, max_hops, &mut vec![from], &mut out);
        out
    }

    /// The per-hop routes of one search: each is a real route of its hop
    /// count whose output is its hops quoted in order, the hop counts are
    /// distinct and within range, and the best of them is `best_path`'s
    /// route to the wei (the same search, read back per layer). Oracle: the
    /// book's own hop quotes and `best_path`.
    #[test]
    fn the_per_hop_routes_are_real_and_hold_the_best() {
        let mut routes_seen = 0usize;
        for seed in 1..40u64 {
            let b = random_book(seed, 7, 16);
            let g = TokenGraph::build(&b).unwrap();
            let table = ZeroTable::build(&g, &b, 4).unwrap();
            for amount in [e18(1), e18(1_000)] {
                for (from, to) in [(a(0), a(1)), (a(2), a(5)), (a(6), a(3))] {
                    for (min_hops, max_hops) in [(1usize, 3u8), (2, 4)] {
                        let (by_hops, _) = best_paths_by_hops(
                            &g, &table, &b, from, to, amount, min_hops, max_hops,
                        );
                        let (best, _) =
                            best_path(&g, &table, &b, from, to, amount, min_hops, max_hops);
                        assert_eq!(
                            by_hops.first().map(|r| (r.amount_out, r.edges.clone())),
                            best.map(|r| (r.amount_out, r.edges)),
                            "seed {seed} {from:?}->{to:?}"
                        );
                        let mut lens: Vec<usize> = by_hops.iter().map(|r| r.edges.len()).collect();
                        lens.sort_unstable();
                        lens.dedup();
                        assert_eq!(lens.len(), by_hops.len(), "one route per hop count");
                        for r in &by_hops {
                            routes_seen += 1;
                            assert!(
                                r.edges.len() >= min_hops && r.edges.len() <= usize::from(max_hops)
                            );
                            let (mut amt, mut at, mut gas) = (amount, from, 0u64);
                            for id in &r.edges {
                                let e = g.edge(*id).unwrap();
                                assert_eq!(e.from, at);
                                amt = b.quote(e, amt).unwrap();
                                at = e.to;
                                gas += e.hop_gas;
                            }
                            assert_eq!((at, amt, gas), (to, r.amount_out, r.hop_gas));
                        }
                    }
                }
            }
        }
        assert!(routes_seen > 200, "{routes_seen}");
    }

    /// Up to three hops the label search returns the output of the best
    /// simple path of the allowed hop counts, exactly as enumeration finds
    /// it; at four it returns at least enumeration's best of three and at
    /// most its best of four (the dominance it documents). Its route
    /// re-quotes to its output, and the table's bound cuts some of the
    /// work.
    #[test]
    fn the_label_search_finds_the_best_path_of_exhaustive_enumeration() {
        let mut saved = 0u64;
        let mut four_better = 0u32;
        for seed in 1..40u64 {
            let b = random_book(seed, 7, 16);
            let g = TokenGraph::build(&b).unwrap();
            let table = ZeroTable::build(&g, &b, 4).unwrap();
            for amount in [e18(1), e18(1_000), e18(1_000_000)] {
                for (from, to) in [(a(0), a(1)), (a(2), a(5)), (a(6), a(3))] {
                    for (min_hops, max_hops) in [(1usize, 3u8), (2, 3), (1, 4), (2, 4)] {
                        let (got, quotes) =
                            best_path(&g, &table, &b, from, to, amount, min_hops, max_hops);
                        let all = brute_outputs(&g, &b, from, to, amount, max_hops);
                        let best_of = |hops: usize| {
                            all.iter()
                                .filter(|(_, h)| *h >= min_hops && *h <= hops)
                                .map(|(o, _)| *o)
                                .max()
                        };
                        let out = got.as_ref().map(|r| r.amount_out);
                        let tag = format!(
                            "seed {seed} {from:?}->{to:?} at {amount} {min_hops}..={max_hops}"
                        );
                        if max_hops == 3 {
                            assert_eq!(out, best_of(3), "{tag}");
                        } else {
                            assert!(out >= best_of(3), "{tag}");
                            assert!(out <= best_of(4), "{tag}");
                            if out > best_of(3) {
                                four_better += 1;
                            }
                        }
                        let Some(r) = got else { continue };
                        assert!(
                            r.edges.len() >= min_hops && r.edges.len() <= usize::from(max_hops)
                        );
                        let (mut amt, mut at, mut gas) = (amount, from, 0u64);
                        for id in &r.edges {
                            let e = g.edge(*id).unwrap();
                            assert_eq!(e.from, at);
                            amt = b.quote(e, amt).unwrap();
                            at = e.to;
                            gas += e.hop_gas;
                        }
                        assert_eq!((at, amt, gas), (to, r.amount_out, r.hop_gas));
                        // Fewer quotes than the edges every layer could take.
                        let ceiling = g.edges().len() as u64 * u64::from(max_hops);
                        assert!(u64::from(quotes) <= ceiling);
                        saved += ceiling - u64::from(quotes);
                    }
                }
            }
        }
        assert!(saved > 0, "the bound never cut anything");
        assert!(
            four_better > 0,
            "no four-hop route ever beat three: the fourth layer is untested"
        );
    }

    /// The branch and bound returns exactly the top `n` nets of all simple
    /// paths (those within the share of the best, when one is set), at
    /// every size: the table's bound never cuts one of them.
    #[test]
    fn the_search_finds_the_same_top_routes_as_exhaustive_enumeration() {
        let mut pruned = 0u32;
        for seed in 1..40u64 {
            let b = random_book(seed, 7, 16);
            let g = TokenGraph::build(&b).unwrap();
            let table = ZeroTable::build(&g, &b, 5).unwrap();
            for amount in [e18(1), e18(1_000), e18(1_000_000)] {
                for ((from, to), share) in [(a(0), a(1)), (a(2), a(5)), (a(6), a(3))]
                    .into_iter()
                    .flat_map(|p| [(p, 0u16), (p, 5_000)])
                {
                    let budget = SearchBudget {
                        max_hops: 5,
                        top_n: 4,
                        max_quotes: 1_000_000,
                        min_share_bps: share,
                    };
                    let got = search(&g, &table, &b, &GAS, from, to, amount, budget).unwrap();
                    assert!(!got.exhausted);
                    pruned += got.pruned;
                    let want = brute_routes(&g, &b, from, to, amount, 5);
                    let nets: Vec<U256> = got.routes.iter().map(|r| r.net).collect();
                    let floor = want
                        .first()
                        .map(|b| *b * U256::from(share) / U256::from(10_000u64))
                        .filter(|f| !f.is_zero());
                    let top: Vec<U256> = want
                        .iter()
                        .copied()
                        .take(4)
                        .filter(|n| floor.is_none_or(|f| *n >= f))
                        .collect();
                    assert_eq!(nets, top, "seed {seed} {from:?}->{to:?} at {amount}");
                    // Each route's output is its own path, re-quoted.
                    for r in &got.routes {
                        let (mut amt, mut at) = (amount, from);
                        for id in &r.edges {
                            let e = g.edge(*id).unwrap();
                            assert_eq!(e.from, at);
                            amt = b.quote(e, amt).unwrap();
                            at = e.to;
                        }
                        assert_eq!((at, amt), (to, r.amount_out));
                    }
                }
            }
        }
        assert!(
            pruned > 0,
            "the bound never cut anything: the test proves nothing"
        );
    }

    /// Depth by hand: token 0 at $1 (18 decimals) in a 1 000 / 2 000 V2
    /// pair. Selling `x` of it into reserve `R` keeps `R / (R + γx)` of the
    /// marginal rate (γ = 0.997), so 2 % is reached at `x = R / (49 γ)` and
    /// the constant-product-equivalent depth is `R / γ` = $1 003.0; the
    /// other side, at token 1's price `1 / (2 · 0.997)` ≈ $0.5015, is about
    /// $1 006, so the depth is the lesser. A pair worth $0.50 is below a
    /// $100 floor: dust, which does not price token 2. A pair between two
    /// unpriced tokens has no depth at all.
    #[test]
    fn depth_is_priced_through_deep_pools_only() {
        let b = book_of(
            5,
            vec![
                pair(1, 0, 1, e18(1_000), e18(2_000)),
                pair(2, 1, 2, e18(1), e18(1)),
                pair(3, 3, 4, e18(5_000), e18(5_000)),
            ],
        );
        let usd1 = raw_value(U256::from(10u64).pow(U256::from(27u64)), 18).unwrap();
        assert_eq!(usd1, e18(1));
        let seed: HashMap<AssetId, RawValue> = [(a(0), usd1)].into_iter().collect();
        let (depth, prices) = pool_depths(&b, &seed, e18(100), 4);
        let d0 = depth[0].unwrap();
        // $1 003.009, to the bisection's resolution (2^-14 of the bracket).
        let want = e18(1_000) * U256::from(1_000u64) / U256::from(997u64);
        let tol = want / U256::from(1_000u64);
        assert!(d0 <= want && want - d0 < tol, "{d0} vs {want}");
        let p1 = prices[&a(1)];
        assert!(p1 > e18(1) * U256::from(5_014u64) / U256::from(10_000u64));
        assert!(p1 < e18(1) * U256::from(5_016u64) / U256::from(10_000u64));
        assert!(depth[1].is_none_or(|d| d < e18(100)), "the dust pair");
        assert!(!prices.contains_key(&a(2)), "dust prices nothing");
        assert_eq!(depth[2], None, "an unpriced island");
        // The filtered graph keeps only the deep pair.
        let g = TokenGraph::build_with(&b, |id| {
            depth[usize::try_from(id.0).unwrap()].is_some_and(|d| d >= e18(100))
        })
        .unwrap();
        assert_eq!(g.edges().len(), 2);
        assert_eq!(
            raw_value(U256::from(10u64).pow(U256::from(27u64)), 6),
            Some(e18(1_000_000_000_000))
        );
    }

    /// Token 1 trades at $0.50 in two deep pairs against token 0 ($1) and
    /// at $500 in one equally deep pair: the median of what its pools offer
    /// is $0.50 (to the pairs' fees). A pair holding token 1 only (its
    /// other side empty) cannot be sold back into and offers nothing.
    #[test]
    fn one_mispriced_pool_is_outvoted() {
        let b = book_of(
            3,
            vec![
                pair(1, 0, 1, e18(1_000_000), e18(2_000_000)),
                pair(2, 0, 1, e18(1_000_000), e18(2_000_000)),
                pair(3, 0, 1, e18(1_000_000), e18(2_000)),
                pair(4, 0, 2, e18(1_000_000), e18(1)),
                pair(5, 0, 2, U256::from(1u64), e18(1_000_000)),
            ],
        );
        let seed: HashMap<AssetId, RawValue> = [(a(0), e18(1))].into_iter().collect();
        let (_, prices) = pool_depths(&b, &seed, e18(10_000), 4);
        let p1 = prices[&a(1)];
        assert!(
            p1 > e18(1) / U256::from(2u64) && p1 < e18(1) * U256::from(51u64) / U256::from(100u64),
            "{p1}"
        );
        // Token 2: $1M of token 0 against 1 token ($1M each, deep both
        // ways) and an empty-sided pair that would say it is worth a
        // millionth of a cent: only the first offers a price.
        let p2 = prices[&a(2)];
        assert!(p2 > e18(900_000) && p2 < e18(1_100_000), "{p2}");
    }

    /// A pool that pays under fair both ways is a pool with a fee, not a
    /// pool off the market: an 8 % Curve pool between two $1 coins (both
    /// directions at 92 % of the fair rate the two 0.3 % pairs set) keeps
    /// its depth, while the 1 000x pool below, which pays over fair one
    /// way, has none. Oracle: the band's own arithmetic (8 % > 5 % under;
    /// nothing over).
    #[test]
    fn a_pool_with_a_wide_spread_is_kept() {
        // Shallower than the pairs: prices come from spot rates with their
        // fees in, and the deepest offer sets the median.
        let mut fat = crate::fixtures::curve(3, &[e18(30_000), e18(30_000)], 20_000, 800_000_000);
        fat.assets = SmallVec::from_slice(&[a(0), a(1)]);
        fat.tokens = SmallVec::from_slice(&[tok(0), tok(1)]);
        let b = book_of(
            2,
            vec![
                pair(1, 0, 1, e18(1_000_000), e18(1_000_000)),
                pair(2, 0, 1, e18(1_000_000), e18(1_000_000)),
                fat,
            ],
        );
        let seed: HashMap<AssetId, RawValue> = [(a(0), e18(1))].into_iter().collect();
        let t = pool_depths_traced(&b, &seed, e18(10_000), 4, None);
        assert!(t.trusted.contains(&a(1)));
        let bps = fair_bps(&b.pools()[2], 0, 1, e18(1), t.prices[&a(1)]).unwrap();
        assert!(bps < 9_500 && bps > 9_000, "8 % under fair: {bps} bps");
        assert!(
            t.depths[2].is_some(),
            "under fair is a fee, not off the market"
        );
        assert_eq!(t.why[2], None);
    }

    /// A deep pool at a price 1 000 times the market's is disregarded: two
    /// pairs agree token 1 is worth $0.50 (trusted), and the third, deep
    /// as it is, sits far off that fair rate. A coin
    /// priced by one pool only is not trusted, and prices nothing further.
    #[test]
    fn a_pool_off_the_market_has_no_depth() {
        let b = book_of(
            4,
            vec![
                pair(1, 0, 1, e18(1_000_000), e18(2_000_000)),
                pair(2, 0, 1, e18(1_000_000), e18(2_000_000)),
                pair(3, 0, 1, e18(1_000_000), e18(2_000)),
                pair(4, 0, 2, e18(1_000_000), e18(1_000_000)),
                pair(5, 2, 3, e18(1_000_000), e18(1_000_000)),
            ],
        );
        let seed: HashMap<AssetId, RawValue> = [(a(0), e18(1))].into_iter().collect();
        let t = pool_depths_traced(&b, &seed, e18(10_000), 4, None);
        assert!(t.trusted.contains(&a(1)));
        assert!(t.depths[0].unwrap() > e18(900_000));
        assert_eq!(t.depths[2], None, "1 000x off the market: disregarded");
        assert!(!t.trusted.contains(&a(2)), "one pool only");
        assert!(t.prices.contains_key(&a(2)));
        assert!(
            !t.prices.contains_key(&a(3)),
            "an untrusted coin prices nothing"
        );
    }

    /// Two deep pools price token 1 at $1 and at $1 000: they never agree,
    /// so its price is unknown and both pools are disregarded.
    #[test]
    fn a_contested_price_disregards_its_pools() {
        let b = book_of(
            2,
            vec![
                pair(1, 0, 1, e18(1_000_000), e18(1_000_000)),
                pair(2, 0, 1, e18(1_000_000), e18(1_000)),
            ],
        );
        let seed: HashMap<AssetId, RawValue> = [(a(0), e18(1))].into_iter().collect();
        let t = pool_depths_traced(&b, &seed, e18(10_000), 4, None);
        assert!(t.contested.contains(&a(1)));
        assert!(!t.prices.contains_key(&a(1)));
        assert_eq!(t.depths, [None, None]);
    }

    /// One pool alone prices token 1 at $1 000 000 a token. With a supply of
    /// 10 million tokens that is a $10 trillion market cap: a honeypot, its
    /// pool disregarded. With a supply of 1 000, the price stands.
    #[test]
    fn a_lone_price_beyond_any_market_cap_is_a_honeypot() {
        let b = book_of(2, vec![pair(1, 0, 1, e18(1_000_000_000), e18(1_000))]);
        let seed: HashMap<AssetId, RawValue> = [(a(0), e18(1))].into_iter().collect();
        let big: HashMap<AssetId, U256> = [(a(1), e18(10_000_000))].into_iter().collect();
        let t = pool_depths_traced(&b, &seed, e18(10_000), 4, Some(&big));
        assert!(t.contested.contains(&a(1)));
        assert_eq!(t.depths, [None]);
        let small: HashMap<AssetId, U256> = [(a(1), e18(1_000))].into_iter().collect();
        let t = pool_depths_traced(&b, &seed, e18(10_000), 4, Some(&small));
        assert!(!t.contested.contains(&a(1)) && t.prices.contains_key(&a(1)));
        assert!(t.depths[0].is_some());
    }

    /// A concentrated pool is as deep as what it actually trades: a V3 pool
    /// whose liquidity sits in one narrow range trades within 2 % only
    /// until the price leaves that range, far less than its in-range
    /// virtual reserves suggest.
    #[test]
    fn a_narrow_v3_range_is_only_as_deep_as_it_trades() {
        use crate::fixtures::{v3, SQRT_ONE};
        let l: u128 = 1_000_000_000_000_000_000_000_000; // 1e24
                                                         // ±10 ticks (±0.1 %) around price 1, and the same L across ±50 %.
        let narrow = v3(1, 500, 10, SQRT_ONE, &[(-10, 10, l)]);
        let wide = v3(2, 500, 10, SQRT_ONE, &[(-6_930, 6_930, l)]);
        let v = e18(1);
        let dn = slip_depth(&narrow, 0, 1, v).unwrap();
        let dw = slip_depth(&wide, 0, 1, v).unwrap();
        // Both have the same virtual reserves (L at price 1, $1e6): only
        // the wide one trades through a 2 % move.
        assert!(dw > dn * U256::from(10u64), "{dn} vs {dw}");
    }

    /// Two providers share the 0/1 pairing: the deeper (Sushi, $4k) takes
    /// it, and Uniswap V2 moves on to its next unique pairing (1/2) even
    /// though its own 0/1 pair ($3k) is deeper than that. Each provider
    /// keeps at most `per_provider`; a pairing is never taken twice.
    #[test]
    fn a_shared_pairing_goes_to_the_deepest_provider() {
        let sushi = |mut p: Pool| {
            if let crate::solver::PoolState::V2(s) = &mut p.state {
                s.factory = liq_wire::wire::V2_FACTORY_SUSHI;
            }
            p
        };
        let b = book_of(
            4,
            vec![
                pair(1, 0, 1, e18(3_000), e18(3_000)),        // UniV2 0/1, $3k
                sushi(pair(2, 0, 1, e18(4_000), e18(4_000))), // Sushi 0/1, $4k
                pair(3, 1, 2, e18(2_000), e18(2_000)),        // UniV2 1/2, $2k
                pair(4, 2, 3, e18(1_000), e18(1_000)),        // UniV2 2/3, $1k
                sushi(pair(5, 1, 2, e18(500), e18(500))),     // Sushi 1/2, $500
            ],
        );
        let depths: Vec<Option<U256>> = [3_000u64, 4_000, 2_000, 1_000, 500]
            .iter()
            .map(|d| Some(e18(*d)))
            .collect();
        let got = curated_pools(&b, &depths, |_| Quota {
            max: 2,
            min_depth: U256::ZERO,
        });
        let picked: Vec<(u32, Provider)> = got.iter().map(|c| (c.pool.0, c.provider)).collect();
        assert_eq!(
            picked,
            [
                (1, Provider::Sushi),     // 0/1 at $4k
                (2, Provider::UniswapV2), // 1/2: 0/1 is taken by Sushi
                (3, Provider::UniswapV2), // 2/3: Uni V2's second slot
            ]
        );
        // Sushi's 1/2 ($500) loses that pairing to Uni V2's deeper one.
        assert!(got.iter().all(|c| c.pool.0 != 4));
    }

    /// A thin direct pool is the best rate at zero size, but at size a
    /// two-hop route through deep pools wins.
    #[test]
    fn a_two_hop_route_beats_a_thin_direct_pool() {
        let b = book_of(
            3,
            vec![
                pair(1, 0, 2, e18(10), e18(10)),
                pair(2, 0, 1, e18(1_000_000), e18(1_000_000)),
                pair(3, 1, 2, e18(1_000_000), e18(1_000_000)),
            ],
        );
        let g = TokenGraph::build(&b).unwrap();
        let t = ZeroTable::build(&g, &b, 3).unwrap();
        let direct = g
            .out_edges(a(0))
            .map(|e| *g.edge(e).unwrap())
            .find(|e| e.to == a(2))
            .unwrap();
        assert_eq!(t.best(&g, a(0), a(2)), edge_rate(&b, &direct));
        let budget = SearchBudget {
            max_hops: 3,
            top_n: 2,
            max_quotes: 10_000,
            min_share_bps: 0,
        };
        let r = search(&g, &t, &b, &GAS, a(0), a(2), e18(100), budget).unwrap();
        assert_eq!(r.routes[0].edges.len(), 2);
        assert!(r.routes[0].net > r.routes[1].net);
    }

    /// The quote budget is a latency cap, and hitting it is reported.
    #[test]
    fn an_exhausted_budget_is_reported() {
        let b = random_book(7, 7, 16);
        let g = TokenGraph::build(&b).unwrap();
        let t = ZeroTable::build(&g, &b, 5).unwrap();
        let budget = SearchBudget {
            max_hops: 5,
            top_n: 4,
            max_quotes: 3,
            min_share_bps: 0,
        };
        let r = search(&g, &t, &b, &GAS, a(0), a(1), e18(1), budget).unwrap();
        assert!(r.exhausted && r.quotes == 3);
    }
}
