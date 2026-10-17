//! The arb thread's core: its own copy of the pool book, kept in step with
//! the node's, and one call per victim.
//!
//! The node's book has one writer, the hot thread. The engine never reads
//! it while solving: [`Engine::on_block`] copies in the pools that changed
//! since the last block ([`PoolBook::sync_from`], allocation-free once each
//! pool's slot is sized), and everything after — the victim's logs, the
//! scratch swaps of every evaluation — happens on the engine's copy and is
//! put back before the next victim.
//!
//! [`Engine::on_victim`] folds a victim's logs ([`apply_victim`]), solves
//! every anchor they leave ([`best_backrun`]), keeps the best that clears
//! the floor, and encodes its plan ([`build_plan`]). Pools a liquidation
//! has claimed for the block ([`Engine::reserve`]) are not anchored: the
//! liquidation's exit was solved on them and goes first.
//!
//! [`Engine::top_of_block`] needs no victim: the block itself moved the
//! pools. Before copying the block in, [`Engine::on_block`] quotes every
//! pool the block changed on the state from before it; after, on the state
//! it left. A pair whose price moved is an anchor the other way, and the
//! largest moves are solved for the top of the next block.
//!
//! A pool whose model disagreed with a simulation of our own plan
//! ([`Engine::quarantine`]) is neither anchored nor exited through.

use crate::anchor::{best_backrun, Anchor, ArbCtx, Backrun};
use crate::plan::{build_plan, expected_weth_gross_rec, kept_after_bid, FlashSource, PlanCtx};
use crate::realize::{for_each_pool, Hop};
use crate::scratch::{Scratch, VICTIM};
use crate::victim::{apply_victim, LogRef, Skip};
use crate::ArbError;
use alloy_primitives::{Address, U256};
use liq_plan::EncodedPlan;
use liq_router::{PoolBook, PoolId, SolveBudget};
use liq_types::AssetId;
use smallvec::SmallVec;

/// What stays fixed while the engine runs.
#[derive(Clone, Debug)]
pub struct EngineCfg {
    pub weth: Address,
    pub weth_id: AssetId,
    /// Assets a flash loan or flash swap can supply.
    pub flashable: Vec<AssetId>,
    pub budget: SolveBudget,
    /// The arb contract's gas around its swaps.
    pub base_gas: u64,
    /// Exit solves per orientation.
    pub max_evals: u16,
    /// Coinbase bid, basis points of realized net, where the caller gives
    /// none (tests); the arb thread passes its bid table's per pool.
    pub bid_bps: u16,
    /// Smallest net after gas worth sending, wei.
    pub min_net_wei: U256,
    /// What the contract must keep after the bid, wei.
    pub min_profit_wei: u128,
    pub sweep: bool,
    /// Fund with the V3 anchor's flash swap where there is one (a flash
    /// loan otherwise).
    pub flash_swap: bool,
    /// Read each block's moves for [`Engine::top_of_block`]. Off, a new
    /// block is only copied in (no before/after quotes) and there are no
    /// top-of-block anchors.
    pub top_of_block: bool,
}

/// The block a backrun targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub number: u64,
    pub timestamp: u64,
    pub base_fee_wei: u128,
}

/// Swaps one plan runs, inline before the list spills.
pub const PLAN_HOPS: usize = 16;

/// A backrun worth sending, with its plan.
#[derive(Clone, Debug)]
pub struct Opportunity {
    pub backrun: Backrun,
    pub plan: EncodedPlan,
    /// How the plan is funded: the anchor's flash swap or a flash loan.
    pub flash: liq_types::FlashProvider,
    /// The token besides the start one the anchor trades (risk scoping).
    pub other: AssetId,
    /// The coinbase bid the plan carries, bps of realized net.
    pub bid_bps: u16,
    /// The gas cost the plan states (its base fee times its gas), wei.
    pub gas_cost_wei: u128,
    /// The WETH the plan leaves the executor before the bid, by the models
    /// ([`crate::expected_weth_gross`]).
    pub gross_wei: U256,
    /// What the executor and its sink keep after the bid, by the models: a
    /// simulation of the plan on the state it was solved on must gain
    /// exactly this (a flash-swap plan, at least this — the V3 pool's
    /// exact-output charge can be a unit under the modelled input).
    pub kept_wei: U256,
    /// Every swap the plan runs, in execution order, as the models say each
    /// pool will report it: what a simulation's swap events are held to.
    pub hops: SmallVec<[Hop; PLAN_HOPS]>,
    /// Each hop pool's state as the plan was solved on it (before its
    /// first swap), where the pool's own views can read it back: a hop that
    /// disagrees with a simulation is a model error only when the pool held
    /// this state there.
    pub pre: SmallVec<[(PoolId, PoolView); PLAN_HOPS]>,
}

/// A pool's state in the fields its contract's views return: a V2 pair's
/// `getReserves()`, a V3 pool's `slot0()` price and tick and
/// `liquidity()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolView {
    V2 {
        reserve0: U256,
        reserve1: U256,
    },
    V3 {
        sqrt_price_x96: U256,
        tick: i32,
        liquidity: u128,
    },
}

/// `pool`'s contract address and [`PoolView`]; `None` for venues whose
/// state no pool contract's view returns (V4, whose pools live in the
/// PoolManager; Curve, Balancer and Fluid, read field by field).
#[must_use]
pub fn pool_view(book: &PoolBook, pool: PoolId) -> Option<(Address, PoolView)> {
    let p = book.get(pool)?;
    match &p.state {
        liq_router::PoolState::V2(s) => Some((
            p.address,
            PoolView::V2 {
                reserve0: s.reserve0,
                reserve1: s.reserve1,
            },
        )),
        liq_router::PoolState::V3(s) if s.v4.is_none() => Some((
            p.address,
            PoolView::V3 {
                sqrt_price_x96: s.sqrt_price_x96,
                tick: s.tick,
                liquidity: s.liquidity,
            },
        )),
        _ => None,
    }
}

/// Why a victim produced nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoBackrun {
    Skipped(Skip),
    /// No anchor pays more than the floor after gas.
    BelowFloor,
}

/// Anchors the top-of-block search solves: the largest price moves.
pub const TOP_ANCHORS: usize = 16;
/// Coins of a pool whose pairs the top-of-block search reads; a pool with
/// more is not read (its pairs grow as the square).
const TOP_COINS: usize = 4;
/// Pools a block is expected to change; the lists start at this capacity.
const MOVED_CAP: usize = 512;

/// One pair of a changed pool, quoted before the block was copied in.
#[derive(Clone, Copy, Debug)]
struct Probe {
    pool: PoolId,
    i: u8,
    j: u8,
    /// One ETH's worth of coin `i`.
    x: U256,
    /// `x` of `i` into `j` before the block.
    before: U256,
}

pub struct Engine {
    book: PoolBook,
    scratch: Scratch,
    cfg: EngineCfg,
    per_eth: Vec<U256>,
    sources: Vec<FlashSource>,
    target: Target,
    reserved: Vec<PoolId>,
    /// Pools whose model a simulation contradicted: never anchored, never
    /// in an exit.
    quarantined: Vec<PoolId>,
    /// The pools the last block changed (reused).
    changed: Vec<PoolId>,
    probes: Vec<Probe>,
    /// The last block's moves as anchors, largest first, with their size
    /// (parts per million of the pair's price).
    top: SmallVec<[(Anchor, U256); TOP_ANCHORS]>,
}

/// Pools one block's liquidations can reserve before the list grows.
const RESERVE_CAP: usize = 64;
/// Pools quarantined before the list grows.
const QUARANTINE_CAP: usize = 64;

const PPM: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);

impl Engine {
    /// A copy of `src` (the node's book) to work on.
    #[must_use]
    pub fn new(src: &PoolBook, cfg: EngineCfg) -> Self {
        let book = src.clone();
        let scratch = Scratch::new(&book);
        Self {
            book,
            scratch,
            cfg,
            per_eth: Vec::new(),
            sources: Vec::new(),
            target: Target::default(),
            reserved: Vec::with_capacity(RESERVE_CAP),
            quarantined: Vec::with_capacity(QUARANTINE_CAP),
            changed: Vec::with_capacity(MOVED_CAP),
            probes: Vec::with_capacity(MOVED_CAP),
            top: SmallVec::new(),
        }
    }

    /// A new block: bring the copy up to date with `src`, and take the
    /// target block's terms. Prices are raw units per `1e18` wei, indexed
    /// by asset id. Returns the pools copied. Also reads, for
    /// [`Self::top_of_block`], which way the block moved each pool it
    /// changed.
    pub fn on_block(
        &mut self,
        src: &PoolBook,
        target: Target,
        per_eth: &[U256],
        sources: &[FlashSource],
    ) -> usize {
        // Nothing of a victim may survive into the next block.
        self.scratch.restore(VICTIM, &mut self.book);
        self.per_eth.clear();
        self.per_eth.extend_from_slice(per_eth);
        self.changed.clear();
        self.probes.clear();
        if self.cfg.top_of_block {
            self.book.changed_in(src, &mut self.changed);
            self.probe_before();
        }
        let copied = self.book.sync_from(src);
        // With the probing off there are no probes, so no anchors.
        self.moves_after();
        self.sources.clear();
        self.sources.extend_from_slice(sources);
        self.target = target;
        self.reserved.clear();
        copied
    }

    /// Keep `pools` (a liquidation's exit for the target block) out of
    /// anchors until the next block.
    pub fn reserve(&mut self, pools: &[PoolId]) {
        self.reserved.extend_from_slice(pools);
    }

    /// Never anchor on or exit through `pool` again: a simulation of our
    /// own plan contradicted its model on the exact state the plan ran on.
    /// `true` when it was not already quarantined.
    pub fn quarantine(&mut self, pool: PoolId) -> bool {
        if self.quarantined.contains(&pool) {
            return false;
        }
        self.quarantined.push(pool);
        true
    }

    /// The quarantined pools.
    #[must_use]
    pub fn quarantined(&self) -> &[PoolId] {
        &self.quarantined
    }

    /// The engine's copy of the book (tests, parity checks).
    #[must_use]
    pub fn book(&self) -> &PoolBook {
        &self.book
    }

    /// The last block's moves, largest first, as the anchors
    /// [`Self::top_of_block`] solves.
    pub fn top_anchors(&self) -> impl Iterator<Item = &Anchor> {
        self.top.iter().map(|(a, _)| a)
    }

    /// One ETH's worth of `pool`'s coin `k`; `None` unpriced.
    fn eth_of(&self, pool: PoolId, k: u8) -> Option<U256> {
        let a = *self.book.get(pool)?.assets.get(usize::from(k))?;
        self.per_eth
            .get(usize::from(a.0))
            .copied()
            .filter(|p| !p.is_zero())
    }

    /// Quote every pair of every changed pool on the state from before the
    /// block, one ETH's worth in.
    fn probe_before(&mut self) {
        self.probes.clear();
        for k in 0..self.changed.len() {
            let Some(&id) = self.changed.get(k) else {
                break;
            };
            let Some(p) = self.book.get(id) else {
                continue;
            };
            let n = p.assets.len();
            if n > TOP_COINS || !p.is_live() {
                continue;
            }
            for i in 0..n {
                for j in i.saturating_add(1)..n {
                    let (Ok(i), Ok(j)) = (u8::try_from(i), u8::try_from(j)) else {
                        continue;
                    };
                    // Priced in whichever coin has a price.
                    let (i, j, x) = match (self.eth_of(id, i), self.eth_of(id, j)) {
                        (Some(x), _) => (i, j, x),
                        (None, Some(x)) => (j, i, x),
                        (None, None) => continue,
                    };
                    let Some(before) = self
                        .book
                        .get(id)
                        .and_then(|p| p.quote_exact_in(i, j, x).ok())
                        .filter(|q| !q.is_zero())
                    else {
                        continue;
                    };
                    self.probes.push(Probe {
                        pool: id,
                        i,
                        j,
                        x,
                        before,
                    });
                }
            }
        }
    }

    /// After the block is copied in: each probed pair quoted again. Fewer
    /// of `j` for the same `i` means the block sold `i` into the pool, so
    /// the backrun sells `j` back (and the other way round). The largest
    /// moves are kept.
    fn moves_after(&mut self) {
        self.top.clear();
        for k in 0..self.probes.len() {
            let Some(&pr) = self.probes.get(k) else {
                break;
            };
            let Some(after) = self
                .book
                .get(pr.pool)
                .filter(|p| p.is_live())
                .and_then(|p| p.quote_exact_in(pr.i, pr.j, pr.x).ok())
                .filter(|q| !q.is_zero())
            else {
                continue;
            };
            let (anchor, diff) = if after < pr.before {
                (
                    Anchor {
                        pool: pr.pool,
                        i: pr.j,
                        j: pr.i,
                        scale: after,
                    },
                    pr.before.saturating_sub(after),
                )
            } else if after > pr.before {
                (
                    Anchor {
                        pool: pr.pool,
                        i: pr.i,
                        j: pr.j,
                        scale: pr.x,
                    },
                    after.saturating_sub(pr.before),
                )
            } else {
                continue;
            };
            let size = diff
                .saturating_mul(PPM)
                .checked_div(pr.before)
                .unwrap_or_default();
            if self.top.len() < TOP_ANCHORS {
                self.top.push((anchor, size));
            } else if let Some(min) = self
                .top
                .iter_mut()
                .min_by_key(|(_, s)| *s)
                .filter(|(_, s)| *s < size)
            {
                *min = (anchor, size);
            }
        }
        self.top.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
    }

    /// The best backrun behind one victim, its logs in emission order.
    /// `bid` names the coinbase bid (bps of realized net) for a plan
    /// anchored on a pool.
    pub fn on_victim(
        &mut self,
        logs: &[LogRef<'_>],
        bid: impl Fn(PoolId) -> u16,
    ) -> Result<Result<Opportunity, NoBackrun>, ArbError> {
        let t = self.target;
        let anchors = match apply_victim(
            &mut self.book,
            &mut self.scratch,
            logs,
            t.number,
            t.timestamp,
        ) {
            Ok(a) => a,
            Err(s) => return Ok(Err(NoBackrun::Skipped(s))),
        };
        let out = self.solve(&anchors, &bid);
        self.scratch.restore(VICTIM, &mut self.book);
        out
    }

    /// The best backrun on what the last block left open, with no victim:
    /// the block's largest moves ([`Self::top_anchors`]) as anchors, on the
    /// state it left. For a bundle of ours alone at the top of the target
    /// block.
    pub fn top_of_block(
        &mut self,
        bid: impl Fn(PoolId) -> u16,
    ) -> Result<Result<Opportunity, NoBackrun>, ArbError> {
        let anchors: SmallVec<[Anchor; TOP_ANCHORS]> = self.top.iter().map(|(a, _)| *a).collect();
        if anchors.is_empty() {
            return Ok(Err(NoBackrun::Skipped(Skip::NoPool)));
        }
        let out = self.solve(&anchors, &bid);
        self.scratch.restore(VICTIM, &mut self.book);
        out
    }

    /// Whether `r` touches a quarantined pool.
    fn uses_quarantined(&self, r: &Backrun) -> bool {
        if self.quarantined.is_empty() {
            return false;
        }
        let mut hit = self.quarantined.contains(&r.anchor.pool);
        for_each_pool(&r.exit, |p| hit |= self.quarantined.contains(&p));
        hit
    }

    /// Solve every anchor not reserved or quarantined on the book as it
    /// stands; the best that clears the floor, planned.
    fn solve(
        &mut self,
        anchors: &[Anchor],
        bid: &impl Fn(PoolId) -> u16,
    ) -> Result<Result<Opportunity, NoBackrun>, ArbError> {
        let t = self.target;
        let ctx = ArbCtx {
            base_fee_wei: t.base_fee_wei,
            per_eth: &self.per_eth,
            flashable: &self.cfg.flashable,
            weth: self.cfg.weth_id,
            budget: self.cfg.budget,
            base_gas: self.cfg.base_gas,
            max_evals: self.cfg.max_evals,
        };
        let mut best: Option<Backrun> = None;
        let mut err = None;
        for a in anchors
            .iter()
            .filter(|a| !self.reserved.contains(&a.pool) && !self.quarantined.contains(&a.pool))
        {
            match best_backrun(&mut self.book, &mut self.scratch, &ctx, *a) {
                Ok(Some(r))
                    if !self.uses_quarantined(&r)
                        && best.as_ref().is_none_or(|w| r.net_wei > w.net_wei) =>
                {
                    best = Some(r)
                }
                Ok(_) => {}
                Err(e) => err = Some(e),
            }
        }
        let Some(r) = best.filter(|r| r.net_wei >= self.cfg.min_net_wei) else {
            return match err {
                // A refused replay or solve is a finding, not a quiet miss.
                Some(e) => Err(e),
                None => Ok(Err(NoBackrun::BelowFloor)),
            };
        };
        let bid_bps = bid(r.anchor.pool);
        let pctx = PlanCtx {
            weth: self.cfg.weth,
            weth_id: self.cfg.weth_id,
            sources: &self.sources,
            bid_bps,
            min_profit_wei: self.cfg.min_profit_wei,
            base_fee_wei: t.base_fee_wei,
            sweep: self.cfg.sweep,
            flash_swap: self.cfg.flash_swap,
        };
        let other = self
            .book
            .get(r.anchor.pool)
            .and_then(|p| {
                let a = *p.assets.get(usize::from(r.anchor.i))?;
                let b = *p.assets.get(usize::from(r.anchor.j))?;
                Some(if a == r.start { b } else { a })
            })
            .unwrap_or(r.start);
        let (batch, plan) = build_plan(&self.book, &r, &pctx)?;
        let flash = batch
            .groups
            .first()
            .map(|g| g.provider)
            .ok_or(ArbError::Unencodable)?;
        let mut pre: SmallVec<[(PoolId, PoolView); PLAN_HOPS]> = SmallVec::new();
        let book = &self.book;
        let mut note = |id: PoolId| {
            if !pre.iter().any(|(p, _)| *p == id) {
                if let Some((_, v)) = pool_view(book, id) {
                    pre.push((id, v));
                }
            }
        };
        note(r.anchor.pool);
        for_each_pool(&r.exit, &mut note);
        let mut hops: SmallVec<[Hop; PLAN_HOPS]> = SmallVec::new();
        let gross_wei = expected_weth_gross_rec(
            &mut self.book,
            &mut self.scratch,
            &r,
            self.cfg.weth_id,
            &mut |h| hops.push(h),
        )?;
        let kept_wei =
            kept_after_bid(gross_wei, batch.gas_cost_wei, bid_bps).ok_or(ArbError::Math)?;
        Ok(Ok(Opportunity {
            backrun: r,
            plan,
            flash,
            other,
            bid_bps,
            gas_cost_wei: batch.gas_cost_wei,
            gross_wei,
            kept_wei,
            hops,
            pre,
        }))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::testbook::*;
    use alloy_primitives::{b256, B256};
    use liq_types::FlashProvider;
    use std::collections::HashMap;

    const SYNC: B256 = b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
    const V2_SWAP: B256 = b256!("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");

    fn node_book() -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A);
        assets.insert(tok(1), B);
        let mut b = PoolBook::new(assets, None, 0);
        b.add(v2(1, E21, E21)).unwrap();
        b.add(v2(2, E21, E21)).unwrap();
        b
    }

    fn cfg() -> EngineCfg {
        EngineCfg {
            weth: tok(0),
            weth_id: A,
            flashable: vec![A],
            budget: SolveBudget::default(),
            base_gas: BASE,
            max_evals: 40,
            bid_bps: 500,
            min_net_wei: U256::from(1u8),
            min_profit_wei: 1,
            sweep: false,
            flash_swap: true,
            top_of_block: true,
        }
    }

    fn words(ws: &[U256]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_be_bytes::<32>()).collect()
    }

    /// A victim's 50 A sale into pool 0 yields a plan; the engine's book is
    /// the node's again afterwards; a liquidation reserving pool 0 leaves
    /// nothing to anchor; a new block with the node's pool moved is copied in.
    #[test]
    fn victim_to_plan_and_back() {
        let mut node = node_book();
        let mut e = Engine::new(&node, cfg());
        let eth = U256::from(1_000_000_000_000_000_000u64);
        let src = [FlashSource {
            asset: A,
            provider: FlashProvider::UniV4,
            source: Address::repeat_byte(0x44),
            available: U256::MAX,
        }];
        let target = Target {
            number: 2,
            timestamp: 24,
            base_fee_wei: 1_000_000_000,
        };
        e.on_block(&node, target, &[eth, eth], &src);

        let e21 = U256::from(E21);
        let sold = U256::from(50u8) * eth;
        let bought = v2_out(sold, e21, e21);
        let (sync, swap) = (
            words(&[e21 + sold, e21 - bought]),
            words(&[sold, U256::ZERO, U256::ZERO, bought]),
        );
        let (ts, tw) = ([SYNC], [V2_SWAP, B256::ZERO, B256::ZERO]);
        let pool = Address::repeat_byte(1);
        let logs = [
            LogRef {
                address: pool,
                topics: &ts,
                data: &sync,
            },
            LogRef {
                address: pool,
                topics: &tw,
                data: &swap,
            },
        ];
        let o = e.on_victim(&logs, |_| 500).unwrap().unwrap();
        assert!(o.backrun.net_wei >= U256::from(1u8));
        assert_eq!(o.backrun.anchor.pool, PoolId(0));
        let decoded = liq_plan::decode_batch(o.plan.as_bytes()).unwrap();
        assert_eq!(decoded.bid_bps, 500);
        let states = |b: &PoolBook| {
            b.pools()
                .iter()
                .map(|p| p.state.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            states(e.book()),
            states(&node),
            "engine book back to the node's"
        );

        e.reserve(&[PoolId(0)]);
        assert_eq!(
            e.on_victim(&logs, |_| 500).unwrap().unwrap_err(),
            NoBackrun::BelowFloor
        );

        node.get_mut(PoolId(1))
            .unwrap()
            .apply_exact_in(0, 1, eth)
            .unwrap();
        // The node's moved pool, and pool 0: the victim's scratch work left
        // it locally changed (restored, but stamped), so it is re-copied
        // from the node — the copy never trusts its own scratch history.
        assert_eq!(e.on_block(&node, target, &[eth, eth], &src), 2);
        assert_eq!(states(e.book()), states(&node));
    }

    /// A block that sold 50 A into pool 0 (applied to the node's book as
    /// the hot thread would) leaves pool 0 as the one top-of-block anchor,
    /// the reverse way. The plan's hops chain on the V2 formula from the
    /// post-block reserves, its gross is what the last hop returns less
    /// what the first borrowed, and what it keeps is the contract's bid
    /// arithmetic on that gross. Quarantining the anchor or the exit pool
    /// leaves nothing.
    #[test]
    fn top_of_block_anchors_the_block_move() {
        let mut node = node_book();
        let mut e = Engine::new(&node, cfg());
        let eth = U256::from(1_000_000_000_000_000_000u64);
        let src = [FlashSource {
            asset: A,
            provider: FlashProvider::UniV4,
            source: Address::repeat_byte(0x44),
            available: U256::MAX,
        }];
        let target = |n: u64| Target {
            number: n,
            timestamp: 12 * n,
            base_fee_wei: 1_000_000_000,
        };
        e.on_block(&node, target(2), &[eth, eth], &src);
        assert_eq!(e.top_anchors().count(), 0, "nothing moved");

        node.get_mut(PoolId(0))
            .unwrap()
            .apply_exact_in(0, 1, U256::from(50u8) * eth)
            .unwrap();
        e.on_block(&node, target(3), &[eth, eth], &src);
        let tops: Vec<Anchor> = e.top_anchors().copied().collect();
        assert_eq!(tops.len(), 1, "only pool 0 moved");
        assert_eq!((tops[0].pool, tops[0].i, tops[0].j), (PoolId(0), 1, 0));

        let o = e.top_of_block(|_| 700).unwrap().unwrap();
        assert_eq!(o.bid_bps, 700);
        assert_eq!(
            liq_plan::decode_batch(o.plan.as_bytes()).unwrap().bid_bps,
            700
        );
        let mut res = [reserves(&node, PoolId(0)), reserves(&node, PoolId(1))];
        for h in &o.hops {
            let r = &mut res[h.pool.0 as usize];
            let (rin, rout) = if h.i == 0 { (r.0, r.1) } else { (r.1, r.0) };
            assert_eq!(h.amount_out, v2_out(h.amount_in, rin, rout), "{h:?}");
            if h.i == 0 {
                *r = (rin + h.amount_in, rout - h.amount_out);
            } else {
                *r = (rout - h.amount_out, rin + h.amount_in);
            }
        }
        let (first, last) = (o.hops.first().unwrap(), o.hops.last().unwrap());
        assert_eq!(o.backrun.start, A);
        assert_eq!(o.gross_wei, last.amount_out - first.amount_in);
        let net = o.gross_wei - U256::from(o.gas_cost_wei);
        assert_eq!(
            o.kept_wei,
            o.gross_wei - net * U256::from(700u16) / U256::from(10_000u16)
        );

        // Off in the config: the same move is copied in, but read as no
        // anchor at all.
        let mut off = Engine::new(
            &node_book(),
            EngineCfg {
                top_of_block: false,
                ..cfg()
            },
        );
        off.on_block(&node_book(), target(2), &[eth, eth], &src);
        off.on_block(&node, target(3), &[eth, eth], &src);
        assert_eq!(off.top_anchors().count(), 0);
        assert_eq!(
            reserves(off.book(), PoolId(0)),
            reserves(&node, PoolId(0)),
            "still copied in"
        );
        assert_eq!(
            off.top_of_block(|_| 700).unwrap().unwrap_err(),
            NoBackrun::Skipped(Skip::NoPool)
        );

        e.quarantine(PoolId(1));
        assert_eq!(
            e.top_of_block(|_| 700).unwrap().unwrap_err(),
            NoBackrun::BelowFloor
        );
        let mut e2 = Engine::new(&node, cfg());
        e2.on_block(&node_book(), target(2), &[eth, eth], &src);
        e2.on_block(&node, target(3), &[eth, eth], &src);
        assert!(e2.quarantine(PoolId(0)));
        assert!(!e2.quarantine(PoolId(0)), "once");
        assert_eq!(
            e2.top_of_block(|_| 700).unwrap().unwrap_err(),
            NoBackrun::BelowFloor
        );
    }
}
