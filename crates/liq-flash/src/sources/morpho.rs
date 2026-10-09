//! Morpho Blue singleton (GUIDE 07 §3a).
//!
//! `available = IERC20(asset).balanceOf(Morpho)`; `fee_bps = 0` (documented
//! free). Balance includes market liquidity, posted collateral, and donations
//! — reconstructed from ERC-20 `Transfer`, not from supply/withdraw amounts.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber};

use super::{HeldAsset, SlotTable, IERC20};
use crate::{FlashSource, GAS_OVERHEAD_STUB};

sol! {
    interface IMorpho {
        event Supply(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets, uint256 shares);
        event Withdraw(bytes32 indexed id, address caller, address indexed onBehalf, address indexed receiver, uint256 assets, uint256 shares);
        event Liquidate(bytes32 indexed id, address indexed caller, address indexed borrower, uint256 repaidAssets, uint256 repaidShares, uint256 seizedAssets, uint256 badDebtAssets, uint256 badDebtShares);
        event FlashLoan(address indexed caller, address indexed token, uint256 assets);
    }
}

/// Morpho Blue singleton.
pub struct MorphoBlue {
    morpho: Address,
    table: SlotTable,
    overhead: u64,
}

impl MorphoBlue {
    #[must_use]
    pub fn new(morpho: Address, assets: &[HeldAsset]) -> Self {
        Self {
            morpho,
            table: SlotTable::from_held(morpho, assets),
            overhead: GAS_OVERHEAD_STUB,
        }
    }

    #[must_use]
    pub fn with_overhead(mut self, gas: u64) -> Self {
        self.overhead = gas;
        self
    }
}

impl LogSubscriber for MorphoBlue {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::with_capacity(4usize.saturating_add(self.table.tokens().count()));
        for t0 in [
            IMorpho::Supply::SIGNATURE_HASH,
            IMorpho::Withdraw::SIGNATURE_HASH,
            IMorpho::Liquidate::SIGNATURE_HASH,
            IMorpho::FlashLoan::SIGNATURE_HASH,
        ] {
            out.push(LogFilter {
                address: self.morpho,
                topic0: t0,
            });
        }
        for token in self.table.tokens() {
            out.push(LogFilter {
                address: token,
                topic0: IERC20::Transfer::SIGNATURE_HASH,
            });
        }
        out
    }
}

impl FlashSource for MorphoBlue {
    #[inline]
    fn provider(&self) -> FlashProvider {
        FlashProvider::Morpho
    }

    #[inline]
    fn source(&self) -> Address {
        self.morpho
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
        CallbackShape::MorphoFlashCallback
    }

    #[inline]
    fn gas_overhead(&self) -> u64 {
        self.overhead
    }

    fn apply_log(&mut self, log: &DecodedLog<'_>) {
        self.table.apply_transfer(log);
    }

    fn seed_reads(&self) -> Vec<crate::SeedRead> {
        self.table.seed_reads()
    }

    fn apply_seed(&mut self, answers: &[Option<alloy_primitives::Bytes>]) {
        self.table.apply_seed(answers);
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sources::fixtures::*;
    use alloy_primitives::{Address, U256};
    use liq_types::LogSubscriber;

    /// Oracle: `balanceOf(Morpho)` on USDC and WETH at block 26M. The seed
    /// asks exactly those two reads (ERC-20 `balanceOf`, selector
    /// `0x70a08231`), takes the answers as the balances, and asks nothing
    /// more; a failed read leaves that token unfunded.
    #[test]
    fn seed_reads_each_balance_once() {
        let held = |asset, token| HeldAsset {
            asset,
            token,
            balance: U256::ZERO,
        };
        let mut m = MorphoBlue::new(MORPHO, &[held(ID_USDC, USDC), held(ID_WETH, WETH)]);
        assert_eq!(m.available(ID_USDC), U256::ZERO, "unseeded funds nothing");
        assert_eq!(
            m.seed_reads(),
            vec![balance_of(USDC, MORPHO), balance_of(WETH, MORPHO)]
        );
        m.apply_seed(&[
            uint_answer(U256::from(MORPHO_USDC_26M)),
            uint_answer(u256(MORPHO_WETH_26M)),
        ]);
        assert_eq!(m.available(ID_USDC), U256::from(MORPHO_USDC_26M));
        assert_eq!(m.available(ID_WETH), u256(MORPHO_WETH_26M));
        assert!(m.seed_reads().is_empty(), "seeded once");

        let mut failed = MorphoBlue::new(MORPHO, &[held(ID_USDC, USDC), held(ID_WETH, WETH)]);
        failed.apply_seed(&[None, uint_answer(u256(MORPHO_WETH_26M))]);
        assert_eq!(failed.available(ID_USDC), U256::ZERO);
        assert_eq!(failed.available(ID_WETH), u256(MORPHO_WETH_26M));
        assert!(
            failed.seed_reads().is_empty(),
            "a failed read is not retried forever"
        );
    }

    fn morpho() -> MorphoBlue {
        MorphoBlue::new(
            MORPHO,
            &[
                HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(MORPHO_USDC_26M),
                },
                HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u256(MORPHO_WETH_26M),
                },
            ],
        )
    }

    #[test]
    fn identities() {
        let m = morpho();
        assert_eq!(m.provider(), FlashProvider::Morpho);
        assert_eq!(m.source(), MORPHO, "oracle: D15 morpho-blue singleton");
        assert_eq!(m.callback(), CallbackShape::MorphoFlashCallback);
        assert_eq!(
            m.fee_bps(ID_USDC, U256::MAX),
            0,
            "oracle: Morpho flashFee ≡ 0"
        );
        assert_eq!(m.gas_overhead(), 0);
    }

    /// Oracle: eth_call IERC20.balanceOf(Morpho) at block 26_000_000.
    #[test]
    fn available_at_26m() {
        let m = morpho();
        assert_eq!(m.available(ID_USDC), U256::from(MORPHO_USDC_26M));
        assert_eq!(m.available(ID_WETH), u256(MORPHO_WETH_26M));
        assert_eq!(m.available(ID_DAI), U256::ZERO);
    }

    #[test]
    fn donation_transfer_is_included() {
        let mut m = morpho();
        let amt = U256::from(42u64);
        let topics = transfer_topics(Address::ZERO, MORPHO);
        let data = u256_be(amt);
        m.apply_log(&log(WETH, &topics, &data));
        assert_eq!(m.available(ID_WETH), u256(MORPHO_WETH_26M) + amt);
    }

    #[test]
    fn subscriptions_supply_withdraw_liquidate_flash() {
        let m = morpho();
        let subs = m.subscriptions();
        let has = |addr, t0| subs.iter().any(|f| f.address == addr && f.topic0 == t0);
        assert!(has(MORPHO, T0_MORPHO_SUPPLY));
        assert!(has(MORPHO, T0_MORPHO_WITHDRAW));
        assert!(has(MORPHO, T0_MORPHO_LIQ));
        assert!(has(MORPHO, T0_MORPHO_FLASH));
        assert!(has(USDC, T0_TRANSFER));
        assert_eq!(IMorpho::Supply::SIGNATURE_HASH, T0_MORPHO_SUPPLY);
        assert_eq!(IMorpho::Withdraw::SIGNATURE_HASH, T0_MORPHO_WITHDRAW);
        assert_eq!(IMorpho::Liquidate::SIGNATURE_HASH, T0_MORPHO_LIQ);
        assert_eq!(IMorpho::FlashLoan::SIGNATURE_HASH, T0_MORPHO_FLASH);
    }
}
