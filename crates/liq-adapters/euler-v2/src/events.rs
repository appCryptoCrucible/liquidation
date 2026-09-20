//! Event ABI from `euler-xyz/euler-vault-kit` @ `bfb325a6e6ca09613d940b46f72ccfe017353933`
//! (`Events.sol`, `Governance.sol`, `GenericFactory.sol`) and EVC
//! `CollateralStatus` / `ControllerStatus` / `AccountStatusCheck`.

use alloy_sol_types::sol;

sol! {
    // GenericFactory
    event Genesis();
    event ProxyCreated(address indexed proxy, bool upgradeable, address implementation, bytes trailingData);
    event SetImplementation(address indexed newImplementation);
    event SetUpgradeAdmin(address indexed newUpgradeAdmin);

    // EVault ERC20 / ERC4626 / EVault
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event Deposit(address indexed sender, address indexed owner, uint256 assets, uint256 shares);
    event Withdraw(
        address indexed sender,
        address indexed receiver,
        address indexed owner,
        uint256 assets,
        uint256 shares
    );
    event EVaultCreated(address indexed creator, address indexed asset, address dToken);
    event VaultStatus(
        uint256 totalShares,
        uint256 totalBorrows,
        uint256 accumulatedFees,
        uint256 cash,
        uint256 interestAccumulator,
        uint256 interestRate,
        uint256 timestamp
    );
    event Borrow(address indexed account, uint256 assets);
    event Repay(address indexed account, uint256 assets);
    event InterestAccrued(address indexed account, uint256 assets);
    event Liquidate(
        address indexed liquidator,
        address indexed violator,
        address collateral,
        uint256 repayAssets,
        uint256 yieldBalance
    );
    event PullDebt(address indexed from, address indexed to, uint256 assets);
    event DebtSocialized(address indexed account, uint256 assets);
    event ConvertFees(
        address indexed sender,
        address indexed protocolReceiver,
        address indexed governorReceiver,
        uint256 protocolShares,
        uint256 governorShares
    );
    event BalanceForwarderStatus(address indexed account, bool status);

    // Governance
    event GovSetGovernorAdmin(address indexed newGovernorAdmin);
    event GovSetFeeReceiver(address indexed newFeeReceiver);
    event GovSetLTV(
        address indexed collateral,
        uint16 borrowLTV,
        uint16 liquidationLTV,
        uint16 initialLiquidationLTV,
        uint48 targetTimestamp,
        uint32 rampDuration
    );
    event GovSetInterestRateModel(address newInterestRateModel);
    event GovSetMaxLiquidationDiscount(uint16 newDiscount);
    event GovSetLiquidationCoolOffTime(uint16 newCoolOffTime);
    event GovSetHookConfig(address indexed newHookTarget, uint32 newHookedOps);
    event GovSetConfigFlags(uint32 newConfigFlags);
    event GovSetCaps(uint16 newSupplyCap, uint16 newBorrowCap);
    event GovSetInterestFee(uint16 newFee);
}

pub mod evc {
    use super::sol;
    sol! {
        event CollateralStatus(address indexed account, address indexed collateral, bool enabled);
        event ControllerStatus(address indexed account, address indexed controller, bool enabled);
        event AccountStatusCheck(address indexed account, address indexed controller);
        event OwnerRegistered(bytes19 indexed addressPrefix, address indexed owner);
        event LockdownModeStatus(bytes19 indexed addressPrefix, bool enabled);
        event VaultStatusCheck(address indexed vault);
    }
}

pub mod halt {
    use super::sol;
    sol! {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
        event Initialized(uint64 version);
    }
}

sol! {
    function accountLiquidity(address account, bool liquidation) external view returns (
        uint256 collateralValue,
        uint256 liabilityValue
    );
    function checkLiquidation(address liquidator, address violator, address collateral) external view returns (
        uint256 maxRepay,
        uint256 maxYield
    );
    function liquidate(address violator, address collateral, uint256 repayAssets, uint256 minYieldBalance) external;
}
