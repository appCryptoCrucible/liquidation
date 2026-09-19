//! `sol!` event ABIs from `docs/coverage/aave-v4.md` + `aave-v3.md` (03C).
//! Decode copies topics+data into a [`bumpalo`] arena (GUIDE 03 §4b).

#![allow(clippy::too_many_arguments)] // sol! event constructors mirror ABI arity

use alloy_primitives::B256;
use alloy_sol_types::{sol, SolEvent};

use crate::source::OwnedLog;
use crate::{IngestError, Result};
use liq_protocol::DecodedLog;

sol! {
    struct PremiumDelta {
        int256 sharesDelta;
        int256 offsetRayDelta;
        uint256 restoredPremiumRay;
    }
    struct AssetConfig {
        address feeReceiver;
        uint16 liquidityFee;
        address irStrategy;
        address reinvestmentController;
    }
    struct SpokeConfig {
        uint40 addCap;
        uint40 drawCap;
        uint24 riskPremiumThreshold;
        bool active;
        bool halted;
    }
    struct ReserveConfig {
        uint24 collateralRisk;
        bool paused;
        bool frozen;
        bool borrowable;
        bool receiveSharesEnabled;
    }
    struct DynamicReserveConfig {
        uint16 collateralFactor;
        uint32 maxLiquidationBonus;
        uint16 liquidationFee;
    }
    struct LiquidationConfig {
        uint128 targetHealthFactor;
        uint64 healthFactorForMaxBonus;
        uint16 liquidationBonusFactor;
    }

    interface IERC20 {
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
    interface IERC1967 {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
    }
    interface IInitializable {
        event Initialized(uint64 version);
    }
    interface IAccessManaged {
        event AuthorityUpdated(address authority);
    }
    interface IERC4626 {
        event Deposit(address indexed sender, address indexed owner, uint256 assets, uint256 shares);
        event Withdraw(address indexed sender, address indexed receiver, address indexed owner, uint256 assets, uint256 shares);
    }
    interface IPayloadsControllerCore {
        event PayloadExecuted(uint40 payloadId);
    }

    interface IHubBase {
        event Add(uint256 indexed assetId, address indexed spoke, uint256 shares, uint256 amount);
        event Remove(uint256 indexed assetId, address indexed spoke, uint256 shares, uint256 amount);
        event Draw(uint256 indexed assetId, address indexed spoke, uint256 drawnShares, uint256 drawnAmount);
        event Restore(uint256 indexed assetId, address indexed spoke, uint256 drawnShares, PremiumDelta premiumDelta, uint256 drawnAmount, uint256 premiumAmount);
        event RefreshPremium(uint256 indexed assetId, address indexed spoke, PremiumDelta premiumDelta);
        event ReportDeficit(uint256 indexed assetId, address indexed spoke, uint256 drawnShares, PremiumDelta premiumDelta, uint256 deficitAmountRay);
        event TransferShares(uint256 indexed assetId, address indexed sender, address indexed receiver, uint256 shares);
    }
    interface IHub {
        event AddAsset(uint256 indexed assetId, address indexed underlying, uint8 decimals);
        event UpdateAsset(uint256 indexed assetId, uint256 drawnIndex, uint256 drawnRate, uint256 accruedFees);
        event UpdateAssetConfig(uint256 indexed assetId, AssetConfig config);
        event AddSpoke(uint256 indexed assetId, address indexed spoke);
        event UpdateSpokeConfig(uint256 indexed assetId, address indexed spoke, SpokeConfig config);
        event MintFeeShares(uint256 indexed assetId, address indexed feeReceiver, uint256 shares, uint256 assets);
        event Sweep(uint256 indexed assetId, address indexed reinvestmentController, uint256 amount);
        event Reclaim(uint256 indexed assetId, address indexed reinvestmentController, uint256 amount);
        event EliminateDeficit(uint256 indexed assetId, address indexed callerSpoke, address indexed coveredSpoke, uint256 shares, uint256 deficitAmountRay);
    }
    interface IAssetInterestRateStrategy {
        event UpdateInterestRateData(address indexed hub, uint256 indexed assetId, uint256 optimalUsageRatio, uint256 baseDrawnRate, uint256 rateGrowthBeforeOptimal, uint256 rateGrowthAfterOptimal);
    }
    interface ISpoke {
        event SetSpokeImmutables(address indexed oracle, uint16 maxUserReservesLimit);
        event UpdateLiquidationConfig(LiquidationConfig config);
        event AddReserve(uint256 indexed reserveId, uint256 indexed assetId, address indexed hub);
        event UpdateReserveConfig(uint256 indexed reserveId, ReserveConfig config);
        event UpdateReservePriceSource(uint256 indexed reserveId, address indexed priceSource);
        event AddDynamicReserveConfig(uint256 indexed reserveId, uint32 indexed dynamicConfigKey, DynamicReserveConfig config);
        event UpdateDynamicReserveConfig(uint256 indexed reserveId, uint32 indexed dynamicConfigKey, DynamicReserveConfig config);
        event UpdatePositionManager(address indexed positionManager, bool active);
        event Supply(uint256 indexed reserveId, address indexed caller, address indexed user, uint256 suppliedShares, uint256 suppliedAmount);
        event Withdraw(uint256 indexed reserveId, address indexed caller, address indexed user, uint256 withdrawnShares, uint256 withdrawnAmount);
        event Borrow(uint256 indexed reserveId, address indexed caller, address indexed user, uint256 drawnShares, uint256 drawnAmount);
        event Repay(uint256 indexed reserveId, address indexed caller, address indexed user, uint256 drawnShares, uint256 totalAmountRepaid, PremiumDelta premiumDelta);
        event LiquidationCall(uint256 indexed collateralReserveId, uint256 indexed debtReserveId, address indexed user, address liquidator, bool receiveShares, uint256 debtAmountRestored, uint256 drawnSharesLiquidated, PremiumDelta premiumDelta, uint256 collateralAmountRemoved, uint256 collateralSharesLiquidated, uint256 collateralSharesToLiquidator);
        event ReportDeficit(uint256 indexed reserveId, address indexed user, uint256 drawnShares, PremiumDelta premiumDelta);
        event SetUsingAsCollateral(uint256 indexed reserveId, address indexed caller, address indexed user, bool usingAsCollateral);
        event UpdateUserRiskPremium(address indexed user, uint256 riskPremium);
        event RefreshAllUserDynamicConfig(address indexed user);
        event RefreshSingleUserDynamicConfig(address indexed user, uint256 reserveId);
        event SetUserPositionManager(address indexed user, address indexed positionManager, bool approve);
        event RefreshPremiumDebt(uint256 indexed reserveId, address indexed user, PremiumDelta premiumDelta);
    }
    interface IAaveOracleV4 {
        event UpdateReserveSource(uint256 indexed reserveId, address indexed source);
        event SetSpoke(address indexed spoke);
    }
    interface ITokenizationSpoke {
        event SetTokenizationSpokeImmutables(address indexed hub, uint256 indexed assetId);
    }
    interface IPositionManagerBase {
        event RegisterSpoke(address indexed spoke, bool registered);
    }
    interface IGiverPositionManager {
        event SupplyOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf, uint256 reserveId, uint256 suppliedShares, uint256 suppliedAmount);
        event RepayOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf, uint256 reserveId, uint256 repaidShares, uint256 repaidAmount);
    }
    interface ITakerPositionManager {
        event WithdrawApproval(address indexed spoke, address indexed owner, address indexed spender, uint256 reserveId, uint256 amount);
        event BorrowApproval(address indexed spoke, address indexed owner, address indexed spender, uint256 reserveId, uint256 amount);
        event WithdrawOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf, uint256 reserveId, uint256 withdrawnShares, uint256 withdrawnAmount);
        event BorrowOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf, uint256 reserveId, uint256 drawnShares, uint256 drawnAmount);
    }
    interface IConfigPositionManager {
        event UpdateConfigPermissions(address indexed spoke, address indexed delegator, address indexed delegatee, uint8 oldPermissions, uint8 newPermissions);
        event SetUsingAsCollateralOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf, uint256 reserveId, bool usingAsCollateral);
        event UpdateUserRiskPremiumOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf);
        event UpdateUserDynamicConfigOnBehalfOf(address indexed spoke, address indexed caller, address indexed onBehalfOf);
    }

    interface IPool {
        event Supply(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint16 indexed referralCode);
        event Withdraw(address indexed reserve, address indexed user, address indexed to, uint256 amount);
        event Borrow(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint8 interestRateMode, uint256 borrowRate, uint16 indexed referralCode);
        event Repay(address indexed reserve, address indexed user, address indexed repayer, uint256 amount, bool useATokens);
        event UserEModeSet(address indexed user, uint8 categoryId);
        event ReserveUsedAsCollateralEnabled(address indexed reserve, address indexed user);
        event ReserveUsedAsCollateralDisabled(address indexed reserve, address indexed user);
        event FlashLoan(address indexed target, address initiator, address indexed asset, uint256 amount, uint8 interestRateMode, uint256 premium, uint16 indexed referralCode);
        event LiquidationCall(address indexed collateralAsset, address indexed debtAsset, address indexed user, uint256 debtToCover, uint256 liquidatedCollateralAmount, address liquidator, bool receiveAToken);
        event ReserveDataUpdated(address indexed reserve, uint256 liquidityRate, uint256 stableBorrowRate, uint256 variableBorrowRate, uint256 liquidityIndex, uint256 variableBorrowIndex);
        event DeficitCovered(address indexed reserve, address caller, uint256 amountCovered);
        event MintedToTreasury(address indexed reserve, uint256 amountMinted);
        event DeficitCreated(address indexed user, address indexed debtAsset, uint256 amountCreated);
        event PositionManagerApproved(address indexed user, address indexed positionManager);
        event PositionManagerRevoked(address indexed user, address indexed positionManager);
    }
    interface IScaledBalanceToken {
        event Mint(address indexed caller, address indexed onBehalfOf, uint256 value, uint256 balanceIncrease, uint256 index);
        event Burn(address indexed from, address indexed target, uint256 value, uint256 balanceIncrease, uint256 index);
    }
    interface IAToken {
        event BalanceTransfer(address indexed from, address indexed to, uint256 value, uint256 index);
        event Initialized(address indexed underlyingAsset, address indexed pool, address treasury, address incentivesController, uint8 aTokenDecimals, string aTokenName, string aTokenSymbol, bytes params);
    }
    interface IVariableDebtToken {
        event Initialized(address indexed underlyingAsset, address indexed pool, address incentivesController, uint8 debtTokenDecimals, string debtTokenName, string debtTokenSymbol, bytes params);
    }
    interface IPoolConfigurator {
        event ReserveInitialized(address indexed asset, address indexed aToken, address stableDebtToken, address variableDebtToken, address interestRateStrategyAddress);
        event ReserveBorrowing(address indexed asset, bool enabled);
        event ReserveFlashLoaning(address indexed asset, bool enabled);
        event PendingLtvChanged(address indexed asset, uint256 ltv);
        event CollateralConfigurationChanged(address indexed asset, uint256 ltv, uint256 liquidationThreshold, uint256 liquidationBonus);
        event ReserveActive(address indexed asset, bool active);
        event ReserveFrozen(address indexed asset, bool frozen);
        event ReservePaused(address indexed asset, bool paused);
        event ReserveFactorChanged(address indexed asset, uint256 oldReserveFactor, uint256 newReserveFactor);
        event BorrowCapChanged(address indexed asset, uint256 oldBorrowCap, uint256 newBorrowCap);
        event SupplyCapChanged(address indexed asset, uint256 oldSupplyCap, uint256 newSupplyCap);
        event LiquidationProtocolFeeChanged(address indexed asset, uint256 oldFee, uint256 newFee);
        event LiquidationGracePeriodChanged(address indexed asset, uint40 gracePeriodUntil);
        event LiquidationGracePeriodDisabled(address indexed asset);
        event AssetCollateralInEModeChanged(address indexed asset, uint8 categoryId, bool collateral);
        event AssetBorrowableInEModeChanged(address indexed asset, uint8 categoryId, bool borrowable);
        event AssetLtvzeroInEModeChanged(address indexed asset, uint8 categoryId, bool ltvzero);
        event EModeCategoryAdded(uint8 indexed categoryId, uint256 ltv, uint256 liquidationThreshold, uint256 liquidationBonus, address oracle, string label);
        event EModeCategoryIsolationChanged(uint8 indexed categoryId, bool isolated);
        event ReserveInterestRateDataChanged(address indexed asset, address indexed strategy, bytes data);
        event ATokenUpgraded(address indexed asset, address indexed proxy, address indexed implementation);
        event VariableDebtTokenUpgraded(address indexed asset, address indexed proxy, address indexed implementation);
        event FlashloanPremiumTotalUpdated(uint128 oldFlashloanPremiumTotal, uint128 newFlashloanPremiumTotal);
    }
    interface IAaveOracleV3 {
        event BaseCurrencySet(address indexed baseCurrency, uint256 baseCurrencyUnit);
        event AssetSourceUpdated(address indexed asset, address indexed source);
        event FallbackOracleUpdated(address indexed fallbackOracle);
    }
    interface IDefaultReserveInterestRateStrategyV2 {
        event RateDataUpdate(address indexed reserve, uint256 optimalUsageRatio, uint256 baseVariableBorrowRate, uint256 variableRateSlope1, uint256 variableRateSlope2);
    }
    interface IPoolAddressesProvider {
        event PoolUpdated(address indexed oldAddress, address indexed newAddress);
        event PoolConfiguratorUpdated(address indexed oldAddress, address indexed newAddress);
        event PriceOracleUpdated(address indexed oldAddress, address indexed newAddress);
        event ACLManagerUpdated(address indexed oldAddress, address indexed newAddress);
        event ACLAdminUpdated(address indexed oldAddress, address indexed newAddress);
        event PriceOracleSentinelUpdated(address indexed oldAddress, address indexed newAddress);
        event ProxyCreated(bytes32 indexed id, address indexed proxyAddress, address indexed implementationAddress);
        event AddressSet(bytes32 indexed id, address indexed oldAddress, address indexed newAddress);
        event AddressSetAsProxy(bytes32 indexed id, address indexed proxyAddress, address oldImplementationAddress, address indexed newImplementationAddress);
    }
}

/// Jump-table index for a coverage topic0. `Unknown` is not a table entry —
/// known-address / unknown-topic logs stay off this enum (carry-forward 03A).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum EventKind {
    Transfer = 0,
    Upgraded,
    AdminChanged,
    OZInitialized,
    AuthorityUpdated,
    PayloadExecuted,
    Erc4626Deposit,
    Erc4626Withdraw,
    V4Add,
    V4Remove,
    V4Draw,
    V4Restore,
    V4RefreshPremium,
    V4HubReportDeficit,
    V4TransferShares,
    V4AddAsset,
    V4UpdateAsset,
    V4UpdateAssetConfig,
    V4AddSpoke,
    V4UpdateSpokeConfig,
    V4MintFeeShares,
    V4Sweep,
    V4Reclaim,
    V4EliminateDeficit,
    V4UpdateInterestRateData,
    V4SetSpokeImmutables,
    V4UpdateLiquidationConfig,
    V4AddReserve,
    V4UpdateReserveConfig,
    V4UpdateReservePriceSource,
    V4AddDynamicReserveConfig,
    V4UpdateDynamicReserveConfig,
    V4UpdatePositionManager,
    V4Supply,
    V4Withdraw,
    V4Borrow,
    V4Repay,
    V4LiquidationCall,
    V4SpokeReportDeficit,
    V4SetUsingAsCollateral,
    V4UpdateUserRiskPremium,
    V4RefreshAllUserDynamicConfig,
    V4RefreshSingleUserDynamicConfig,
    V4SetUserPositionManager,
    V4RefreshPremiumDebt,
    V4UpdateReserveSource,
    V4SetSpoke,
    V4SetTokenizationSpokeImmutables,
    V4RegisterSpoke,
    V4SupplyOnBehalfOf,
    V4RepayOnBehalfOf,
    V4WithdrawApproval,
    V4BorrowApproval,
    V4WithdrawOnBehalfOf,
    V4BorrowOnBehalfOf,
    V4UpdateConfigPermissions,
    V4SetUsingAsCollateralOnBehalfOf,
    V4UpdateUserRiskPremiumOnBehalfOf,
    V4UpdateUserDynamicConfigOnBehalfOf,
    V3Supply,
    V3Withdraw,
    V3Borrow,
    V3Repay,
    V3UserEModeSet,
    V3CollateralEnabled,
    V3CollateralDisabled,
    V3FlashLoan,
    V3LiquidationCall,
    V3ReserveDataUpdated,
    V3DeficitCovered,
    V3MintedToTreasury,
    V3DeficitCreated,
    V3PositionManagerApproved,
    V3PositionManagerRevoked,
    V3Mint,
    V3Burn,
    V3BalanceTransfer,
    V3ReserveInitialized,
    V3ReserveBorrowing,
    V3ReserveFlashLoaning,
    V3PendingLtvChanged,
    V3CollateralConfigurationChanged,
    V3ReserveActive,
    V3ReserveFrozen,
    V3ReservePaused,
    V3ReserveFactorChanged,
    V3BorrowCapChanged,
    V3SupplyCapChanged,
    V3LiquidationProtocolFeeChanged,
    V3LiquidationGracePeriodChanged,
    V3LiquidationGracePeriodDisabled,
    V3AssetCollateralInEModeChanged,
    V3AssetBorrowableInEModeChanged,
    V3AssetLtvzeroInEModeChanged,
    V3EModeCategoryAdded,
    V3EModeCategoryIsolationChanged,
    V3ReserveInterestRateDataChanged,
    V3ATokenUpgraded,
    V3VariableDebtTokenUpgraded,
    V3FlashloanPremiumTotalUpdated,
    V3BaseCurrencySet,
    V3AssetSourceUpdated,
    V3FallbackOracleUpdated,
    V3RateDataUpdate,
    V3PoolUpdated,
    V3PoolConfiguratorUpdated,
    V3PriceOracleUpdated,
    V3AclManagerUpdated,
    V3AclAdminUpdated,
    V3PriceOracleSentinelUpdated,
    V3ProxyCreated,
    V3AddressSet,
    V3AddressSetAsProxy,
    V3ATokenInitialized,
    V3VTokenInitialized,
}

/// Per-block bump arena. Reset at the top of each block; decoded logs borrow
/// from it and cannot outlive the block (GUIDE 03 §4b).
pub struct DecodeArena {
    bump: bumpalo::Bump,
}

impl DecodeArena {
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            bump: bumpalo::Bump::with_capacity(n),
        }
    }

    /// Pointer-bump free of this block's copies. Does not return memory to
    /// the OS; capacity is reused.
    pub fn reset(&mut self) {
        self.bump.reset();
    }

    /// Bytes currently reserved (for the zero-alloc high-water test).
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.bump.allocated_bytes()
    }

    /// Copy `log` into the arena. Infallible: the bump grows; it does not
    /// fail. The only heap traffic is a bump; after warmup this does not
    /// touch the global allocator.
    #[must_use]
    pub fn copy<'a>(&'a self, log: &OwnedLog) -> DecodedLog<'a> {
        let topics = self.bump.alloc_slice_copy(log.topics.as_slice());
        let data = self.bump.alloc_slice_copy(log.data.as_slice());
        DecodedLog {
            address: log.address,
            topics,
            data,
            block: log.block,
            timestamp: log.timestamp,
        }
    }
}

/// Fill `out` with the coverage topic0 → [`EventKind`] jump table (startup).
pub(crate) fn fill_topic0(out: &mut std::collections::HashMap<B256, EventKind>) {
    out.insert(IERC20::Transfer::SIGNATURE_HASH, EventKind::Transfer);
    out.insert(IERC1967::Upgraded::SIGNATURE_HASH, EventKind::Upgraded);
    out.insert(
        IERC1967::AdminChanged::SIGNATURE_HASH,
        EventKind::AdminChanged,
    );
    out.insert(
        IInitializable::Initialized::SIGNATURE_HASH,
        EventKind::OZInitialized,
    );
    out.insert(
        IAccessManaged::AuthorityUpdated::SIGNATURE_HASH,
        EventKind::AuthorityUpdated,
    );
    out.insert(
        IPayloadsControllerCore::PayloadExecuted::SIGNATURE_HASH,
        EventKind::PayloadExecuted,
    );
    out.insert(IERC4626::Deposit::SIGNATURE_HASH, EventKind::Erc4626Deposit);
    out.insert(
        IERC4626::Withdraw::SIGNATURE_HASH,
        EventKind::Erc4626Withdraw,
    );
    out.insert(IHubBase::Add::SIGNATURE_HASH, EventKind::V4Add);
    out.insert(IHubBase::Remove::SIGNATURE_HASH, EventKind::V4Remove);
    out.insert(IHubBase::Draw::SIGNATURE_HASH, EventKind::V4Draw);
    out.insert(IHubBase::Restore::SIGNATURE_HASH, EventKind::V4Restore);
    out.insert(
        IHubBase::RefreshPremium::SIGNATURE_HASH,
        EventKind::V4RefreshPremium,
    );
    out.insert(
        IHubBase::ReportDeficit::SIGNATURE_HASH,
        EventKind::V4HubReportDeficit,
    );
    out.insert(
        IHubBase::TransferShares::SIGNATURE_HASH,
        EventKind::V4TransferShares,
    );
    out.insert(IHub::AddAsset::SIGNATURE_HASH, EventKind::V4AddAsset);
    out.insert(IHub::UpdateAsset::SIGNATURE_HASH, EventKind::V4UpdateAsset);
    out.insert(
        IHub::UpdateAssetConfig::SIGNATURE_HASH,
        EventKind::V4UpdateAssetConfig,
    );
    out.insert(IHub::AddSpoke::SIGNATURE_HASH, EventKind::V4AddSpoke);
    out.insert(
        IHub::UpdateSpokeConfig::SIGNATURE_HASH,
        EventKind::V4UpdateSpokeConfig,
    );
    out.insert(
        IHub::MintFeeShares::SIGNATURE_HASH,
        EventKind::V4MintFeeShares,
    );
    out.insert(IHub::Sweep::SIGNATURE_HASH, EventKind::V4Sweep);
    out.insert(IHub::Reclaim::SIGNATURE_HASH, EventKind::V4Reclaim);
    out.insert(
        IHub::EliminateDeficit::SIGNATURE_HASH,
        EventKind::V4EliminateDeficit,
    );
    out.insert(
        IAssetInterestRateStrategy::UpdateInterestRateData::SIGNATURE_HASH,
        EventKind::V4UpdateInterestRateData,
    );
    out.insert(
        ISpoke::SetSpokeImmutables::SIGNATURE_HASH,
        EventKind::V4SetSpokeImmutables,
    );
    out.insert(
        ISpoke::UpdateLiquidationConfig::SIGNATURE_HASH,
        EventKind::V4UpdateLiquidationConfig,
    );
    out.insert(ISpoke::AddReserve::SIGNATURE_HASH, EventKind::V4AddReserve);
    out.insert(
        ISpoke::UpdateReserveConfig::SIGNATURE_HASH,
        EventKind::V4UpdateReserveConfig,
    );
    out.insert(
        ISpoke::UpdateReservePriceSource::SIGNATURE_HASH,
        EventKind::V4UpdateReservePriceSource,
    );
    out.insert(
        ISpoke::AddDynamicReserveConfig::SIGNATURE_HASH,
        EventKind::V4AddDynamicReserveConfig,
    );
    out.insert(
        ISpoke::UpdateDynamicReserveConfig::SIGNATURE_HASH,
        EventKind::V4UpdateDynamicReserveConfig,
    );
    out.insert(
        ISpoke::UpdatePositionManager::SIGNATURE_HASH,
        EventKind::V4UpdatePositionManager,
    );
    out.insert(ISpoke::Supply::SIGNATURE_HASH, EventKind::V4Supply);
    out.insert(ISpoke::Withdraw::SIGNATURE_HASH, EventKind::V4Withdraw);
    out.insert(ISpoke::Borrow::SIGNATURE_HASH, EventKind::V4Borrow);
    out.insert(ISpoke::Repay::SIGNATURE_HASH, EventKind::V4Repay);
    out.insert(
        ISpoke::LiquidationCall::SIGNATURE_HASH,
        EventKind::V4LiquidationCall,
    );
    out.insert(
        ISpoke::ReportDeficit::SIGNATURE_HASH,
        EventKind::V4SpokeReportDeficit,
    );
    out.insert(
        ISpoke::SetUsingAsCollateral::SIGNATURE_HASH,
        EventKind::V4SetUsingAsCollateral,
    );
    out.insert(
        ISpoke::UpdateUserRiskPremium::SIGNATURE_HASH,
        EventKind::V4UpdateUserRiskPremium,
    );
    out.insert(
        ISpoke::RefreshAllUserDynamicConfig::SIGNATURE_HASH,
        EventKind::V4RefreshAllUserDynamicConfig,
    );
    out.insert(
        ISpoke::RefreshSingleUserDynamicConfig::SIGNATURE_HASH,
        EventKind::V4RefreshSingleUserDynamicConfig,
    );
    out.insert(
        ISpoke::SetUserPositionManager::SIGNATURE_HASH,
        EventKind::V4SetUserPositionManager,
    );
    out.insert(
        ISpoke::RefreshPremiumDebt::SIGNATURE_HASH,
        EventKind::V4RefreshPremiumDebt,
    );
    out.insert(
        IAaveOracleV4::UpdateReserveSource::SIGNATURE_HASH,
        EventKind::V4UpdateReserveSource,
    );
    out.insert(
        IAaveOracleV4::SetSpoke::SIGNATURE_HASH,
        EventKind::V4SetSpoke,
    );
    out.insert(
        ITokenizationSpoke::SetTokenizationSpokeImmutables::SIGNATURE_HASH,
        EventKind::V4SetTokenizationSpokeImmutables,
    );
    out.insert(
        IPositionManagerBase::RegisterSpoke::SIGNATURE_HASH,
        EventKind::V4RegisterSpoke,
    );
    out.insert(
        IGiverPositionManager::SupplyOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4SupplyOnBehalfOf,
    );
    out.insert(
        IGiverPositionManager::RepayOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4RepayOnBehalfOf,
    );
    out.insert(
        ITakerPositionManager::WithdrawApproval::SIGNATURE_HASH,
        EventKind::V4WithdrawApproval,
    );
    out.insert(
        ITakerPositionManager::BorrowApproval::SIGNATURE_HASH,
        EventKind::V4BorrowApproval,
    );
    out.insert(
        ITakerPositionManager::WithdrawOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4WithdrawOnBehalfOf,
    );
    out.insert(
        ITakerPositionManager::BorrowOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4BorrowOnBehalfOf,
    );
    out.insert(
        IConfigPositionManager::UpdateConfigPermissions::SIGNATURE_HASH,
        EventKind::V4UpdateConfigPermissions,
    );
    out.insert(
        IConfigPositionManager::SetUsingAsCollateralOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4SetUsingAsCollateralOnBehalfOf,
    );
    out.insert(
        IConfigPositionManager::UpdateUserRiskPremiumOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4UpdateUserRiskPremiumOnBehalfOf,
    );
    out.insert(
        IConfigPositionManager::UpdateUserDynamicConfigOnBehalfOf::SIGNATURE_HASH,
        EventKind::V4UpdateUserDynamicConfigOnBehalfOf,
    );
    out.insert(IPool::Supply::SIGNATURE_HASH, EventKind::V3Supply);
    out.insert(IPool::Withdraw::SIGNATURE_HASH, EventKind::V3Withdraw);
    out.insert(IPool::Borrow::SIGNATURE_HASH, EventKind::V3Borrow);
    out.insert(IPool::Repay::SIGNATURE_HASH, EventKind::V3Repay);
    out.insert(
        IPool::UserEModeSet::SIGNATURE_HASH,
        EventKind::V3UserEModeSet,
    );
    out.insert(
        IPool::ReserveUsedAsCollateralEnabled::SIGNATURE_HASH,
        EventKind::V3CollateralEnabled,
    );
    out.insert(
        IPool::ReserveUsedAsCollateralDisabled::SIGNATURE_HASH,
        EventKind::V3CollateralDisabled,
    );
    out.insert(IPool::FlashLoan::SIGNATURE_HASH, EventKind::V3FlashLoan);
    out.insert(
        IPool::LiquidationCall::SIGNATURE_HASH,
        EventKind::V3LiquidationCall,
    );
    out.insert(
        IPool::ReserveDataUpdated::SIGNATURE_HASH,
        EventKind::V3ReserveDataUpdated,
    );
    out.insert(
        IPool::DeficitCovered::SIGNATURE_HASH,
        EventKind::V3DeficitCovered,
    );
    out.insert(
        IPool::MintedToTreasury::SIGNATURE_HASH,
        EventKind::V3MintedToTreasury,
    );
    out.insert(
        IPool::DeficitCreated::SIGNATURE_HASH,
        EventKind::V3DeficitCreated,
    );
    out.insert(
        IPool::PositionManagerApproved::SIGNATURE_HASH,
        EventKind::V3PositionManagerApproved,
    );
    out.insert(
        IPool::PositionManagerRevoked::SIGNATURE_HASH,
        EventKind::V3PositionManagerRevoked,
    );
    out.insert(IScaledBalanceToken::Mint::SIGNATURE_HASH, EventKind::V3Mint);
    out.insert(IScaledBalanceToken::Burn::SIGNATURE_HASH, EventKind::V3Burn);
    out.insert(
        IAToken::BalanceTransfer::SIGNATURE_HASH,
        EventKind::V3BalanceTransfer,
    );
    out.insert(
        IPoolConfigurator::ReserveInitialized::SIGNATURE_HASH,
        EventKind::V3ReserveInitialized,
    );
    out.insert(
        IPoolConfigurator::ReserveBorrowing::SIGNATURE_HASH,
        EventKind::V3ReserveBorrowing,
    );
    out.insert(
        IPoolConfigurator::ReserveFlashLoaning::SIGNATURE_HASH,
        EventKind::V3ReserveFlashLoaning,
    );
    out.insert(
        IPoolConfigurator::PendingLtvChanged::SIGNATURE_HASH,
        EventKind::V3PendingLtvChanged,
    );
    out.insert(
        IPoolConfigurator::CollateralConfigurationChanged::SIGNATURE_HASH,
        EventKind::V3CollateralConfigurationChanged,
    );
    out.insert(
        IPoolConfigurator::ReserveActive::SIGNATURE_HASH,
        EventKind::V3ReserveActive,
    );
    out.insert(
        IPoolConfigurator::ReserveFrozen::SIGNATURE_HASH,
        EventKind::V3ReserveFrozen,
    );
    out.insert(
        IPoolConfigurator::ReservePaused::SIGNATURE_HASH,
        EventKind::V3ReservePaused,
    );
    out.insert(
        IPoolConfigurator::ReserveFactorChanged::SIGNATURE_HASH,
        EventKind::V3ReserveFactorChanged,
    );
    out.insert(
        IPoolConfigurator::BorrowCapChanged::SIGNATURE_HASH,
        EventKind::V3BorrowCapChanged,
    );
    out.insert(
        IPoolConfigurator::SupplyCapChanged::SIGNATURE_HASH,
        EventKind::V3SupplyCapChanged,
    );
    out.insert(
        IPoolConfigurator::LiquidationProtocolFeeChanged::SIGNATURE_HASH,
        EventKind::V3LiquidationProtocolFeeChanged,
    );
    out.insert(
        IPoolConfigurator::LiquidationGracePeriodChanged::SIGNATURE_HASH,
        EventKind::V3LiquidationGracePeriodChanged,
    );
    out.insert(
        IPoolConfigurator::LiquidationGracePeriodDisabled::SIGNATURE_HASH,
        EventKind::V3LiquidationGracePeriodDisabled,
    );
    out.insert(
        IPoolConfigurator::AssetCollateralInEModeChanged::SIGNATURE_HASH,
        EventKind::V3AssetCollateralInEModeChanged,
    );
    out.insert(
        IPoolConfigurator::AssetBorrowableInEModeChanged::SIGNATURE_HASH,
        EventKind::V3AssetBorrowableInEModeChanged,
    );
    out.insert(
        IPoolConfigurator::AssetLtvzeroInEModeChanged::SIGNATURE_HASH,
        EventKind::V3AssetLtvzeroInEModeChanged,
    );
    out.insert(
        IPoolConfigurator::EModeCategoryAdded::SIGNATURE_HASH,
        EventKind::V3EModeCategoryAdded,
    );
    out.insert(
        IPoolConfigurator::EModeCategoryIsolationChanged::SIGNATURE_HASH,
        EventKind::V3EModeCategoryIsolationChanged,
    );
    out.insert(
        IPoolConfigurator::ReserveInterestRateDataChanged::SIGNATURE_HASH,
        EventKind::V3ReserveInterestRateDataChanged,
    );
    out.insert(
        IPoolConfigurator::ATokenUpgraded::SIGNATURE_HASH,
        EventKind::V3ATokenUpgraded,
    );
    out.insert(
        IPoolConfigurator::VariableDebtTokenUpgraded::SIGNATURE_HASH,
        EventKind::V3VariableDebtTokenUpgraded,
    );
    out.insert(
        IPoolConfigurator::FlashloanPremiumTotalUpdated::SIGNATURE_HASH,
        EventKind::V3FlashloanPremiumTotalUpdated,
    );
    out.insert(
        IAaveOracleV3::BaseCurrencySet::SIGNATURE_HASH,
        EventKind::V3BaseCurrencySet,
    );
    out.insert(
        IAaveOracleV3::AssetSourceUpdated::SIGNATURE_HASH,
        EventKind::V3AssetSourceUpdated,
    );
    out.insert(
        IAaveOracleV3::FallbackOracleUpdated::SIGNATURE_HASH,
        EventKind::V3FallbackOracleUpdated,
    );
    out.insert(
        IDefaultReserveInterestRateStrategyV2::RateDataUpdate::SIGNATURE_HASH,
        EventKind::V3RateDataUpdate,
    );
    out.insert(
        IPoolAddressesProvider::PoolUpdated::SIGNATURE_HASH,
        EventKind::V3PoolUpdated,
    );
    out.insert(
        IPoolAddressesProvider::PoolConfiguratorUpdated::SIGNATURE_HASH,
        EventKind::V3PoolConfiguratorUpdated,
    );
    out.insert(
        IPoolAddressesProvider::PriceOracleUpdated::SIGNATURE_HASH,
        EventKind::V3PriceOracleUpdated,
    );
    out.insert(
        IPoolAddressesProvider::ACLManagerUpdated::SIGNATURE_HASH,
        EventKind::V3AclManagerUpdated,
    );
    out.insert(
        IPoolAddressesProvider::ACLAdminUpdated::SIGNATURE_HASH,
        EventKind::V3AclAdminUpdated,
    );
    out.insert(
        IPoolAddressesProvider::PriceOracleSentinelUpdated::SIGNATURE_HASH,
        EventKind::V3PriceOracleSentinelUpdated,
    );
    out.insert(
        IPoolAddressesProvider::ProxyCreated::SIGNATURE_HASH,
        EventKind::V3ProxyCreated,
    );
    out.insert(
        IPoolAddressesProvider::AddressSet::SIGNATURE_HASH,
        EventKind::V3AddressSet,
    );
    out.insert(
        IPoolAddressesProvider::AddressSetAsProxy::SIGNATURE_HASH,
        EventKind::V3AddressSetAsProxy,
    );
    out.insert(
        IAToken::Initialized::SIGNATURE_HASH,
        EventKind::V3ATokenInitialized,
    );
    out.insert(
        IVariableDebtToken::Initialized::SIGNATURE_HASH,
        EventKind::V3VTokenInitialized,
    );
}

fn chk<E: SolEvent>(topics: &[B256], data: &[u8]) -> Result<()> {
    E::decode_raw_log(topics.iter().copied(), data)
        .map(|_| ())
        .map_err(|_| IngestError::MalformedLog)
}

/// ABI-decode a known event into its sol! struct (stack for Copy events).
/// Dynamic-field events (string/bytes) are classified only — full decode
/// would heap-allocate and is the adapter's job from [`DecodedLog`].
pub fn decode_typed(kind: EventKind, topics: &[B256], data: &[u8]) -> Result<()> {
    match kind {
        EventKind::Transfer => chk::<IERC20::Transfer>(topics, data),
        EventKind::Upgraded => chk::<IERC1967::Upgraded>(topics, data),
        EventKind::AdminChanged => chk::<IERC1967::AdminChanged>(topics, data),
        EventKind::OZInitialized => chk::<IInitializable::Initialized>(topics, data),
        EventKind::AuthorityUpdated => chk::<IAccessManaged::AuthorityUpdated>(topics, data),
        EventKind::PayloadExecuted => chk::<IPayloadsControllerCore::PayloadExecuted>(topics, data),
        EventKind::Erc4626Deposit => chk::<IERC4626::Deposit>(topics, data),
        EventKind::Erc4626Withdraw => chk::<IERC4626::Withdraw>(topics, data),
        EventKind::V4Add => chk::<IHubBase::Add>(topics, data),
        EventKind::V4Remove => chk::<IHubBase::Remove>(topics, data),
        EventKind::V4Draw => chk::<IHubBase::Draw>(topics, data),
        EventKind::V4Restore => chk::<IHubBase::Restore>(topics, data),
        EventKind::V4RefreshPremium => chk::<IHubBase::RefreshPremium>(topics, data),
        EventKind::V4HubReportDeficit => chk::<IHubBase::ReportDeficit>(topics, data),
        EventKind::V4TransferShares => chk::<IHubBase::TransferShares>(topics, data),
        EventKind::V4AddAsset => chk::<IHub::AddAsset>(topics, data),
        EventKind::V4UpdateAsset => chk::<IHub::UpdateAsset>(topics, data),
        EventKind::V4UpdateAssetConfig => chk::<IHub::UpdateAssetConfig>(topics, data),
        EventKind::V4AddSpoke => chk::<IHub::AddSpoke>(topics, data),
        EventKind::V4UpdateSpokeConfig => chk::<IHub::UpdateSpokeConfig>(topics, data),
        EventKind::V4MintFeeShares => chk::<IHub::MintFeeShares>(topics, data),
        EventKind::V4Sweep => chk::<IHub::Sweep>(topics, data),
        EventKind::V4Reclaim => chk::<IHub::Reclaim>(topics, data),
        EventKind::V4EliminateDeficit => chk::<IHub::EliminateDeficit>(topics, data),
        EventKind::V4UpdateInterestRateData => {
            chk::<IAssetInterestRateStrategy::UpdateInterestRateData>(topics, data)
        }
        EventKind::V4SetSpokeImmutables => chk::<ISpoke::SetSpokeImmutables>(topics, data),
        EventKind::V4UpdateLiquidationConfig => {
            chk::<ISpoke::UpdateLiquidationConfig>(topics, data)
        }
        EventKind::V4AddReserve => chk::<ISpoke::AddReserve>(topics, data),
        EventKind::V4UpdateReserveConfig => chk::<ISpoke::UpdateReserveConfig>(topics, data),
        EventKind::V4UpdateReservePriceSource => {
            chk::<ISpoke::UpdateReservePriceSource>(topics, data)
        }
        EventKind::V4AddDynamicReserveConfig => {
            chk::<ISpoke::AddDynamicReserveConfig>(topics, data)
        }
        EventKind::V4UpdateDynamicReserveConfig => {
            chk::<ISpoke::UpdateDynamicReserveConfig>(topics, data)
        }
        EventKind::V4UpdatePositionManager => chk::<ISpoke::UpdatePositionManager>(topics, data),
        EventKind::V4Supply => chk::<ISpoke::Supply>(topics, data),
        EventKind::V4Withdraw => chk::<ISpoke::Withdraw>(topics, data),
        EventKind::V4Borrow => chk::<ISpoke::Borrow>(topics, data),
        EventKind::V4Repay => chk::<ISpoke::Repay>(topics, data),
        EventKind::V4LiquidationCall => chk::<ISpoke::LiquidationCall>(topics, data),
        EventKind::V4SpokeReportDeficit => chk::<ISpoke::ReportDeficit>(topics, data),
        EventKind::V4SetUsingAsCollateral => chk::<ISpoke::SetUsingAsCollateral>(topics, data),
        EventKind::V4UpdateUserRiskPremium => chk::<ISpoke::UpdateUserRiskPremium>(topics, data),
        EventKind::V4RefreshAllUserDynamicConfig => {
            chk::<ISpoke::RefreshAllUserDynamicConfig>(topics, data)
        }
        EventKind::V4RefreshSingleUserDynamicConfig => {
            chk::<ISpoke::RefreshSingleUserDynamicConfig>(topics, data)
        }
        EventKind::V4SetUserPositionManager => chk::<ISpoke::SetUserPositionManager>(topics, data),
        EventKind::V4RefreshPremiumDebt => chk::<ISpoke::RefreshPremiumDebt>(topics, data),
        EventKind::V4UpdateReserveSource => chk::<IAaveOracleV4::UpdateReserveSource>(topics, data),
        EventKind::V4SetSpoke => chk::<IAaveOracleV4::SetSpoke>(topics, data),
        EventKind::V4SetTokenizationSpokeImmutables => {
            chk::<ITokenizationSpoke::SetTokenizationSpokeImmutables>(topics, data)
        }
        EventKind::V4RegisterSpoke => chk::<IPositionManagerBase::RegisterSpoke>(topics, data),
        EventKind::V4SupplyOnBehalfOf => {
            chk::<IGiverPositionManager::SupplyOnBehalfOf>(topics, data)
        }
        EventKind::V4RepayOnBehalfOf => chk::<IGiverPositionManager::RepayOnBehalfOf>(topics, data),
        EventKind::V4WithdrawApproval => {
            chk::<ITakerPositionManager::WithdrawApproval>(topics, data)
        }
        EventKind::V4BorrowApproval => chk::<ITakerPositionManager::BorrowApproval>(topics, data),
        EventKind::V4WithdrawOnBehalfOf => {
            chk::<ITakerPositionManager::WithdrawOnBehalfOf>(topics, data)
        }
        EventKind::V4BorrowOnBehalfOf => {
            chk::<ITakerPositionManager::BorrowOnBehalfOf>(topics, data)
        }
        EventKind::V4UpdateConfigPermissions => {
            chk::<IConfigPositionManager::UpdateConfigPermissions>(topics, data)
        }
        EventKind::V4SetUsingAsCollateralOnBehalfOf => {
            chk::<IConfigPositionManager::SetUsingAsCollateralOnBehalfOf>(topics, data)
        }
        EventKind::V4UpdateUserRiskPremiumOnBehalfOf => {
            chk::<IConfigPositionManager::UpdateUserRiskPremiumOnBehalfOf>(topics, data)
        }
        EventKind::V4UpdateUserDynamicConfigOnBehalfOf => {
            chk::<IConfigPositionManager::UpdateUserDynamicConfigOnBehalfOf>(topics, data)
        }
        EventKind::V3Supply => chk::<IPool::Supply>(topics, data),
        EventKind::V3Withdraw => chk::<IPool::Withdraw>(topics, data),
        EventKind::V3Borrow => chk::<IPool::Borrow>(topics, data),
        EventKind::V3Repay => chk::<IPool::Repay>(topics, data),
        EventKind::V3UserEModeSet => chk::<IPool::UserEModeSet>(topics, data),
        EventKind::V3CollateralEnabled => {
            chk::<IPool::ReserveUsedAsCollateralEnabled>(topics, data)
        }
        EventKind::V3CollateralDisabled => {
            chk::<IPool::ReserveUsedAsCollateralDisabled>(topics, data)
        }
        EventKind::V3FlashLoan => chk::<IPool::FlashLoan>(topics, data),
        EventKind::V3LiquidationCall => chk::<IPool::LiquidationCall>(topics, data),
        EventKind::V3ReserveDataUpdated => chk::<IPool::ReserveDataUpdated>(topics, data),
        EventKind::V3DeficitCovered => chk::<IPool::DeficitCovered>(topics, data),
        EventKind::V3MintedToTreasury => chk::<IPool::MintedToTreasury>(topics, data),
        EventKind::V3DeficitCreated => chk::<IPool::DeficitCreated>(topics, data),
        EventKind::V3PositionManagerApproved => chk::<IPool::PositionManagerApproved>(topics, data),
        EventKind::V3PositionManagerRevoked => chk::<IPool::PositionManagerRevoked>(topics, data),
        EventKind::V3Mint => chk::<IScaledBalanceToken::Mint>(topics, data),
        EventKind::V3Burn => chk::<IScaledBalanceToken::Burn>(topics, data),
        EventKind::V3BalanceTransfer => chk::<IAToken::BalanceTransfer>(topics, data),
        EventKind::V3ReserveInitialized => {
            chk::<IPoolConfigurator::ReserveInitialized>(topics, data)
        }
        EventKind::V3ReserveBorrowing => chk::<IPoolConfigurator::ReserveBorrowing>(topics, data),
        EventKind::V3ReserveFlashLoaning => {
            chk::<IPoolConfigurator::ReserveFlashLoaning>(topics, data)
        }
        EventKind::V3PendingLtvChanged => chk::<IPoolConfigurator::PendingLtvChanged>(topics, data),
        EventKind::V3CollateralConfigurationChanged => {
            chk::<IPoolConfigurator::CollateralConfigurationChanged>(topics, data)
        }
        EventKind::V3ReserveActive => chk::<IPoolConfigurator::ReserveActive>(topics, data),
        EventKind::V3ReserveFrozen => chk::<IPoolConfigurator::ReserveFrozen>(topics, data),
        EventKind::V3ReservePaused => chk::<IPoolConfigurator::ReservePaused>(topics, data),
        EventKind::V3ReserveFactorChanged => {
            chk::<IPoolConfigurator::ReserveFactorChanged>(topics, data)
        }
        EventKind::V3BorrowCapChanged => chk::<IPoolConfigurator::BorrowCapChanged>(topics, data),
        EventKind::V3SupplyCapChanged => chk::<IPoolConfigurator::SupplyCapChanged>(topics, data),
        EventKind::V3LiquidationProtocolFeeChanged => {
            chk::<IPoolConfigurator::LiquidationProtocolFeeChanged>(topics, data)
        }
        EventKind::V3LiquidationGracePeriodChanged => {
            chk::<IPoolConfigurator::LiquidationGracePeriodChanged>(topics, data)
        }
        EventKind::V3LiquidationGracePeriodDisabled => {
            chk::<IPoolConfigurator::LiquidationGracePeriodDisabled>(topics, data)
        }
        EventKind::V3AssetCollateralInEModeChanged => {
            chk::<IPoolConfigurator::AssetCollateralInEModeChanged>(topics, data)
        }
        EventKind::V3AssetBorrowableInEModeChanged => {
            chk::<IPoolConfigurator::AssetBorrowableInEModeChanged>(topics, data)
        }
        EventKind::V3AssetLtvzeroInEModeChanged => {
            chk::<IPoolConfigurator::AssetLtvzeroInEModeChanged>(topics, data)
        }
        EventKind::V3EModeCategoryIsolationChanged => {
            chk::<IPoolConfigurator::EModeCategoryIsolationChanged>(topics, data)
        }
        EventKind::V3ATokenUpgraded => chk::<IPoolConfigurator::ATokenUpgraded>(topics, data),
        EventKind::V3VariableDebtTokenUpgraded => {
            chk::<IPoolConfigurator::VariableDebtTokenUpgraded>(topics, data)
        }
        EventKind::V3FlashloanPremiumTotalUpdated => {
            chk::<IPoolConfigurator::FlashloanPremiumTotalUpdated>(topics, data)
        }
        EventKind::V3BaseCurrencySet => chk::<IAaveOracleV3::BaseCurrencySet>(topics, data),
        EventKind::V3AssetSourceUpdated => chk::<IAaveOracleV3::AssetSourceUpdated>(topics, data),
        EventKind::V3FallbackOracleUpdated => {
            chk::<IAaveOracleV3::FallbackOracleUpdated>(topics, data)
        }
        EventKind::V3RateDataUpdate => {
            chk::<IDefaultReserveInterestRateStrategyV2::RateDataUpdate>(topics, data)
        }
        EventKind::V3PoolUpdated => chk::<IPoolAddressesProvider::PoolUpdated>(topics, data),
        EventKind::V3PoolConfiguratorUpdated => {
            chk::<IPoolAddressesProvider::PoolConfiguratorUpdated>(topics, data)
        }
        EventKind::V3PriceOracleUpdated => {
            chk::<IPoolAddressesProvider::PriceOracleUpdated>(topics, data)
        }
        EventKind::V3AclManagerUpdated => {
            chk::<IPoolAddressesProvider::ACLManagerUpdated>(topics, data)
        }
        EventKind::V3AclAdminUpdated => {
            chk::<IPoolAddressesProvider::ACLAdminUpdated>(topics, data)
        }
        EventKind::V3PriceOracleSentinelUpdated => {
            chk::<IPoolAddressesProvider::PriceOracleSentinelUpdated>(topics, data)
        }
        EventKind::V3ProxyCreated => chk::<IPoolAddressesProvider::ProxyCreated>(topics, data),
        EventKind::V3AddressSet => chk::<IPoolAddressesProvider::AddressSet>(topics, data),
        EventKind::V3AddressSetAsProxy => {
            chk::<IPoolAddressesProvider::AddressSetAsProxy>(topics, data)
        }
        // string/bytes: skip ABI decode on the hot path (would allocate).
        EventKind::V3EModeCategoryAdded
        | EventKind::V3ReserveInterestRateDataChanged
        | EventKind::V3ATokenInitialized
        | EventKind::V3VTokenInitialized => Ok(()),
    }
}

/// Encode a V4 Spoke Supply into an [`OwnedLog`] (tests / fixtures).
#[cfg(test)]
pub(crate) fn encode_v4_supply(
    address: alloy_primitives::Address,
    reserve_id: alloy_primitives::U256,
    caller: alloy_primitives::Address,
    user: alloy_primitives::Address,
    shares: alloy_primitives::U256,
    amount: alloy_primitives::U256,
    block: crate::BlockNum,
    timestamp: crate::Timestamp,
) -> Result<OwnedLog> {
    let ev = ISpoke::Supply {
        reserveId: reserve_id,
        caller,
        user,
        suppliedShares: shares,
        suppliedAmount: amount,
    };
    let topics = ev.encode_topics();
    let mut av = arrayvec::ArrayVec::new();
    for t in topics {
        av.try_push(t.into())
            .map_err(|_| IngestError::MalformedLog)?;
    }
    Ok(OwnedLog {
        address,
        topics: av,
        data: ev.encode_data(),
        block,
        timestamp,
        tx_index: 0,
        log_index: 0,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{fill_topic0, IERC20};
    use alloy_primitives::{keccak256, B256};
    use alloy_sol_types::SolEvent;
    use std::collections::{HashMap, HashSet};

    fn parse_b256(hex: &str) -> Option<B256> {
        if hex.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            let at = i.saturating_mul(2);
            let byte = u8::from_str_radix(hex.get(at..at.saturating_add(2))?, 16).ok()?;
            *slot = byte;
        }
        Some(B256::from(out))
    }

    fn coverage_topic0s() -> HashSet<B256> {
        let mut set = HashSet::new();
        for path in [
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/coverage/aave-v4.md"
            ),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/coverage/aave-v3.md"
            ),
        ] {
            let text = std::fs::read_to_string(path).unwrap();
            for line in text.lines() {
                if !line.starts_with('|') {
                    continue;
                }
                for word in line.split('|') {
                    let w = word.trim();
                    if let Some(hex) = w.strip_prefix("0x") {
                        if let Some(b) = parse_b256(hex) {
                            set.insert(b);
                        }
                    }
                }
            }
        }
        set
    }

    /// Number of unique topic0s across both 03C coverage files: 59 V4 rows +
    /// 61 V3 rows, less the 5 hashes shared between them (`Transfer`,
    /// `Upgraded`, ... ). Pinned so a parser that silently matched nothing
    /// could not make this test vacuous, and so a coverage row added by a
    /// later 03C pass fails here until it has a decoder.
    const COVERAGE_TOPIC0S: usize = 115;

    /// Oracle: committed 03C coverage files. Every unique topic0 must be in
    /// the sol! jump table, and the table must hold nothing else — the two
    /// sets are equal. Negative: BorrowAllowanceDelegated is absent.
    #[test]
    fn sol_hashes_cover_every_coverage_topic0() {
        let docs = coverage_topic0s();
        assert_eq!(
            docs.len(),
            COVERAGE_TOPIC0S,
            "coverage docs parsed to {} topic0s, expected {COVERAGE_TOPIC0S}",
            docs.len()
        );
        let mut table = HashMap::new();
        fill_topic0(&mut table);
        let missing: Vec<_> = docs
            .iter()
            .filter(|t| !table.contains_key(*t))
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "coverage topic0s missing from sol! table ({}/{}): {missing:x?}",
            missing.len(),
            docs.len()
        );
        let extra: Vec<_> = table
            .keys()
            .filter(|t| !docs.contains(*t))
            .copied()
            .collect();
        assert!(
            extra.is_empty(),
            "sol! table decodes topic0s no coverage row names: {extra:x?}"
        );
        assert_eq!(table.len(), COVERAGE_TOPIC0S);
        let ba = B256::from(keccak256(
            b"BorrowAllowanceDelegated(address,address,address,uint256)",
        ));
        assert!(
            !table.contains_key(&ba),
            "BorrowAllowanceDelegated must not be a coverage decoder row"
        );
        assert!(!docs.contains(&ba));
        assert_eq!(IERC20::Transfer::SIGNATURE_HASH.0[0], 0xdd);
    }
}
