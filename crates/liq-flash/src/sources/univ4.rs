//! Uniswap V4 PoolManager singleton (GUIDE 07 §3a).
//!
//! `available = IERC20(asset).balanceOf(PoolManager)`; `fee_bps = 0`.
//! Native ETH (`ADDRESS_ZERO`) is never flashed — WETH is the debt asset.

use alloy_primitives::Address;
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber};

use super::{HeldAsset, SlotTable, IERC20};
use crate::{FlashSource, GAS_OVERHEAD_STUB};
use alloy_primitives::U256;

sol! {
    interface IPoolManager {
        event ModifyLiquidity(bytes32 indexed id, address indexed sender, int24 tickLower, int24 tickUpper, int256 liquidityDelta, bytes32 salt);
        event Swap(bytes32 indexed id, address indexed sender, int128 amount0, int128 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick, uint24 fee);
    }
}

/// PoolManager singleton.
pub struct UniV4PoolManager {
    manager: Address,
    table: SlotTable,
}

impl UniV4PoolManager {
    #[must_use]
    pub fn new(manager: Address, assets: &[HeldAsset]) -> Self {
        Self {
            manager,
            table: SlotTable::from_held(manager, assets),
        }
    }
}

impl LogSubscriber for UniV4PoolManager {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::with_capacity(2usize.saturating_add(self.table.tokens().count()));
        out.push(LogFilter {
            address: self.manager,
            topic0: IPoolManager::Swap::SIGNATURE_HASH,
        });
        out.push(LogFilter {
            address: self.manager,
            topic0: IPoolManager::ModifyLiquidity::SIGNATURE_HASH,
        });
        for token in self.table.tokens() {
            out.push(LogFilter {
                address: token,
                topic0: IERC20::Transfer::SIGNATURE_HASH,
            });
        }
        out
    }
}

impl FlashSource for UniV4PoolManager {
    #[inline]
    fn provider(&self) -> FlashProvider {
        FlashProvider::UniV4
    }

    #[inline]
    fn source(&self) -> Address {
        self.manager
    }

    #[inline]
    fn available(&self, asset: AssetId) -> U256 {
        self.table.available(asset)
    }

    #[inline]
    fn fee_bps(&self, _asset: AssetId, _amount: U256) -> u16 {
        0
    }

    #[inline]
    fn callback(&self) -> CallbackShape {
        CallbackShape::UniV4UnlockCallback
    }

    #[inline]
    fn gas_overhead(&self) -> u64 {
        GAS_OVERHEAD_STUB
    }

    fn apply_log(&mut self, log: &DecodedLog<'_>) {
        self.table.apply_transfer(log);
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sources::fixtures::*;
    use alloy_primitives::{Address, U256};
    use liq_types::LogSubscriber;

    fn pm() -> UniV4PoolManager {
        UniV4PoolManager::new(
            POOL_MANAGER,
            &[
                HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(PM_USDC_26M),
                },
                HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u256(PM_WETH_26M),
                },
                HeldAsset {
                    asset: ID_NATIVE,
                    token: Address::ZERO,
                    balance: U256::from(1u64),
                },
            ],
        )
    }

    #[test]
    fn identities() {
        let p = pm();
        assert_eq!(p.provider(), FlashProvider::UniV4);
        assert_eq!(
            p.source(),
            POOL_MANAGER,
            "oracle: D15 uniswap-v4 pool_manager"
        );
        assert_eq!(p.callback(), CallbackShape::UniV4UnlockCallback);
        assert_eq!(p.gas_overhead(), 0);
        assert_eq!(p.fee_bps(ID_USDC, U256::MAX), 0);
    }

    /// Oracle: eth_call IERC20.balanceOf(PoolManager) at block 26_000_000.
    #[test]
    fn available_at_26m_native_eth_is_zero() {
        let p = pm();
        assert_eq!(p.available(ID_USDC), U256::from(PM_USDC_26M));
        assert_eq!(p.available(ID_WETH), u256(PM_WETH_26M));
        assert_eq!(
            p.available(ID_NATIVE),
            U256::ZERO,
            "oracle: GUIDE-07 — do not flash native ETH"
        );
        assert_eq!(p.available(ID_DAI), U256::ZERO);
    }

    #[test]
    fn transfer_credits_manager() {
        let mut p = pm();
        let amt = U256::from(7u64);
        let topics = transfer_topics(Address::ZERO, POOL_MANAGER);
        let data = u256_be(amt);
        p.apply_log(&log(USDC, &topics, &data));
        assert_eq!(p.available(ID_USDC), U256::from(PM_USDC_26M) + amt);
    }

    #[test]
    fn subscriptions_swap_modify_and_transfers() {
        let p = pm();
        let subs = p.subscriptions();
        let has = |addr, t0| subs.iter().any(|f| f.address == addr && f.topic0 == t0);
        assert!(has(POOL_MANAGER, T0_V4_SWAP));
        assert!(has(POOL_MANAGER, T0_V4_MODIFY));
        assert!(has(USDC, T0_TRANSFER));
        assert!(has(WETH, T0_TRANSFER));
        assert!(!has(Address::ZERO, T0_TRANSFER));
        assert_eq!(IPoolManager::Swap::SIGNATURE_HASH, T0_V4_SWAP);
        assert_eq!(IPoolManager::ModifyLiquidity::SIGNATURE_HASH, T0_V4_MODIFY);
    }
}
