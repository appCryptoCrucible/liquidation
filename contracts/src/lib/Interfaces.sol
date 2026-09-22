// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/*
 * External ABIs the Executor calls. Every signature is taken from the pinned
 * source the off-chain adapter was reviewed against — never from docs:
 *
 *   Aave V3   aave-dao/aave-v3-origin @ 8305565ae   (WP 04B)
 *   Aave V4   aave/aave-v4            @ 40232a0a    (WP 04A)
 *   Morpho    morpho-org/morpho-blue  @ 8e26ca6a    (WP 15A-1)
 *   Uniswap V3 / V4, Sky DSS Flash                  (WP 07A)
 */

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}

interface IWETH {
    function balanceOf(address) external view returns (uint256);
    function deposit() external payable;
    function withdraw(uint256) external;
}

// ───────────────────────────── Aave V3 Pool ─────────────────────────────
interface IAavePool {
    function flashLoanSimple(
        address receiver, address asset, uint256 amount,
        bytes calldata params, uint16 referralCode
    ) external;
    function liquidationCall(
        address collateral, address debt, address user,
        uint256 debtToCover, bool receiveAToken
    ) external;
    function getUserAccountData(address user) external view returns (
        uint256 totalCollateralBase, uint256 totalDebtBase, uint256 availableBorrowsBase,
        uint256 currentLiquidationThreshold, uint256 ltv, uint256 healthFactor
    );
}

// ───────────────────────────── Aave V4 Spoke ────────────────────────────
/// `ISpoke` pin 40232a0a. Reserves are addressed by `reserveId`, not by
/// underlying — the plan leg carries both ids in its adapter tail.
interface IAaveV4Spoke {
    struct UserAccountData {
        uint256 riskPremium;
        uint256 avgCollateralFactor;
        uint256 healthFactor;
        uint256 totalCollateralValue;
        uint256 totalDebtValueRay;
        uint256 activeCollateralCount;
        uint256 borrowCount;
    }
    function liquidationCall(
        uint256 collateralReserveId, uint256 debtReserveId, address user,
        uint256 debtToCover, bool receiveShares
    ) external;
    function getUserAccountData(address user) external view returns (UserAccountData memory);
}

// ───────────────────────────── Morpho Blue ──────────────────────────────
struct MarketParams {
    address loanToken;
    address collateralToken;
    address oracle;
    address irm;
    uint256 lltv;
}

/// `IMorpho` pin 8e26ca6a. `Id` is `bytes32` on the wire.
interface IMorpho {
    struct Market {
        uint128 totalSupplyAssets;
        uint128 totalSupplyShares;
        uint128 totalBorrowAssets;
        uint128 totalBorrowShares;
        uint128 lastUpdate;
        uint128 fee;
    }
    struct Position {
        uint256 supplyShares;
        uint128 borrowShares;
        uint128 collateral;
    }
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
    function idToMarketParams(bytes32 id) external view returns (MarketParams memory);
    function market(bytes32 id) external view returns (Market memory);
    function position(bytes32 id, address user) external view returns (Position memory);
    function accrueInterest(MarketParams memory marketParams) external;
    function liquidate(
        MarketParams memory marketParams, address borrower,
        uint256 seizedAssets, uint256 repaidShares, bytes memory data
    ) external returns (uint256 seized, uint256 repaid);
}

// ───────────────────────────── Uniswap V3 ───────────────────────────────
interface IUniV3Pool {
    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external;
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function swap(
        address recipient, bool zeroForOne, int256 amountSpecified,
        uint160 sqrtPriceLimitX96, bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

// ───────────────────────────── Uniswap V4 ───────────────────────────────
interface IPoolManager {
    function unlock(bytes calldata data) external returns (bytes memory);
    function take(address currency, address to, uint256 amount) external;
    function sync(address currency) external;
    function settle() external payable returns (uint256);
}

// ───────────────────────────── Sky DSS Flash ────────────────────────────
/// ERC-3156 Dai flash-mint. Mainnet 0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA.
/// DAI only; the off-chain encoder enforces `debtAsset == dai()`.
interface IDssFlash {
    function flashLoan(
        address receiver, address token, uint256 amount, bytes calldata data
    ) external returns (bool);
}

// ───────────────────────────── Euler V2 (EVK) ───────────────────────────
/// `IEVault` liquidation module pin `bfb325a6`. Call the **debt** vault.
/// `collateral` is the collateral vault (shares), not the underlying.
interface IEVault {
    function EVC() external view returns (address);
    function checkLiquidation(address liquidator, address violator, address collateral)
        external view returns (uint256 maxRepay, uint256 maxYield);
    function liquidate(address violator, address collateral, uint256 repayAssets, uint256 minYieldBalance)
        external;
    /// Pulls `amount` of underlying from the caller and burns `receiver`'s debt.
    /// `type(uint256).max` repays the full owed balance.
    function repay(uint256 amount, address receiver) external returns (uint256);
    /// Releases this vault as the caller's controller. Only valid with no debt.
    function disableController() external;
    /// `amount` is shares (`Vault.sol` `toShares`). Returns assets. Pin `bfb325a6`.
    function redeem(uint256 amount, address receiver, address owner) external returns (uint256);
}

/// Ethereum Vault Connector. `batch` of a self-call uses delegatecall so
/// `msg.sender` stays the executor (`enableController`'s owner check).
interface IEVC {
    struct BatchItem {
        address targetContract;
        address onBehalfOfAccount;
        uint256 value;
        bytes data;
    }
    function batch(BatchItem[] calldata items) external payable;
    function enableController(address account, address vault) external payable;
}

// ───────────────────────────── Silo V2 ──────────────────────────────────
/// `IPartialLiquidation` pin `570a668a`. Call the **hook receiver**, never the
/// Silo ERC-4626. Pin topic0 `LiquidationCall` `0x3a84f644…`.
interface ISiloHook {
    function maxLiquidation(address borrower)
        external view returns (uint256 collateralToLiquidate, uint256 debtToRepay, bool sTokenRequired);
    function liquidationCall(
        address collateralAsset, address debtAsset, address borrower,
        uint256 maxDebtToCover, bool receiveSToken
    ) external returns (uint256 withdrawCollateral, uint256 repayDebtAssets);
}

// ───────────────────────────── Liquity V2 ───────────────────────────────
/// `ITroveManager.batchLiquidateTroves` selector `0xef49a6b4` pin `c8a5a4ee`.
/// Status: 1 = active, 4 = zombie. Empty array → `EmptyData`; none
/// liquidatable → `NothingToLiquidate`.
interface ITroveManager {
    function getTroveStatus(uint256 troveId) external view returns (uint8);
    function batchLiquidateTroves(uint256[] calldata troveArray) external;
}

// ───────────────────────────── Fluid T1 ─────────────────────────────────
/// T1 `liquidate` pin `9496626f`. T2/T3/T4 are different ABIs — do not call this
/// on a non-T1 vault. `colPerUnitDebt_` is **1e18** min coll per debt (slip
/// `(actualCol * 1e18) / actualDebt`). Not internal `colPerDebt` (1e27).
interface IFluidT1 {
    function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_)
        external payable returns (uint256 actualDebtAmt_, uint256 actualColAmt_);
}

// ───────────────────────────── Gearbox V3 ───────────────────────────────
/// `ICreditFacadeV3` pin `510fc654`. Call the **facade**, never the manager.
/// Partial only; full `liquidateCreditAccount` + MultiCall is unwired.
struct PriceUpdate {
    address priceFeed;
    bytes data;
}

interface ICreditFacadeV3 {
    function partiallyLiquidateCreditAccount(
        address creditAccount, address token, uint256 repaidAmount,
        uint256 minSeizedAmount, address to, PriceUpdate[] calldata priceUpdates
    ) external returns (uint256 seizedAmount);
}

// ───────────────────────────── Compound V2 ──────────────────────────────
/// Official Unitroller pin `a3214f67`. CEther vs CErc20 is a config flag,
/// never `underlying()` on-chain.
interface ICToken {
    function comptroller() external view returns (address);
    /// `CTokenInterfaces.redeem` pin `a3214f67`. Same selector on CEther and CErc20.
    /// Returns 0 on success. CEther sends ETH; CErc20 sends `underlying`.
    function redeem(uint256 redeemTokens) external returns (uint256);
}

interface IComptroller {
    function getAccountLiquidity(address account)
        external view returns (uint256 err, uint256 liquidity, uint256 shortfall);
    function isDeprecated(address cToken) external view returns (bool);
}

interface ICErc20 {
    function liquidateBorrow(address borrower, uint256 repayAmount, address cTokenCollateral)
        external returns (uint256);
}

interface ICEther {
    function liquidateBorrow(address borrower, address cTokenCollateral) external payable;
}
