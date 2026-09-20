//! Event ABI from `liquity/bold` @ `c8a5a4ee`:
//! `ITroveEvents.sol`, `IStabilityPoolEvents.sol`, `BorrowerOperations`,
//! `MainnetPriceFeedBase`, plus ERC-1967 halt.

use alloy_sol_types::sol;

sol! {
    event TroveUpdated(
        uint256 indexed troveId,
        uint256 debt,
        uint256 coll,
        uint256 stake,
        uint256 annualInterestRate,
        uint256 snapshotOfTotalCollRedist,
        uint256 snapshotOfTotalDebtRedist
    );
    event TroveOperation(
        uint256 indexed troveId,
        uint8 operation,
        uint256 annualInterestRate,
        uint256 debtIncreaseFromRedist,
        uint256 debtIncreaseFromUpfrontFee,
        int256 debtChangeFromOperation,
        uint256 collIncreaseFromRedist,
        int256 collChangeFromOperation
    );
    event BatchedTroveUpdated(
        uint256 indexed troveId,
        address interestBatchManager,
        uint256 batchDebtShares,
        uint256 coll,
        uint256 stake,
        uint256 snapshotOfTotalCollRedist,
        uint256 snapshotOfTotalDebtRedist
    );
    event BatchUpdated(
        address indexed interestBatchManager,
        uint8 operation,
        uint256 debt,
        uint256 coll,
        uint256 annualInterestRate,
        uint256 annualManagementFee,
        uint256 totalDebtShares,
        uint256 debtIncreaseFromUpfrontFee
    );
    event Liquidation(
        uint256 debtOffsetBySP,
        uint256 debtRedistributed,
        uint256 boldGasCompensation,
        uint256 collGasCompensation,
        uint256 collSentToSP,
        uint256 collRedistributed,
        uint256 collSurplus,
        uint256 lColl,
        uint256 lBoldDebt,
        uint256 price
    );
    event Redemption(
        uint256 attemptedBoldAmount,
        uint256 actualBoldAmount,
        uint256 ethSent,
        uint256 ethFee,
        uint256 price,
        uint256 redemptionPrice
    );
    event RedemptionFeePaidToTrove(uint256 indexed troveId, uint256 ethFee);
    event TroveNFTAddressChanged(address newTroveNFTAddress);
    event BorrowerOperationsAddressChanged(address newBorrowerOperationsAddress);
    event BoldTokenAddressChanged(address newBoldTokenAddress);
    event StabilityPoolAddressChanged(address stabilityPoolAddress);
    event GasPoolAddressChanged(address gasPoolAddress);
    event CollSurplusPoolAddressChanged(address collSurplusPoolAddress);
    event SortedTrovesAddressChanged(address sortedTrovesAddress);
    event CollateralRegistryAddressChanged(address collateralRegistryAddress);
    event ActivePoolAddressChanged(address newActivePoolAddress);
    event DefaultPoolAddressChanged(address newDefaultPoolAddress);
    event PriceFeedAddressChanged(address newPriceFeedAddress);
    event TroveManagerAddressChanged(address newTroveManagerAddress);
    event StabilityPoolBoldBalanceUpdated(uint256 newBalance);
    event StabilityPoolCollBalanceUpdated(uint256 newBalance);
    event DepositUpdated(
        address indexed depositor,
        uint256 newDeposit,
        uint256 stashedColl,
        uint256 snapshotP,
        uint256 snapshotS,
        uint256 snapshotB,
        uint256 snapshotScale
    );
    event DepositOperation(
        address indexed depositor,
        uint8 operation,
        uint256 depositLossSinceLastOperation,
        int256 topUpOrWithdrawal,
        uint256 yieldGainSinceLastOperation,
        uint256 yieldGainClaimed,
        uint256 ethGainSinceLastOperation,
        uint256 ethGainClaimed
    );
    event P_Updated(uint256 P);
    event S_Updated(uint256 S, uint256 scale);
    event B_Updated(uint256 B, uint256 scale);
    event ScaleUpdated(uint256 currentScale);
    event ShutDown(uint256 tcr);
    event ShutDownFromOracleFailure(address failedOracleAddr);
}

/// `ITroveEvents.Operation` ordinals @ `c8a5a4ee`.
pub mod op {
    pub const OPEN_TROVE: u8 = 0;
    pub const CLOSE_TROVE: u8 = 1;
    pub const ADJUST_TROVE: u8 = 2;
    pub const ADJUST_TROVE_INTEREST_RATE: u8 = 3;
    pub const APPLY_PENDING_DEBT: u8 = 4;
    pub const LIQUIDATE: u8 = 5;
    pub const REDEEM_COLLATERAL: u8 = 6;
    pub const OPEN_TROVE_AND_JOIN_BATCH: u8 = 7;
    pub const SET_INTEREST_BATCH_MANAGER: u8 = 8;
    pub const REMOVE_FROM_BATCH: u8 = 9;
}

pub mod halt {
    use super::sol;
    sol! {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
        event Initialized(uint64 version);
    }
}

/// `TroveManager.batchLiquidateTroves(uint256[] _troveArray)` — 10R ABI.
/// Empty calldata array reverts `EmptyData`; no liquidatable id reverts
/// `NothingToLiquidate`. Not an `ExecutorAdapter` discriminant yet.
pub mod liq {
    use super::sol;
    sol! {
        function batchLiquidateTroves(uint256[] troveArray);
    }
}
