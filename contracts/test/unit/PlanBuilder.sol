// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/*
 * Test-side packed-plan encoder (PLAN-ENCODING §1). Field-for-field the
 * layout `PlanDecoder` walks; the production encoder is `liq-plan` (Rust)
 * and the cross-language fixtures in `test/fixtures/` pin the two together.
 * This exists only so the flow tests can build plans inline.
 */
library PlanBuilder {
    uint8 internal constant F_SWEEP        = 1;
    uint8 internal constant L_TAKE_BALANCE = 1;
    uint8 internal constant L_EXACT_OUT    = 2;
    /// Swap-leg flags bits 2–7: the liquidation leg a repay swap serves, as
    /// its index in the group plus one (0: untied).
    uint8 internal constant L_TIE_SHIFT    = 2;

    uint8 internal constant P_AAVE = 0; uint8 internal constant P_UNIV3 = 1; uint8 internal constant P_UNIV4 = 2;
    uint8 internal constant P_MORPHO = 3; uint8 internal constant P_SKY = 4; uint8 internal constant P_NONE = 5;
    uint8 internal constant P_UNIV3_SWAP = 6;
    uint8 internal constant A_V3 = 0; uint8 internal constant A_V4 = 1; uint8 internal constant A_MORPHO = 2;
    uint8 internal constant A_EULER = 3; uint8 internal constant A_SILO = 4; uint8 internal constant A_LIQUITY = 5;
    uint8 internal constant A_FLUID = 6; uint8 internal constant A_GEARBOX = 7; uint8 internal constant A_COMPOUND = 8;
    uint8 internal constant S_POOL = 0; uint8 internal constant S_ROUTER = 1;
    uint8 internal constant S_V2 = 2; uint8 internal constant S_CURVE = 3;
    uint8 internal constant S_CURVE_CRYPTO = 4;
    uint8 internal constant S_UNIV4 = 9;
    uint8 internal constant S_UNWRAP_4626 = 5;
    uint8 internal constant S_PENDLE_PT_REDEEM = 6;
    uint8 internal constant S_CURVE_LP_ONE_COIN = 7;
    uint8 internal constant S_PENDLE_MARKET_SELL = 8;

    function header(uint8 flags, uint16 bidBps, uint128 gasCostWei, uint128 minProfit, uint8 groups)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(flags, bidBps, gasCostWei, minProfit, groups);
    }

    function groupHead(uint8 provider, address src, address debt, uint128 amount, uint8 liqCount, uint8 repayCount)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(provider, src, debt, amount, liqCount, repayCount);
    }

    function legV3(address pool, address borrower, address coll, uint128 repay) internal pure returns (bytes memory) {
        return abi.encodePacked(A_V3, pool, borrower, coll, repay);
    }

    function legV4(address spoke, address borrower, address coll, uint128 repay, uint16 collId, uint16 debtId)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_V4, spoke, borrower, coll, repay, collId, debtId);
    }

    function legMorpho(address morpho, address borrower, address coll, uint128 repay, bytes32 id)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_MORPHO, morpho, borrower, coll, repay, id);
    }

    function legEuler(
        address vault,
        address borrower,
        address coll,
        uint128 repay,
        uint256 minYield,
        address collVault
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(A_EULER, vault, borrower, coll, repay, minYield, collVault);
    }

    function legSilo(address hook, address borrower, address coll, uint128 repay)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_SILO, hook, borrower, coll, repay);
    }

    function legLiquity(address tm, address borrower, address coll, uint128 repay, uint256 troveId)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_LIQUITY, tm, borrower, coll, repay, troveId);
    }

    uint8 internal constant FL_DEBT1 = 1; uint8 internal constant FL_COL1 = 2; uint8 internal constant FL_ABSORB = 4;
    uint8 internal constant FL_NATIVE_DEBT = 8; uint8 internal constant FL_NATIVE_COL = 16;

    function legFluid(address vault, address borrower, address coll, uint128 repay, uint256 colPer)
        internal pure returns (bytes memory)
    {
        return legFluidT(vault, borrower, coll, repay, 1, FL_ABSORB, colPer, 0, 0);
    }

    /// Tail: u8 type | u8 flags | colPerUnitDebt | debtPerShareMax | colPerShareMin.
    function legFluidT(
        address vault, address borrower, address coll, uint128 repay,
        uint8 vtype, uint8 flags, uint256 colPer, uint256 debtPerShare, uint256 colPerShare
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(A_FLUID, vault, borrower, coll, repay, vtype, flags, colPer, debtPerShare, colPerShare);
    }

    uint8 internal constant GB_PARTIAL = 0; uint8 internal constant GB_FULL = 1;

    function legGearbox(address facade, address borrower, address coll, uint128 repay, uint256 minSeized, uint8 mode)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_GEARBOX, facade, borrower, coll, repay, minSeized, mode);
    }

    function legCompound(address cDebt, address borrower, address coll, uint128 repay, address cColl, uint8 isCEther)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_COMPOUND, cDebt, borrower, coll, repay, cColl, isCEther);
    }

    function swap(uint8 venue, address tIn, address tOut, uint8 flags, uint128 amount, bytes memory data)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(venue, tIn, tOut, flags, amount, uint16(data.length), data);
    }

    /// `flags` with the swap tied to liquidation leg `leg` of its group: the
    /// swap is skipped when that leg did not fill.
    function tie(uint8 flags, uint8 leg) internal pure returns (uint8) {
        require(leg < 63, "tie: leg index");
        return flags | uint8((uint256(leg) + 1) << L_TIE_SHIFT);
    }

    /// Uniswap V4 pool leg (venue 9): the pool key, `currency0 ‖ currency1 ‖
    /// fee (3) ‖ tickSpacing (3) ‖ hooks`.
    function v4Swap(
        address c0, address c1, uint24 fee, int24 tickSpacing, address hooks,
        address tIn, address tOut, uint8 flags, uint128 amount
    ) internal pure returns (bytes memory) {
        return swap(S_UNIV4, tIn, tOut, flags, amount, abi.encodePacked(c0, c1, fee, tickSpacing, hooks));
    }

    function poolSwap(address pool, address tIn, address tOut, uint8 flags, uint128 amount) internal pure returns (bytes memory) {
        return swap(S_POOL, tIn, tOut, flags, amount, abi.encodePacked(pool));
    }

    /// Pool-direct V3 leg on a fork: the pool, then the factory id (1 =
    /// SushiSwap V3, 2 = PancakeSwap V3). Uniswap's is `poolSwap`.
    function forkSwap(address pool, uint8 fid, address tIn, address tOut, uint8 flags, uint128 amount)
        internal pure returns (bytes memory)
    {
        return swap(S_POOL, tIn, tOut, flags, amount, abi.encodePacked(pool, fid));
    }

    /// Pair-direct V2 leg. `fid`: 0 = Uniswap V2, 1 = SushiSwap.
    function v2Swap(address pair, uint8 fid, address tIn, address tOut, uint8 flags, uint128 amount)
        internal pure returns (bytes memory)
    {
        return swap(S_V2, tIn, tOut, flags, amount, abi.encodePacked(pair, fid));
    }

    /// Pool-direct Curve leg (exact input only). `h` is the index of the
    /// MetaRegistry handler the pool is registered in.
    function curveSwap(address pool, uint8 i, uint8 j, uint8 h, address tIn, address tOut, uint8 flags, uint128 amount)
        internal pure returns (bytes memory)
    {
        return swap(S_CURVE, tIn, tOut, flags, amount, abi.encodePacked(pool, i, j, h));
    }

    function curveCryptoSwap(
        address pool, uint8 i, uint8 j, uint8 h, address tIn, address tOut, uint8 flags, uint128 amount
    ) internal pure returns (bytes memory) {
        return swap(S_CURVE_CRYPTO, tIn, tOut, flags, amount, abi.encodePacked(pool, i, j, h));
    }

    /// Redeem all held `vault` shares into its asset.
    function unwrap4626(address vault, address asset) internal pure returns (bytes memory) {
        return swap(S_UNWRAP_4626, vault, asset, L_TAKE_BALANCE, 0, abi.encodePacked(vault));
    }

    /// Withdraw a Curve NG LP (the pool) as coin `i` (`tokenOut`). `h` is
    /// the index of the MetaRegistry handler the pool is registered in.
    function curveLpOneCoin(address pool, uint8 i, uint8 h, address tokenOut) internal pure returns (bytes memory) {
        return swap(S_CURVE_LP_ONE_COIN, pool, tokenOut, L_TAKE_BALANCE, 0, abi.encodePacked(pool, i, h));
    }

    /// Sell a live Pendle PT on `market`, then redeem the SY into `tokenOut`.
    function pendleMarketSell(address pt, address market, address tokenOut) internal pure returns (bytes memory) {
        return swap(S_PENDLE_MARKET_SELL, pt, tokenOut, L_TAKE_BALANCE, 0, abi.encodePacked(market));
    }

    /// Redeem an expired Pendle PT through its YT and SY into `tokenOut`.
    function pendlePtRedeem(address pt, address yt, address tokenOut) internal pure returns (bytes memory) {
        return swap(S_PENDLE_PT_REDEEM, pt, tokenOut, L_TAKE_BALANCE, 0, abi.encodePacked(yt));
    }

    function routerSwap(address router, address tIn, address tOut, uint8 flags, uint128 amount, bytes memory call)
        internal pure returns (bytes memory)
    {
        return swap(S_ROUTER, tIn, tOut, flags, amount, abi.encodePacked(router, call));
    }

    /// Profit blob: count byte + legs.
    function profit(uint8 n, bytes memory legs) internal pure returns (bytes memory) {
        return abi.encodePacked(n, legs);
    }
}
