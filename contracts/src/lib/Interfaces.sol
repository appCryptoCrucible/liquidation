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
    function allowance(address owner, address spender) external view returns (uint256);
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
    /// Reverts unless the user's health factor is below 1.
    function liquidationCall(
        address collateral, address debt, address user,
        uint256 debtToCover, bool receiveAToken
    ) external;
    /// Not called by the Executor (`liquidationCall` checks health itself);
    /// the fork suite reads it as its oracle.
    function getUserAccountData(address user) external view returns (
        uint256 totalCollateralBase, uint256 totalDebtBase, uint256 availableBorrowsBase,
        uint256 currentLiquidationThreshold, uint256 ltv, uint256 healthFactor
    );
}

// ─────────────────────── Aave governance v3 payloads ────────────────────
/// `PayloadsControllerCore.executePayload`, deployed implementation
/// 0x7222182cb9c5320587b5148bf03eee107ad64578 (Sourcify). No access check;
/// reverts unless the payload is Queued and `block.timestamp > queuedAt + delay`.
interface IPayloadsController {
    function executePayload(uint40 payloadId) external payable;
}

/// Sky executive spell (`DssExec`, dss-exec-lib). `cast()` has no access
/// check; it calls `pause.exec(action, tag, sig, eta)`, which requires the
/// plan to be plotted and `now >= eta`. Office hours are enforced by the
/// action.
interface IDssSpell {
    function action() external view returns (address);
    function tag() external view returns (bytes32);
    function sig() external view returns (bytes memory);
    function eta() external view returns (uint256);
    function cast() external;
}

/// Sky `DSPause`: `plans[keccak256(abi.encode(usr, tag, fax, eta))]` is set
/// only by an authorized `plot` (the Chief's hat) and cleared on `exec`.
interface IDSPause {
    function plans(bytes32) external view returns (bool);
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
    /// Reverts unless the user's health factor is below 1.
    function liquidationCall(
        uint256 collateralReserveId, uint256 debtReserveId, address user,
        uint256 debtToCover, bool receiveShares
    ) external;
    /// Not called by the Executor; the fork suite reads it as its oracle.
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
/// Uniswap V4 `PoolKey` (v4-core `types/PoolKey.sol`). Currency `0` is
/// native ETH; `currency0 < currency1`.
struct V4PoolKey {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

/// Uniswap V4 `IPoolManager.SwapParams`. `amountSpecified < 0` is an exact
/// input, `> 0` an exact output.
struct V4SwapParams {
    bool zeroForOne;
    int256 amountSpecified;
    uint160 sqrtPriceLimitX96;
}

interface IPoolManager {
    function unlock(bytes calldata data) external returns (bytes memory);
    function take(address currency, address to, uint256 amount) external;
    function sync(address currency) external;
    function settle() external payable returns (uint256);
    /// Returns the caller's `BalanceDelta`: amount0 in the high 128 bits,
    /// amount1 in the low, each negative when owed to the pool.
    function swap(V4PoolKey memory key, V4SwapParams memory params, bytes calldata hookData)
        external returns (int256 swapDelta);
}

// ───────────────────────────── Sky DSS Flash ────────────────────────────
/// ERC-3156 Dai flash-mint. Mainnet 0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA.
/// DAI only; the off-chain encoder enforces `debtAsset == dai()`.
/// Uniswap V2 pair (and forks sharing the pair ABI, e.g. SushiSwap).
interface IUniV2Pair {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

/// Curve StableSwap plain pool (int128 coin indices).
interface ICurvePool {
    function coins(uint256 i) external view returns (address);
    function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) external;
    /// StableSwap-NG: burns the caller's LP (the pool is its own LP token)
    /// and pays coin `i` to the caller.
    function remove_liquidity_one_coin(uint256 burn_amount, int128 i, uint256 min_received)
        external
        returns (uint256);
}

/// Curve crypto pool (twocrypto-ng, tricrypto-ng, the original
/// `CurveCryptoSwap2`): unsigned coin indices. The return value is not read.
interface ICurveCryptoPool {
    function coins(uint256 i) external view returns (address);
    function exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy) external;
}

/// ERC-4626 vault, as an unwrap step: redeem shares for `asset()`.
/// Pendle principal token (`PendlePrincipalToken`).
interface IPendlePT {
    function YT() external view returns (address);
}

/// Pendle yield token (`PendleYieldToken`): PT (+YT before expiry) sent to it
/// first, then `redeemPY` pays SY. After expiry PT alone redeems.
interface IPendleYT {
    function PT() external view returns (address);
    function SY() external view returns (address);
    function isExpired() external view returns (bool);
    function redeemPY(address receiver) external returns (uint256 amountSyOut);
}

/// Pendle market (`PendleMarketV6`): PT sent to it first, then
/// `swapExactPtForSy` pays SY to `receiver`.
interface IPendleMarket {
    function readTokens() external view returns (address sy, address pt, address yt);
    function swapExactPtForSy(address receiver, uint256 exactPtIn, bytes calldata data)
        external
        returns (uint256 netSyOut, uint256 netSyFee);
}

interface IPendleMarketFactory {
    function isValidMarket(address market) external view returns (bool);
}

/// Pendle standardized yield (`SYBase.redeem`): burns the caller's shares.
interface IPendleSY {
    function redeem(
        address receiver,
        uint256 amountSharesToRedeem,
        address tokenOut,
        uint256 minTokenOut,
        bool burnFromInternalBalance
    ) external returns (uint256 amountTokenOut);
}

interface IERC4626Unwrap {
    function asset() external view returns (address);
    function redeem(uint256 shares, address receiver, address owner) external returns (uint256);
}

/// Curve MetaRegistry (deployed source on Sourcify). Its own
/// `is_registered(pool)` asks every handler below `registry_length` in turn
/// and is true when any of them holds the pool. `get_registry(i)` is the
/// handler at index `i`: handlers are only ever appended or replaced in
/// place, so an index past the list reads the zero address.
interface ICurveMetaRegistry {
    function get_registry(uint256 i) external view returns (address);
}

/// One MetaRegistry handler: the registry API over a single base registry
/// or factory. `is_registered` is false, not a revert, for a pool it does
/// not hold.
interface ICurveRegistryHandler {
    function is_registered(address pool) external view returns (bool);
}

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
    /// Reverts `E_ExcessiveRepayAmount` when `repayAssets` is above the
    /// violator's maximum repay (zero for a healthy one) and `E_MinYield`
    /// when the yield is below `minYieldBalance`.
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
    /// Marks `vault` as a collateral the account's controller(s) will read
    /// during the account status check. Without this the seized shares are
    /// held but not counted toward the account's collateral set, so the
    /// controller's health check at the end of the batch sees the debt
    /// (before `repay` clears it) against a smaller collateral set than the
    /// executor actually holds.
    function enableCollateral(address account, address vault) external payable;
}

// ───────────────────────────── Silo V2 ──────────────────────────────────
/// `IPartialLiquidation` pin `570a668a`. Call the **hook receiver**, never the
/// Silo ERC-4626. Pin topic0 `LiquidationCall` `0x3a84f644…`.
interface ISiloHook {
    /// Not called by the Executor; the fork suite reads it as its oracle.
    function maxLiquidation(address borrower)
        external view returns (uint256 collateralToLiquidate, uint256 debtToRepay, bool sTokenRequired);
    /// Reverts for a solvent borrower. With `receiveSToken == false` the
    /// hook redeems the seized shares to the caller, and reverts when the
    /// collateral silo is short of liquidity.
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

// ───────────────────────────── Fluid vaults ──────────────────────────────
/// `Instadapp/fluid-contracts-public` at `9496626f`. Four types, four ABIs —
/// dispatch on the vault's type, never the selector (T2 `liquidate` and T3
/// `liquidate` share one signature). `colPerUnitDebt_` is **1e18** min
/// collateral (token, or col shares on T2/T4) per unit of debt (token, or
/// debt shares on T3/T4): `(actualCol * 1e18) / actualDebt`. Not 1e27.
///
/// T1: normal collateral, normal debt. Native debt needs `msg.value ==
/// debtAmt_`; the surplus over what was repaid is refunded.
interface IFluidT1 {
    function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_)
        external payable returns (uint256 actualDebtAmt_, uint256 actualColAmt_);
}

/// T2: smart collateral (DEX col shares), normal debt. The col shares come
/// out in one token when one of the two per-share minimums is zero
/// (`withdrawPerfectInOneToken`). Excess ETH is refunded (`_validateEth`).
interface IFluidT2 {
    function liquidate(
        uint256 debtAmt_,
        uint256 colPerUnitDebt_,
        uint256 token0ColAmtPerUnitShares_,
        uint256 token1ColAmtPerUnitShares_,
        address to_,
        bool absorb_
    ) external payable returns (uint256 actualDebt_, uint256 actualColShares_, uint256 token0Col_, uint256 token1Col_);
}

/// T3: normal collateral, smart debt (DEX debt shares). `liquidate` pays
/// exactly the token amounts given (one of them zero pays in one token),
/// burns the debt shares that buys, and reverts below `debtSharesMin_`.
/// Excess ETH is refunded (`_validateEth`).
interface IFluidT3 {
    function liquidate(
        uint256 token0DebtAmt_,
        uint256 token1DebtAmt_,
        uint256 debtSharesMin_,
        uint256 colPerUnitDebt_,
        address to_,
        bool absorb_
    ) external payable returns (uint256 actualDebtShares_, uint256 actualCol_);
}

/// T4: smart collateral and smart debt. T3's payback and T2's withdraw.
interface IFluidT4 {
    function liquidate(
        uint256 token0DebtAmt_,
        uint256 token1DebtAmt_,
        uint256 debtSharesMin_,
        uint256 colPerUnitDebt_,
        uint256 token0ColAmtPerUnitShares_,
        uint256 token1ColAmtPerUnitShares_,
        address to_,
        bool absorb_
    ) external payable returns (uint256 actualDebtShares_, uint256 actualColShares_, uint256 token0Col_, uint256 token1Col_);
}

// ───────────────────────────── Gearbox V3 ───────────────────────────────
/// `CreditFacadeV3`. Live debt is on v3.1 facades (`version() == 310`,
/// core-v3 `510fc654`, enumerated from the v3.1 address provider), which
/// have both paths. The v3.0 facades (`version() == 301`, Sourcify-verified)
/// have only the 3-arg full liquidation, which v3.1 keeps as a wrapper with
/// empty loss-policy data. Call the **facade**, never the manager.
struct PriceUpdate {
    address priceFeed;
    bytes data;
}

struct MultiCall {
    address target;
    bytes callData;
}

interface ICreditFacadeV3 {
    /// Reverts unless `debt != 0` and (`twvUSD < totalDebtUSD` or expired).
    /// `calls` may only add collateral, withdraw collateral and call
    /// adapters (`LIQUIDATE_CREDIT_ACCOUNT_FLAGS`); non-underlying balances
    /// must not increase. The manager then pays the pool from the account's
    /// underlying, requires the account to keep the borrower's share, and
    /// sends the rest of its underlying to `to`.
    function liquidateCreditAccount(address creditAccount, address to, MultiCall[] calldata calls) external;
    /// v3.1 only. Pulls `repaidAmount` underlying (manager as spender),
    /// seizes `token` at the liquidation discount, then runs a full
    /// collateral check — so it only succeeds when the account ends healthy.
    function partiallyLiquidateCreditAccount(
        address creditAccount, address token, uint256 repaidAmount,
        uint256 minSeizedAmount, address to, PriceUpdate[] calldata priceUpdates
    ) external returns (uint256 seizedAmount);
    /// `addCollateral` pulls with the **manager** as spender
    /// (`CreditManagerV3.addCollateral(payer, …)` → `safeTransferFrom`), so
    /// the allowance belongs to this address, not to the facade.
    function creditManager() external view returns (address);
}

/// Facade multicall selectors (`ICreditFacadeV3Multicall`, v3.0). Targets
/// are the facade itself.
interface ICreditFacadeV3Multicall {
    function addCollateral(address token, uint256 amount) external;
    /// `amount == type(uint256).max` withdraws the balance less 1 wei.
    function withdrawCollateral(address token, uint256 amount, address to) external;
}

// ───────────────────────────── Compound V2 ──────────────────────────────
/// Official Unitroller pin `a3214f67`. CEther vs CErc20 is a config flag,
/// never `underlying()` on-chain.
interface ICToken {
    /// `CTokenInterfaces.redeem` pin `a3214f67`. Same selector on CEther and CErc20.
    /// Returns 0 on success. CEther sends ETH; CErc20 sends `underlying`.
    function redeem(uint256 redeemTokens) external returns (uint256);
}

/// `liquidateBorrow` asks the Comptroller's `liquidateBorrowAllowed`: the
/// borrower must have a shortfall, or the market be deprecated. Refused, a
/// CErc20 returns a non-zero error code and CEther reverts.
interface ICErc20 {
    function liquidateBorrow(address borrower, uint256 repayAmount, address cTokenCollateral)
        external returns (uint256);
}

interface ICEther {
    function liquidateBorrow(address borrower, address cTokenCollateral) external payable;
}

// ───────────────────────────── Balancer V2 ──────────────────────────────
/// The V2 Vault's single swap (`Vault.swap`, `balancer-v2-monorepo`
/// `IVault.sol`). `kind`: 0 = GIVEN_IN, 1 = GIVEN_OUT. `limit` is the least
/// out (GIVEN_IN) or the most in (GIVEN_OUT) the caller accepts.
struct BalancerSingleSwap {
    bytes32 poolId;
    uint8 kind;
    address assetIn;
    address assetOut;
    uint256 amount;
    bytes userData;
}

struct BalancerFunds {
    address sender;
    bool fromInternalBalance;
    address payable recipient;
    bool toInternalBalance;
}

interface IBalancerVault {
    function swap(BalancerSingleSwap memory singleSwap, BalancerFunds memory funds, uint256 limit, uint256 deadline)
        external payable returns (uint256 amountCalculated);
}

/// `BasePoolFactory.isPoolFromFactory` (the 2022 pool factories on).
interface IBalancerPoolFactory {
    function isPoolFromFactory(address pool) external view returns (bool);
}

// ───────────────────────────── Fluid DEX ────────────────────────────────
/// A Fluid DEX T1 pool (`FluidDexT1`, `iDexT1.sol`). The pool pulls the input
/// token from its caller to the Liquidity layer itself, so the caller
/// approves the pool; native ETH is sent as `msg.value`.
interface IFluidDexPool {
    function DEX_ID() external view returns (uint256);
    function swapIn(bool swap0to1, uint256 amountIn, uint256 amountOutMin, address to)
        external payable returns (uint256 amountOut);
    function swapOut(bool swap0to1, uint256 amountOut, uint256 amountInMax, address to)
        external payable returns (uint256 amountIn);
    /// `constantsView()` returns a static struct of 18 words (`dexId`,
    /// `liquidity`, `factory`, five implementations, `deployerContract`,
    /// `token0`, `token1`, six slots, `oracleMapping`); `DexModule` reads it
    /// with a raw static call, by offset.
}

interface IFluidDexFactory {
    function getDexAddress(uint256 dexId) external view returns (address);
}
