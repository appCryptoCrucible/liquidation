//! The simulator's chain state: Reth's own provider, by block hash.

use liq_sim::{BlockRef, BlockState, SimError};
use reth_ethereum::evm::revm::database::StateProviderDatabase;
use reth_ethereum::storage::StateProviderFactory;

/// Reth's provider as `liq_sim`'s state source. A simulation opens the
/// state of the block the store is at, by hash, and drops it when done. A
/// block Reth no longer holds (reorged out, pruned) is refused, never
/// replaced by another block's state.
pub(crate) struct NodeState<P>(P);

impl<P> NodeState<P> {
    pub(crate) const fn new(provider: P) -> Self {
        Self(provider)
    }
}

impl<P> liq_sim::StateProviderFactory for NodeState<P>
where
    P: StateProviderFactory + Sync + 'static,
{
    fn state_at(&self, at: BlockRef) -> Result<BlockState, SimError> {
        match self.0.state_by_block_hash(at.hash) {
            // `StateProviderBox` is `Send`, not `Sync`: `locked` serialises
            // its reads (one simulation at a time reads it).
            Ok(state) => Ok(BlockState::locked(StateProviderDatabase::new(state))),
            Err(e) => {
                tracing::warn!(
                    block = at.number,
                    hash = %at.hash,
                    error = %e,
                    "node has no state for the simulated block"
                );
                Err(SimError::StateUnavailable)
            }
        }
    }
}
