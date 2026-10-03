//! Jobs simulated on the node's own state (GUIDE 11): the block the store
//! is at, read through Reth's provider, with the Executor the jobs are sent
//! to — and the two modules it delegatecalls.

use std::sync::Arc;

use alloy_primitives::Address;

use crate::env::NextBlock;
use crate::verify::{executor_anchors, verify, Bundle};
use crate::warm::{executor_stack, ExecutorCode, ExecutorSpec, Simulator, PLANNED_EXECUTOR};
use crate::{SimError, SimOutcome, StateProviderFactory};

/// The in-process simulator of the live loop. Each [`NodeSim::verify`]
/// opens the state of the parent block, overlays the Executor, and runs
/// the bundle in the next block. Nothing is kept between calls, so no
/// state from an earlier block leaks into a later one.
pub struct NodeSim {
    state: Arc<dyn StateProviderFactory>,
    executor: Address,
    /// The compiled system, placed when it is not deployed. `None`: the
    /// chain's accounts (the Executor and the modules it names) as they are.
    code: Option<ExecutorCode>,
}

impl NodeSim {
    /// The deployed Executor (`venues.executor`): its code, storage and
    /// balances as the chain has them, and the modules its immutables name.
    #[must_use]
    pub fn deployed(state: Arc<dyn StateProviderFactory>, executor: Address) -> Self {
        Self {
            state,
            executor,
            code: None,
        }
    }

    /// No Executor deployed: the compiled system, built for `spec`, with
    /// the core at [`PLANNED_EXECUTOR`] — the address undeployed jobs are
    /// signed to and recorded at, never sent — and its modules where the
    /// core delegatecalls them. Needs the forge artifacts.
    pub fn compiled(
        state: Arc<dyn StateProviderFactory>,
        spec: &ExecutorSpec,
    ) -> Result<Self, SimError> {
        Ok(Self {
            state,
            executor: PLANNED_EXECUTOR,
            code: Some(executor_stack(spec)?),
        })
    }

    /// Where the simulated calls go: the address the jobs are sent to.
    #[must_use]
    pub const fn executor(&self) -> Address {
        self.executor
    }

    /// `bundle` in the block after `at.parent`, on that block's state. The
    /// Executor's own `WETH` and `PROFIT_SINK` say where profit is measured.
    pub fn verify(&self, bundle: &Bundle, at: &NextBlock) -> Result<SimOutcome, SimError> {
        let env = at.mainnet_env()?;
        let state = Arc::new(self.state.state_at(at.parent)?);
        let placed = self
            .code
            .as_ref()
            .map(|c| c.placements(self.executor).to_vec())
            .unwrap_or_default();
        let mut sim =
            Simulator::with_executor(state, self.executor, &placed, Address::ZERO, Address::ZERO)?;
        let (weth, sink) = executor_anchors(&mut sim.db, &env, self.executor)?;
        sim.weth = weth;
        sim.profit_account = sink;
        verify(&mut sim, bundle, &env)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{execute_calldata, BlockRef, BlockState, MemoryFactory, SimTx, Trigger};
    use alloy_primitives::{address, keccak256, Bytes, B256, U256};
    use revm::database::{CacheDB, EmptyDB};
    use revm::database_interface::{DBErrorMarker, DatabaseRef};
    use revm::state::AccountInfo;

    const OPERATOR: Address = address!("0000000000000000000000000000000000000A01");
    const SINK: Address = address!("0000000000000000000000000000000000000A02");
    const BACKRUN: Address = address!("0000000000000000000000000000000000000A03");

    fn at() -> NextBlock {
        NextBlock {
            parent: BlockRef {
                number: 23_500_000,
                hash: B256::repeat_byte(1),
            },
            parent_timestamp: 1_790_000_000,
            parent_gas_limit: 60_000_000,
        }
    }

    fn compiled(factory: MemoryFactory) -> NodeSim {
        NodeSim::compiled(
            Arc::new(factory),
            &ExecutorSpec::mainnet(OPERATOR, BACKRUN, SINK),
        )
        .expect("forge build Executor.json")
    }

    fn one_call(caller: Address, to: Address, data: Bytes) -> Bundle {
        Bundle {
            trigger: Trigger::InterestDrift,
            calls: vec![SimTx {
                caller,
                to,
                value: U256::ZERO,
                data,
                gas_limit: at().max_tx_gas(),
                access_list: Vec::new(),
            }],
            min_profit: U256::ZERO,
            health: None,
        }
    }

    /// The compiled Executor runs at the planned address with the
    /// immutables it was built for: a stranger gets its `NotOperator`
    /// revert, and the operator's malformed plan reverts in its decoder.
    #[test]
    fn compiled_executor_answers_at_the_planned_address() {
        let sim = compiled(MemoryFactory::empty());
        assert_eq!(sim.executor(), PLANNED_EXECUTOR);
        let plan = execute_calldata(&[0xde, 0xad]);
        let not_operator = &keccak256("NotOperator()")[..4];
        let stranger = address!("0000000000000000000000000000000000000bad");
        match sim.verify(&one_call(stranger, sim.executor(), plan.clone()), &at()) {
            Err(SimError::Revert { reason }) => assert_eq!(reason.get(..4), Some(not_operator)),
            other => panic!("expected NotOperator, got {other:?}"),
        }
        // Both operator keys get past the operator check to the decoder.
        for key in [OPERATOR, BACKRUN] {
            match sim.verify(&one_call(key, sim.executor(), plan.clone()), &at()) {
                Err(SimError::Revert { reason }) => {
                    assert_ne!(reason.get(..4), Some(not_operator), "{key}");
                }
                other => panic!("expected the decoder's revert for {key}, got {other:?}"),
            }
        }
    }

    /// `WETH` and `PROFIT_SINK` come from the Executor's own code: where
    /// the profit of a simulated job is measured.
    #[test]
    fn anchors_are_the_executors_immutables() {
        let sim = compiled(MemoryFactory::empty());
        let env = at().mainnet_env().unwrap();
        let state = Arc::new(MemoryFactory::empty().state_at(at().parent).unwrap());
        let placed = sim.code.as_ref().unwrap().placements(sim.executor);
        let mut overlay =
            Simulator::with_executor(state, sim.executor, &placed, Address::ZERO, Address::ZERO)
                .unwrap();
        let (weth, sink) = executor_anchors(&mut overlay.db, &env, sim.executor).unwrap();
        assert_eq!(weth, crate::warm::WETH);
        assert_eq!(sink, SINK);
    }

    /// Reverts unless TIMESTAMP and NUMBER are `timestamp` and `number`.
    fn block_check(number: u32, timestamp: u32) -> revm::state::Bytecode {
        let mut code = vec![0x42, 0x63];
        code.extend_from_slice(&timestamp.to_be_bytes());
        code.extend_from_slice(&[
            0x14, 0x60, 0x0e, 0x57, 0x60, 0x00, 0x80, 0xfd, 0x5b, 0x43, 0x63,
        ]);
        code.extend_from_slice(&number.to_be_bytes());
        code.extend_from_slice(&[0x14, 0x60, 0x1d, 0x57, 0x60, 0x00, 0x80, 0xfd, 0x5b, 0x00]);
        assert_eq!((code[0x0e], code[0x1d]), (0x5b, 0x5b));
        revm::state::Bytecode::new_raw(code.into())
    }

    /// A job runs in the block after the store's tip: one higher, one slot
    /// later. The parent's own number and time revert.
    #[test]
    fn jobs_run_in_the_next_block() {
        let next = address!("00000000000000000000000000000000000000e1");
        let parent = address!("00000000000000000000000000000000000000e2");
        let mut cache = CacheDB::new(EmptyDB::default());
        for (addr, code) in [
            (next, block_check(23_500_001, 1_790_000_012)),
            (parent, block_check(23_500_000, 1_790_000_000)),
        ] {
            cache.insert_account_info(
                addr,
                AccountInfo {
                    code_hash: code.hash_slow(),
                    code: Some(code),
                    nonce: 1,
                    ..Default::default()
                },
            );
        }
        let sim = compiled(MemoryFactory::from_cache(cache));
        sim.verify(&one_call(OPERATOR, next, Bytes::new()), &at())
            .expect("the next block's number and timestamp");
        assert!(matches!(
            sim.verify(&one_call(OPERATOR, parent, Bytes::new()), &at()),
            Err(SimError::Revert { .. })
        ));
    }

    /// A deployed address with no code is refused before anything runs.
    #[test]
    fn deployed_address_without_code_is_refused() {
        let sim = NodeSim::deployed(
            Arc::new(MemoryFactory::empty()),
            address!("000000000000000000000000000000000000beef"),
        );
        let bundle = one_call(OPERATOR, sim.executor(), execute_calldata(&[1]));
        assert!(matches!(
            sim.verify(&bundle, &at()),
            Err(SimError::Bytecode(_))
        ));
    }

    struct Gone;

    impl StateProviderFactory for Gone {
        fn state_at(&self, _: BlockRef) -> Result<BlockState, SimError> {
            Err(SimError::StateUnavailable)
        }
    }

    /// The parent's state is gone (reorged out, pruned): refused, never
    /// simulated on another block's state.
    #[test]
    fn missing_parent_state_is_refused() {
        let sim = NodeSim::deployed(Arc::new(Gone), PLANNED_EXECUTOR);
        let bundle = one_call(OPERATOR, sim.executor(), execute_calldata(&[1]));
        assert_eq!(sim.verify(&bundle, &at()), Err(SimError::StateUnavailable));
    }

    #[derive(Debug)]
    struct ReadFailed;

    impl std::fmt::Display for ReadFailed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("read failed")
        }
    }

    impl std::error::Error for ReadFailed {}

    impl DBErrorMarker for ReadFailed {}

    /// `Send` but not `Sync`, like Reth's `StateProviderBox`.
    struct OneThread {
        reads: std::cell::Cell<u32>,
        fail: bool,
    }

    impl DatabaseRef for OneThread {
        type Error = ReadFailed;

        fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, ReadFailed> {
            self.reads.set(self.reads.get().saturating_add(1));
            if self.fail {
                Err(ReadFailed)
            } else {
                Ok(None)
            }
        }
        fn code_by_hash_ref(&self, _: B256) -> Result<revm::state::Bytecode, ReadFailed> {
            Err(ReadFailed)
        }
        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, ReadFailed> {
            Ok(U256::ZERO)
        }
        fn block_hash_ref(&self, _: u64) -> Result<B256, ReadFailed> {
            Ok(B256::ZERO)
        }
    }

    /// `BlockState::locked` serves a provider that is not `Sync`; its read
    /// errors become `StateUnavailable`.
    #[test]
    fn locked_state_serves_a_send_only_provider() {
        let ok = BlockState::locked(OneThread {
            reads: std::cell::Cell::new(0),
            fail: false,
        });
        assert_eq!(ok.basic_ref(Address::ZERO), Ok(None));
        let bad = BlockState::locked(OneThread {
            reads: std::cell::Cell::new(0),
            fail: true,
        });
        assert_eq!(
            bad.basic_ref(Address::ZERO),
            Err(SimError::StateUnavailable)
        );
        assert_eq!(
            bad.code_by_hash_ref(B256::ZERO),
            Err(SimError::StateUnavailable)
        );
    }
}
