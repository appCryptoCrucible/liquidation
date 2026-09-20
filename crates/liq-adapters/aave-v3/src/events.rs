//! Event ABI from `aave-dao/aave-v3-origin` @ `8305565ae`. topic0 is asserted
//! against `docs/coverage/aave-v3.md` in the conformance tests.

use alloy_sol_types::sol;

pub mod pool {
    use super::sol;
    sol! {
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
}

pub mod cfg {
    use super::sol;
    sol! {
        event ReserveInitialized(address indexed asset, address indexed aToken, address stableDebtToken, address variableDebtToken, address interestRateStrategyAddress);
        event ReserveBorrowing(address indexed asset, bool enabled);
        event ReserveFlashLoaning(address indexed asset, bool enabled);
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
        event PendingLtvChanged(address indexed asset, uint256 ltv);
        event ATokenUpgraded(address indexed asset, address indexed proxy, address indexed implementation);
        event VariableDebtTokenUpgraded(address indexed asset, address indexed proxy, address indexed implementation);
        event FlashloanPremiumTotalUpdated(uint128 oldFlashloanPremiumTotal, uint128 newFlashloanPremiumTotal);
    }
}

pub mod oracle {
    use super::sol;
    sol! {
        event AssetSourceUpdated(address indexed asset, address indexed source);
        event FallbackOracleUpdated(address indexed fallbackOracle);
        event BaseCurrencySet(address indexed baseCurrency, uint256 baseCurrencyUnit);
    }
}

pub mod provider {
    use super::sol;
    sol! {
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

pub mod token {
    use super::sol;
    sol! {
        event Transfer(address indexed from, address indexed to, uint256 value);
        event Mint(address indexed caller, address indexed onBehalfOf, uint256 value, uint256 balanceIncrease, uint256 index);
        event Burn(address indexed from, address indexed target, uint256 value, uint256 balanceIncrease, uint256 index);
        event BalanceTransfer(address indexed from, address indexed to, uint256 value, uint256 index);
        event BorrowAllowanceDelegated(address indexed fromUser, address indexed toUser, address indexed asset, uint256 amount);
        event DelegateChanged(address indexed delegator, address indexed delegatee, uint8 delegationType);
    }
}

pub mod sentinel {
    use super::sol;
    sol! {
        event SequencerOracleUpdated(address indexed newSequencerOracle);
        event GracePeriodUpdated(uint256 newGracePeriod);
        event AnswerUpdated(int256 indexed current, uint256 indexed roundId, uint256 updatedAt);
    }
}

pub mod halt {
    use super::sol;
    sol! {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
        event Initialized(uint64 version);
        event AuthorityUpdated(address authority);
    }
}

pub mod irs {
    use super::sol;
    sol! {
        event RateDataUpdate(address indexed reserve, uint256 baseVariableBorrowRate, uint256 variableRateSlope1, uint256 variableRateSlope2, uint256 optimalUsageRatio);
    }
}
