//! Governance liquidations (GUIDE 08 §5 `ParamChange`).
//!
//! Two timelocked, permissionless sources of parameter changes:
//! - Aave: a queued payload anyone may execute from the first block with
//!   `timestamp > queuedAt + delay`;
//! - Sky spells (Spark's parameters): a scheduled spell anyone may `cast()`
//!   from `eta` inclusive, inside office hours.
//!
//! The worker thread watches each head; when the next block can apply one
//! it simulates the call on the head state as that next block and hands the
//! resulting logs to the hot thread. The hot thread lays
//! those logs over committed state through the adapters' own `apply_log`
//! (a [`liq_state::Overlay`], nothing committed), folds the positions they
//! touch, and plans one liquidation per account for the next block. The
//! plans carry the action, so the Executor applies the change itself
//! before liquidating. The same runs again every block the change stays
//! executable and unapplied.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Bytes;
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{sol, SolCall};
use liq_config::{Intern, Registry};
use liq_engine::{Candidate, Engine, ProtocolPrices, TriggerCause, World};
use liq_exec::chain::{ChainClient, Head, SimCall, SimLog};
use liq_exec::gov::GovAction;
use liq_flash::{FlashIndex, Haircut};
use liq_oracle::{executable_at, GovernanceConfig, GovernancePoller};
use liq_protocol::{DecodedLog, DirtySet, Protocol, RouteCache};
use liq_state::{Overlay, StateStore, StateView};
use liq_types::{PositionId, ProtocolId};

use crate::bind::BoundProtocol;

/// Seconds between slots. The next block's timestamp is at least this after
/// the head's; a missed slot only makes it later, which stays inside the
/// payload's window except in the last slot before it expires.
pub const SLOT_SECONDS: u64 = 12;
/// Payload ids re-read at every head.
pub const PAYLOAD_LOOKBACK: u64 = 64;
/// How often the worker asks the node for its head.
const HEAD_POLL: Duration = Duration::from_millis(250);
/// Ring capacity worker → hot thread.
pub const GOV_RING: usize = 64;
/// Caller of the simulated governance call; neither has an access check.
const SIM_CALLER: Address = alloy_primitives::address!("000000000000000000000000000000000000dEaD");
/// Sky chainlog; `MCD_ADM` is the Chief whose `hat` is the spell that can
/// be scheduled.
pub const SKY_CHAINLOG: Address =
    alloy_primitives::address!("dA0Ab1e0017DEbCd72Be8599041a2aa3bA7e740F");

sol! {
    interface IChainlog {
        function getAddress(bytes32 key) external view returns (address);
    }
    interface IChief {
        function hat() external view returns (address);
    }
    /// `DssExec` (dss-exec-lib).
    interface IDssSpell {
        function eta() external view returns (uint256);
        function done() external view returns (bool);
        function expiration() external view returns (uint256);
        function nextCastTime() external view returns (uint256);
        function cast() external;
    }
}

/// A governance action simulated as the next block, on the head state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GovSim {
    pub action: GovAction,
    /// Head the simulation ran on; the overlay is only valid on this tip.
    pub base_block: u64,
    pub target_block: u64,
    pub target_ts: u64,
    pub exec_gas: u64,
    pub logs: Vec<SimLog>,
}

#[derive(Debug, thiserror::Error)]
pub enum GovError {
    #[error("overlay: {0}")]
    State(#[from] liq_state::StateError),
    #[error("apply_log: {0}")]
    Protocol(#[from] liq_protocol::ProtocolError),
}

/// `(emitter, topic0)` → indexes into the bound protocols, from each
/// adapter's own subscriptions. Built once, off the hot loop.
pub struct GovRoutes {
    map: HashMap<(Address, B256), Vec<usize>>,
}

impl GovRoutes {
    #[must_use]
    pub fn new(protocols: &[BoundProtocol]) -> Self {
        let mut map: HashMap<(Address, B256), Vec<usize>> = HashMap::new();
        for (i, p) in protocols.iter().enumerate() {
            for f in p.as_dyn().subscriptions() {
                let slot = map.entry((f.address, f.topic0)).or_default();
                if !slot.contains(&i) {
                    slot.push(i);
                }
            }
        }
        Self { map }
    }

    fn get(&self, address: Address, topic0: B256) -> &[usize] {
        self.map
            .get(&(address, topic0))
            .map_or(&[][..], Vec::as_slice)
    }
}

/// Apply `sim.logs` through the adapters that subscribe to them, into
/// `overlay` over `store`. Returns what each log dirtied, with its protocol.
/// Logs no adapter tracks (the executor's and controller's own) are skipped.
pub fn apply_sim_logs(
    protocols: &[BoundProtocol],
    routes: &GovRoutes,
    store: &StateStore,
    overlay: &mut Overlay,
    sim: &GovSim,
) -> Result<Vec<(ProtocolId, DirtySet)>, GovError> {
    let mut writer = overlay.writer(store)?;
    let mut out = Vec::new();
    for log in &sim.logs {
        let Some(topic0) = log.topics.first() else {
            continue;
        };
        for &i in routes.get(log.address, *topic0) {
            let Some(p) = protocols.get(i) else {
                continue;
            };
            let decoded = DecodedLog {
                address: log.address,
                topics: &log.topics,
                data: &log.data,
                block: sim.target_block,
                timestamp: sim.target_ts,
            };
            let set = p.as_dyn().apply_log(&mut writer, &decoded)?;
            if !matches!(set, DirtySet::None) {
                out.push((p.id(), set));
            }
        }
    }
    Ok(out)
}

/// Positions whose health the dirty sets can change, sorted and unique.
/// A reprice or accrual row reaches every position holding that slot in
/// that market; protocol-wide reaches every position of the protocol.
pub fn affected_positions(
    view: &StateView<'_>,
    dirty: &[(ProtocolId, DirtySet)],
) -> Vec<PositionId> {
    let mut out: Vec<PositionId> = Vec::new();
    let mut rows: Vec<liq_protocol::MarketSlot> = Vec::new();
    let mut wide: Vec<ProtocolId> = Vec::new();
    for (protocol, set) in dirty {
        match set {
            DirtySet::None => {}
            DirtySet::Positions(ids) => out.extend(ids.iter().copied()),
            DirtySet::MarketAccrual(r) | DirtySet::MarketReprice(r) => {
                rows.extend(r.iter().copied())
            }
            DirtySet::ProtocolWide => wide.push(*protocol),
        }
    }
    if !rows.is_empty() || !wide.is_empty() {
        // One pass over the store, however many rows the payload touched.
        let n = u32::try_from(view.len()).unwrap_or(u32::MAX);
        for id in (0..n).map(PositionId) {
            let Ok(p) = view.position(id) else { continue };
            if wide.contains(&p.key.protocol)
                || rows
                    .iter()
                    .any(|r| r.market == p.key.market && p.config.contains(r.slot))
            {
                out.push(id);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The accounts `sim`'s change makes liquidatable and fundable that are not
/// without it, most valuable first, each with cause `ParamChange` on its own
/// market; and the overlay holding the change (for pins and plan views).
/// Nothing is committed and no engine table is touched.
#[allow(clippy::too_many_arguments)] // the pieces of an engine World, plus the change
pub fn newly_liquidatable(
    engine: &mut Engine,
    protocols: &[BoundProtocol],
    routes: &GovRoutes,
    store: &StateStore,
    sim: &GovSim,
    flash: &FlashIndex,
    route_cache: &dyn RouteCache,
    haircut: Haircut,
    prices: Option<&dyn ProtocolPrices>,
) -> Result<(Overlay, Vec<Candidate>), GovError> {
    let mut ov = Overlay::new();
    let dirty = apply_sim_logs(protocols, routes, store, &mut ov, sim)?;
    if dirty.is_empty() {
        tracing::info!(action = ?sim.action, "governance change touches no tracked market");
        return Ok((ov, Vec::new()));
    }
    let ids = affected_positions(&store.view(sim.target_ts), &dirty);
    let proto_refs: Vec<&dyn Protocol> = protocols.iter().map(BoundProtocol::as_dyn).collect();
    // Replaced per candidate by its own market below.
    let cause = TriggerCause::ParamChange {
        market: liq_types::MarketId(0),
    };
    let mut after = engine.probe(
        &World {
            view: store.view_with(&ov, sim.target_ts)?,
            protocols: &proto_refs,
            flash,
            routes: route_cache,
            haircut,
            overlay: prices,
        },
        &ids,
        &cause,
    );
    let after_ids: Vec<PositionId> = after.iter().map(|c| c.position).collect();
    let before = engine.probe(
        &World {
            view: store.view(sim.target_ts),
            protocols: &proto_refs,
            flash,
            routes: route_cache,
            haircut,
            overlay: prices,
        },
        &after_ids,
        &cause,
    );
    // Already liquidatable without the change: the ordinary drain has it.
    after.retain(|c| !before.iter().any(|b| b.position == c.position));
    for c in &mut after {
        c.cause = TriggerCause::ParamChange {
            market: c.quote.key.market,
        };
    }
    after.sort_by_key(|c| std::cmp::Reverse(c.est_value));
    tracing::info!(
        action = ?sim.action,
        target = sim.target_block,
        affected = ids.len(),
        newly_liquidatable = after.len(),
        "governance change evaluated"
    );
    Ok((ov, after))
}

/// The next block after `head`: number and earliest timestamp.
fn next_block(head: Head) -> Option<(u64, u64)> {
    Some((
        head.number.checked_add(1)?,
        head.timestamp.checked_add(SLOT_SECONDS)?,
    ))
}

/// Simulate `action` on `head` as the next block. `None` when it reverts
/// (not due, office hours, already applied) or the node refuses.
pub async fn simulate_action(chain: &ChainClient, head: Head, action: GovAction) -> Option<GovSim> {
    let (target_block, target_ts) = next_block(head)?;
    let (to, data) = match action {
        GovAction::Payload { controller, id } => {
            (controller, liq_exec::executor::execute_payload_calldata(id))
        }
        GovAction::Spell(spell) => (spell, Bytes::from(IDssSpell::castCall {}.abi_encode())),
    };
    let call = SimCall {
        from: SIM_CALLER,
        to,
        data,
        gas: None,
        gas_price: None,
    };
    let r = match chain
        .simulate(head.number, target_block, target_ts, None, &[call])
        .await
    {
        Ok(mut r) => r.pop()?,
        Err(e) => {
            tracing::error!(error = %e, ?action, "governance simulation failed");
            return None;
        }
    };
    if !r.success {
        tracing::debug!(
            ?action,
            block = target_block,
            "governance call reverts in simulation — not sent"
        );
        return None;
    }
    Some(GovSim {
        action,
        base_block: head.number,
        target_block,
        target_ts,
        exec_gas: r.gas_used,
        logs: r.logs,
    })
}

/// Aave payloads the block after `head` can execute, simulated on `head` as
/// that block.
pub async fn simulate_executable(
    poller: &GovernancePoller,
    chain: &ChainClient,
    head: Head,
) -> Vec<GovSim> {
    let Some((_, target_ts)) = next_block(head) else {
        return Vec::new();
    };
    let queued = match poller.queued_payloads(head.number).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, block = head.number, "governance payload read failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for q in queued {
        match executable_at(&q.view, target_ts) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::error!(error = %e, payload = q.view.id, "payload window refused");
                continue;
            }
        }
        let action = GovAction::Payload {
            controller: q.controller,
            id: q.view.id,
        };
        if let Some(sim) = simulate_action(chain, head, action).await {
            out.push(sim);
        }
    }
    out
}

/// Sky spells that may still be cast. A spell can only be scheduled while it
/// is the Chief's hat, so watching the hat every head finds each one; it is
/// kept until it is cast or expires.
pub struct SpellWatch {
    chief: Address,
    spells: Vec<Address>,
}

impl SpellWatch {
    /// The Chief from the Sky chainlog (`MCD_ADM`) at `block`.
    pub async fn from_chainlog(chain: &ChainClient, block: u64) -> Option<Self> {
        let key = B256::right_padding_from(b"MCD_ADM");
        let chief = read(
            chain,
            SKY_CHAINLOG,
            IChainlog::getAddressCall { key },
            block,
        )
        .await?;
        (!chief.is_zero()).then_some(Self {
            chief,
            spells: Vec::new(),
        })
    }

    /// Spells tracked right now, oldest first.
    #[must_use]
    pub fn spells(&self) -> &[Address] {
        &self.spells
    }

    /// Spells the block after `head` can cast, simulated on `head` as that
    /// block. `nextCastTime` (eta and office hours) is only a pre-filter;
    /// the simulation decides.
    pub async fn simulate_castable(&mut self, chain: &ChainClient, head: Head) -> Vec<GovSim> {
        let Some((_, target_ts)) = next_block(head) else {
            return Vec::new();
        };
        let n = head.number;
        if let Some(hat) = read(chain, self.chief, IChief::hatCall {}, n).await {
            if !hat.is_zero() && !self.spells.contains(&hat) {
                self.spells.push(hat);
            }
        }
        let mut keep = Vec::new();
        let mut out = Vec::new();
        for spell in std::mem::take(&mut self.spells) {
            let (Some(done), Some(expiration), Some(eta)) = (
                read(chain, spell, IDssSpell::doneCall {}, n).await,
                read(chain, spell, IDssSpell::expirationCall {}, n).await,
                read(chain, spell, IDssSpell::etaCall {}, n).await,
            ) else {
                continue; // not a DssExec spell
            };
            if done || U256::from(head.timestamp) >= expiration {
                continue;
            }
            keep.push(spell);
            if eta.is_zero() {
                continue; // not scheduled yet
            }
            let due = read(chain, spell, IDssSpell::nextCastTimeCall {}, n).await;
            if due.is_some_and(|t| t <= U256::from(target_ts)) {
                if let Some(sim) = simulate_action(chain, head, GovAction::Spell(spell)).await {
                    out.push(sim);
                }
            }
        }
        self.spells = keep;
        out
    }
}

async fn read<C: SolCall>(
    chain: &ChainClient,
    to: Address,
    call: C,
    block: u64,
) -> Option<C::Return> {
    let raw = chain
        .call_at(to, &Bytes::from(call.abi_encode()), block)
        .await
        .ok()?;
    C::abi_decode_returns(&raw).ok()
}

/// Worker thread `liq-bot-gov`. Discovers the Aave timelocks from the
/// registry and the Sky Chief from its chainlog, then at every new head
/// pushes each executable action's simulation onto `tx`. A full ring drops
/// the newest; the next head retries.
pub fn spawn_gov_worker(
    registry: Registry,
    intern: Intern,
    rpc_url: String,
    mut tx: rtrb::Producer<GovSim>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("liq-bot-gov".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "governance worker runtime refused");
                    return;
                }
            };
            rt.block_on(async move {
                let chain = match ChainClient::new(&rpc_url) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(error = %e, "governance worker: no chain client");
                        return;
                    }
                };
                let head = match chain.head().await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!(error = %e, "governance worker: head unavailable");
                        return;
                    }
                };
                let poller = match GovernanceConfig::new(PAYLOAD_LOOKBACK) {
                    Ok(cfg) => match GovernancePoller::from_registry(
                        &registry,
                        &intern,
                        &rpc_url,
                        cfg,
                        head.number,
                    )
                    .await
                    {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::error!(error = %e, "Aave timelock discovery failed — payloads not watched");
                            None
                        }
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "governance lookback refused");
                        None
                    }
                };
                let mut spells = SpellWatch::from_chainlog(&chain, head.number).await;
                if spells.is_none() {
                    tracing::error!("Sky Chief unreadable from the chainlog — spells not watched");
                }
                if poller.is_none() && spells.is_none() {
                    return;
                }
                tracing::info!(
                    aave = poller.is_some(),
                    sky = spells.is_some(),
                    "governance worker watching"
                );
                let mut last = 0u64;
                while !stop.load(Ordering::Acquire) {
                    match chain.head().await {
                        Ok(h) if h.number != last => {
                            last = h.number;
                            let mut sims = Vec::new();
                            if let Some(p) = poller.as_ref() {
                                sims.extend(simulate_executable(p, &chain, h).await);
                            }
                            if let Some(w) = spells.as_mut() {
                                sims.extend(w.simulate_castable(&chain, h).await);
                            }
                            for sim in sims {
                                let action = sim.action;
                                if tx.push(sim).is_err() {
                                    tracing::error!(?action, "governance ring full — dropped");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::error!(error = %e, "governance head poll failed"),
                    }
                    tokio::time::sleep(HEAD_POLL).await;
                }
            });
        })
}

#[cfg(test)]
mod e2e_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use liq_protocol::{MarketRow, MarketSlot, StateWriter};
    use liq_state::{StoreConfig, UndoCapacity};
    use liq_types::{AssetId, MarketId, PositionKey};
    use smallvec::smallvec;

    const P1: ProtocolId = ProtocolId(1);
    const P2: ProtocolId = ProtocolId(2);
    const M10: MarketId = MarketId(10);
    const M11: MarketId = MarketId(11);

    /// Two markets of two slots; one position per `(protocol, market, slot)`
    /// with a balance in that slot only, ids in order.
    fn store(positions: &[(ProtocolId, MarketId, u16)]) -> StateStore {
        let mut st = StateStore::new(StoreConfig {
            base: 0,
            positions: 8,
            markets: 4,
            undo: UndoCapacity {
                ops: 64,
                extras: 8,
                rows: 8,
            },
        });
        st.begin_block(1).unwrap();
        for m in [M10, M11] {
            for _ in 0..2 {
                st.push_market(m, MarketRow::blank(AssetId(0), 18)).unwrap();
            }
        }
        for (i, &(protocol, market, slot)) in positions.iter().enumerate() {
            let key = PositionKey {
                protocol,
                market,
                user: Address::repeat_byte(u8::try_from(i).unwrap() + 1),
            };
            let id = st.intern(&key).unwrap();
            st.set_supply(id, slot, 1).unwrap();
        }
        st
    }

    #[test]
    fn affected_positions_follow_each_dirty_shape() {
        let st = store(&[
            (P1, M10, 1), // 0: holds the repriced slot
            (P1, M10, 0), // 1: same market, other slot
            (P1, M11, 1), // 2: same slot index, other market
            (P2, M11, 0), // 3: other protocol
        ]);
        let view = st.view(0);
        let reprice = DirtySet::MarketReprice(smallvec![MarketSlot {
            market: M10,
            slot: 1
        }]);
        assert_eq!(
            affected_positions(&view, &[(P1, reprice.clone())]),
            vec![PositionId(0)]
        );
        assert_eq!(
            affected_positions(&view, &[(P1, DirtySet::ProtocolWide)]),
            vec![PositionId(0), PositionId(1), PositionId(2)]
        );
        // Union, sorted, no duplicates.
        let got = affected_positions(
            &view,
            &[
                (P1, reprice),
                (
                    P2,
                    DirtySet::Positions(smallvec![PositionId(3), PositionId(0)]),
                ),
                (P1, DirtySet::None),
            ],
        );
        assert_eq!(got, vec![PositionId(0), PositionId(3)]);
        assert!(affected_positions(&view, &[]).is_empty());
    }

    fn rpc() -> Option<String> {
        std::env::var("MAINNET_RPC_URL")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Payload 469 at the block before it was executed: the worker's own
    /// step, run on that head, yields it as the only executable payload,
    /// targeted at the next block with the real execution's gas and logs.
    /// Oracle: `liq-exec` `payload_469_simulation_matches_the_real_receipt`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn worker_step_yields_payload_469_at_the_block_before() {
        let Some(url) = rpc() else { return };
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let chain = ChainClient::new(&url).unwrap();
        let base = 26_019_517u64;
        let ts = chain.block_timestamp(base).await.unwrap();
        let poller = GovernancePoller::from_registry(
            &reg,
            &intern,
            &url,
            GovernanceConfig::new(PAYLOAD_LOOKBACK).unwrap(),
            base,
        )
        .await
        .unwrap();
        let sims = simulate_executable(
            &poller,
            &chain,
            Head {
                number: base,
                timestamp: ts,
            },
        )
        .await;
        let s = sims
            .iter()
            .find(|s| matches!(s.action, GovAction::Payload { id: 469, .. }))
            .expect("469 executable");
        assert_eq!(s.target_block, base + 1);
        assert_eq!(s.target_ts, 1_789_916_831);
        assert_eq!(s.logs.len(), 12);
        assert!(s.exec_gas > 2_000_000);
    }

    /// Sky spell 0xF01b…BaDC at the block before it was cast (26076917):
    /// the spell watch finds it as the Chief's hat, sees it scheduled and
    /// due, and simulates `cast()` as the next block. Oracle: the fork test
    /// `ForkGovSpell.t.sol` casts the same spell at the same state.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn spell_watch_yields_the_hat_spell_at_the_block_before() {
        let Some(url) = rpc() else { return };
        let chain = ChainClient::new(&url).unwrap();
        let base = 26_076_916u64;
        let ts = chain.block_timestamp(base).await.unwrap();
        let mut w = SpellWatch::from_chainlog(&chain, base).await.unwrap();
        let spell = alloy_primitives::address!("F01b594aF26fC8A8ae1e24DCaF904ECB6Fd1BaDC");
        let sims = w
            .simulate_castable(
                &chain,
                Head {
                    number: base,
                    timestamp: ts,
                },
            )
            .await;
        assert_eq!(w.spells(), &[spell], "the hat is tracked");
        let s = sims
            .iter()
            .find(|s| s.action == GovAction::Spell(spell))
            .expect("castable in the next block");
        assert_eq!(s.target_block, base + 1);
        assert!(!s.logs.is_empty());
        // Once cast, it is no longer tracked.
        let after = 26_076_917u64;
        let ts2 = chain.block_timestamp(after).await.unwrap();
        let sims = w
            .simulate_castable(
                &chain,
                Head {
                    number: after,
                    timestamp: ts2,
                },
            )
            .await;
        assert!(sims.is_empty());
        assert!(w.spells().is_empty(), "done spells are dropped");
    }
}
