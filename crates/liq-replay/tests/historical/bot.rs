//! The bot's own searcher, built the way `liq_bot::startup::run` builds it,
//! on a seeded store and the pre-state endpoint instead of a node: one
//! protocol bound, the pools around the liquidation's tokens, its own price
//! and state readers, the drain, and the compiled Executor simulated on the
//! pre-state. No ExEx, no sending: jobs land in an inbox the test reads.

use alloy_primitives::Address;
use liq_bot::bind::{self, BoundProtocol};
use liq_bot::drain::{DrainJoin, LiveSim};
use liq_bot::index::{self, BoundIndex};
use liq_bot::lease::SubmitLease;
use liq_bot::{bands, gas_model, protocol_prices, routes, stall, startup, state_reads};
use liq_config::rpc::HttpRpc;
use liq_config::{Intern, Registry};
use liq_exec::path::{ExecInbox, ExecJob};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct BotSpec<'a> {
    /// The repository's `config/`.
    pub repo_config: &'a Path,
    /// The protocol TOML to bind, e.g. `aave-v3.toml`; the others are left out.
    pub protocol_toml: &'a str,
    /// The liquidation's emitter. A Compound config is cut to the fork that
    /// lists it: binding reads each fork's facts live, and the harness
    /// answers those over the network (a local node answers them in
    /// production).
    pub focus: Address,
    /// The pre-state endpoint.
    pub rpc_url: &'a str,
    /// The bot's tip: block N−1.
    pub tip: u64,
    /// Tokens the liquidation touches; pools are kept when both their tokens
    /// are these or the majors.
    pub tokens: &'a [Address],
    pub state: Arc<dyn liq_sim::StateProviderFactory>,
    pub operator: Address,
    pub backrun_operator: Address,
    pub profit_sink: Address,
    /// Scratch directory for this run's config copy.
    pub scratch: &'a Path,
}

pub struct Bot {
    pub shared: &'static liq_bot::shared::Shared,
    pub adapters: &'static [BoundProtocol],
    pub index: &'static BoundIndex,
    pub hook: DrainJoin,
    pub jobs: crossbeam_channel::Receiver<ExecJob>,
    pub intern: Intern,
    pub registry: Registry,
    pub price_reader: Arc<protocol_prices::ReaderShared>,
    pub state_reader: Arc<state_reads::StateReaderShared>,
    pub stop: Arc<AtomicBool>,
    pub pools_kept: usize,
    pub bands: Arc<bands::BandShared>,
    /// Every bundle the drain asked its simulator to verify.
    pub simulated: Arc<std::sync::Mutex<Vec<liq_sim::Bundle>>>,
    /// Where the simulator placed the Executor.
    pub executor: Address,
}

/// Majors an exit may route through.
const MAJORS: [Address; 7] = [
    alloy_primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
    alloy_primitives::address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC
    alloy_primitives::address!("dAC17F958D2ee523a2206206994597C13D831ec7"), // USDT
    alloy_primitives::address!("6B175474E89094C44Da98b954EedeAC495271d0F"), // DAI
    alloy_primitives::address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), // WBTC
    alloy_primitives::address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"), // wstETH
    alloy_primitives::address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"), // cbBTC
];

/// `compound-v2.toml` with only the forks that list `focus`.
fn compound_focused(raw: &str, focus: Address) -> String {
    let needle = format!("{focus:x}");
    // The generator writes CRLF on Windows.
    let raw = raw.replace("\r\n", "\n");
    let mut parts = raw.split("\n[[forks]]\n");
    let mut out = parts.next().unwrap_or_default().to_string();
    for fork in parts {
        // The official Unitroller is required beside any other fork.
        let fork_lc = fork.to_lowercase();
        if fork_lc.contains(&needle)
            || fork_lc.contains("0x3d9819210a31b4961b30ef54be2aed79b9c9cd3b")
        {
            out.push_str("\n[[forks]]\n");
            out.push_str(fork);
        }
    }
    out
}

fn copy_config(from: &Path, to: &Path, keep_protocol: &str, focus: Address) -> std::io::Result<()> {
    // Aave V3 is always bound beside the protocol under test: its oracle is
    // the USD getter the drain restates ratio prices with (Morpho), as in
    // production, where every protocol is bound.
    let keep = [keep_protocol, "aave-v3.toml"];
    if to.exists() {
        std::fs::remove_dir_all(to)?;
    }
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            let protocols = e.file_name() == "protocols";
            std::fs::create_dir_all(&target)?;
            for f in std::fs::read_dir(e.path())? {
                let f = f?;
                if protocols && !keep.iter().any(|k| f.file_name() == *k) {
                    continue;
                }
                if protocols && f.file_name() == "compound-v2.toml" {
                    let raw = std::fs::read_to_string(f.path())?;
                    std::fs::write(target.join(f.file_name()), compound_focused(&raw, focus))?;
                } else if f.file_type()?.is_file() {
                    std::fs::copy(f.path(), target.join(f.file_name()))?;
                }
            }
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

pub async fn build(spec: BotSpec<'_>) -> Result<Bot, String> {
    let dir: PathBuf = spec.scratch.join("config");
    copy_config(spec.repo_config, &dir, spec.protocol_toml, spec.focus)
        .map_err(|e| e.to_string())?;
    let mut config = liq_config::load(&dir).map_err(|e| e.to_string())?;
    config.rpc_url = spec.rpc_url.to_string();
    // `LIQ_REGISTRY_FILE`: a candidate registry (pools proposed but not yet
    // admitted, `data/review/*-registry.candidate.json`, with its
    // `asset-ids.json` beside it) in place of the committed one.
    config.registry_path = match std::env::var("LIQ_REGISTRY_FILE") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => spec
            .repo_config
            .parent()
            .ok_or("config has no parent")?
            .join("registry/registry.json"),
    };
    let registry = Registry::from_path(&config.registry_path).map_err(|e| e.to_string())?;
    let intern = Intern::from_registry(&registry).map_err(|e| e.to_string())?;

    // Adapters: the one protocol, with the live reads four adapters assert.
    let rpc = HttpRpc::connect(spec.rpc_url).map_err(|e| e.to_string())?;
    let live = liq_bot::live_rpc::LiveRpc::new(rpc);
    let loaded = bind::load_protocols(&dir, &intern, Some((&live, spec.tip)));
    // The other protocols' TOMLs were left out on purpose; only this one's
    // omission is a failure.
    let stem = spec.protocol_toml.trim_end_matches(".toml");
    if let Some((name, why)) = loaded
        .omitted
        .iter()
        .find(|(n, _)| n.trim_end_matches(".toml") == stem)
    {
        return Err(format!("{name} omitted: {why}"));
    }
    let adapters = bind::leak_protocols(loaded);

    // The index as production loads it, from the repository's config,
    // except that by default only pools whose two tokens are the
    // liquidation's or the majors are kept (a run reads every kept pool's
    // state); `LIQ_REPLAY_POOLS=graph` adds the graph's own pools. The bot can
    // route no better here than in production, never better.
    let mut keep: HashSet<Address> = spec.tokens.iter().chain(MAJORS.iter()).copied().collect();
    // A token with no pool of its own exits through its unwrap: what it
    // unwraps into is kept too (and that token's unwrap, in turn), and for
    // a Curve LP, which is its own pool, the pool's coins. Without them the
    // loader refuses the unwrap, which production's full registry does not.
    let mut lps: HashSet<Address> = HashSet::new();
    let mut frontier: Vec<Address> = spec.tokens.to_vec();
    while let Some(t) = frontier.pop() {
        let Some(u) = registry.tokens.get(&t).and_then(|e| e.unwrap) else {
            continue;
        };
        if keep.insert(u.into) {
            frontier.push(u.into);
        }
        if let Some(pool) = registry.pools.get(&t) {
            lps.insert(t);
            for c in [pool.token0, pool.token1]
                .into_iter()
                .chain(pool.coins.iter().copied())
            {
                if keep.insert(c) {
                    frontier.push(c);
                }
            }
        }
    }
    let mut filtered = registry.clone();
    // `LIQ_REPLAY_POOLS=graph`: also the graph's own pools (the curated
    // backbone and every token's spokes, `data/graph-pools.json`, written
    // by `liq_bot::graph_build`'s `write_graph_pools`), so the graph routes
    // as production's does without reading every registry pool.
    let graph_pools: HashSet<Address> = if std::env::var("LIQ_REPLAY_POOLS").as_deref()
        == Ok("graph")
    {
        let path = std::path::Path::new(spec.repo_config).join("../data/graph-pools.json");
        let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str::<Vec<Address>>(&raw)
            .map_err(|e| e.to_string())?
            .into_iter()
            .collect()
    } else {
        HashSet::new()
    };
    // Every pool on a pair of the large-swap set (`config/large-swap.toml`)
    // too: production holds them all and routes large sales across them,
    // judging each on the block's own state, not today's.
    let large: HashSet<Address> =
        std::fs::read_to_string(std::path::Path::new(spec.repo_config).join("large-swap.toml"))
            .ok()
            .and_then(|raw| raw.parse::<toml::Table>().ok())
            .and_then(|t| t.get("tokens")?.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
    filtered.pools.retain(|addr, p| {
        lps.contains(addr)
            || graph_pools.contains(addr)
            || (keep.contains(&p.token0) && keep.contains(&p.token1))
            || (!large.is_empty()
                && large.contains(&p.token0)
                && large.contains(&p.token1)
                && p.coins.iter().all(|c| large.contains(c)))
    });
    let pools_kept = filtered.pools.len();
    let mut load = index::load_index(spec.repo_config, &intern, &filtered);
    // Seeded exactly as startup seeds it: prices, pools, flash sources.
    startup::seed_index(&mut load, spec.rpc_url).await;
    liq_bot::graph_build::attach(&mut load, &intern, spec.rpc_url, spec.repo_config).await;
    let index = index::leak_index(load);

    let stop = Arc::new(AtomicBool::new(false));
    let (warm, routes) = routes::warm_handles();
    let shared = startup::leak_shared(
        &config,
        SubmitLease::granted_shadow(),
        routes,
        intern.asset_id_capacity(),
    );
    let band_shared = bands::BandShared::new();
    routes::spawn_warm_thread(
        warm,
        Arc::clone(&stop),
        Arc::clone(&index.book),
        Some(Arc::clone(&band_shared)),
    )
    .map_err(|e| e.to_string())?;

    let mut assemble = bind::intern_view(&intern).with_bands(Arc::clone(&band_shared));
    bind::intern_adapter_tokens(&mut assemble, adapters);
    let model = gas_model::GasModel::load(&dir.join("liq-gas.toml"));
    let wrap = model
        .as_ref()
        .map_or_else(bind::WrapGas::default, gas_model::GasModel::select_wrap);
    let weth = bind::registry_weth(&intern).ok_or("registry WETH")?;
    let mut select = bind::select_bind(wrap, weth, intern.protocol("aave-v4"));
    if let Some(sb) = select.as_mut() {
        for pin in bind::compound_validate_pins(adapters) {
            sb.validate.add_compound(pin);
        }
    }
    let bid_cfg = bind::load_bid_config(&dir.join("bid.toml"), &intern);
    let oracle = liq_router::GasOracle::with_priority_cap(liq_router::gas::DEFAULT_PRIORITY_CAP);
    let fee = oracle.as_ref().and_then(|o| bind::fee_from_oracle(o, 0));

    let (inbox, jobs) = ExecInbox::pair(64);
    let lease = Arc::new(AtomicBool::new(true));
    let node_sim = liq_sim::NodeSim::compiled(
        Arc::clone(&spec.state),
        &liq_sim::ExecutorSpec::mainnet(spec.operator, spec.backrun_operator, spec.profit_sink),
    )
    .map_err(|e| format!("compiled Executor: {e}"))?;
    let simulated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sim = super::drive::RecordingSim {
        inner: LiveSim::new(node_sim, lease),
        seen: Arc::clone(&simulated),
    };
    let executor = liq_bot::drain::DrainSim::executor(&sim);

    let price_reader = protocol_prices::ReaderShared::new();
    protocol_prices::spawn_reader(
        adapters,
        spec.rpc_url.to_string(),
        Arc::clone(&price_reader),
        Arc::clone(&stop),
    )
    .map_err(|e| e.to_string())?;
    let state_reader = state_reads::StateReaderShared::new();
    state_reads::spawn_state_reader(
        adapters,
        spec.rpc_url.to_string(),
        Arc::clone(&state_reader),
        Arc::clone(&stop),
    )
    .map_err(|e| e.to_string())?;

    let hook = DrainJoin::live(
        Arc::clone(&shared.flash),
        shared.routes.clone(),
        assemble,
        Some(inbox),
        Some(spec.operator),
        config.chain_id,
        adapters,
        select,
        fee,
        oracle,
    )
    .with_engine_capacity(intern.asset_id_capacity(), 1 << 12)
    .with_assets(&intern)
    .with_gas_model(model.as_ref(), &|f: &str| {
        bind::resolve_family(&intern, adapters, f)
    })
    .with_index(index)
    .with_bid_cfg(bid_cfg)
    .with_header_clock(Arc::new(stall::HeaderClock::new()))
    .with_price_reader(Arc::clone(&price_reader))
    .with_state_reader(Arc::clone(&state_reader))
    .with_sim(Box::new(sim))
    .with_backrun_operator(spec.backrun_operator);

    Ok(Bot {
        shared,
        adapters,
        index,
        hook,
        jobs,
        intern,
        registry,
        price_reader,
        state_reader,
        stop,
        pools_kept,
        bands: band_shared,
        simulated,
        executor,
    })
}
