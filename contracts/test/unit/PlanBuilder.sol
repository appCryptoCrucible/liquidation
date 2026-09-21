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

    uint8 internal constant P_AAVE = 0; uint8 internal constant P_UNIV3 = 1; uint8 internal constant P_UNIV4 = 2;
    uint8 internal constant P_MORPHO = 3; uint8 internal constant P_SKY = 4;
    uint8 internal constant A_V3 = 0; uint8 internal constant A_V4 = 1; uint8 internal constant A_MORPHO = 2;
    uint8 internal constant A_EULER = 3; uint8 internal constant A_SILO = 4; uint8 internal constant A_LIQUITY = 5;
    uint8 internal constant A_FLUID = 6; uint8 internal constant A_GEARBOX = 7; uint8 internal constant A_COMPOUND = 8;
    uint8 internal constant S_POOL = 0; uint8 internal constant S_ROUTER = 1;

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

    function legEuler(address vault, address borrower, address coll, uint128 repay, uint256 minYield)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_EULER, vault, borrower, coll, repay, minYield);
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

    function legFluid(address vault, address borrower, address coll, uint128 repay, uint256 colPer)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_FLUID, vault, borrower, coll, repay, colPer);
    }

    function legGearbox(address facade, address borrower, address coll, uint128 repay, uint256 minSeized)
        internal pure returns (bytes memory)
    {
        return abi.encodePacked(A_GEARBOX, facade, borrower, coll, repay, minSeized);
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

    function poolSwap(address pool, address tIn, address tOut, uint8 flags, uint128 amount) internal pure returns (bytes memory) {
        return swap(S_POOL, tIn, tOut, flags, amount, abi.encodePacked(pool));
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
