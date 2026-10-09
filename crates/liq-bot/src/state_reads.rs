//! Protocol state read from chain every block, off the hot path.
//!
//! Some protocols' liquidatable size is only knowable by asking the
//! protocol: Fluid liquidates a vault's whole underwater tick range at
//! once, and each vault answers what that would be through a dead-address
//! simulation. The adapter names the reads ([`Protocol::state_reads`]),
//! the follow-up reads each answer needs ([`Protocol::state_follow_ups`]),
//! and folds the answers into its state ([`Protocol::apply_state_reads`]).
//!
//! * The hot thread collects the read set (startup and every
//!   [`crate::protocol_prices::READS_REFRESH_BLOCKS`]) and publishes it.
//! * Per-position reads ([`Protocol::position_reads`]: a Gearbox account
//!   after a multicall) join that set. The hot thread re-asks every position
//!   a block touched and every position its state batch changed, and
//!   republishes when the set moved; a full scan runs with each rebuild and
//!   whenever blocks arrived without their own `after_block` (one
//!   notification can carry several).
//! * The reader thread, once per new head, runs both stages pinned to that
//!   head and publishes the answers — latest wins. When the read set gains
//!   reads at the same head (an account needs a read after this block's
//!   multicall), it runs just those and republishes the head's batch with
//!   their answers added, so the account settles this block, not next.
//! * The ingest thread folds a batch only while its block is still the tip
//!   (`AfterBlock::amend`): the writes land in that block's undo record, so
//!   a reorg unwinds them with the block.
//!
//! [`Protocol::state_reads`]: liq_protocol::Protocol::state_reads
//! [`Protocol::state_follow_ups`]: liq_protocol::Protocol::state_follow_ups
//! [`Protocol::position_reads`]: liq_protocol::Protocol::position_reads
//! [`Protocol::apply_state_reads`]: liq_protocol::Protocol::apply_state_reads

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Bytes;
use arc_swap::ArcSwap;
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_protocol::{StateAnswer, StateRead};
use liq_state::StateView;
use liq_types::PositionId;

use crate::bind::BoundProtocol;
use crate::pool_seed::{aggregate, call};
use crate::protocol_prices::ViewRows;

/// How often the reader checks for a new head.
const POLL: Duration = Duration::from_millis(200);
/// Reads per multicall. Each first-stage read is a whole liquidation
/// simulation (hundreds of thousands of gas), so batches stay small enough
/// for a node's `eth_call` gas cap.
const BATCH: usize = 16;

/// Every adapter's first-stage reads: `(index into the bound protocols, read)`.
pub type StateReadSet = Vec<(usize, StateRead)>;

/// One read's outcome. `success == false` carries the revert data.
#[derive(Clone, Debug)]
pub struct Answered {
    pub protocol: usize,
    pub read: StateRead,
    pub success: bool,
    pub data: Bytes,
}

/// One block's answers, first stage then follow-ups.
#[derive(Debug, Default)]
pub struct StateBatch {
    pub block: u64,
    /// Publication order. A head can be published twice (its reads, then
    /// with reads added at the same head); the ingest thread folds each once.
    pub seq: u64,
    pub answers: Vec<Answered>,
    /// Reads whose multicall failed outright (their vaults get no update
    /// this block, and go stale).
    pub failed: usize,
}

impl StateBatch {
    /// This protocol's answers, borrowed as the adapter takes them.
    #[must_use]
    pub fn answers_for(&self, protocol: usize) -> Vec<StateAnswer<'_>> {
        self.answers
            .iter()
            .filter(|a| a.protocol == protocol)
            .map(|a| StateAnswer {
                read: &a.read,
                success: a.success,
                data: &a.data,
            })
            .collect()
    }
}

/// Shared between the hot thread and the reader.
pub struct StateReaderShared {
    pub reads: ArcSwap<StateReadSet>,
    pub latest: ArcSwap<Option<Arc<StateBatch>>>,
}

impl StateReaderShared {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reads: ArcSwap::from_pointee(Vec::new()),
            latest: ArcSwap::from_pointee(None),
        })
    }
}

/// Collect every adapter's first-stage reads against current state.
#[must_use]
pub fn collect_state_reads(protocols: &[BoundProtocol], view: StateView<'_>) -> StateReadSet {
    let rows = ViewRows(view);
    let mut out = Vec::new();
    for (i, p) in protocols.iter().enumerate() {
        for r in p.as_dyn().state_reads(&rows) {
            out.push((i, r));
        }
    }
    out
}

/// What [`Protocol::position_reads`] wants for one position now, tagged
/// with its protocol's index. Empty for an unknown id or protocol.
///
/// [`Protocol::position_reads`]: liq_protocol::Protocol::position_reads
#[must_use]
pub fn position_reads_for(
    protocols: &[BoundProtocol],
    view: &StateView<'_>,
    id: PositionId,
) -> Vec<(usize, StateRead)> {
    let Ok(pos) = view.position(id) else {
        return Vec::new();
    };
    let Some(i) = protocols.iter().position(|p| p.id() == pos.key.protocol) else {
        return Vec::new();
    };
    let Some(p) = protocols.get(i) else {
        return Vec::new();
    };
    p.as_dyn()
        .position_reads(pos)
        .into_iter()
        .map(|r| (i, r))
        .collect()
}

/// [`Protocol::resync_reads`] for one position, tagged with its protocol's
/// index. Empty for an unknown id, or an adapter that cannot resync.
///
/// [`Protocol::resync_reads`]: liq_protocol::Protocol::resync_reads
#[must_use]
pub fn resync_reads_for(
    protocols: &[BoundProtocol],
    view: &StateView<'_>,
    id: PositionId,
) -> StateReadSet {
    let Ok(pos) = view.position(id) else {
        return Vec::new();
    };
    let Some((i, p)) = protocols
        .iter()
        .enumerate()
        .find(|(_, p)| p.id() == pos.key.protocol)
    else {
        return Vec::new();
    };
    p.as_dyn()
        .resync_reads(pos)
        .into_iter()
        .map(|r| (i, r))
        .collect()
}

/// Every position's [`position_reads_for`], keyed by position. One pass over
/// the store.
#[must_use]
pub fn collect_position_reads(
    protocols: &[BoundProtocol],
    view: &StateView<'_>,
) -> HashMap<PositionId, StateReadSet> {
    let mut out = HashMap::new();
    let n = u32::try_from(view.len()).unwrap_or(u32::MAX);
    for id in (0..n).map(PositionId) {
        let reads = position_reads_for(protocols, view, id);
        if !reads.is_empty() {
            out.insert(id, reads);
        }
    }
    out
}

/// Reads in `now` that `before` did not have (first stage only; their
/// follow-ups come from their answers).
#[must_use]
pub fn added_reads(before: &StateReadSet, now: &StateReadSet) -> StateReadSet {
    now.iter()
        .filter(|r| !before.contains(r))
        .cloned()
        .collect()
}

/// The set the reader runs: adapter-wide reads, then every position's.
#[must_use]
pub fn merged_reads(
    adapter: &StateReadSet,
    positions: &HashMap<PositionId, StateReadSet>,
) -> StateReadSet {
    let mut ids: Vec<&PositionId> = positions.keys().collect();
    ids.sort_unstable();
    let mut out = adapter.clone();
    for id in ids {
        if let Some(r) = positions.get(id) {
            out.extend(r.iter().cloned());
        }
    }
    out
}

async fn run_stage(rpc: &HttpRpc, reads: &[(usize, StateRead)], block: u64, out: &mut StateBatch) {
    for chunk in reads.chunks(BATCH) {
        let calls = chunk
            .iter()
            .map(|(_, r)| call(r.target, r.calldata.to_vec()))
            .collect();
        let Some(res) = aggregate(rpc, calls, block).await else {
            out.failed = out.failed.saturating_add(chunk.len());
            continue;
        };
        for ((pi, read), row) in chunk.iter().zip(res) {
            out.answers.push(Answered {
                protocol: *pi,
                read: read.clone(),
                success: row.success,
                data: row.returnData,
            });
        }
    }
}

/// Both stages of `reads` at `block`.
pub async fn read_state_block(
    protocols: &[BoundProtocol],
    rpc: &HttpRpc,
    reads: &StateReadSet,
    block: u64,
) -> StateBatch {
    let mut batch = StateBatch {
        block,
        ..StateBatch::default()
    };
    run_stage(rpc, reads, block, &mut batch).await;
    let mut follow: StateReadSet = Vec::new();
    for a in &batch.answers {
        let Some(p) = protocols.get(a.protocol) else {
            continue;
        };
        let answer = StateAnswer {
            read: &a.read,
            success: a.success,
            data: &a.data,
        };
        for r in p.as_dyn().state_follow_ups(answer) {
            follow.push((a.protocol, r));
        }
    }
    if !follow.is_empty() {
        run_stage(rpc, &follow, block, &mut batch).await;
    }
    batch
}

/// The reader thread: one batch per new head, published to `shared.latest`.
pub fn spawn_state_reader(
    protocols: &'static [BoundProtocol],
    rpc_url: String,
    shared: Arc<StateReaderShared>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, std::io::Error> {
    std::thread::Builder::new()
        .name("liq-bot-state".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "state reader runtime refused — Fluid vaults never quote");
                    return;
                }
            };
            let rpc = match HttpRpc::connect(&rpc_url) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "state reader RPC connect failed — Fluid vaults never quote");
                    return;
                }
            };
            let mut last = 0u64;
            let mut last_reads: Arc<StateReadSet> = Arc::new(Vec::new());
            let mut seq = 0u64;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL);
                let reads = shared.reads.load_full();
                if reads.is_empty() {
                    continue;
                }
                let head = match rt.block_on(rpc.block_number()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "state reader: head unavailable");
                        continue;
                    }
                };
                let mut batch = if head > last {
                    rt.block_on(read_state_block(protocols, &rpc, &reads, head))
                } else if Arc::ptr_eq(&reads, &last_reads) {
                    continue;
                } else {
                    let added = added_reads(&last_reads, &reads);
                    last_reads = Arc::clone(&reads);
                    if added.is_empty() {
                        continue;
                    }
                    let mut b = rt.block_on(read_state_block(protocols, &rpc, &added, last));
                    if let Some(prev) = shared.latest.load_full().as_ref().as_ref() {
                        if prev.block == last {
                            let mut answers = prev.answers.clone();
                            answers.append(&mut b.answers);
                            b.answers = answers;
                            b.failed = b.failed.saturating_add(prev.failed);
                        }
                    }
                    b
                };
                if batch.failed != 0 {
                    tracing::warn!(block = batch.block, failed = batch.failed, reads = reads.len(), "protocol state reads failed");
                }
                seq = seq.saturating_add(1);
                batch.seq = seq;
                last = last.max(head);
                last_reads = reads;
                shared.latest.store(Arc::new(Some(Arc::new(batch))));
            }
        })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, B256, U256};
    use alloy_sol_types::{sol, SolCall};
    use liq_protocol::conformance::JournalStore;
    use liq_protocol::{MarketRows, StateWriter};
    use liq_types::{MarketId, PositionId};

    struct NoRows;
    impl MarketRows for NoRows {
        fn rows(&self, _: MarketId) -> Option<&[liq_protocol::MarketRow]> {
            None
        }
    }

    sol! {
        struct LiquidationStruct {
            address vault;
            address token0In;
            address token0Out;
            address token1In;
            address token1Out;
            uint256 inAmt;
            uint256 outAmt;
            uint256 inAmtWithAbsorb;
            uint256 outAmtWithAbsorb;
            bool absorbAvailable;
        }
        function getVaultLiquidation(address vault_, uint256 tokenInAmt_) returns (LiquidationStruct);
        function estimatePaybackPerfectInOneToken(address dex_, uint256 shares_, uint256 maxToken0_, uint256 maxToken1_) returns (uint256);
        function estimateWithdrawPerfectInOneToken(address dex_, uint256 shares_, uint256 minToken0_, uint256 minToken1_) returns (uint256);
    }

    /// Fluid's own resolvers (`deployments/mainnet/*.json` @ `9496626f`).
    const VAULT_RESOLVER: Address = address!("0xA5C3E16523eeeDDcC34706b0E6bE88b4c6EA95cC");
    const DEX_RESOLVER: Address = address!("0x11D80CfF056Cef4F9E6d23da8672fE9873e5cC07");

    /// Live, one block: every Fluid vault bound from chain, both read stages
    /// at the head, folded into a store. Each vault's stored liquidation
    /// equals Fluid's `VaultResolver.getVaultLiquidation` at the same block,
    /// in the variant the adapter chose, and each one-token amount equals
    /// the `DexResolver` estimate.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_fluid_vault_answers_match_the_resolvers() {
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rpc = HttpRpc::connect(&url).unwrap();
        let block = rt.block_on(rpc.block_number()).unwrap();
        let live = crate::live_rpc::LiveRpc::new(HttpRpc::connect(&url).unwrap());
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let intern = liq_config::Intern::from_registry(
            &liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let mut out = crate::bind::ProtocolLoad::default();
        crate::bind::push_fluid(
            &root.join("config/protocols"),
            &intern,
            Some((&live, block)),
            &mut out,
        );
        assert!(out.omitted.is_empty(), "omitted: {:?}", out.omitted);
        let protocols: &'static [BoundProtocol] = Box::leak(out.protocols.into_boxed_slice());
        let BoundProtocol::Fluid(f) = &protocols[0] else {
            panic!("fluid not bound");
        };
        let cfg = f.config();
        let reads: StateReadSet = liq_protocol::Protocol::state_reads(f, &NoRows)
            .into_iter()
            .map(|r| (0, r))
            .collect();
        let batch = rt.block_on(read_state_block(protocols, &rpc, &reads, block));
        assert_eq!(batch.failed, 0, "multicall batches failed");
        let mut st = JournalStore::default();
        let ts = 1;
        liq_protocol::Protocol::apply_state_reads(f, &mut st, ts, &batch.answers_for(0)).unwrap();
        let call =
            |to: Address, data: Vec<u8>| rt.block_on(rpc.call_at(to, data.into(), block)).unwrap();
        let (mut checked, mut smart) = (0usize, 0usize);
        for id in (0..st.positions_len()).map(PositionId) {
            let vault = st.position_key(id).unwrap().user;
            let pin = cfg.pin_of(vault).unwrap();
            let x: liq_adapters_fluid::VaultExtra = *st.extra(id).unwrap().view().unwrap();
            let raw = call(
                VAULT_RESOLVER,
                getVaultLiquidationCall {
                    vault_: vault,
                    tokenInAmt_: U256::ZERO,
                }
                .abi_encode(),
            );
            let l = getVaultLiquidationCall::abi_decode_returns(&raw).unwrap();
            let absorb = x.flags & liq_adapters_fluid::VaultExtra::ABSORB != 0;
            let (want_debt, want_col) = if absorb {
                (l.inAmtWithAbsorb, l.outAmtWithAbsorb)
            } else {
                (l.inAmt, l.outAmt)
            };
            assert_eq!(
                (U256::from(x.debt_units), U256::from(x.col_units)),
                (want_debt, want_col),
                "vault {vault} (type {}) absorb {absorb}",
                pin.vault_type
            );
            let n_col = pin.col_tokens().len();
            if pin.debt_tokens().len() == 2 && x.debt_units != 0 {
                for t in 0..2u16 {
                    let raw = call(
                        DEX_RESOLVER,
                        estimatePaybackPerfectInOneTokenCall {
                            dex_: pin.borrow,
                            shares_: U256::from(x.debt_units),
                            maxToken0_: if t == 0 { U256::MAX >> 1 } else { U256::ZERO },
                            maxToken1_: if t == 1 { U256::MAX >> 1 } else { U256::ZERO },
                        }
                        .abi_encode(),
                    );
                    let want =
                        estimatePaybackPerfectInOneTokenCall::abi_decode_returns(&raw).unwrap();
                    let slot = u16::try_from(n_col).unwrap() + t;
                    assert_eq!(
                        U256::from(st.debt(id, slot).unwrap()),
                        want,
                        "vault {vault} debt token {t}"
                    );
                }
                smart += 1;
            }
            if n_col == 2 && x.col_units != 0 {
                for t in 0..2u16 {
                    let raw = call(
                        DEX_RESOLVER,
                        estimateWithdrawPerfectInOneTokenCall {
                            dex_: pin.supply,
                            shares_: U256::from(x.col_units),
                            minToken0_: if t == 0 { U256::from(1u8) } else { U256::ZERO },
                            minToken1_: if t == 1 { U256::from(1u8) } else { U256::ZERO },
                        }
                        .abi_encode(),
                    );
                    let want =
                        estimateWithdrawPerfectInOneTokenCall::abi_decode_returns(&raw).unwrap();
                    assert_eq!(
                        U256::from(st.supply(id, t).unwrap()),
                        want,
                        "vault {vault} col token {t}"
                    );
                }
                smart += 1;
            }
            checked += 1;
        }
        eprintln!(
            "fluid block {block}: {} vaults bound, {} read, {checked} with a liquidation checked ({smart} smart sides)",
            cfg.vault_pins.len(),
            reads.len() / 2
        );
        assert!(cfg.vault_pins.len() >= 150);
    }

    /// Live, one block: open credit accounts of the bound v3.1 managers are
    /// marked stale through the adapter's own `StartMultiCall` fold, read
    /// through both stages here, and folded. Each settled account's debt,
    /// interest checkpoint and enabled mask equal the manager's own
    /// `calcDebtAndCollateral(account, DEBT_COLLATERAL)` at the same block —
    /// a different view than the `creditAccountInfo` getter the reads use —
    /// and every account leaves `STALE`. With the per-block interest reads
    /// folded too, each account with debt is valued at the block's
    /// timestamp: its total debt equals the manager's `debt + accruedInterest
    /// + accruedFees` to the wei.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_gearbox_accounts_settle_from_chain() {
        use alloy_sol_types::SolEvent;
        use liq_adapters_gearbox::events::views::ICreditManagerV3;
        use liq_adapters_gearbox::events::{facade, factory, DEBT_COLLATERAL_TASK};
        use liq_adapters_gearbox::layout::AccountExtra;
        use liq_protocol::{DecodedLog, Protocol};
        sol! { function creditAccounts() external view returns (address[] memory); }
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rpc = HttpRpc::connect(&url).unwrap();
        let block = rt.block_on(rpc.block_number()).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let intern = liq_config::Intern::from_registry(
            &liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        // `push_gearbox`'s steps, through an RPC that paces and retries
        // non-revert errors: a hosted endpoint rate-limits the bind's call
        // burst, and `LiveRpc`, built for the bot's own node, does not retry.
        struct Retrying<'a>(&'a tokio::runtime::Runtime, &'a HttpRpc);
        impl liq_adapters_gearbox::RegistryRpc for Retrying<'_> {
            fn eth_call(
                &self,
                to: Address,
                data: &[u8],
                block: u64,
            ) -> Result<Bytes, liq_adapters_gearbox::ConfigError> {
                // Paced under the endpoint's per-second budget.
                std::thread::sleep(std::time::Duration::from_millis(40));
                for attempt in 0..6u32 {
                    match self
                        .0
                        .block_on(self.1.call_at(to, Bytes::copy_from_slice(data), block))
                    {
                        Ok(b) => return Ok(b),
                        // A revert is an answer (the feed walk probes with
                        // views most feeds do not have); retry the rest.
                        Err(e) if e.to_string().contains("reverted") => break,
                        Err(e) => {
                            eprintln!("bind call {to} attempt {attempt}: {e}");
                            std::thread::sleep(std::time::Duration::from_millis(1000));
                        }
                    }
                }
                Err(liq_adapters_gearbox::ConfigError::RegistryCall(to))
            }
        }
        let raw = std::fs::read_to_string(root.join("config/protocols/gearbox.toml")).unwrap();
        let mut cfg = liq_adapters_gearbox::Config::from_toml(&raw).unwrap();
        let t0 = std::time::Instant::now();
        cfg.assert_live_registry(&Retrying(&rt, &rpc), block)
            .unwrap();
        eprintln!(
            "bound {} managers ({} skipped) in {:?}",
            cfg.managers.len(),
            cfg.skipped.len(),
            t0.elapsed()
        );
        cfg.bind_assets_from_intern(&intern).unwrap();
        let protocols: &'static [BoundProtocol] = Box::leak(Box::new([BoundProtocol::Gearbox(
            liq_adapters_gearbox::GearboxV3::new(cfg).unwrap(),
        )]));
        let BoundProtocol::Gearbox(g) = &protocols[0] else {
            panic!("gearbox not bound");
        };
        let call = |to: Address, data: Vec<u8>| rt.block_on(rpc.call_at(to, data.into(), block));
        let fold = |st: &mut JournalStore, address: Address, topics: Vec<B256>, data: Vec<u8>| {
            let log = DecodedLog {
                address,
                topics: &topics,
                data: &data,
                block,
                timestamp: 1,
            };
            g.apply_log(st, &log).unwrap();
        };
        let mut st = JournalStore::default();
        let mut accounts = Vec::new();
        for m in &g.config().managers {
            let ev = factory::AddCreditManager {
                creditManager: m.manager,
                masterCreditAccount: Address::ZERO,
            };
            fold(
                &mut st,
                m.factory,
                ev.encode_topics().into_iter().map(|t| t.0).collect(),
                ev.encode_data(),
            );
            let raw = call(m.manager, creditAccountsCall {}.abi_encode()).unwrap();
            for acc in creditAccountsCall::abi_decode_returns(&raw)
                .unwrap()
                .into_iter()
                .take(5)
            {
                let ev = facade::StartMultiCall {
                    creditAccount: acc,
                    caller: acc,
                };
                fold(
                    &mut st,
                    m.facade,
                    ev.encode_topics().into_iter().map(|t| t.0).collect(),
                    ev.encode_data(),
                );
                let dec = m
                    .tokens
                    .iter()
                    .find(|t| t.slot == 0)
                    .map_or(0, |t| t.decimals);
                accounts.push((m.manager, m.market, acc, dec));
            }
        }
        assert!(
            accounts.len() >= 5,
            "only {} open accounts found",
            accounts.len()
        );
        let view_store = &st;
        let reads: StateReadSet = (0..view_store.positions_len())
            .map(PositionId)
            .flat_map(|id| g.position_reads(view_store.view(id, 1).unwrap()))
            .map(|r| (0, r))
            .collect();
        assert_eq!(
            reads.len(),
            accounts.len(),
            "one info read per stale account"
        );
        let mut reads = reads;
        reads.extend(g.state_reads(&NoRows).into_iter().map(|r| (0, r)));
        let batch = rt.block_on(read_state_block(protocols, &rpc, &reads, block));
        assert_eq!(batch.failed, 0, "multicall batches failed");
        assert!(batch.answers.len() > reads.len(), "balance follow-ups ran");
        g.apply_state_reads(&mut st, 1, &batch.answers_for(0))
            .unwrap();
        let ts = {
            use alloy_provider::Provider;
            let provider = alloy_provider::ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_http(url.parse().unwrap());
            rt.block_on(async { provider.get_block_by_number(block.into()).await })
                .unwrap()
                .unwrap()
                .header
                .timestamp
        };
        // A flat 1.0 for every asset: health's debt value is then the total
        // debt in underlying units, scaled to WAD.
        let n = g
            .config()
            .assets
            .iter()
            .map(|a| a.asset.0)
            .max()
            .unwrap_or(0);
        let flat = liq_types::PriceVector(
            (0..=n)
                .map(|a| liq_types::Price {
                    asset: liq_types::AssetId(a),
                    price: liq_types::Ray::ONE,
                    source: liq_types::SourceKind::Canonical,
                    block,
                    ts,
                })
                .collect(),
        );
        let mut accrued = 0usize;
        for (manager, market, acc, dec) in accounts {
            let id = (0..st.positions_len())
                .map(PositionId)
                .find(|&id| {
                    let k = st.position_key(id).unwrap();
                    k.user == acc && k.market == market
                })
                .unwrap();
            let x: AccountExtra = *st.extra(id).unwrap().view().unwrap();
            assert_eq!(x.flags & AccountExtra::STALE, 0, "{acc} still stale");
            let raw = call(
                manager,
                ICreditManagerV3::calcDebtAndCollateralCall {
                    creditAccount: acc,
                    task: DEBT_COLLATERAL_TASK,
                }
                .abi_encode(),
            )
            .unwrap();
            let cdd =
                ICreditManagerV3::calcDebtAndCollateralCall::abi_decode_returns(&raw).unwrap();
            assert_eq!(U256::from(st.debt(id, 0).unwrap()), cdd.debt, "{acc} debt");
            // `_calcDebtAndCollateral` reports the checkpoint as 0 for a
            // debt-free account (`CreditManagerV3.sol:719` @ `510fc654`);
            // the getter keeps the stored one. Health reads it only with debt.
            let index = if cdd.debt.is_zero() {
                U256::ZERO
            } else {
                U256::from(x.cumulative_index_last_update)
            };
            assert_eq!(index, cdd.cumulativeIndexLastUpdate, "{acc} index");
            assert_eq!(
                U256::from(x.enabled_tokens_mask),
                cdd.enabledTokensMask,
                "{acc} mask"
            );
            if cdd.debt.is_zero() {
                continue;
            }
            // Accounts holding a token the registry never interned fail
            // closed in health; they are not this check's subject.
            let Ok(h) = g.health(st.view(id, ts).unwrap(), &flat) else {
                continue;
            };
            let total = cdd.debt + cdd.accruedInterest + cdd.accruedFees;
            let want =
                liq_adapters_gearbox::math::value_wad(total, liq_types::fixed::RAY, dec).unwrap();
            assert_eq!(
                h.debt_value.raw(),
                want,
                "{acc} total debt at block {block}"
            );
            accrued += 1;
        }
        assert!(accrued > 0, "no account with debt was compared");
    }

    /// Live: every Silo silo's totals read at ten consecutive blocks and
    /// folded, as the reader does each head, then projected 50 blocks
    /// (about ten minutes) on, against the silo's own `getDebtAssets()` /
    /// `getCollateralAssets()` there. Only borrowed silos that did not
    /// accrue over the span (an accrual resets the base). A silo with a
    /// measured rate must recover the span's interest, debt and collateral
    /// (net of fees), to within a tenth of it; one without must not have
    /// grown the 100 units a measurement needs over the reads (it is then
    /// held at the chain's totals, short of the interest since).
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_silo_totals_project_to_what_the_silo_reports_later() {
        use alloy_sol_types::SolEvent;
        use liq_adapters_silo_v2::events::{factory, silo};
        use liq_adapters_silo_v2::health::totals_at;
        sol! {
            function getDebtAssets() returns (uint256);
            function getCollateralAssets() returns (uint256);
            function utilizationData() returns (uint256, uint256, uint64);
            function getCurrentBlockTimestamp() returns (uint256);
        }
        const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rpc = HttpRpc::connect(&url).unwrap();
        let head = rt.block_on(rpc.block_number()).unwrap();
        let (first, b0, b1) = (head - 60, head - 51, head - 1);
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut out = crate::bind::ProtocolLoad::default();
        crate::bind::push_silo(&root.join("config/protocols"), &mut out);
        assert!(out.omitted.is_empty(), "omitted: {:?}", out.omitted);
        let protocols: &'static [BoundProtocol] = Box::leak(out.protocols.into_boxed_slice());
        let BoundProtocol::SiloV2(p) = &protocols[0] else {
            panic!("silo not bound");
        };
        let cfg = p.config();
        let call = |to: Address, data: Vec<u8>, block: u64| {
            rt.block_on(rpc.call_at(to, data.into(), block)).unwrap()
        };
        let ts_at = |block: u64| -> u64 {
            let raw = call(
                MULTICALL3,
                getCurrentBlockTimestampCall {}.abi_encode(),
                block,
            );
            u64::try_from(U256::from_be_slice(&raw[..32])).unwrap()
        };
        let ts1 = ts_at(b1);

        // The silos borrowed from on chain (the bot's own store knows which
        // from their events; asking only those keeps the reads per block
        // what a node serves).
        let silos: Vec<Address> = cfg
            .pairs
            .iter()
            .flat_map(|p| [p.silo0.silo, p.silo1.silo])
            .collect();
        let debts = rt
            .block_on(crate::pool_seed::aggregate(
                &rpc,
                silos
                    .iter()
                    .map(|s| crate::pool_seed::call(*s, getDebtAssetsCall {}.abi_encode()))
                    .collect(),
                first,
            ))
            .expect("debt totals");
        let borrowed: Vec<Address> = silos
            .iter()
            .zip(&debts)
            .filter(|(_, r)| {
                r.success && r.returnData.len() >= 32 && r.returnData[..32] != [0u8; 32]
            })
            .map(|(s, _)| *s)
            .collect();

        // List every pair through the adapter's own `NewSilo`, and give each
        // borrowed silo a unit of debt so its totals are asked for (the read
        // then replaces them with the chain's).
        let mut st = JournalStore::default();
        let fold = |st: &mut JournalStore, at: Address, ev: &dyn Fn() -> (Vec<B256>, Vec<u8>)| {
            let (topics, data) = ev();
            let log = liq_protocol::DecodedLog {
                address: at,
                topics: &topics,
                data: &data,
                block: 1,
                timestamp: 1,
            };
            liq_protocol::Protocol::apply_log(p, st, &log).unwrap();
        };
        for pair in &cfg.pairs {
            let listed = factory::NewSilo {
                implementation: Address::ZERO,
                token0: pair.silo0.token,
                token1: pair.silo1.token,
                silo0: pair.silo0.silo,
                silo1: pair.silo1.silo,
                siloConfig: pair.silo_config,
            };
            fold(&mut st, cfg.factories[0], &|| {
                (
                    listed.encode_topics().into_iter().map(|t| t.0).collect(),
                    listed.encode_data(),
                )
            });
            for side in [&pair.silo0, &pair.silo1] {
                if !borrowed.contains(&side.silo) {
                    continue;
                }
                let unit = silo::Borrow {
                    sender: Address::repeat_byte(1),
                    receiver: Address::repeat_byte(1),
                    owner: Address::repeat_byte(1),
                    assets: U256::from(1u8),
                    shares: U256::from(1u8),
                };
                fold(&mut st, side.silo, &|| {
                    (
                        unit.encode_topics().into_iter().map(|t| t.0).collect(),
                        unit.encode_data(),
                    )
                });
            }
        }
        struct Rows<'a>(&'a JournalStore);
        impl MarketRows for Rows<'_> {
            fn rows(&self, m: MarketId) -> Option<&[liq_protocol::MarketRow]> {
                self.0.markets(m).ok()
            }
        }
        let reads: StateReadSet = liq_protocol::Protocol::state_reads(p, &Rows(&st))
            .into_iter()
            .map(|r| (0, r))
            .collect();
        assert_eq!(reads.len(), borrowed.len() * 4);
        for b in first..=b0 {
            let ts = ts_at(b);
            // A free-tier endpoint throttles bursts: retry a block's batch.
            let mut batch = rt.block_on(read_state_block(protocols, &rpc, &reads, b));
            for _ in 0..4 {
                if batch.failed == 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
                batch = rt.block_on(read_state_block(protocols, &rpc, &reads, b));
            }
            assert_eq!(batch.failed, 0, "multicall batches failed");
            liq_protocol::Protocol::apply_state_reads(p, &mut st, ts, &batch.answers_for(0))
                .unwrap();
        }

        let word = |raw: &[u8], i: usize| U256::from_be_slice(&raw[i * 32..i * 32 + 32]);
        let (mut compared, mut unmeasured, mut worst) = (0usize, 0usize, 0u128);
        for pair in &cfg.pairs {
            for (slot, side) in [(0u16, &pair.silo0), (1, &pair.silo1)] {
                let row = st
                    .market(liq_protocol::MarketSlot {
                        market: pair.market,
                        slot,
                    })
                    .unwrap();
                let body = row.body::<liq_adapters_silo_v2::layout::SiloRow>().unwrap();
                let debt0 = body.total_debt_assets;
                if debt0 == 0 {
                    continue;
                }
                let irts =
                    |b: u64| word(&call(side.silo, utilizationDataCall {}.abi_encode(), b), 2);
                if irts(first) != irts(b1) {
                    continue; // accrued over the span: a new base
                }
                let measured =
                    body.flags & liq_adapters_silo_v2::layout::SiloRow::GROWTH_KNOWN != 0;
                if !measured {
                    let at_first = word(
                        &call(side.silo, getDebtAssetsCall {}.abi_encode(), first),
                        0,
                    );
                    let grew = u128::try_from(U256::from(debt0) - at_first).unwrap();
                    assert!(
                        grew < 100,
                        "silo {:#x}: grew {grew} over the reads but no rate was measured",
                        side.silo
                    );
                    unmeasured += 1;
                    continue;
                }
                let debt1 = word(&call(side.silo, getDebtAssetsCall {}.abi_encode(), b1), 0);
                let coll1 = word(
                    &call(side.silo, getCollateralAssetsCall {}.abi_encode(), b1),
                    0,
                );
                let (pd, pc) = totals_at(row, ts1).unwrap();
                for (what, base, proj, actual) in [
                    ("debt", debt0, pd, debt1),
                    ("collateral", body.total_collateral_assets, pc, coll1),
                ] {
                    let actual = u128::try_from(actual).unwrap();
                    let grew = actual - base;
                    let err = proj.abs_diff(actual);
                    assert!(
                        err <= grew / 10 + 1,
                        "silo {:#x} {what}: projected {proj}, chain {actual}, the span's growth {grew}",
                        side.silo
                    );
                    if let Some(bps) = (err * 10_000).checked_div(grew) {
                        worst = worst.max(bps);
                    }
                }
                compared += 1;
            }
        }
        eprintln!(
            "silo blocks {b0}..{b1}: {compared} borrowed silos projected ({unmeasured} still measuring); worst error {worst} bps of the span's interest"
        );
        assert!(compared > 0);
    }
}
