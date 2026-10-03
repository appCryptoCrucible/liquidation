//! One process: Reth, and the liquidation loop as its ExEx.
//!
//! `reth node` starts the execution client. The ExEx future is spawned on
//! Reth's runtime, waits until the configured RPC answers `eth_chainId`, then
//! runs the same startup as `liq-bot` and copies each live notification onto
//! the hot-thread ring. `FinishedHeight` is sent only after that thread
//! confirms the store.

mod forward;
mod sim_state;

use reth_ethereum::node::EthereumNode;

fn main() -> eyre::Result<()> {
    // Links the mimalloc global allocator in `liq-bot`. Do not also enable
    // Reth's jemalloc feature: one process, one allocator.
    let _ = liq_bot::alloc::hot_path_flag();
    reth_ethereum::cli::Cli::parse_args().run(async move |builder, _| {
        let handle = builder
            .node(EthereumNode::default())
            .install_exex("liquidator", async move |ctx| {
                Ok(forward::liquidator_exex(ctx))
            })
            .launch()
            .await?;
        handle.wait_for_node_exit().await
    })
}
