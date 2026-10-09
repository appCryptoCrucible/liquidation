//! Startup seed of the flash sources. A source follows its logs by what each
//! one moves, so one built at startup holds nothing until it is read once
//! from chain: each holder's `balanceOf` (Morpho, the V4 PoolManager, every
//! V3 pool), an Aave pool's reserve list, premium, reserve flags and aToken
//! balances, the Sky module's `max` and `End.live`. Before this seed existed
//! every source started at zero and the engine found no liquidation
//! fundable.
//!
//! Every read is pinned to one block and batched through Multicall3, as
//! [`crate::pool_seed`] seeds the book. Sources ask in rounds
//! ([`FlashSource::seed_reads`]) because an Aave pool must learn its
//! reserves before it can ask for their balances. A read that fails leaves
//! what it would have set unfunded. Logs keep every source current from
//! the seeded block on.

use alloy_primitives::Bytes;
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_flash::{FlashSource, SeedRead};

use crate::pool_seed::{aggregate, call};

/// Reads per Multicall3 batch.
const BATCH: usize = 150;
/// An Aave pool needs three rounds. A source still asking after this many
/// is logged and left as it is.
const MAX_ROUNDS: usize = 4;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlashSeedStats {
    pub sources: usize,
    /// Reads sent, over every round.
    pub reads: usize,
    /// Reads that failed, reverted or returned nothing.
    pub failed: usize,
    pub rounds: usize,
    /// The block every read was pinned to (`0`: the head was unavailable).
    pub block: u64,
}

/// Seed every source at the current head.
pub async fn seed_flash(sources: &mut [Box<dyn FlashSource>], rpc: &HttpRpc) -> FlashSeedStats {
    let mut stats = FlashSeedStats {
        sources: sources.len(),
        ..FlashSeedStats::default()
    };
    let block = match rpc.block_number().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "flash seed skipped — head unavailable; every flash source stays unfunded");
            return stats;
        }
    };
    stats.block = block;
    for round in 0..MAX_ROUNDS {
        let asks: Vec<(usize, Vec<SeedRead>)> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.seed_reads()))
            .filter(|(_, r)| !r.is_empty())
            .collect();
        if asks.is_empty() {
            break;
        }
        stats.rounds = round.saturating_add(1);
        let mut answers: Vec<Vec<Option<Bytes>>> =
            asks.iter().map(|(_, r)| vec![None; r.len()]).collect();
        let order: Vec<(usize, &SeedRead)> = asks
            .iter()
            .enumerate()
            .flat_map(|(k, (_, reads))| reads.iter().map(move |r| (k, r)))
            .collect();
        let mut next: Vec<usize> = vec![0; asks.len()];
        for chunk in order.chunks(BATCH) {
            let calls = chunk
                .iter()
                .map(|(_, r)| call(r.to, r.data.to_vec()))
                .collect();
            let res = aggregate(rpc, calls, block).await;
            if res.is_none() {
                tracing::error!(round, reads = chunk.len(), block, "flash seed batch failed");
            }
            for (i, (k, _)) in chunk.iter().enumerate() {
                let got = res
                    .as_ref()
                    .and_then(|r| r.get(i))
                    .filter(|r| r.success && !r.returnData.is_empty())
                    .map(|r| r.returnData.clone());
                if got.is_none() {
                    stats.failed = stats.failed.saturating_add(1);
                }
                let Some(j) = next.get_mut(*k) else { continue };
                if let Some(slot) = answers.get_mut(*k).and_then(|a| a.get_mut(*j)) {
                    *slot = got;
                }
                *j = j.saturating_add(1);
            }
        }
        stats.reads = stats.reads.saturating_add(order.len());
        for ((i, _), a) in asks.iter().zip(&answers) {
            if let Some(s) = sources.get_mut(*i) {
                s.apply_seed(a);
            }
        }
    }
    let unsettled = sources
        .iter()
        .filter(|s| !s.seed_reads().is_empty())
        .count();
    if unsettled > 0 {
        tracing::error!(
            unsettled,
            rounds = MAX_ROUNDS,
            "flash sources still asking for seed reads — left as they are"
        );
    }
    tracing::info!(
        sources = stats.sources,
        reads = stats.reads,
        failed = stats.failed,
        rounds = stats.rounds,
        block,
        "flash sources seeded"
    );
    stats
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use alloy_sol_types::{sol, SolCall};
    use liq_flash::{AavePool, HeldAsset, MorphoBlue, SkyDssFlash, UniV3Pool, UniV4PoolManager};
    use liq_types::AssetId;

    sol! {
        function balanceOf(address who) returns (uint256);
        function max() returns (uint256);
        function live() returns (uint256);
    }

    const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const DAI: Address = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
    const AAVE_CORE: Address = address!("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
    const AAVE_CONFIGURATOR: Address = address!("0x64b761D848206f447Fe2dd461b0c635Ec39EbB27");
    const A_USDC: Address = address!("0x98C23E9d8f34FEFb1B7BD6a91B7FF122F4e16F5c");
    const A_WETH: Address = address!("0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8");
    const MORPHO: Address = address!("0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
    const POOL_MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
    const V3_USDC_WETH_500: Address = address!("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
    const DSS_FLASH: Address = address!("0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA");
    const END: Address = address!("0x0e2e8F1D1326A4B9633D96222Ce399c708B19c28");

    /// Live (`MAINNET_RPC_URL`). Oracle: one plain `eth_call` per value at
    /// the block the seed pinned, outside Multicall3 and outside the
    /// sources' own decoding — `balanceOf` of each aToken, of Morpho, of the
    /// PoolManager and of the 5-bp V3 pool, and `DssFlash.max()` with
    /// `End.live()`. Every seeded source lends exactly that. An Aave pool
    /// takes three rounds (list, reserves, balances); `toll()` reverting is
    /// the one expected failure.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn seeds_live_sources_to_their_chain_balances() {
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let (id_usdc, id_weth, id_dai) = (AssetId(0), AssetId(1), AssetId(2));
        let held = |asset, token| HeldAsset {
            asset,
            token,
            balance: U256::ZERO,
        };
        let held = [held(id_usdc, USDC), held(id_weth, WETH), held(id_dai, DAI)];
        let mut sources: Vec<Box<dyn FlashSource>> = vec![
            Box::new(AavePool::unseeded(AAVE_CORE, AAVE_CONFIGURATOR, &held)),
            Box::new(MorphoBlue::new(MORPHO, &held)),
            Box::new(UniV4PoolManager::new(POOL_MANAGER, &held)),
            Box::new(UniV3Pool::new(
                V3_USDC_WETH_500,
                USDC,
                WETH,
                id_usdc,
                id_weth,
                500,
                U256::ZERO,
                U256::ZERO,
            )),
            Box::new(SkyDssFlash::new(
                DSS_FLASH,
                END,
                id_dai,
                U256::ZERO,
                U256::ZERO,
                false,
            )),
        ];
        let stats = seed_flash(&mut sources, &rpc).await;
        assert_eq!(stats.rounds, 3, "{stats:?}");
        assert_eq!(stats.failed, 1, "only toll() reverts: {stats:?}");
        let at = stats.block;
        let read = |to: Address, data: Vec<u8>| {
            let rpc = &rpc;
            async move {
                let raw = rpc.call_at(to, data.into(), at).await.unwrap();
                U256::from_be_slice(&raw)
            }
        };
        let bal = |token: Address, who: Address| read(token, balanceOfCall { who }.abi_encode());

        assert_eq!(
            sources[0].available(id_usdc),
            bal(USDC, A_USDC).await,
            "aUSDC"
        );
        assert_eq!(
            sources[0].available(id_weth),
            bal(WETH, A_WETH).await,
            "aWETH"
        );
        for token in [(id_usdc, USDC), (id_weth, WETH), (id_dai, DAI)] {
            assert_eq!(
                sources[1].available(token.0),
                bal(token.1, MORPHO).await,
                "Morpho"
            );
            assert_eq!(
                sources[2].available(token.0),
                bal(token.1, POOL_MANAGER).await,
                "V4"
            );
        }
        assert_eq!(
            sources[3].available(id_usdc),
            bal(USDC, V3_USDC_WETH_500).await
        );
        assert_eq!(
            sources[3].available(id_weth),
            bal(WETH, V3_USDC_WETH_500).await
        );
        let live = read(END, liveCall {}.abi_encode()).await == U256::from(1u8);
        let max = read(DSS_FLASH, maxCall {}.abi_encode()).await;
        assert_eq!(
            sources[4].available(id_dai),
            if live { max } else { U256::ZERO }
        );
        assert!(
            sources.iter().all(|s| s.seed_reads().is_empty()),
            "every source settled"
        );
    }
}
