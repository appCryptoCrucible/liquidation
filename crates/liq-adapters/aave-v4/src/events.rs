//! Event ABI of the tracked contracts, transcribed from the interfaces at
//! `aave/aave-v4` @ `40232a0a` (`IHubBase`, `IHub`, `ISpoke`, `IAaveOracle`)
//! and OpenZeppelin's `IERC1967`/`Initializable`/`IAccessManaged`. The
//! `topic0` of every event here is asserted against the 03C coverage table
//! (`docs/coverage/aave-v4.md`) in `tests/coverage.rs`, so a transcription
//! slip fails the build's tests rather than silently un-subscribing a row.
//!
//! Rows of the coverage table this adapter does **not** subscribe to, and
//! why (the router never hands them to `apply_log`):
//!
//! * `hub.setInterestRateData` — emitted by the rate strategy, a contract
//!   the registry does not pin; the rate it produces arrives in the next
//!   `UpdateAsset`, which is the canonical fold.
//! * `tok.*` — TokenizationSpoke holds no borrower positions; its hub-side
//!   effects are `hub.add`/`hub.remove` logs on the Hub.
//! * `pm.*` — PositionManager logs duplicate the Spoke log emitted in the
//!   same call; the Spoke log is the fold.
//! * `gov.payloadExecuted` — governance execution chain (WP 08B).

use alloy_sol_types::sol;

/// `IHubBase` + `IHub` events (the Hub proxy).
pub mod hub {
    use super::sol;
    sol! {
        struct PremiumDelta { int256 sharesDelta; int256 offsetRayDelta; uint256 restoredPremiumRay; }
        struct AssetConfig { address feeReceiver; uint16 liquidityFee; address irStrategy; address reinvestmentController; }
        struct SpokeConfig { uint40 addCap; uint40 drawCap; uint24 riskPremiumThreshold; bool active; bool halted; }

        event Add(uint256 indexed assetId, address indexed spoke, uint256 shares, uint256 amount);
        event Remove(uint256 indexed assetId, address indexed spoke, uint256 shares, uint256 amount);
        event Draw(uint256 indexed assetId, address indexed spoke, uint256 drawnShares, uint256 drawnAmount);
        event Restore(uint256 indexed assetId, address indexed spoke, uint256 drawnShares, PremiumDelta premiumDelta, uint256 drawnAmount, uint256 premiumAmount);
        event RefreshPremium(uint256 indexed assetId, address indexed spoke, PremiumDelta premiumDelta);
        event ReportDeficit(uint256 indexed assetId, address indexed spoke, uint256 drawnShares, PremiumDelta premiumDelta, uint256 deficitAmountRay);
        event TransferShares(uint256 indexed assetId, address indexed sender, address indexed receiver, uint256 shares);
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
}

/// `ISpoke` events (the Spoke proxy).
pub mod spoke {
    use super::sol;
    sol! {
        struct PremiumDelta { int256 sharesDelta; int256 offsetRayDelta; uint256 restoredPremiumRay; }
        struct LiquidationConfig { uint128 targetHealthFactor; uint64 healthFactorForMaxBonus; uint16 liquidationBonusFactor; }
        struct ReserveConfig { uint24 collateralRisk; bool paused; bool frozen; bool borrowable; bool receiveSharesEnabled; }
        struct DynamicReserveConfig { uint16 collateralFactor; uint32 maxLiquidationBonus; uint16 liquidationFee; }

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
}

/// `IAaveOracle` events (the spoke's oracle).
pub mod oracle {
    use super::sol;
    sol! {
        event UpdateReserveSource(uint256 indexed reserveId, address indexed source);
        event SetSpoke(address indexed spoke);
    }
}

/// Halt-class events any tracked proxy may emit (`docs/coverage/aave-v4.md`
/// `halt` rows): ERC-1967 upgrade/admin, OZ initializer, AccessManaged
/// authority.
pub mod halt {
    use super::sol;
    sol! {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
        event Initialized(uint64 version);
        event AuthorityUpdated(address authority);
    }
}
