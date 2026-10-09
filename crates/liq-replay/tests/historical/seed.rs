//! The store as the bot would hold it at block N−1 for one liquidation. The
//! pre-state's own views are turned into the events each adapter folds and
//! sent through the bot's ingest (`liq_node::apply_block`, the log router,
//! every handler) as one synthetic block N−1; the borrowers then get the
//! adapter's resync reads. The bot's code builds every row; only the source
//! of the events is synthetic.

use super::state::{call, Header};
use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall, SolEvent};
use arrayvec::ArrayVec;
use liq_adapters_aave_v3::events::{cfg as aave_cfg, pool as aave_pool};
use liq_bot::bind::{self, BoundProtocol};
use liq_bot::index::BoundIndex;
use liq_node::{
    ApplyCtx, CollapsedDirty, DecodeArena, DirtyAccumulator, LogHandler, LogRouter, OwnedBlock,
    OwnedLog,
};
use liq_protocol::{DirtySet, Protocol, StateAnswer, StateRead};
use liq_sim::SimError;
use liq_state::StateStore;
use liq_types::PositionKey;
use revm::database_interface::DatabaseRef;

sol! {
    struct ReserveDataLegacySnap {
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
    struct EModeCollateralConfigSnap {
        uint16 ltv;
        uint16 liquidationThreshold;
        uint16 liquidationBonus;
    }
    interface IAaveSnap {
        function getReservesList() external view returns (address[] memory);
        function getReserveData(address asset) external view returns (ReserveDataLegacySnap memory);
        function getReserveNormalizedIncome(address asset) external view returns (uint256);
        function getReserveNormalizedVariableDebt(address asset) external view returns (uint256);
        function getLiquidationGracePeriod(address asset) external view returns (uint40);
        function getEModeCategoryCollateralConfig(uint8 id) external view returns (EModeCollateralConfigSnap memory);
        function getEModeCategoryCollateralBitmap(uint8 id) external view returns (uint128);
        function getEModeCategoryBorrowableBitmap(uint8 id) external view returns (uint128);
        function getEModeCategoryLtvzeroBitmap(uint8 id) external view returns (uint128);
        function getReserveAddressById(uint16 id) external view returns (address);
        function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
    }
    interface IErc20Snap {
        event Transfer(address indexed from, address indexed to, uint256 value);
        function balanceOf(address who) external view returns (uint256);
    }
}

/// Views, run against the pre-state as `eth_call` at the bot's tip.
pub struct Views<'a, D> {
    pub db: &'a D,
    pub at: &'a Header,
}

impl<D: DatabaseRef<Error = SimError>> Views<'_, D> {
    pub fn call<C: SolCall>(&self, to: Address, c: C) -> Result<C::Return, String> {
        let ran = call(
            self.db,
            self.at,
            Address::ZERO,
            to,
            c.abi_encode().into(),
            50_000_000,
        )?;
        if !ran.success {
            return Err(format!("{} reverted on {to:#x}", C::SIGNATURE));
        }
        C::abi_decode_returns(&ran.output).map_err(|e| format!("{}: {e}", C::SIGNATURE))
    }

    pub fn balance(&self, token: Address, who: Address) -> Result<U256, String> {
        self.call(token, IErc20Snap::balanceOfCall { who })
    }
}

/// The synthetic block's logs, in order.
pub struct Synth {
    pub logs: Vec<OwnedLog>,
    number: u64,
    timestamp: u64,
}

impl Synth {
    pub fn new(at: &Header) -> Self {
        Self {
            logs: Vec::new(),
            number: at.number,
            timestamp: at.timestamp,
        }
    }

    pub fn push<E: SolEvent>(&mut self, emitter: Address, ev: &E) {
        self.push_at(emitter, ev, self.timestamp);
    }

    /// A log stamped with an earlier time: state the chain last wrote then,
    /// which the adapter accrues forward from.
    pub fn push_at<E: SolEvent>(&mut self, emitter: Address, ev: &E, timestamp: u64) {
        let data = ev.encode_log_data();
        let topics: ArrayVec<_, 4> = data.topics().iter().copied().collect();
        let log_index = u32::try_from(self.logs.len()).unwrap();
        self.logs.push(OwnedLog {
            address: emitter,
            topics,
            data: data.data.to_vec(),
            block: self.number,
            timestamp,
            tx_index: 0,
            log_index,
        });
    }

    pub fn block(self, at: &Header) -> OwnedBlock {
        OwnedBlock {
            number: at.number,
            timestamp: at.timestamp,
            gas_limit: at.gas_limit,
            gas_used: at.gas_used,
            base_fee_per_gas: at.base_fee,
            logs: self.logs,
        }
    }
}

fn bits(word: U256, from: usize, len: usize) -> U256 {
    (word >> from) & ((U256::from(1u8) << len) - U256::from(1u8))
}

fn bit(word: U256, i: usize) -> bool {
    word.bit(i)
}

/// One Aave V3 pool as events: every reserve's initialization, risk
/// parameters, flags, protocol fee, grace period and indexes accrued to the
/// tip; e-mode categories and their asset sets; the flash premium; and each
/// borrower interned. Returns the reserves `(asset, aToken)`.
pub fn aave_v3<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    pool: Address,
    configurator: Address,
    borrowers: &[Address],
    s: &mut Synth,
) -> Result<Vec<(Address, Address)>, String> {
    let list = v.call(pool, IAaveSnap::getReservesListCall {})?;
    let mut reserves = Vec::new();
    for &asset in &list {
        let rd = v.call(pool, IAaveSnap::getReserveDataCall { asset })?;
        let c = rd.configuration;
        s.push(
            configurator,
            &aave_cfg::ReserveInitialized {
                asset,
                aToken: rd.aTokenAddress,
                stableDebtToken: rd.stableDebtTokenAddress,
                variableDebtToken: rd.variableDebtTokenAddress,
                interestRateStrategyAddress: rd.interestRateStrategyAddress,
            },
        );
        s.push(
            configurator,
            &aave_cfg::CollateralConfigurationChanged {
                asset,
                ltv: bits(c, 0, 16),
                liquidationThreshold: bits(c, 16, 16),
                liquidationBonus: bits(c, 32, 16),
            },
        );
        if !bit(c, 56) {
            s.push(
                configurator,
                &aave_cfg::ReserveActive {
                    asset,
                    active: false,
                },
            );
        }
        if bit(c, 57) {
            s.push(
                configurator,
                &aave_cfg::ReserveFrozen {
                    asset,
                    frozen: true,
                },
            );
        }
        if bit(c, 60) {
            s.push(
                configurator,
                &aave_cfg::ReservePaused {
                    asset,
                    paused: true,
                },
            );
        }
        s.push(
            configurator,
            &aave_cfg::ReserveBorrowing {
                asset,
                enabled: bit(c, 58),
            },
        );
        s.push(
            configurator,
            &aave_cfg::ReserveFlashLoaning {
                asset,
                enabled: bit(c, 63),
            },
        );
        s.push(
            configurator,
            &aave_cfg::LiquidationProtocolFeeChanged {
                asset,
                oldFee: U256::ZERO,
                newFee: bits(c, 152, 16),
            },
        );
        if let Ok(until) = v.call(pool, IAaveSnap::getLiquidationGracePeriodCall { asset }) {
            if u64::try_from(until).unwrap_or(0) > v.at.timestamp {
                s.push(
                    configurator,
                    &aave_cfg::LiquidationGracePeriodChanged {
                        asset,
                        gracePeriodUntil: until,
                    },
                );
            }
        }
        let li = v.call(pool, IAaveSnap::getReserveNormalizedIncomeCall { asset })?;
        let vi = v.call(
            pool,
            IAaveSnap::getReserveNormalizedVariableDebtCall { asset },
        )?;
        s.push(
            pool,
            &aave_pool::ReserveDataUpdated {
                reserve: asset,
                liquidityRate: U256::from(rd.currentLiquidityRate),
                stableBorrowRate: U256::ZERO,
                variableBorrowRate: U256::from(rd.currentVariableBorrowRate),
                liquidityIndex: li,
                variableBorrowIndex: vi,
            },
        );
        reserves.push((asset, rd.aTokenAddress));
    }
    for id in 1..=u8::MAX {
        let Ok(cat) = v.call(pool, IAaveSnap::getEModeCategoryCollateralConfigCall { id }) else {
            break;
        };
        if cat.liquidationThreshold == 0 {
            continue;
        }
        s.push(
            configurator,
            &aave_cfg::EModeCategoryAdded {
                categoryId: id,
                ltv: U256::from(cat.ltv),
                liquidationThreshold: U256::from(cat.liquidationThreshold),
                liquidationBonus: U256::from(cat.liquidationBonus),
                oracle: Address::ZERO,
                label: String::new(),
            },
        );
        let coll = v.call(pool, IAaveSnap::getEModeCategoryCollateralBitmapCall { id })?;
        let borrow = v.call(pool, IAaveSnap::getEModeCategoryBorrowableBitmapCall { id })?;
        let ltv0 = v
            .call(pool, IAaveSnap::getEModeCategoryLtvzeroBitmapCall { id })
            .unwrap_or(0);
        for b in 0..128u16 {
            let mask = 1u128 << b;
            if (coll | borrow | ltv0) & mask == 0 {
                continue;
            }
            let asset = v.call(pool, IAaveSnap::getReserveAddressByIdCall { id: b })?;
            if coll & mask != 0 {
                s.push(
                    configurator,
                    &aave_cfg::AssetCollateralInEModeChanged {
                        asset,
                        categoryId: id,
                        collateral: true,
                    },
                );
            }
            if borrow & mask != 0 {
                s.push(
                    configurator,
                    &aave_cfg::AssetBorrowableInEModeChanged {
                        asset,
                        categoryId: id,
                        borrowable: true,
                    },
                );
            }
            if ltv0 & mask != 0 {
                s.push(
                    configurator,
                    &aave_cfg::AssetLtvzeroInEModeChanged {
                        asset,
                        categoryId: id,
                        ltvzero: true,
                    },
                );
            }
        }
    }
    let premium = v.call(pool, IAaveSnap::FLASHLOAN_PREMIUM_TOTALCall {})?;
    s.push(
        configurator,
        &aave_cfg::FlashloanPremiumTotalUpdated {
            oldFlashloanPremiumTotal: 0,
            newFlashloanPremiumTotal: premium,
        },
    );
    let first = *list.first().ok_or("pool lists no reserves")?;
    for &user in borrowers {
        s.push(
            pool,
            &aave_pool::ReserveUsedAsCollateralEnabled {
                reserve: first,
                user,
            },
        );
    }
    Ok(reserves)
}

sol! {
    struct ReserveDataV2Snap {
        uint256 configuration;
        uint128 liquidityIndex;
        uint128 variableBorrowIndex;
        uint128 currentLiquidityRate;
        uint128 currentVariableBorrowRate;
        uint128 currentStableBorrowRate;
        uint40 lastUpdateTimestamp;
        address aTokenAddress;
        address stableDebtTokenAddress;
        address variableDebtTokenAddress;
        address interestRateStrategyAddress;
        uint8 id;
    }
    interface IAaveV2Snap {
        function getReservesList() external view returns (address[] memory);
        function getReserveData(address asset) external view returns (ReserveDataV2Snap memory);
        function getReserveNormalizedIncome(address asset) external view returns (uint256);
        function getReserveNormalizedVariableDebt(address asset) external view returns (uint256);
    }
    interface IGraceSnap {
        function gracePeriodUntil(address asset) external view returns (uint40);
    }
}

/// An Aave V2 pool as the events the adapter's V2 version folds: each
/// configured reserve's listing, risk parameters, activity / frozen state
/// and indexes at the tip, its grace window from the sentinel, and the
/// borrowers interned (their balances come from the resync reads).
pub fn aave_v2<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    pool: Address,
    configurator: Address,
    grace_sentinel: Address,
    configured: &[Address],
    borrowers: &[Address],
    s: &mut Synth,
) -> Result<(), String> {
    use liq_adapters_aave_v3::events::v2;
    let list = v.call(pool, IAaveV2Snap::getReservesListCall {})?;
    let mut first = None;
    for &asset in &list {
        if !configured.contains(&asset) {
            continue;
        }
        first.get_or_insert(asset);
        let rd = v.call(pool, IAaveV2Snap::getReserveDataCall { asset })?;
        let c = rd.configuration;
        s.push(
            configurator,
            &aave_cfg::ReserveInitialized {
                asset,
                aToken: rd.aTokenAddress,
                stableDebtToken: rd.stableDebtTokenAddress,
                variableDebtToken: rd.variableDebtTokenAddress,
                interestRateStrategyAddress: rd.interestRateStrategyAddress,
            },
        );
        s.push(
            configurator,
            &aave_cfg::CollateralConfigurationChanged {
                asset,
                ltv: bits(c, 0, 16),
                liquidationThreshold: bits(c, 16, 16),
                liquidationBonus: bits(c, 32, 16),
            },
        );
        if !bit(c, 56) {
            s.push(configurator, &v2::cfg::ReserveDeactivated { asset });
        }
        if bit(c, 57) {
            s.push(configurator, &v2::cfg::ReserveFrozen { asset });
        }
        if bit(c, 58) {
            s.push(
                configurator,
                &v2::cfg::BorrowingEnabledOnReserve {
                    asset,
                    stableRateEnabled: bit(c, 59),
                },
            );
        }
        if grace_sentinel != Address::ZERO {
            if let Ok(until) = v.call(grace_sentinel, IGraceSnap::gracePeriodUntilCall { asset }) {
                if u64::try_from(until).unwrap_or(0) >= v.at.timestamp {
                    s.push(grace_sentinel, &v2::grace::GracePeriodSet { asset, until });
                }
            }
        }
        let li = v.call(pool, IAaveV2Snap::getReserveNormalizedIncomeCall { asset })?;
        let vi = v.call(
            pool,
            IAaveV2Snap::getReserveNormalizedVariableDebtCall { asset },
        )?;
        s.push(
            pool,
            &aave_pool::ReserveDataUpdated {
                reserve: asset,
                liquidityRate: U256::from(rd.currentLiquidityRate),
                stableBorrowRate: U256::from(rd.currentStableBorrowRate),
                variableBorrowRate: U256::from(rd.currentVariableBorrowRate),
                liquidityIndex: li,
                variableBorrowIndex: vi,
            },
        );
    }
    let first = first.ok_or("no configured V2 reserve")?;
    for &user in borrowers {
        s.push(
            pool,
            &aave_pool::ReserveUsedAsCollateralEnabled {
                reserve: first,
                user,
            },
        );
    }
    Ok(())
}

/// Fold the synthetic block through the bot's ingest: the same router and
/// handlers the hot thread runs. Returns the block's dirty sets.
pub fn ingest(
    store: &mut StateStore,
    adapters: &'static [BoundProtocol],
    index: &'static BoundIndex,
    sink: &'static dyn liq_types::HaltSink,
    block: &OwnedBlock,
) -> Result<CollapsedDirty, String> {
    let subs = bind::router_subscribers(adapters, index);
    let router = LogRouter::from_subscribers(&subs).map_err(|e| e.to_string())?;
    let handlers = bind::router_handlers(adapters, index, sink);
    let refs: Vec<&dyn LogHandler> = handlers
        .iter()
        .map(|h| h.as_ref() as &dyn LogHandler)
        .collect();
    let mut arena = DecodeArena::with_capacity(1 << 20);
    let mut dirty = DirtyAccumulator::new();
    {
        let mut ctx = ApplyCtx {
            store: &mut *store,
            router: &router,
            handlers: &refs,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        liq_node::apply_block(&mut ctx, block, None).map_err(|e| format!("apply_block: {e}"))?;
    }
    dirty
        .collapse(store, block.timestamp)
        .cloned()
        .map_err(|e| e.to_string())
}

/// The adapter's resync reads for one borrower, answered on the pre-state
/// and applied as the bot applies them, until the position settles.
pub fn resync<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    proto: &dyn Protocol,
    store: &mut StateStore,
    key: &PositionKey,
) -> Result<(), String> {
    let ts = v.at.timestamp;
    let id = store.position_id(key).ok_or("borrower not interned")?;
    let exec = |r: &StateRead| -> Result<(bool, Vec<u8>), String> {
        let ran = call(
            v.db,
            v.at,
            Address::ZERO,
            r.target,
            r.calldata.clone(),
            50_000_000,
        )?;
        Ok((ran.success, ran.output.to_vec()))
    };
    for _ in 0..8 {
        let reads = {
            let view = store.view(ts);
            let pos = view.position(id).map_err(|e| e.to_string())?;
            proto.resync_reads(pos)
        };
        if reads.is_empty() {
            return Err("the adapter has no resync reads".into());
        }
        let first = reads.iter().map(exec).collect::<Result<Vec<_>, _>>()?;
        let answers: Vec<StateAnswer<'_>> = reads
            .iter()
            .zip(&first)
            .map(|(r, (ok, d))| StateAnswer {
                read: r,
                success: *ok,
                data: d,
            })
            .collect();
        let follow: Vec<StateRead> = answers
            .iter()
            .flat_map(|a| proto.state_follow_ups(*a))
            .collect();
        let second = follow.iter().map(exec).collect::<Result<Vec<_>, _>>()?;
        let mut all = answers.clone();
        all.extend(follow.iter().zip(&second).map(|(r, (ok, d))| StateAnswer {
            read: r,
            success: *ok,
            data: d,
        }));
        let sets = proto
            .apply_state_reads(store, ts, &all)
            .map_err(|e| e.to_string())?;
        let settled = sets
            .iter()
            .any(|s| matches!(s, DirtySet::Positions(ids) if ids.contains(&id)));
        if settled {
            return Ok(());
        }
    }
    Err("the borrower did not settle in 8 rounds".into())
}

sol! {
    struct MorphoParamsSnap {
        address loanToken;
        address collateralToken;
        address oracle;
        address irm;
        uint256 lltv;
    }
    struct MorphoMarketSnap {
        uint128 totalSupplyAssets;
        uint128 totalSupplyShares;
        uint128 totalBorrowAssets;
        uint128 totalBorrowShares;
        uint128 lastUpdate;
        uint128 fee;
    }
    interface IMorphoSnap {
        function idToMarketParams(bytes32 id) external view returns (address loanToken, address collateralToken, address oracle, address irm, uint256 lltv);
        function market(bytes32 id) external view returns (uint128 totalSupplyAssets, uint128 totalSupplyShares, uint128 totalBorrowAssets, uint128 totalBorrowShares, uint128 lastUpdate, uint128 fee);
        function position(bytes32 id, address user) external view returns (uint256 supplyShares, uint128 borrowShares, uint128 collateral);
    }
    interface IIrmSnap {
        function borrowRateView(MorphoParamsSnap memory marketParams, MorphoMarketSnap memory market) external view returns (uint256);
    }
    interface IMorphoOracleSnap {
        function price() external view returns (uint256);
    }
    interface IComptrollerSnap {
        function getAllMarkets() external view returns (address[] memory);
        /// The two fields every fork returns (Compound adds `isComped`; Fuse does not).
        function markets(address cToken) external view returns (bool isListed, uint256 collateralFactorMantissa);
        function closeFactorMantissa() external view returns (uint256);
        function liquidationIncentiveMantissa() external view returns (uint256);
        function oracle() external view returns (address);
        function getAssetsIn(address account) external view returns (address[] memory);
        function getAccountLiquidity(address account) external view returns (uint256 err, uint256 liquidity, uint256 shortfall);
        function seizeGuardianPaused() external view returns (bool);
    }
    interface ICTokenSnap {
        function getCash() external view returns (uint256);
        function totalBorrows() external view returns (uint256);
        function totalReserves() external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function borrowIndex() external view returns (uint256);
        function reserveFactorMantissa() external view returns (uint256);
        function borrowBalanceStored(address account) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
        function underlying() external view returns (address);
        function accrualBlockNumber() external view returns (uint256);
    }
}

/// A Morpho market's `(collateral, loan)` tokens, from the singleton.
pub fn morpho_tokens<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    morpho: Address,
    id: alloy_primitives::B256,
) -> Result<(Address, Address), String> {
    let p = v.call(morpho, IMorphoSnap::idToMarketParamsCall { id })?;
    Ok((p.collateralToken, p.loanToken))
}

/// What the singleton says of the borrower, beside its oracle: the health
/// oracle's inputs.
pub struct MorphoChain {
    pub borrow_shares: u128,
    pub collateral: u128,
    pub total_borrow_assets: u128,
    pub total_borrow_shares: u128,
    pub price: U256,
    pub lltv: U256,
}

/// One Morpho market as the events the adapter folds, stamped with the
/// market's own `lastUpdate` (the adapter accrues from there, as the
/// singleton does): its creation, fee, totals, the borrow rate the IRM
/// reports, and the borrower's shares and collateral. Everyone else's supply
/// and debt are one synthetic account's.
pub fn morpho<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    morpho: Address,
    id: alloy_primitives::B256,
    borrower: Address,
    s: &mut Synth,
) -> Result<MorphoChain, String> {
    use liq_adapters_morpho_blue::events as ev;
    let p = v.call(morpho, IMorphoSnap::idToMarketParamsCall { id })?;
    let m = v.call(morpho, IMorphoSnap::marketCall { id })?;
    let pos = v.call(morpho, IMorphoSnap::positionCall { id, user: borrower })?;
    let ts = u64::try_from(m.lastUpdate).map_err(|e| e.to_string())?;
    let rest = Address::repeat_byte(0xEE);
    s.push_at(
        morpho,
        &ev::CreateMarket {
            id,
            marketParams: ev::MarketParams {
                loanToken: p.loanToken,
                collateralToken: p.collateralToken,
                oracle: p.oracle,
                irm: p.irm,
                lltv: p.lltv,
            },
        },
        ts,
    );
    s.push_at(
        morpho,
        &ev::SetFee {
            id,
            newFee: U256::from(m.fee),
        },
        ts,
    );
    s.push_at(
        morpho,
        &ev::Supply {
            id,
            caller: rest,
            onBehalf: rest,
            assets: U256::from(m.totalSupplyAssets),
            shares: U256::from(m.totalSupplyShares),
        },
        ts,
    );
    let others = m
        .totalBorrowShares
        .checked_sub(pos.borrowShares)
        .ok_or("borrower holds more shares than the market")?;
    s.push_at(
        morpho,
        &ev::Borrow {
            id,
            caller: rest,
            onBehalf: rest,
            receiver: rest,
            assets: U256::from(m.totalBorrowAssets),
            shares: U256::from(others),
        },
        ts,
    );
    s.push_at(
        morpho,
        &ev::Borrow {
            id,
            caller: borrower,
            onBehalf: borrower,
            receiver: borrower,
            assets: U256::ZERO,
            shares: U256::from(pos.borrowShares),
        },
        ts,
    );
    s.push_at(
        morpho,
        &ev::SupplyCollateral {
            id,
            caller: borrower,
            onBehalf: borrower,
            assets: U256::from(pos.collateral),
        },
        ts,
    );
    let rate = if p.irm.is_zero() {
        U256::ZERO
    } else {
        v.call(
            p.irm,
            IIrmSnap::borrowRateViewCall {
                marketParams: MorphoParamsSnap {
                    loanToken: p.loanToken,
                    collateralToken: p.collateralToken,
                    oracle: p.oracle,
                    irm: p.irm,
                    lltv: p.lltv,
                },
                market: MorphoMarketSnap {
                    totalSupplyAssets: m.totalSupplyAssets,
                    totalSupplyShares: m.totalSupplyShares,
                    totalBorrowAssets: m.totalBorrowAssets,
                    totalBorrowShares: m.totalBorrowShares,
                    lastUpdate: m.lastUpdate,
                    fee: m.fee,
                },
            },
        )?
    };
    s.push_at(
        morpho,
        &ev::AccrueInterest {
            id,
            prevBorrowRate: rate,
            interest: U256::ZERO,
            feeShares: U256::ZERO,
        },
        ts,
    );
    let price = v.call(p.oracle, IMorphoOracleSnap::priceCall {})?;
    Ok(MorphoChain {
        borrow_shares: pos.borrowShares,
        collateral: pos.collateral,
        total_borrow_assets: m.totalBorrowAssets,
        total_borrow_shares: m.totalBorrowShares,
        price,
        lltv: p.lltv,
    })
}

sol! {
    struct LTVFullSnap {
        uint16 borrowLTV;
        uint16 liquidationLTV;
        uint16 initialLiquidationLTV;
        uint48 targetTimestamp;
        uint32 rampDuration;
    }
    struct HookConfigSnap {
        address hookTarget;
        uint32 hookedOps;
    }
    struct AccountLiquiditySnap {
        uint256 collateralValue;
        uint256 liabilityValue;
    }
    interface IEVaultSnap {
        function asset() external view returns (address);
        function oracle() external view returns (address);
        function unitOfAccount() external view returns (address);
        function LTVList() external view returns (address[] memory);
        function LTVFull(address collateral) external view returns (LTVFullSnap memory);
        function maxLiquidationDiscount() external view returns (uint16);
        function liquidationCoolOffTime() external view returns (uint16);
        function hookConfig() external view returns (HookConfigSnap memory);
        function configFlags() external view returns (uint32);
        function interestFee() external view returns (uint16);
        function interestRate() external view returns (uint256);
        function interestAccumulator() external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function totalBorrows() external view returns (uint256);
        function accumulatedFees() external view returns (uint256);
        function cash() external view returns (uint256);
        function accountLiquidity(address account, bool liquidation) external view returns (AccountLiquiditySnap memory);
    }
}

/// An EVK vault's `ProxyCreated` as the factory logged it: the proxy's
/// trailing metadata is `asset ‖ oracle ‖ unitOfAccount`.
fn euler_proxy<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    factory: Address,
    vault: Address,
    s: &mut Synth,
) -> Result<Address, String> {
    use liq_adapters_euler_v2::events as ev;
    let asset = v.call(vault, IEVaultSnap::assetCall {})?;
    let oracle = v.call(vault, IEVaultSnap::oracleCall {})?;
    let unit = v.call(vault, IEVaultSnap::unitOfAccountCall {})?;
    let mut trailing = Vec::with_capacity(60);
    for a in [asset, oracle, unit] {
        trailing.extend_from_slice(a.as_slice());
    }
    s.push(
        factory,
        &ev::ProxyCreated {
            proxy: vault,
            upgradeable: true,
            implementation: Address::ZERO,
            trailingData: trailing.into(),
        },
    );
    Ok(asset)
}

/// One Euler debt vault as the events the adapter folds: its listing and
/// every collateral vault it recognizes (their own listings, so their share
/// transfers are routed), each collateral's LTVs, the governor's
/// liquidation parameters, and a `VaultStatus` with the accumulator and
/// rate at the tip. The borrower is interned by the EVC's
/// `ControllerStatus`; the adapter's resync reads then load their debt,
/// shares and collateral enables. Returns the debt asset and the
/// collateral vaults.
pub fn euler<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    factory: Address,
    evc: Address,
    vault: Address,
    borrower: Address,
    s: &mut Synth,
) -> Result<(Address, Vec<Address>), String> {
    use liq_adapters_euler_v2::events as ev;
    let asset = euler_proxy(v, factory, vault, s)?;
    let colls = v.call(vault, IEVaultSnap::LTVListCall {})?;
    for &c in &colls {
        if c != vault {
            euler_proxy(v, factory, c, s)?;
        }
    }
    for &c in &colls {
        let l = v.call(vault, IEVaultSnap::LTVFullCall { collateral: c })?;
        s.push(
            vault,
            &ev::GovSetLTV {
                collateral: c,
                borrowLTV: l.borrowLTV,
                liquidationLTV: l.liquidationLTV,
                initialLiquidationLTV: l.initialLiquidationLTV,
                targetTimestamp: l.targetTimestamp,
                rampDuration: l.rampDuration,
            },
        );
    }
    s.push(
        vault,
        &ev::GovSetMaxLiquidationDiscount {
            newDiscount: v.call(vault, IEVaultSnap::maxLiquidationDiscountCall {})?,
        },
    );
    s.push(
        vault,
        &ev::GovSetLiquidationCoolOffTime {
            newCoolOffTime: v.call(vault, IEVaultSnap::liquidationCoolOffTimeCall {})?,
        },
    );
    let hook = v.call(vault, IEVaultSnap::hookConfigCall {})?;
    s.push(
        vault,
        &ev::GovSetHookConfig {
            newHookTarget: hook.hookTarget,
            newHookedOps: hook.hookedOps,
        },
    );
    s.push(
        vault,
        &ev::GovSetConfigFlags {
            newConfigFlags: v.call(vault, IEVaultSnap::configFlagsCall {})?,
        },
    );
    s.push(
        vault,
        &ev::GovSetInterestFee {
            newFee: v.call(vault, IEVaultSnap::interestFeeCall {})?,
        },
    );
    // EVK's views accrue to the block's time, so the accumulator is the
    // tip's and the status is stamped at the tip.
    s.push(
        vault,
        &ev::VaultStatus {
            totalShares: v.call(vault, IEVaultSnap::totalSupplyCall {})?,
            totalBorrows: v.call(vault, IEVaultSnap::totalBorrowsCall {})?,
            accumulatedFees: v.call(vault, IEVaultSnap::accumulatedFeesCall {})?,
            cash: v.call(vault, IEVaultSnap::cashCall {})?,
            interestAccumulator: v.call(vault, IEVaultSnap::interestAccumulatorCall {})?,
            interestRate: v.call(vault, IEVaultSnap::interestRateCall {})?,
            timestamp: U256::from(v.at.timestamp),
        },
    );
    s.push(
        evc,
        &ev::evc::ControllerStatus {
            account: borrower,
            controller: vault,
            enabled: true,
        },
    );
    Ok((asset, colls))
}

/// The vault's own verdict on the account: `(collateral, liability)` value
/// in the unit of account, liquidation LTVs applied.
pub fn euler_liquidity<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    vault: Address,
    account: Address,
) -> Result<(U256, U256), String> {
    let r = v.call(
        vault,
        IEVaultSnap::accountLiquidityCall {
            account,
            liquidation: true,
        },
    )?;
    Ok((r.collateralValue, r.liabilityValue))
}

/// An EVK vault's asset.
pub fn euler_asset<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    vault: Address,
) -> Result<Address, String> {
    v.call(vault, IEVaultSnap::assetCall {})
}

sol! {
    interface ISiloSnap {
        function config() external view returns (address);
        function isSolvent(address borrower) external view returns (bool);
        function getTotalAssetsStorage(uint8 assetType) external view returns (uint256);
        function getCollateralAndDebtTotalsStorage() external view returns (uint256, uint256);
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }
    interface ISiloConfigSnap {
        function getSilos() external view returns (address, address);
        function borrowerCollateralSilo(address borrower) external view returns (address);
    }
}

/// `ISilo.AssetType`: Protected, Collateral, Debt.
const SILO_PROTECTED: u8 = 0;
const SILO_COLLATERAL: u8 = 1;
const SILO_DEBT: u8 = 2;

/// The two silos of the pair `silo` belongs to, as its `SiloConfig` lists them.
pub fn silo_pair<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    silo: Address,
) -> Result<(Address, Address), String> {
    let cfg = v.call(silo, ISiloSnap::configCall {})?;
    v.call(cfg, ISiloConfigSnap::getSilosCall {})
        .map(|r| (r._0, r._1))
}

/// One Silo pair as the events the adapter folds: its `NewSilo` listing,
/// then the borrower's protected and collateral shares on each silo
/// (`DepositProtected` / `Deposit`), its debt shares (`Borrow`), and the
/// silo its collateral counts in (`CollateralTypeChanged`, after the borrow,
/// which would otherwise default to the other silo). The amounts in assets
/// are left at zero: [`silo_totals`] sets each silo's totals from the chain.
pub fn silo<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    factory: Address,
    pair: &liq_adapters_silo_v2::config::PairConfig,
    borrower: Address,
    s: &mut Synth,
) -> Result<(), String> {
    use liq_adapters_silo_v2::events::{factory as fev, silo as sev};
    s.push(
        factory,
        &fev::NewSilo {
            implementation: Address::ZERO,
            token0: pair.silo0.token,
            token1: pair.silo1.token,
            silo0: pair.silo0.silo,
            silo1: pair.silo1.silo,
            siloConfig: pair.silo_config,
        },
    );
    let bal = |token: Address| v.call(token, ISiloSnap::balanceOfCall { account: borrower });
    for side in [&pair.silo0, &pair.silo1] {
        let protected = bal(side.protected_share)?;
        if !protected.is_zero() {
            s.push(
                side.silo,
                &sev::DepositProtected {
                    sender: borrower,
                    owner: borrower,
                    assets: U256::ZERO,
                    shares: protected,
                },
            );
        }
        let collateral = bal(side.silo)?;
        if !collateral.is_zero() {
            s.push(
                side.silo,
                &sev::Deposit {
                    sender: borrower,
                    owner: borrower,
                    assets: U256::ZERO,
                    shares: collateral,
                },
            );
        }
        let debt = bal(side.debt_share)?;
        if !debt.is_zero() {
            s.push(
                side.silo,
                &sev::Borrow {
                    sender: borrower,
                    receiver: borrower,
                    owner: borrower,
                    assets: U256::ZERO,
                    shares: debt,
                },
            );
        }
    }
    let coll_silo = v.call(
        pair.silo_config,
        ISiloConfigSnap::borrowerCollateralSiloCall { borrower },
    )?;
    if !coll_silo.is_zero() {
        s.push(coll_silo, &sev::CollateralTypeChanged { borrower });
    }
    Ok(())
}

/// Each silo of `pair`'s totals as the chain stores them: assets by
/// `getTotalAssetsStorage` (checked against `getCollateralAndDebtTotalsStorage`,
/// which must agree on collateral and debt), shares by each share token's
/// `totalSupply`. Written into the pair's rows after ingest; the bot's
/// state reads then grow a borrowed silo's to the tip.
pub fn silo_totals<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    store: &mut StateStore,
    pair: &liq_adapters_silo_v2::config::PairConfig,
) -> Result<(), String> {
    use liq_adapters_silo_v2::layout::SiloRow;
    use liq_protocol::{MarketSlot, StateWriter};
    for (slot, side) in [(0u16, &pair.silo0), (1u16, &pair.silo1)] {
        let assets = |t: u8| {
            v.call(
                side.silo,
                ISiloSnap::getTotalAssetsStorageCall { assetType: t },
            )
        };
        let (protected, collateral, debt) = (
            assets(SILO_PROTECTED)?,
            assets(SILO_COLLATERAL)?,
            assets(SILO_DEBT)?,
        );
        let storage = v.call(
            side.silo,
            ISiloSnap::getCollateralAndDebtTotalsStorageCall {},
        )?;
        if (storage._0, storage._1) != (collateral, debt) {
            return Err(format!(
                "silo {:#x}: getTotalAssetsStorage disagrees with getCollateralAndDebtTotalsStorage",
                side.silo
            ));
        }
        let supply = |t: Address| v.call(t, ISiloSnap::totalSupplyCall {});
        let narrow = |x: U256| u128::try_from(x).map_err(|_| "silo total past u128".to_string());
        let at = MarketSlot {
            market: pair.market,
            slot,
        };
        let mut row = *store.market(at).map_err(|e| e.to_string())?;
        {
            let b: &mut SiloRow = row.body_mut().map_err(|e| e.to_string())?;
            b.total_protected_assets = narrow(protected)?;
            b.total_collateral_assets = narrow(collateral)?;
            b.total_debt_assets = narrow(debt)?;
            b.total_protected_shares = narrow(supply(side.protected_share)?)?;
            b.total_collateral_shares = narrow(supply(side.silo)?)?;
            b.total_debt_shares = narrow(supply(side.debt_share)?)?;
        }
        store.set_market(at, row).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The silo's own verdict: `isSolvent(borrower)`.
pub fn silo_solvent<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    silo: Address,
    borrower: Address,
) -> Result<bool, String> {
    v.call(silo, ISiloSnap::isSolventCall { borrower })
}

/// A cToken's underlying; `None` for cETH, which has none.
pub fn ctoken_underlying<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    ctoken: Address,
) -> Option<Address> {
    v.call(ctoken, ICTokenSnap::underlyingCall {}).ok()
}

/// The Comptroller's own verdict on the account: `(liquidity, shortfall)`.
pub fn compound_liquidity<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    comptroller: Address,
    account: Address,
) -> Result<(U256, U256), String> {
    let r = v.call(
        comptroller,
        IComptrollerSnap::getAccountLiquidityCall { account },
    )?;
    if !r.err.is_zero() {
        return Err(format!("getAccountLiquidity error {}", r.err));
    }
    Ok((r.liquidity, r.shortfall))
}

/// One Compound V2 Comptroller as the events the adapter folds: every
/// listed market with its collateral factor, reserve factor and stored
/// totals (what the last `AccrueInterest` left, which is what the bot holds
/// at the tip), the Comptroller's parameters, and the borrower's entered
/// markets, cToken balances and stored borrows.
pub fn compound<D: DatabaseRef<Error = SimError>>(
    v: &Views<'_, D>,
    comptroller: Address,
    borrower: Address,
    s: &mut Synth,
) -> Result<(), String> {
    use liq_adapters_compound_v2::events::{comptroller as cmp, ctoken as ct, pause_global};
    let all = v.call(comptroller, IComptrollerSnap::getAllMarketsCall {})?;
    let one = U256::from(10u64).pow(U256::from(18u8));
    s.push(
        comptroller,
        &cmp::NewCloseFactor {
            oldCloseFactorMantissa: U256::ZERO,
            newCloseFactorMantissa: v
                .call(comptroller, IComptrollerSnap::closeFactorMantissaCall {})?,
        },
    );
    s.push(
        comptroller,
        &cmp::NewLiquidationIncentive {
            oldLiquidationIncentiveMantissa: U256::ZERO,
            newLiquidationIncentiveMantissa: v.call(
                comptroller,
                IComptrollerSnap::liquidationIncentiveMantissaCall {},
            )?,
        },
    );
    s.push(
        comptroller,
        &cmp::NewPriceOracle {
            oldPriceOracle: Address::ZERO,
            newPriceOracle: v.call(comptroller, IComptrollerSnap::oracleCall {})?,
        },
    );
    if v.call(comptroller, IComptrollerSnap::seizeGuardianPausedCall {})
        .unwrap_or(false)
    {
        s.push(
            comptroller,
            &pause_global::ActionPaused {
                action: "Seize".into(),
                pauseState: true,
            },
        );
    }
    let rest = Address::repeat_byte(0xEE);
    for &c in &all {
        let listed = v.call(comptroller, IComptrollerSnap::marketsCall { cToken: c })?;
        if !listed.isListed {
            continue;
        }
        s.push(comptroller, &cmp::MarketListed { cToken: c });
        s.push(
            comptroller,
            &cmp::NewCollateralFactor {
                cToken: c,
                oldCollateralFactorMantissa: U256::ZERO,
                newCollateralFactorMantissa: listed.collateralFactorMantissa,
            },
        );
        // Supply first, then the totals. Reserves only ever grow by
        // `reserveFactor * interest`, so they are set with the factor at one
        // and the real factor follows.
        s.push(
            c,
            &ct::Mint {
                minter: rest,
                mintAmount: U256::ZERO,
                mintTokens: v.call(c, ICTokenSnap::totalSupplyCall {})?,
            },
        );
        s.push(
            c,
            &ct::NewReserveFactor {
                oldReserveFactorMantissa: U256::ZERO,
                newReserveFactorMantissa: one,
            },
        );
        let total_borrows = v.call(c, ICTokenSnap::totalBorrowsCall {})?;
        // Stamped when the cToken last accrued: the adapter projects
        // interest from there. One slot per block since the merge.
        let accrued = v.call(c, ICTokenSnap::accrualBlockNumberCall {})?;
        let behind = v.at.number.saturating_sub(accrued.saturating_to::<u64>());
        let accrual_ts = v.at.timestamp.saturating_sub(behind.saturating_mul(12));
        s.push_at(
            c,
            &ct::AccrueInterest {
                cashPrior: v.call(c, ICTokenSnap::getCashCall {})?,
                interestAccumulated: v.call(c, ICTokenSnap::totalReservesCall {})?,
                borrowIndex: v.call(c, ICTokenSnap::borrowIndexCall {})?,
                totalBorrows: total_borrows,
            },
            accrual_ts,
        );
        s.push(
            c,
            &ct::NewReserveFactor {
                oldReserveFactorMantissa: one,
                newReserveFactorMantissa: v.call(c, ICTokenSnap::reserveFactorMantissaCall {})?,
            },
        );
        let held = v.call(c, ICTokenSnap::balanceOfCall { owner: borrower })?;
        if !held.is_zero() {
            s.push(
                c,
                &ct::Transfer {
                    from: Address::ZERO,
                    to: borrower,
                    amount: held,
                },
            );
        }
        let owed = v.call(
            c,
            ICTokenSnap::borrowBalanceStoredCall { account: borrower },
        )?;
        if !owed.is_zero() {
            s.push(
                c,
                &ct::Borrow {
                    borrower,
                    borrowAmount: U256::ZERO,
                    accountBorrows: owed,
                    totalBorrows: total_borrows,
                },
            );
        }
    }
    for c in v.call(
        comptroller,
        IComptrollerSnap::getAssetsInCall { account: borrower },
    )? {
        s.push(
            comptroller,
            &cmp::MarketEntered {
                cToken: c,
                account: borrower,
            },
        );
    }
    Ok(())
}
