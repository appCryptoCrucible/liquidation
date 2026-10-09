//! Aave V3/Spark Pool instance (GUIDE 07 §3a).
//!
//! `available = IERC20(asset).balanceOf(aToken)` iff flashLoanEnabled &&
//! active && !paused. Fee is `FLASHLOAN_PREMIUM_TOTAL` from
//! `FlashloanPremiumTotalUpdated` (03C carry-forward), not a constant.
//!
//! Aave V4 Hub/Spoke has no flash path in 03C coverage; this type is the
//! V3-style Pool. A later V4 spoke that emits the same events reuses it.

use std::collections::HashSet;

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall, SolEvent};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber};

use super::{apply_holder_transfer, balance_answer, balance_read, HeldAsset, IERC20};
use crate::{FlashSource, SeedRead, GAS_OVERHEAD_STUB};

sol! {
    interface IPool {
        event Supply(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint16 indexed referralCode);
        event Withdraw(address indexed reserve, address indexed user, address indexed to, uint256 amount);
        event Borrow(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint8 interestRateMode, uint256 borrowRate, uint16 indexed referralCode);
        event Repay(address indexed reserve, address indexed user, address indexed repayer, uint256 amount, bool useATokens);
        event LiquidationCall(address indexed collateralAsset, address indexed debtAsset, address indexed user, uint256 debtToCover, uint256 liquidatedCollateralAmount, address liquidator, bool receiveAToken);
        function getReservesList() external view returns (address[] memory);
        function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
        function getReserveData(address asset) external view returns (ReserveDataLegacy memory);
    }
    /// `Pool.getReserveData`: 3.0 `ReserveData`, kept as
    /// `ReserveDataLegacy` from 3.2 on (same layout).
    struct ReserveDataLegacy {
        uint256 configuration;
        uint128 liquidityIndex;
        uint128 currentLiquidityRate;
        uint128 variableBorrowIndex;
        uint128 currentVariableBorrowRate;
        uint128 currentStableBorrowRate;
        uint40 lastUpdateTimestamp;
        uint16 id;
        address aTokenAddress;
        address stableDebtTokenAddress;
        address variableDebtTokenAddress;
        address interestRateStrategyAddress;
        uint128 accruedToTreasury;
        uint128 unbacked;
        uint128 isolationModeTotalDebt;
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

/// `ReserveConfiguration` bits a flash loan checks
/// (`ValidationLogic.validateFlashloanSimple`).
const ACTIVE_BIT: usize = 56;
const PAUSED_BIT: usize = 60;
const FLASHLOAN_ENABLED_BIT: usize = 63;

/// How far [`FlashSource::seed_reads`] has brought a pool.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Stage {
    /// The reserve list and the flash premium next.
    List,
    /// Each listed reserve's aToken and flags next.
    Reserves,
    /// Each reserve's balance in its aToken next.
    Balances,
    /// Nothing left to read, or built with its reserves.
    Done,
}

/// One Aave Pool (`flashSource` address).
pub struct AavePool {
    pool: Address,
    configurator: Address,
    premium_total: u16,
    slots: Vec<Option<Slot>>,
    overhead: u64,
    /// Interned tokens a reserve may be: the seed maps the pool's list onto
    /// these ids. Emptied once seeded.
    listing: Vec<HeldAsset>,
    stage: Stage,
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
            overhead: GAS_OVERHEAD_STUB,
            listing: Vec::new(),
            stage: Stage::Done,
        }
    }

    /// A pool whose reserves, premium, flags and balances come from chain
    /// ([`FlashSource::seed_reads`]): each of `assets` the pool lists
    /// becomes a reserve. Until seeded it funds nothing.
    #[must_use]
    pub fn unseeded(pool: Address, configurator: Address, assets: &[HeldAsset]) -> Self {
        Self {
            pool,
            configurator,
            premium_total: 0,
            slots: Vec::new(),
            overhead: GAS_OVERHEAD_STUB,
            listing: assets
                .iter()
                .filter(|a| !a.token.is_zero())
                .cloned()
                .collect(),
            stage: Stage::List,
        }
    }

    #[must_use]
    pub fn with_overhead(mut self, gas: u64) -> Self {
        self.overhead = gas;
        self
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
        self.overhead
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

    fn seed_reads(&self) -> Vec<SeedRead> {
        let to = self.pool;
        match self.stage {
            Stage::List => vec![
                SeedRead {
                    to,
                    data: IPool::getReservesListCall {}.abi_encode().into(),
                },
                SeedRead {
                    to,
                    data: IPool::FLASHLOAN_PREMIUM_TOTALCall {}.abi_encode().into(),
                },
            ],
            Stage::Reserves => self
                .slots
                .iter()
                .flatten()
                .map(|s| SeedRead {
                    to,
                    data: IPool::getReserveDataCall {
                        asset: s.underlying,
                    }
                    .abi_encode()
                    .into(),
                })
                .collect(),
            Stage::Balances => self
                .slots
                .iter()
                .flatten()
                .map(|s| balance_read(s.underlying, s.atoken))
                .collect(),
            Stage::Done => Vec::new(),
        }
    }

    /// An unread list or premium leaves the pool unfunded rather than at a
    /// guessed fee; an unread reserve is dropped; an unread balance is zero.
    fn apply_seed(&mut self, answers: &[Option<Bytes>]) {
        let answer = |i: usize| answers.get(i).and_then(Option::as_ref);
        match self.stage {
            Stage::List => {
                let list =
                    answer(0).and_then(|a| IPool::getReservesListCall::abi_decode_returns(a).ok());
                let premium = answer(1)
                    .and_then(|a| IPool::FLASHLOAN_PREMIUM_TOTALCall::abi_decode_returns(a).ok());
                let (Some(list), Some(premium)) = (list, premium) else {
                    self.slots.clear();
                    self.listing = Vec::new();
                    self.stage = Stage::Done;
                    return;
                };
                self.premium_total = Self::set_u16_premium(U256::from(premium));
                let listed: HashSet<Address> = list.into_iter().collect();
                for a in &self.listing {
                    if !listed.contains(&a.token) {
                        continue;
                    }
                    let i = usize::from(a.asset.0);
                    if self.slots.len() <= i {
                        self.slots.resize(i.saturating_add(1), None);
                    }
                    if let Some(slot) = self.slots.get_mut(i) {
                        *slot = Some(Slot {
                            underlying: a.token,
                            atoken: Address::ZERO,
                            balance: U256::ZERO,
                            flash_enabled: false,
                            active: false,
                            paused: false,
                        });
                    }
                }
                self.stage = Stage::Reserves;
            }
            Stage::Reserves => {
                let mut j = 0usize;
                for entry in &mut self.slots {
                    if entry.is_none() {
                        continue;
                    }
                    let data = answer(j)
                        .and_then(|a| IPool::getReserveDataCall::abi_decode_returns(a).ok());
                    j = j.saturating_add(1);
                    match (data, entry.as_mut()) {
                        (Some(d), Some(s)) if !d.aTokenAddress.is_zero() => {
                            let c = d.configuration;
                            s.atoken = d.aTokenAddress;
                            s.active = c.bit(ACTIVE_BIT);
                            s.paused = c.bit(PAUSED_BIT);
                            s.flash_enabled = c.bit(FLASHLOAN_ENABLED_BIT);
                        }
                        _ => *entry = None,
                    }
                }
                self.stage = Stage::Balances;
            }
            Stage::Balances => {
                for (j, s) in self.slots.iter_mut().flatten().enumerate() {
                    s.balance = balance_answer(answer(j)).unwrap_or(U256::ZERO);
                }
                self.listing = Vec::new();
                self.stage = Stage::Done;
            }
            Stage::Done => {}
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

    fn reserve_data(config: U256, atoken: Address) -> Option<alloy_primitives::Bytes> {
        Some(
            IPool::getReserveDataCall::abi_encode_returns(&ReserveDataLegacy {
                configuration: config,
                liquidityIndex: 0,
                currentLiquidityRate: 0,
                variableBorrowIndex: 0,
                currentVariableBorrowRate: 0,
                currentStableBorrowRate: 0,
                lastUpdateTimestamp: alloy_primitives::aliases::U40::ZERO,
                id: 0,
                aTokenAddress: atoken,
                stableDebtTokenAddress: Address::ZERO,
                variableDebtTokenAddress: Address::ZERO,
                interestRateStrategyAddress: Address::ZERO,
                accruedToTreasury: 0,
                unbacked: 0,
                isolationModeTotalDebt: 0,
            })
            .into(),
        )
    }

    fn hex_word(s: &str) -> U256 {
        U256::from_str_radix(s, 16).unwrap()
    }

    fn held(asset: AssetId, token: Address) -> HeldAsset {
        HeldAsset {
            asset,
            token,
            balance: U256::ZERO,
        }
    }

    /// Seed `p` through its three rounds with block-26M answers for USDC
    /// and WETH (`configs` override their configuration words).
    fn seed_26m(p: &mut AavePool, usdc_cfg: U256, weth_cfg: U256) {
        let list = vec![WETH, USDC, Address::repeat_byte(0x77)];
        p.apply_seed(&[
            Some(IPool::getReservesListCall::abi_encode_returns(&list).into()),
            uint_answer(U256::from(PREMIUM_26M)),
        ]);
        p.apply_seed(&[
            reserve_data(usdc_cfg, A_USDC),
            reserve_data(weth_cfg, A_WETH),
        ]);
        p.apply_seed(&[
            uint_answer(U256::from(AUSDC_USDC_26M)),
            uint_answer(u256(AWETH_WETH_26M)),
        ]);
    }

    /// Oracle: block 26M — the Core pool lists USDC and WETH, its premium
    /// is 5, each reserve's `getReserveData` names its aToken and a
    /// configuration Aave's own data provider reads as active, unpaused and
    /// flash-enabled, and `balanceOf(aToken)` is what it lends. An unseeded
    /// pool funds nothing; seeded from those answers it equals the pool
    /// built by hand from the same block (`pool(true, true, false)`),
    /// subscribes to both reserves' transfers, and asks nothing more. DAI
    /// is interned but the list does not name it here, so it gets no
    /// reserve.
    #[test]
    fn seed_learns_reserves_premium_flags_and_balances() {
        let assets = [held(ID_USDC, USDC), held(ID_WETH, WETH), held(ID_DAI, DAI)];
        let mut p = AavePool::unseeded(AAVE_POOL, AAVE_CONFIGURATOR, &assets);
        assert_eq!(p.available(ID_USDC), U256::ZERO, "unseeded funds nothing");
        let r1 = p.seed_reads();
        assert_eq!(r1.len(), 2);
        assert!(r1.iter().all(|r| r.to == AAVE_POOL));
        assert_eq!(
            &r1[0].data[..],
            &[0xd1, 0x94, 0x6d, 0xbc],
            "getReservesList()"
        );
        assert_eq!(
            &r1[1].data[..],
            &[0x07, 0x4b, 0x2e, 0x43],
            "FLASHLOAN_PREMIUM_TOTAL()"
        );
        let list = vec![WETH, USDC, Address::repeat_byte(0x77)];
        p.apply_seed(&[
            Some(IPool::getReservesListCall::abi_encode_returns(&list).into()),
            uint_answer(U256::from(PREMIUM_26M)),
        ]);
        let r2 = p.seed_reads();
        let asked: Vec<Address> = r2
            .iter()
            .map(|r| {
                IPool::getReserveDataCall::abi_decode(&r.data)
                    .unwrap()
                    .asset
            })
            .collect();
        assert_eq!(
            asked,
            vec![USDC, WETH],
            "one getReserveData per listed, interned reserve"
        );
        p.apply_seed(&[
            reserve_data(hex_word(USDC_CONFIG_26M), A_USDC),
            reserve_data(hex_word(WETH_CONFIG_26M), A_WETH),
        ]);
        assert_eq!(
            p.seed_reads(),
            vec![balance_of(USDC, A_USDC), balance_of(WETH, A_WETH)]
        );
        p.apply_seed(&[
            uint_answer(U256::from(AUSDC_USDC_26M)),
            uint_answer(u256(AWETH_WETH_26M)),
        ]);
        assert!(p.seed_reads().is_empty(), "seeded");

        let hand = pool(true, true, false);
        for id in [ID_USDC, ID_WETH, ID_DAI] {
            assert_eq!(p.available(id), hand.available(id), "asset {}", id.0);
            assert_eq!(p.fee_bps(id, U256::ONE), hand.fee_bps(id, U256::ONE));
        }
        let subs = p.subscriptions();
        let has = |addr, t0| subs.iter().any(|f| f.address == addr && f.topic0 == t0);
        assert!(has(USDC, T0_TRANSFER) && has(WETH, T0_TRANSFER));
        assert!(!has(DAI, T0_TRANSFER), "not a reserve here");
    }

    /// Oracle: `ReserveConfiguration` (`8305565ae`): bit 56 active, bit 60
    /// paused, bit 63 flash loans enabled, which
    /// `ValidationLogic.validateFlashloanSimple` requires. A reserve read as
    /// paused, inactive or flash-disabled lends nothing; the other still does.
    #[test]
    fn seed_flags_gate_what_a_reserve_lends() {
        let usdc = hex_word(USDC_CONFIG_26M);
        let weth = hex_word(WETH_CONFIG_26M);
        let bit = |n: usize| U256::from(1u8) << n;
        for (cfg, why) in [
            (usdc | bit(60), "paused"),
            (usdc & !bit(56), "inactive"),
            (usdc & !bit(63), "flash loans disabled"),
        ] {
            let mut p = AavePool::unseeded(
                AAVE_POOL,
                AAVE_CONFIGURATOR,
                &[held(ID_USDC, USDC), held(ID_WETH, WETH)],
            );
            seed_26m(&mut p, cfg, weth);
            assert_eq!(p.available(ID_USDC), U256::ZERO, "{why}");
            assert_eq!(
                p.available(ID_WETH),
                u256(AWETH_WETH_26M),
                "{why}: WETH unaffected"
            );
        }
    }

    /// An unread list or premium leaves the pool unfunded (no guessed fee)
    /// and asks nothing more; a reverted `getReserveData` drops only that
    /// reserve.
    #[test]
    fn seed_failures_fund_nothing_rather_than_guess() {
        let assets = [held(ID_USDC, USDC), held(ID_WETH, WETH)];
        let mut p = AavePool::unseeded(AAVE_POOL, AAVE_CONFIGURATOR, &assets);
        let list = vec![WETH, USDC];
        p.apply_seed(&[
            Some(IPool::getReservesListCall::abi_encode_returns(&list).into()),
            None,
        ]);
        assert!(p.seed_reads().is_empty());
        assert_eq!(p.available(ID_USDC), U256::ZERO);

        let mut q = AavePool::unseeded(AAVE_POOL, AAVE_CONFIGURATOR, &assets);
        q.apply_seed(&[
            Some(IPool::getReservesListCall::abi_encode_returns(&list).into()),
            uint_answer(U256::from(PREMIUM_26M)),
        ]);
        q.apply_seed(&[None, reserve_data(hex_word(WETH_CONFIG_26M), A_WETH)]);
        assert_eq!(
            q.seed_reads(),
            vec![balance_of(WETH, A_WETH)],
            "USDC dropped"
        );
        q.apply_seed(&[uint_answer(u256(AWETH_WETH_26M))]);
        assert_eq!(q.available(ID_USDC), U256::ZERO);
        assert_eq!(q.available(ID_WETH), u256(AWETH_WETH_26M));
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
