//! Uniswap V3 pool (GUIDE 07 §3a). One source per pool address.
//!
//! `available = IERC20(asset).balanceOf(pool)` for token0 or token1.
//! `fee_bps = pool.fee() / 100`. Flash fee rounds UP (`FullMath.mulDivRoundingUp`);
//! 07B must use `mul_div(..., Rounding::Up)` against 1e6, not `fee_bps/10_000`.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber};

use super::{apply_holder_transfer, IERC20};
use crate::{FlashSource, GAS_OVERHEAD_STUB};

sol! {
    interface IUniswapV3Pool {
        event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
        event Flash(address indexed sender, address indexed recipient, uint256 amount0, uint256 amount1, uint256 paid0, uint256 paid1);
    }
}

/// One V3 pool. `fee` is the immutable `pool.fee()` (pips / 1e6).
pub struct UniV3Pool {
    pool: Address,
    token0: Address,
    token1: Address,
    asset0: AssetId,
    asset1: AssetId,
    fee: u32,
    bal0: U256,
    bal1: U256,
}

impl UniV3Pool {
    #[must_use]
    pub fn new(
        pool: Address,
        token0: Address,
        token1: Address,
        asset0: AssetId,
        asset1: AssetId,
        fee: u32,
        bal0: U256,
        bal1: U256,
    ) -> Self {
        Self {
            pool,
            token0,
            token1,
            asset0,
            asset1,
            fee,
            bal0,
            bal1,
        }
    }
}

impl LogSubscriber for UniV3Pool {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::with_capacity(6);
        for t0 in [
            IUniswapV3Pool::Mint::SIGNATURE_HASH,
            IUniswapV3Pool::Burn::SIGNATURE_HASH,
            IUniswapV3Pool::Swap::SIGNATURE_HASH,
            IUniswapV3Pool::Flash::SIGNATURE_HASH,
        ] {
            out.push(LogFilter {
                address: self.pool,
                topic0: t0,
            });
        }
        out.push(LogFilter {
            address: self.token0,
            topic0: IERC20::Transfer::SIGNATURE_HASH,
        });
        out.push(LogFilter {
            address: self.token1,
            topic0: IERC20::Transfer::SIGNATURE_HASH,
        });
        out
    }
}

impl FlashSource for UniV3Pool {
    #[inline]
    fn provider(&self) -> FlashProvider {
        FlashProvider::UniV3
    }

    #[inline]
    fn source(&self) -> Address {
        self.pool
    }

    #[inline]
    fn available(&self, asset: AssetId) -> U256 {
        if asset == self.asset0 {
            self.bal0
        } else if asset == self.asset1 {
            self.bal1
        } else {
            U256::ZERO
        }
    }

    #[inline]
    fn fee_bps(&self, asset: AssetId, _amount: U256) -> u16 {
        if asset != self.asset0 && asset != self.asset1 {
            return 0;
        }
        const PIP_PER_BP: u32 = 100;
        u16::try_from(self.fee.checked_div(PIP_PER_BP).unwrap_or(0)).unwrap_or(u16::MAX)
    }

    #[inline]
    fn callback(&self) -> CallbackShape {
        CallbackShape::UniV3FlashCallback
    }

    #[inline]
    fn gas_overhead(&self) -> u64 {
        GAS_OVERHEAD_STUB
    }

    fn apply_log(&mut self, log: &DecodedLog<'_>) {
        apply_holder_transfer(self.token0, self.pool, &mut self.bal0, log);
        apply_holder_transfer(self.token1, self.pool, &mut self.bal1, log);
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sources::fixtures::*;
    use alloy_primitives::{Address, U256};
    use liq_types::LogSubscriber;

    fn v3_500() -> UniV3Pool {
        UniV3Pool::new(
            UNIV3_USDC_WETH_500,
            USDC,
            WETH,
            ID_USDC,
            ID_WETH,
            500,
            U256::from(V3_500_USDC_26M),
            u256(V3_500_WETH_26M),
        )
    }

    #[test]
    fn identities() {
        let p = v3_500();
        assert_eq!(p.provider(), FlashProvider::UniV3);
        assert_eq!(
            p.source(),
            UNIV3_USDC_WETH_500,
            "oracle: D15 factory.getPool USDC/WETH 500"
        );
        assert_eq!(p.callback(), CallbackShape::UniV3FlashCallback);
        assert_eq!(p.gas_overhead(), 0);
    }

    /// Oracle: eth_call token balances + `fee()` at block 26_000_000.
    #[test]
    fn available_and_fee_at_26m() {
        let p = v3_500();
        assert_eq!(p.available(ID_USDC), U256::from(V3_500_USDC_26M));
        assert_eq!(p.available(ID_WETH), u256(V3_500_WETH_26M));
        assert_eq!(p.available(ID_DAI), U256::ZERO);
        assert_eq!(p.fee_bps(ID_USDC, U256::ZERO), 5, "oracle: fee()=500 / 100");
        assert_eq!(p.fee_bps(ID_DAI, U256::ZERO), 0);
        let p100 = UniV3Pool::new(
            UNIV3_USDC_WETH_100,
            USDC,
            WETH,
            ID_USDC,
            ID_WETH,
            100,
            U256::ZERO,
            U256::ZERO,
        );
        assert_eq!(
            p100.fee_bps(ID_WETH, U256::ZERO),
            1,
            "oracle: fee()=100 / 100"
        );
        assert_eq!(p100.source(), UNIV3_USDC_WETH_100);
    }

    #[test]
    fn debit_above_balance_fail_closes_to_zero() {
        let mut p = v3_500();
        let too_much = U256::from(V3_500_USDC_26M)
            .checked_add(U256::from(1u64))
            .unwrap();
        let topics = transfer_topics(UNIV3_USDC_WETH_500, Address::ZERO);
        let data = u256_be(too_much);
        p.apply_log(&log(USDC, &topics, &data));
        assert_eq!(p.available(ID_USDC), U256::ZERO);
    }

    #[test]
    fn transfer_out_debits() {
        let mut p = v3_500();
        let amt = U256::from(1_000_000u64);
        let topics = transfer_topics(UNIV3_USDC_WETH_500, Address::ZERO);
        let data = u256_be(amt);
        p.apply_log(&log(USDC, &topics, &data));
        assert_eq!(p.available(ID_USDC), U256::from(V3_500_USDC_26M) - amt);
        assert_eq!(p.available(ID_WETH), u256(V3_500_WETH_26M));
    }

    #[test]
    fn subscriptions_mint_burn_swap_flash_and_transfers() {
        let p = v3_500();
        let subs = p.subscriptions();
        let has = |addr, t0| subs.iter().any(|f| f.address == addr && f.topic0 == t0);
        assert!(has(UNIV3_USDC_WETH_500, T0_V3_MINT));
        assert!(has(UNIV3_USDC_WETH_500, T0_V3_BURN));
        assert!(has(UNIV3_USDC_WETH_500, T0_V3_SWAP));
        assert!(has(UNIV3_USDC_WETH_500, T0_V3_FLASH));
        assert!(has(USDC, T0_TRANSFER));
        assert!(has(WETH, T0_TRANSFER));
        assert_eq!(IUniswapV3Pool::Swap::SIGNATURE_HASH, T0_V3_SWAP);
        assert_eq!(IUniswapV3Pool::Flash::SIGNATURE_HASH, T0_V3_FLASH);
        assert_eq!(IUniswapV3Pool::Mint::SIGNATURE_HASH, T0_V3_MINT);
        assert_eq!(IUniswapV3Pool::Burn::SIGNATURE_HASH, T0_V3_BURN);
    }

    /// Oracle: ERC-20 Transfer is additive. Credit then debit of the same
    /// amount is identity on the tracked balance (mathematical invariant).
    #[test]
    fn credit_then_debit_restores_balance() {
        use proptest::prelude::*;
        let cfg = ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        };
        proptest!(cfg, |(amt in 1u64..=10_000_000u64)| {
            let mut p = v3_500();
            let before = p.available(ID_USDC);
            let v = U256::from(amt);
            let credit = transfer_topics(Address::ZERO, UNIV3_USDC_WETH_500);
            let data = u256_be(v);
            p.apply_log(&log(USDC, &credit, &data));
            let debit = transfer_topics(UNIV3_USDC_WETH_500, Address::ZERO);
            p.apply_log(&log(USDC, &debit, &data));
            prop_assert_eq!(p.available(ID_USDC), before);
        });
    }
}
