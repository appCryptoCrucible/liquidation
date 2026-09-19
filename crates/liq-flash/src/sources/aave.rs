//! Aave V3/Spark Pool instance (GUIDE 07 §3a).
//!
//! `available = IERC20(asset).balanceOf(aToken)` iff flashLoanEnabled &&
//! active && !paused. Fee is `FLASHLOAN_PREMIUM_TOTAL` from
//! `FlashloanPremiumTotalUpdated` (03C carry-forward), not a constant.
//!
//! Aave V4 Hub/Spoke has no flash path in 03C coverage; this type is the
//! V3-style Pool. A later V4 spoke that emits the same events reuses it.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber};

use super::{apply_holder_transfer, IERC20};
use crate::{FlashSource, GAS_OVERHEAD_STUB};

sol! {
    interface IPool {
        event Supply(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint16 indexed referralCode);
        event Withdraw(address indexed reserve, address indexed user, address indexed to, uint256 amount);
        event Borrow(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint8 interestRateMode, uint256 borrowRate, uint16 indexed referralCode);
        event Repay(address indexed reserve, address indexed user, address indexed repayer, uint256 amount, bool useATokens);
        event LiquidationCall(address indexed collateralAsset, address indexed debtAsset, address indexed user, uint256 debtToCover, uint256 liquidatedCollateralAmount, address liquidator, bool receiveAToken);
    }
    interface IPoolConfigurator {
        event ReserveFlashLoaning(address indexed asset, bool enabled);
        event ReserveActive(address indexed asset, bool active);
        event ReservePaused(address indexed asset, bool paused);
        event FlashloanPremiumTotalUpdated(uint128 oldFlashloanPremiumTotal, uint128 newFlashloanPremiumTotal);
    }
}

/// Constructor seed for one reserve of this Pool.
#[derive(Clone, Debug)]
pub struct AaveReserve {
    pub asset: AssetId,
    pub underlying: Address,
    pub atoken: Address,
    pub balance: U256,
    pub flash_enabled: bool,
    pub active: bool,
    pub paused: bool,
}

#[derive(Clone, Debug)]
struct Slot {
    underlying: Address,
    atoken: Address,
    balance: U256,
    flash_enabled: bool,
    active: bool,
    paused: bool,
}

/// One Aave Pool (`flashSource` address).
pub struct AavePool {
    pool: Address,
    configurator: Address,
    premium_total: u16,
    slots: Vec<Option<Slot>>,
}

impl AavePool {
    #[must_use]
    pub fn new(
        pool: Address,
        configurator: Address,
        premium_total: u16,
        reserves: &[AaveReserve],
    ) -> Self {
        let mut slots = Vec::new();
        for r in reserves {
            let i = usize::from(r.asset.0);
            if slots.len() <= i {
                slots.resize(i.saturating_add(1), None);
            }
            if let Some(slot) = slots.get_mut(i) {
                *slot = Some(Slot {
                    underlying: r.underlying,
                    atoken: r.atoken,
                    balance: r.balance,
                    flash_enabled: r.flash_enabled,
                    active: r.active,
                    paused: r.paused,
                });
            }
        }
        Self {
            pool,
            configurator,
            premium_total,
            slots,
        }
    }

    fn slot_by_underlying_mut(&mut self, token: Address) -> Option<&mut Slot> {
        self.slots
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|s| s.underlying == token)
    }

    fn set_u16_premium(new: U256) -> u16 {
        const MAX: U256 = U256::from_limbs([u16::MAX as u64, 0, 0, 0]);
        if new > MAX {
            u16::MAX
        } else {
            u16::try_from(new.as_limbs().first().copied().unwrap_or(0)).unwrap_or(u16::MAX)
        }
    }
}

impl LogSubscriber for AavePool {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let n = 5usize
            .saturating_add(4)
            .saturating_add(self.slots.iter().flatten().count());
        let mut out = Vec::with_capacity(n);
        for t0 in [
            IPool::Supply::SIGNATURE_HASH,
            IPool::Withdraw::SIGNATURE_HASH,
            IPool::Borrow::SIGNATURE_HASH,
            IPool::Repay::SIGNATURE_HASH,
            IPool::LiquidationCall::SIGNATURE_HASH,
        ] {
            out.push(LogFilter {
                address: self.pool,
                topic0: t0,
            });
        }
        for t0 in [
            IPoolConfigurator::ReserveFlashLoaning::SIGNATURE_HASH,
            IPoolConfigurator::ReserveActive::SIGNATURE_HASH,
            IPoolConfigurator::ReservePaused::SIGNATURE_HASH,
            IPoolConfigurator::FlashloanPremiumTotalUpdated::SIGNATURE_HASH,
        ] {
            out.push(LogFilter {
                address: self.configurator,
                topic0: t0,
            });
        }
        for s in self.slots.iter().flatten() {
            out.push(LogFilter {
                address: s.underlying,
                topic0: IERC20::Transfer::SIGNATURE_HASH,
            });
        }
        out
    }
}

impl FlashSource for AavePool {
    #[inline]
    fn provider(&self) -> FlashProvider {
        FlashProvider::Aave
    }

    #[inline]
    fn source(&self) -> Address {
        self.pool
    }

    #[inline]
    fn available(&self, asset: AssetId) -> U256 {
        match self.slots.get(usize::from(asset.0)) {
            Some(Some(s)) if s.flash_enabled && s.active && !s.paused => s.balance,
            _ => U256::ZERO,
        }
    }

    #[inline]
    fn fee_bps(&self, asset: AssetId, _amount: U256) -> u16 {
        match self.slots.get(usize::from(asset.0)) {
            Some(Some(_)) => self.premium_total,
            _ => 0,
        }
    }

    #[inline]
    fn callback(&self) -> CallbackShape {
        CallbackShape::AaveExecuteOperation
    }

    #[inline]
    fn gas_overhead(&self) -> u64 {
        GAS_OVERHEAD_STUB
    }

    fn apply_log(&mut self, log: &DecodedLog<'_>) {
        let Some(&t0) = log.topics.first() else {
            return;
        };
        if log.address == self.configurator {
            if t0 == IPoolConfigurator::FlashloanPremiumTotalUpdated::SIGNATURE_HASH {
                if let Ok(ev) = IPoolConfigurator::FlashloanPremiumTotalUpdated::decode_raw_log(
                    log.topics.iter().copied(),
                    log.data,
                ) {
                    self.premium_total =
                        Self::set_u16_premium(U256::from(ev.newFlashloanPremiumTotal));
                }
                return;
            }
            if t0 == IPoolConfigurator::ReserveFlashLoaning::SIGNATURE_HASH {
                if let Ok(ev) = IPoolConfigurator::ReserveFlashLoaning::decode_raw_log(
                    log.topics.iter().copied(),
                    log.data,
                ) {
                    if let Some(s) = self.slot_by_underlying_mut(ev.asset) {
                        s.flash_enabled = ev.enabled;
                    }
                }
                return;
            }
            if t0 == IPoolConfigurator::ReserveActive::SIGNATURE_HASH {
                if let Ok(ev) = IPoolConfigurator::ReserveActive::decode_raw_log(
                    log.topics.iter().copied(),
                    log.data,
                ) {
                    if let Some(s) = self.slot_by_underlying_mut(ev.asset) {
                        s.active = ev.active;
                    }
                }
                return;
            }
            if t0 == IPoolConfigurator::ReservePaused::SIGNATURE_HASH {
                if let Ok(ev) = IPoolConfigurator::ReservePaused::decode_raw_log(
                    log.topics.iter().copied(),
                    log.data,
                ) {
                    if let Some(s) = self.slot_by_underlying_mut(ev.asset) {
                        s.paused = ev.paused;
                    }
                }
            }
            return;
        }
        for slot in self.slots.iter_mut().flatten() {
            apply_holder_transfer(slot.underlying, slot.atoken, &mut slot.balance, log);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::bool_assert_comparison,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::sources::fixtures::*;
    use alloy_primitives::{Address, U256};
    use liq_types::LogSubscriber;

    fn pool(usdc_flash: bool, usdc_active: bool, usdc_paused: bool) -> AavePool {
        AavePool::new(
            AAVE_POOL,
            AAVE_CONFIGURATOR,
            5,
            &[
                AaveReserve {
                    asset: ID_USDC,
                    underlying: USDC,
                    atoken: A_USDC,
                    balance: U256::from(AUSDC_USDC_26M),
                    flash_enabled: usdc_flash,
                    active: usdc_active,
                    paused: usdc_paused,
                },
                AaveReserve {
                    asset: ID_WETH,
                    underlying: WETH,
                    atoken: A_WETH,
                    balance: u256(AWETH_WETH_26M),
                    flash_enabled: true,
                    active: true,
                    paused: false,
                },
            ],
        )
    }

    #[test]
    fn identities_and_stub_gas() {
        let p = pool(true, true, false);
        assert_eq!(p.provider(), FlashProvider::Aave);
        assert_eq!(p.source(), AAVE_POOL, "oracle: D15 aave-v3 pool");
        assert_eq!(p.callback(), CallbackShape::AaveExecuteOperation);
        assert_eq!(p.gas_overhead(), 0, "oracle: 10C stub, not a measurement");
        assert_eq!(p.callback().provider(), FlashProvider::Aave);
    }

    /// Oracle: eth_call IERC20(USDC).balanceOf(aUSDC) at block 26_000_000.
    #[test]
    fn available_matches_block_26m_ausdc() {
        let p = pool(true, true, false);
        assert_eq!(p.available(ID_USDC), U256::from(AUSDC_USDC_26M));
        assert_eq!(p.available(ID_WETH), u256(AWETH_WETH_26M));
        assert_eq!(p.available(ID_DAI), U256::ZERO, "untracked → 0");
        assert_eq!(
            p.fee_bps(ID_USDC, U256::ZERO),
            5,
            "oracle: FLASHLOAN_PREMIUM_TOTAL at 26M"
        );
    }

    #[test]
    fn disabled_paused_inactive_are_zero() {
        assert_eq!(pool(false, true, false).available(ID_USDC), U256::ZERO);
        assert_eq!(pool(true, false, false).available(ID_USDC), U256::ZERO);
        assert_eq!(pool(true, true, true).available(ID_USDC), U256::ZERO);
        let live = pool(true, true, true);
        assert_eq!(live.available(ID_WETH), u256(AWETH_WETH_26M));
    }

    #[test]
    fn transfer_to_atoken_credits() {
        let mut p = pool(true, true, false);
        let before = p.available(ID_USDC);
        let amt = U256::from(1_000_000u64);
        let topics = transfer_topics(Address::ZERO, A_USDC);
        let data = u256_be(amt);
        p.apply_log(&log(USDC, &topics, &data));
        assert_eq!(p.available(ID_USDC), before + amt);
    }

    #[test]
    fn transfer_unrelated_is_noop() {
        let mut p = pool(true, true, false);
        let before = p.available(ID_USDC);
        let topics = transfer_topics(Address::ZERO, WETH);
        let data = u256_be(U256::from(99u64));
        p.apply_log(&log(USDC, &topics, &data));
        assert_eq!(p.available(ID_USDC), before);
    }

    #[test]
    fn premium_event_sets_fee_to_zero() {
        let mut p = pool(true, true, false);
        let t0 = IPoolConfigurator::FlashloanPremiumTotalUpdated::SIGNATURE_HASH;
        let topics = [t0];
        let mut data = [0u8; 64];
        data[63] = 0; // newFlashloanPremiumTotal = 0 (waiver path, GUIDE-07 §2)
        p.apply_log(&log(AAVE_CONFIGURATOR, &topics, &data));
        assert_eq!(p.fee_bps(ID_USDC, U256::from(1u64)), 0);
        assert_eq!(p.available(ID_USDC), U256::from(AUSDC_USDC_26M));
    }

    #[test]
    fn flash_loaning_off_zeros_available() {
        let mut p = pool(true, true, false);
        let t0 = IPoolConfigurator::ReserveFlashLoaning::SIGNATURE_HASH;
        let topics = [t0, word(USDC)];
        let data = [0u8; 32];
        p.apply_log(&log(AAVE_CONFIGURATOR, &topics, &data));
        assert_eq!(p.available(ID_USDC), U256::ZERO);
        assert_eq!(p.available(ID_WETH), u256(AWETH_WETH_26M));
    }

    #[test]
    fn pause_event_zeros_available() {
        let mut p = pool(true, true, false);
        let t0 = IPoolConfigurator::ReservePaused::SIGNATURE_HASH;
        let topics = [t0, word(USDC)];
        let mut data = [0u8; 32];
        data[31] = 1;
        p.apply_log(&log(AAVE_CONFIGURATOR, &topics, &data));
        assert_eq!(p.available(ID_USDC), U256::ZERO);
        assert_eq!(p.available(ID_WETH), u256(AWETH_WETH_26M));
    }

    #[test]
    fn subscriptions_include_03c_premium_and_transfers() {
        let p = pool(true, true, false);
        let subs = p.subscriptions();
        let has = |addr, t0| subs.iter().any(|f| f.address == addr && f.topic0 == t0);
        assert!(has(AAVE_POOL, T0_AAVE_SUPPLY));
        assert!(has(AAVE_POOL, T0_AAVE_WITHDRAW));
        assert!(has(AAVE_POOL, T0_AAVE_BORROW));
        assert!(has(AAVE_POOL, T0_AAVE_REPAY));
        assert!(has(AAVE_POOL, T0_AAVE_LIQ));
        assert!(has(AAVE_CONFIGURATOR, T0_PREMIUM), "03C carry-forward");
        assert!(has(AAVE_CONFIGURATOR, T0_FLASH_LOANING));
        assert!(has(AAVE_CONFIGURATOR, T0_RESERVE_ACTIVE));
        assert!(has(AAVE_CONFIGURATOR, T0_RESERVE_PAUSED));
        assert!(has(USDC, T0_TRANSFER));
        assert!(has(WETH, T0_TRANSFER));
        assert_eq!(
            IPoolConfigurator::FlashloanPremiumTotalUpdated::SIGNATURE_HASH,
            T0_PREMIUM
        );
    }

    #[test]
    fn sol_topic0s_match_03c() {
        assert_eq!(IPool::Supply::SIGNATURE_HASH, T0_AAVE_SUPPLY);
        assert_eq!(IPool::Withdraw::SIGNATURE_HASH, T0_AAVE_WITHDRAW);
        assert_eq!(IPool::Borrow::SIGNATURE_HASH, T0_AAVE_BORROW);
        assert_eq!(IPool::Repay::SIGNATURE_HASH, T0_AAVE_REPAY);
        assert_eq!(IPool::LiquidationCall::SIGNATURE_HASH, T0_AAVE_LIQ);
        assert_eq!(IERC20::Transfer::SIGNATURE_HASH, T0_TRANSFER);
    }
}
