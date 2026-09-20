//! Event ABI from `silo-finance/silo-contracts-v2` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7`.
//! Liquidation is emitted by the **hook receiver**, not the Silo ERC-4626.

use alloy_sol_types::sol;

pub mod factory {
    use super::sol;
    sol! {
        event NewSilo(
            address indexed implementation,
            address indexed token0,
            address indexed token1,
            address silo0,
            address silo1,
            address siloConfig
        );
        event NewSiloShareTokens(
            address indexed protectedShareToken,
            address indexed collateralShareToken,
            address indexed debtShareToken
        );
        event NewSiloHook(address indexed silo, address indexed hook);
    }
}

pub mod hook {
    use super::sol;
    // Pin IPartialLiquidation.LiquidationCall — three indexed addresses.
    // liq-watch silo::LiquidationCall(liquidator, borrower, repay, withdraw)
    // is a different topic0 (10R must use this pin ABI).
    sol! {
        event LiquidationCall(
            address indexed liquidator,
            address indexed silo,
            address indexed borrower,
            uint256 repayDebtAssets,
            uint256 withdrawCollateral,
            bool receiveSToken
        );
        event LiquidationStart(uint8 indexed liquidationType);
    }
}

pub mod silo {
    use super::sol;
    sol! {
        event Deposit(address indexed sender, address indexed owner, uint256 assets, uint256 shares);
        event DepositProtected(address indexed sender, address indexed owner, uint256 assets, uint256 shares);
        event Withdraw(address indexed sender, address indexed receiver, address indexed owner, uint256 assets, uint256 shares);
        event WithdrawProtected(address indexed sender, address indexed receiver, address indexed owner, uint256 assets, uint256 shares);
        event Borrow(address indexed sender, address indexed receiver, address indexed owner, uint256 assets, uint256 shares);
        event Repay(address indexed sender, address indexed owner, uint256 assets, uint256 shares);
        event CollateralTypeChanged(address indexed borrower);
        event AccruedInterest(uint256 hooksBefore);
        event FlashLoan(uint256 amount);
        event HooksUpdated(uint24 hooksBefore, uint24 hooksAfter);
        event WithdrawnFees(uint256 daoFees, uint256 deployerFees, bool redirectedDeployerFees);
        event DeployerFeesRedirected(uint256 deployerFees);
        event Transfer(address indexed from, address indexed to, uint256 value);
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

/// 10R wire ABI — call the hook receiver, never the Silo.
pub mod liquidation_abi {
    use super::sol;
    sol! {
        function liquidationCall(
            address _collateralAsset,
            address _debtAsset,
            address _borrower,
            uint256 _maxDebtToCover,
            bool _receiveSToken
        ) external returns (uint256 withdrawCollateral, uint256 repayDebtAssets);

        function maxLiquidation(address _borrower)
            external
            view
            returns (uint256 collateralToLiquidate, uint256 debtToRepay, bool sTokenRequired);

        function isSolvent(address _borrower) external view returns (bool);
    }
}
