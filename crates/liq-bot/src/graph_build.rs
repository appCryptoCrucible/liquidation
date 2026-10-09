//! The token graph exits route through (coverage plan, Phase 4), built off
//! the hot path once the book is seeded, and attached to the book.
//!
//! * Prices: the canonical feeds, spread through the book by pools that
//!   corroborate each other ([`liq_router::graph::pool_depths_traced`]);
//!   honeypots, stranded positions and contested tokens are disregarded,
//!   and a single pool may not price a token past a $1T market cap (each
//!   token's `totalSupply`, read here).
//! * Pools: the curated backbone (Uniswap V2, V3 and V4: every unique pool
//!   of $1M or more, at most 50 each; Sushi and each Curve venue: their ten
//!   deepest; a pairing goes to its deepest provider), each token's
//!   [`SPOKES`] deepest pools, which join it to the backbone, and, on the
//!   large-swap set (`config/large-swap.toml`: the tokens large
//!   liquidations move), every pool of its minimum depth on every venue,
//!   with no per-provider count: a large sale is routed across all of them.
//! * Use: `solve_pair` tries two-hop exits through the intermediate tokens
//!   the graph proposes, beside the direct and WETH exits, and keeps the one
//!   that leaves the most after gas: the graph can only add routes.
//!
//! `LIQ_GRAPH_ROUTING=off` leaves the book without a graph (today's
//! routing), for comparing the two.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alloy_primitives::U256;
use alloy_sol_types::{sol, SolCall};
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_router::graph::{
    curated_pools, pool_depths_traced, raw_value, spoke_pools, GraphRoutes, Provider, Quota,
    RawValue,
};
use liq_types::AssetId;

use crate::index::IndexLoad;

sol! {
    function totalSupply() external view returns (uint256);
}

/// Each token's deepest pools added to the backbone.
pub const SPOKES: usize = 3;
/// Hops the graph's zero-size table covers.
const TABLE_HOPS: u8 = 4;
/// A pool must be this deep (USD) to price a token.
const PRICE_FLOOR_USD: u64 = 10_000;
const PRICE_PASSES: u8 = 6;

const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// The curated set's quota per provider.
#[must_use]
pub fn quota(p: Provider) -> Quota {
    match p {
        Provider::UniswapV2 | Provider::UniswapV3 | Provider::UniswapV4 => Quota {
            max: 50,
            min_depth: U256::from(1_000_000u64).saturating_mul(WAD),
        },
        _ => Quota {
            max: 10,
            min_depth: U256::ZERO,
        },
    }
}

/// Whether graph routing is on (`LIQ_GRAPH_ROUTING` other than `off`).
#[must_use]
pub fn enabled() -> bool {
    std::env::var("LIQ_GRAPH_ROUTING").map_or(true, |v| v != "off")
}

/// Build the graph over `load`'s seeded book and attach it. A failure
/// leaves the book without a graph (today's routing) and is logged.
pub async fn attach(
    load: &mut IndexLoad,
    intern: &liq_config::Intern,
    rpc_url: &str,
    config_dir: &std::path::Path,
) {
    if !enabled() {
        load.book.set_graph(None);
        tracing::info!("graph routing off (LIQ_GRAPH_ROUTING=off)");
        return;
    }
    let t0 = std::time::Instant::now();
    let large = LargeSwap::load(&config_dir.join("large-swap.toml"), intern);
    let Some((keep, stats)) = select(load, intern, rpc_url, large.as_ref()).await else {
        return;
    };
    let book = &load.book;
    match GraphRoutes::build(book, |id| keep.contains(&id.0), TABLE_HOPS) {
        Ok(g) => {
            tracing::info!(
                seeds = stats.seeds,
                priced = stats.priced,
                trusted = stats.trusted,
                contested = stats.contested,
                backbone = stats.backbone,
                spokes = stats.spokes,
                large_swap = stats.large_swap,
                pools = keep.len(),
                tokens = g.graph.nodes().len(),
                edges = g.graph.edges().len(),
                chain_edges = g.chain_graph.edges().len(),
                ms = t0.elapsed().as_millis(),
                "graph routing built"
            );
            load.book.set_graph(Some(Arc::new(g)));
        }
        Err(e) => tracing::error!(error = %e, "graph routing off: build failed"),
    }
}

/// What [`select`] found.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectStats {
    pub seeds: usize,
    pub priced: usize,
    pub trusted: usize,
    pub contested: usize,
    pub backbone: usize,
    pub spokes: usize,
    /// Pools kept for the large-swap set beyond the backbone and spokes.
    pub large_swap: usize,
}

/// `config/large-swap.toml`: the tokens large liquidations move, and the
/// least depth (USD, WAD) a pool on a pair of them needs to join the graph.
pub struct LargeSwap {
    pub tokens: HashSet<AssetId>,
    pub min_depth: U256,
}

#[derive(serde::Deserialize)]
struct LargeSwapToml {
    min_depth_usd: u64,
    tokens: Vec<alloy_primitives::Address>,
}

impl LargeSwap {
    /// The committed set, or `None` (logged) when the file is absent or
    /// malformed, or names a token the registry does not intern.
    pub fn load(path: &std::path::Path, intern: &liq_config::Intern) -> Option<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "large-swap set not read — large sales route on the curated graph alone");
                return None;
            }
        };
        let t: LargeSwapToml = match toml::from_str(&raw) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "large-swap set malformed — large sales route on the curated graph alone");
                return None;
            }
        };
        let mut tokens = HashSet::new();
        for a in &t.tokens {
            match intern.asset(*a) {
                Some(id) => {
                    tokens.insert(id);
                }
                None => {
                    tracing::error!(token = %a, "large-swap set names a token the registry does not intern — set not used");
                    return None;
                }
            }
        }
        Some(Self {
            tokens,
            min_depth: U256::from(t.min_depth_usd).saturating_mul(WAD),
        })
    }
}

/// The graph's pools on `load`'s seeded book (indices into its pools): the
/// curated backbone and every token's spokes. `None` without canonical
/// prices.
pub async fn select(
    load: &IndexLoad,
    intern: &liq_config::Intern,
    rpc_url: &str,
    large: Option<&LargeSwap>,
) -> Option<(HashSet<u32>, SelectStats)> {
    let Some(canonical) = load.canonical.as_ref() else {
        tracing::error!("graph routing off: no canonical prices to start from");
        return None;
    };
    let seed: HashMap<AssetId, RawValue> = intern
        .assets()
        .iter()
        .filter_map(|r| {
            let p = canonical.price(r.id).filter(|p| p.ts != 0)?;
            Some((r.id, raw_value(p.price.raw(), r.decimals)?))
        })
        .collect();
    let supply = match HttpRpc::connect(rpc_url) {
        Ok(rpc) => supplies(&rpc, intern).await,
        Err(e) => {
            tracing::error!(error = %e, "graph: supplies not read — single-pool prices go unchecked");
            HashMap::new()
        }
    };
    let book = &load.book;
    let trace = pool_depths_traced(
        book,
        &seed,
        U256::from(PRICE_FLOOR_USD).saturating_mul(WAD),
        PRICE_PASSES,
        (!supply.is_empty()).then_some(&supply),
    );
    // Each contested token (its pools never agreed on a price, so it and
    // its pools are disregarded) with the offers it had: what to look at
    // when a pool that should route does not.
    for a in &trace.contested {
        let offers: Vec<String> = trace
            .offers
            .get(a)
            .map(|v| {
                v.iter()
                    .map(|o| {
                        format!(
                            "pool {} from {} pass {} depth {} price {:?}",
                            o.pool.0, o.from.0, o.pass, o.depth, o.price
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        tracing::warn!(asset = a.0, offers = ?offers, "graph: contested price — token and its pools disregarded");
    }
    let curated = curated_pools(book, &trace.depths, quota);
    let spokes = spoke_pools(book, &trace.depths, SPOKES);
    let mut keep: HashSet<u32> = curated
        .iter()
        .map(|c| c.pool.0)
        .chain(spokes.iter().map(|p| p.0))
        .collect();
    // The large-swap set: every pool of its depth on a pair of its tokens.
    let mut large_swap = 0usize;
    if let Some(l) = large {
        for (i, (pool, d)) in book.pools().iter().zip(&trace.depths).enumerate() {
            let (Some(d), Ok(i)) = (*d, u32::try_from(i)) else {
                continue;
            };
            if d >= l.min_depth
                && pool.assets.len() >= 2
                && pool.assets.iter().all(|a| l.tokens.contains(a))
                && keep.insert(i)
            {
                large_swap = large_swap.saturating_add(1);
            }
        }
        // Pools on large-swap pairs left out, and why: no depth (a coin
        // unpriced, or the pool not live) or under the floor.
        for (i, (pool, d)) in book.pools().iter().zip(&trace.depths).enumerate() {
            let Ok(i) = u32::try_from(i) else { continue };
            if pool.assets.len() < 2
                || !pool.assets.iter().all(|a| l.tokens.contains(a))
                || keep.contains(&i)
            {
                continue;
            }
            let unpriced: Vec<u16> = pool
                .assets
                .iter()
                .filter(|a| !trace.prices.contains_key(a))
                .map(|a| a.0)
                .collect();
            tracing::warn!(
                pool = %pool.address,
                live = pool.is_live(),
                depth = ?d,
                why = ?trace.why.get(i as usize).copied().flatten(),
                unpriced = ?unpriced,
                "graph: large-swap pool left out"
            );
        }
    }
    let stats = SelectStats {
        seeds: seed.len(),
        priced: trace.prices.len(),
        trusted: trace.trusted.len(),
        contested: trace.contested.len(),
        backbone: curated.len(),
        spokes: spokes.len(),
        large_swap,
    };
    Some((keep, stats))
}

/// Every interned token's `totalSupply` at the head.
async fn supplies(rpc: &HttpRpc, intern: &liq_config::Intern) -> HashMap<AssetId, U256> {
    let Ok(block) = rpc.block_number().await else {
        return HashMap::new();
    };
    let recs: Vec<_> = intern.assets().iter().collect();
    let mut out = HashMap::new();
    for chunk in recs.chunks(300) {
        let calls = chunk
            .iter()
            .map(|r| crate::pool_seed::call(r.address, totalSupplyCall {}.abi_encode()))
            .collect();
        let Some(res) = crate::pool_seed::aggregate(rpc, calls, block).await else {
            continue;
        };
        for (r, x) in chunk.iter().zip(res) {
            if let (true, Some(w)) = (x.success, x.returnData.get(..32)) {
                out.insert(r.id, U256::from_be_slice(w));
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// Writes the graph's pools at the head to `data/graph-pools.json`: the
    /// list the historical replay loads beside each event's own pools
    /// (`LIQ_REPLAY_POOLS=graph`), so its graph routes as production's does
    /// without reading every registry pool. Re-run to refresh it.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL; writes data/graph-pools.json"]
    fn write_graph_pools() {
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = liq_config::Intern::from_registry(&reg).unwrap();
        let mut load = crate::index::load_index(&root.join("config"), &intern, &reg);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(crate::startup::seed_index(&mut load, &url));
        let large = super::LargeSwap::load(&root.join("config/large-swap.toml"), &intern);
        let (keep, stats) = rt
            .block_on(super::select(&load, &intern, &url, large.as_ref()))
            .unwrap();
        let mut pools: Vec<String> = keep
            .iter()
            .filter_map(|i| load.book.get(liq_router::PoolId(*i)))
            .map(|p| format!("{:#x}", p.address))
            .collect();
        pools.sort();
        let out = root.join("data/graph-pools.json");
        std::fs::write(
            &out,
            serde_json::to_string_pretty(&pools).unwrap()
                + "
",
        )
        .unwrap();
        eprintln!(
            "{} graph pools ({stats:?}) written to {}",
            pools.len(),
            out.display()
        );
        assert!(!pools.is_empty());
    }
}
