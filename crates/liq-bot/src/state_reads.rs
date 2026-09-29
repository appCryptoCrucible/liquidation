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
//! * The reader thread, once per new head, runs both stages pinned to that
//!   head and publishes the answers — latest wins.
//! * The ingest thread folds a batch only while its block is still the tip
//!   (`AfterBlock::amend`): the writes land in that block's undo record, so
//!   a reorg unwinds them with the block.
//!
//! [`Protocol::state_reads`]: liq_protocol::Protocol::state_reads
//! [`Protocol::state_follow_ups`]: liq_protocol::Protocol::state_follow_ups
//! [`Protocol::apply_state_reads`]: liq_protocol::Protocol::apply_state_reads

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Bytes;
use arc_swap::ArcSwap;
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_protocol::{StateAnswer, StateRead};
use liq_state::StateView;

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
                if head <= last {
                    continue;
                }
                let batch = rt.block_on(read_state_block(protocols, &rpc, &reads, head));
                if batch.failed != 0 {
                    tracing::warn!(block = head, failed = batch.failed, reads = reads.len(), "protocol state reads failed");
                }
                last = head;
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
    use alloy_primitives::{address, Address, U256};
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
}
