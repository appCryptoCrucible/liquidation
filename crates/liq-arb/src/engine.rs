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

use crate::anchor::{best_backrun, ArbCtx, Backrun};
use crate::plan::{build_plan, FlashSource, PlanCtx};
use crate::scratch::{Scratch, VICTIM};
use crate::victim::{apply_victim, LogRef, Skip};
use crate::ArbError;
use alloy_primitives::{Address, U256};
use liq_plan::EncodedPlan;
use liq_router::{PoolBook, PoolId, SolveBudget};
use liq_types::AssetId;

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
    /// Coinbase bid, basis points of realized net.
    pub bid_bps: u16,
    /// Smallest net after gas worth sending, wei.
    pub min_net_wei: U256,
    /// What the contract must keep after the bid, wei.
    pub min_profit_wei: u128,
    pub sweep: bool,
    /// Fund with the V3 anchor's flash swap where there is one (a flash
    /// loan otherwise).
    pub flash_swap: bool,
}

/// The block a backrun targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub number: u64,
    pub timestamp: u64,
    pub base_fee_wei: u128,
}

/// A backrun worth sending, with its plan.
#[derive(Clone, Debug)]
pub struct Opportunity {
    pub backrun: Backrun,
    pub plan: EncodedPlan,
    /// How the plan is funded: the anchor's flash swap or a flash loan.
    pub flash: liq_types::FlashProvider,
    /// The token besides the start one the anchor trades (risk scoping).
    pub other: AssetId,
}

/// Why a victim produced nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoBackrun {
    Skipped(Skip),
    /// No anchor pays more than the floor after gas.
    BelowFloor,
}

pub struct Engine {
    book: PoolBook,
    scratch: Scratch,
    cfg: EngineCfg,
    per_eth: Vec<U256>,
    sources: Vec<FlashSource>,
    target: Target,
    reserved: Vec<PoolId>,
}

/// Pools one block's liquidations can reserve before the list grows.
const RESERVE_CAP: usize = 64;

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
        }
    }

    /// A new block: bring the copy up to date with `src`, and take the
    /// target block's terms. Prices are raw units per `1e18` wei, indexed
    /// by asset id. Returns the pools copied.
    pub fn on_block(
        &mut self,
        src: &PoolBook,
        target: Target,
        per_eth: &[U256],
        sources: &[FlashSource],
    ) -> usize {
        // Nothing of a victim may survive into the next block.
        self.scratch.restore(VICTIM, &mut self.book);
        let copied = self.book.sync_from(src);
        self.per_eth.clear();
        self.per_eth.extend_from_slice(per_eth);
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

    /// The engine's copy of the book (tests, parity checks).
    #[must_use]
    pub fn book(&self) -> &PoolBook {
        &self.book
    }

    /// The best backrun behind one victim, its logs in emission order.
    pub fn on_victim(
        &mut self,
        logs: &[LogRef<'_>],
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
        for a in anchors.iter().filter(|a| !self.reserved.contains(&a.pool)) {
            match best_backrun(&mut self.book, &mut self.scratch, &ctx, *a) {
                Ok(Some(r)) if best.as_ref().is_none_or(|w| r.net_wei > w.net_wei) => {
                    best = Some(r)
                }
                Ok(_) => {}
                Err(e) => err = Some(e),
            }
        }
        let out = match best.filter(|r| r.net_wei >= self.cfg.min_net_wei) {
            Some(r) => {
                let pctx = PlanCtx {
                    weth: self.cfg.weth,
                    weth_id: self.cfg.weth_id,
                    sources: &self.sources,
                    bid_bps: self.cfg.bid_bps,
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
                build_plan(&self.book, &r, &pctx).and_then(|(batch, plan)| {
                    let flash = batch
                        .groups
                        .first()
                        .map(|g| g.provider)
                        .ok_or(ArbError::Unencodable)?;
                    Ok(Ok(Opportunity {
                        backrun: r,
                        plan,
                        flash,
                        other,
                    }))
                })
            }
            None => match err {
                // A refused replay or solve is a finding, not a quiet miss.
                Some(e) => Err(e),
                None => Ok(Err(NoBackrun::BelowFloor)),
            },
        };
        self.scratch.restore(VICTIM, &mut self.book);
        out
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
        let o = e.on_victim(&logs).unwrap().unwrap();
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
            e.on_victim(&logs).unwrap().unwrap_err(),
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
}
