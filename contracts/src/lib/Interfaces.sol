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
