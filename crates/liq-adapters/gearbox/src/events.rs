//! Event + view ABI from `Gearbox-protocol/core-v3` @ `510fc6541c3767ce825929b4c311826fe81d6fa5`.
//! Liquidation entry is **CreditFacadeV3**, not the manager (`creditFacadeOnly`).

use alloy_sol_types::sol;

pub mod facade {
    use super::sol;
    sol! {
        event OpenCreditAccount(
            address indexed creditAccount,
            address indexed onBehalfOf,
            address indexed caller,
            uint256 referralCode
        );
        event CloseCreditAccount(address indexed creditAccount, address indexed borrower);
        event LiquidateCreditAccount(
            address indexed creditAccount,
            address indexed liquidator,
            address to,
            uint256 remainingFunds
        );
        event PartiallyLiquidateCreditAccount(
            address indexed creditAccount,
            address indexed token,
            address indexed liquidator,
            uint256 repaidDebt,
            uint256 seizedCollateral,
            uint256 fee
        );
        event AddCollateral(address indexed creditAccount, address indexed token, uint256 amount);
        event WithdrawCollateral(
            address indexed creditAccount,
            address indexed token,
            uint256 amount,
            address to
        );
        event StartMultiCall(address indexed creditAccount, address indexed caller);
        event WithdrawPhantomToken(address indexed creditAccount, address indexed token, uint256 amount);
        event Execute(address indexed creditAccount, address indexed targetContract);
        event FinishMultiCall();
        event Paused(address account);
        event Unpaused(address account);
    }
}

pub mod configurator {
    use super::sol;
    sol! {
        event AddCollateralToken(address indexed token);
        event SetTokenLiquidationThreshold(address indexed token, uint16 liquidationThreshold);
        event ScheduleTokenLiquidationThresholdRamp(
            address indexed token,
            uint16 liquidationThresholdInitial,
            uint16 liquidationThresholdFinal,
            uint40 timestampRampStart,
            uint40 timestampRampEnd
        );
        event ForbidToken(address indexed token);
        event AllowToken(address indexed token);
        event AllowAdapter(address indexed targetContract, address indexed adapter);
        event ForbidAdapter(address indexed targetContract, address indexed adapter);
        event UpdateFees(
            uint16 feeLiquidation,
            uint16 liquidationPremium,
            uint16 feeLiquidationExpired,
            uint16 liquidationPremiumExpired
        );
        event SetPriceOracle(address indexed priceOracle);
        event SetCreditFacade(address indexed creditFacade);
        event CreditConfiguratorUpgraded(address indexed creditConfigurator);
        event SetBorrowingLimits(uint256 minDebt, uint256 maxDebt);
        event SetMaxDebtPerBlockMultiplier(uint8 maxDebtPerBlockMultiplier);
        event SetLossPolicy(address indexed lossPolicy);
        event SetExpirationDate(uint40 expirationDate);
    }
}

pub mod manager {
    use super::sol;
    sol! {
        event SetCreditConfigurator(address indexed newConfigurator);
    }
}

pub mod pool {
    use super::sol;
    sol! {
        event Borrow(address indexed creditManager, address indexed creditAccount, uint256 amount);
        event Repay(address indexed creditManager, uint256 borrowedAmount, uint256 profit, uint256 loss);
        event AddCreditManager(address indexed creditManager);
        event SetInterestRateModel(address indexed newInterestRateModel);
        event SetPoolQuotaKeeper(address indexed newPoolQuotaKeeper);
        event SetTotalDebtLimit(uint256 limit);
        event SetCreditManagerDebtLimit(address indexed creditManager, uint256 newLimit);
        event SetWithdrawFee(uint256 fee);
        event IncurUncoveredLoss(address indexed creditManager, uint256 loss);
        event Refer(address indexed onBehalfOf, uint256 indexed referralCode, uint256 amount);
    }
}

pub mod factory {
    use super::sol;
    sol! {
        event DeployCreditAccount(address indexed creditAccount, address indexed creditManager);
        event TakeCreditAccount(address indexed creditAccount, address indexed creditManager);
        event ReturnCreditAccount(address indexed creditAccount, address indexed creditManager);
        event AddCreditManager(address indexed creditManager, address masterCreditAccount);
        event Rescue(address indexed creditAccount, address indexed target, bytes data);
    }
}

pub mod quota {
    use super::sol;
    sol! {
        event UpdateQuota(address indexed creditAccount, address indexed token, int96 quotaChange);
        event UpdateTokenQuotaRate(address indexed token, uint16 rate);
        event SetGauge(address indexed newGauge);
        event AddCreditManager(address indexed creditManager);
        event AddQuotaToken(address indexed token);
        event SetTokenLimit(address indexed token, uint96 limit);
        event SetQuotaIncreaseFee(address indexed token, uint16 fee);
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

/// Pin views used at boot (`fees()` must be live) and for `health_probe`.
pub mod views {
    use super::sol;
    sol! {
        struct CollateralDebtData {
            uint256 debt;
            uint256 cumulativeIndexNow;
            uint256 cumulativeIndexLastUpdate;
            uint128 cumulativeQuotaInterest;
            uint256 accruedInterest;
            uint256 accruedFees;
            uint256 totalDebtUSD;
            uint256 totalValue;
            uint256 totalValueUSD;
            uint256 twvUSD;
            uint256 enabledTokensMask;
            uint256 quotedTokensMask;
            address[] quotedTokens;
            address poolQuotaKeeper;
        }

        interface IContractsRegister {
            function isCreditManager(address) external view returns (bool);
            function getCreditManagers() external view returns (address[] memory);
        }

        interface ICreditManagerV3 {
            function pool() external view returns (address);
            function underlying() external view returns (address);
            function creditFacade() external view returns (address);
            function creditConfigurator() external view returns (address);
            function accountFactory() external view returns (address);
            function fees()
                external
                view
                returns (
                    uint16 feeInterest,
                    uint16 feeLiquidation,
                    uint16 liquidationDiscount,
                    uint16 feeLiquidationExpired,
                    uint16 liquidationDiscountExpired
                );
            function collateralTokensCount() external view returns (uint8);
            function getTokenByMask(uint256 tokenMask) external view returns (address token);
            function liquidationThresholds(address token) external view returns (uint16 lt);
            function ltParams(address token)
                external
                view
                returns (
                    uint16 ltInitial,
                    uint16 ltFinal,
                    uint40 timestampRampStart,
                    uint24 rampDuration
                );
            function quotedTokensMask() external view returns (uint256);
            function calcDebtAndCollateral(address creditAccount, uint8 task)
                external
                view
                returns (CollateralDebtData memory cdd);
        }

        interface ICreditFacadeV3 {
            function expirable() external view returns (bool);
            function expirationDate() external view returns (uint40);
        }

        interface IPoolV3 {
            function poolQuotaKeeper() external view returns (address);
        }

        interface IERC20 {
            function decimals() external view returns (uint8);
        }
    }
}

/// 10E wire ABI — call the facade, never the manager. Partial only.
pub mod liquidation_abi {
    use super::sol;
    sol! {
        struct MultiCall {
            address target;
            bytes callData;
        }
        struct PriceUpdate {
            address priceFeed;
            bytes data;
        }

        function liquidateCreditAccount(
            address creditAccount,
            address to,
            MultiCall[] calls,
            bytes lossPolicyData
        ) external;

        function liquidateCreditAccount(address creditAccount, address to, MultiCall[] calls) external;

        function partiallyLiquidateCreditAccount(
            address creditAccount,
            address token,
            uint256 repaidAmount,
            uint256 minSeizedAmount,
            address to,
            PriceUpdate[] priceUpdates
        ) external returns (uint256 seizedAmount);
    }
}

/// Pin `CollateralCalcTask.DEBT_COLLATERAL`.
pub const DEBT_COLLATERAL_TASK: u8 = 3;
