// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/*
 * PlanDecoder — packed-calldata decode of `Executor.execute(bytes plan)`.
 *
 * PLAN-ENCODING.md is the single source of truth for this layout; the Rust
 * decoder in `liq-exec::wire` mirrors every function here and the two are
 * tested against the same committed fixtures (`contracts/test/fixtures/
 * plan_v1_{full,min}.hex`). A mismatch is a silent misliquidation, not a
 * compile error.
 *
 * No offset is ever encoded. Every offset is derived by walking — an encoded
 * offset is a second source of truth for a fact already in the data.
 *
 * Every calldata slice below is bounds-checked by the compiler: a truncated
 * or over-claimed plan reverts here, before any external call is made.
 */

/// Header + derived offsets (PLAN-ENCODING §1a).
struct Plan {
    uint8   flags;
    uint16  bidBps;           // fraction of realized net paid to block.coinbase
    uint128 gasCostWei;       // predicted total gas cost, from the off-chain oracle
    uint128 minProfit;        // in wei, what we keep AFTER the bid
    uint8   groupCount;
    uint256 profitSwapOffset; // offset of the profit-swap count byte
}

/// One flashloan and everything it funds (PLAN-ENCODING §1b).
struct FlashGroup {
    uint8   provider;
    address flashSource;
    address debtAsset;
    uint128 flashAmount;
    uint8   liqCount;
    uint8   repaySwapCount;
    uint256 liqOffset;
    uint256 repaySwapOffset;
}

/// One position (PLAN-ENCODING §1b). `tailOffset` points at the adapter-
/// specific tail that follows the 77 fixed bytes; its length is
/// `PlanDecoder.tailLen(adapter)`.
struct LiqLeg {
    uint8   adapter;
    address market;           // Aave V3 Pool · V4 Spoke · Morpho singleton
    address borrower;
    address collateralAsset;
    uint128 repayAmount;
    uint256 tailOffset;
}

/// One swap leg (PLAN-ENCODING §1c). `data` is referenced, not copied.
struct SwapLeg {
    uint8   venue;
    address tokenIn;
    address tokenOut;
    uint8   flags;
    uint128 amount;
    uint256 dataOffset;
    uint16  dataLen;
}

library PlanDecoder {
    uint256 internal constant HEADER_LEN        = 35;
    uint256 internal constant GROUP_HEAD_LEN    = 59;
    uint256 internal constant LIQ_LEG_LEN       = 77;   // fixed part; + tailLen(adapter)
    uint256 internal constant SWAP_LEG_HEAD_LEN = 60;

    // Adapter ids — `liq_protocol::plan::ExecutorAdapter` discriminants (D48).
    uint8 internal constant A_AAVE_V3 = 0;
    uint8 internal constant A_AAVE_V4 = 1;
    uint8 internal constant A_MORPHO  = 2;

    /// Adapter tails. V3 addresses reserves by underlying — nothing extra.
    /// V4 `liquidationCall` takes `(collateralReserveId, debtReserveId)`;
    /// Morpho `liquidate` takes `MarketParams`, recovered on-chain from the
    /// 32-byte market `Id`. Neither fits the 77 fixed bytes.
    uint256 internal constant TAIL_AAVE_V3 = 0;
    uint256 internal constant TAIL_AAVE_V4 = 4;   // u16 collateralReserveId | u16 debtReserveId
    uint256 internal constant TAIL_MORPHO  = 32;  // bytes32 market Id

    error UnknownAdapter(uint8 a);
    error NoGroups();
    /// Walked length != `plan.length`: encoder and decoder disagree.
    error BadPlanLength(uint256 walked, uint256 actual);

    // ─────────────────────────────── header ───────────────────────────────

    /// Decodes the header and walks the whole plan once, so every group,
    /// leg and swap blob is bounds-checked before execution starts.
    function header(bytes calldata plan) internal pure returns (Plan memory p) {
        p.flags      = uint8(plan[0]);
        p.bidBps     = uint16(bytes2(plan[1:3]));
        p.gasCostWei = uint128(bytes16(plan[3:19]));
        p.minProfit  = uint128(bytes16(plan[19:35]));
        p.groupCount = uint8(plan[HEADER_LEN]);
        if (p.groupCount == 0) revert NoGroups();

        uint256 cur = HEADER_LEN + 1;
        for (uint256 g; g < p.groupCount; ++g) {
            (, cur) = group(plan, cur);
        }
        p.profitSwapOffset = cur;
        cur = skipSwapLegs(plan, cur + 1, uint8(plan[cur]));
        if (cur != plan.length) revert BadPlanLength(cur, plan.length);
    }

    // ─────────────────────────────── groups ───────────────────────────────

    function group(bytes calldata plan, uint256 o)
        internal pure returns (FlashGroup memory fg, uint256 next)
    {
        fg.provider       = uint8(plan[o]);
        fg.flashSource    = address(bytes20(plan[o + 1  : o + 21]));
        fg.debtAsset      = address(bytes20(plan[o + 21 : o + 41]));
        fg.flashAmount    = uint128(bytes16(plan[o + 41 : o + 57]));
        fg.liqCount       = uint8(plan[o + 57]);
        fg.repaySwapCount = uint8(plan[o + 58]);
        fg.liqOffset      = o + GROUP_HEAD_LEN;
        fg.repaySwapOffset = skipLiqLegs(plan, fg.liqOffset, fg.liqCount);
        next = skipSwapLegs(plan, fg.repaySwapOffset, fg.repaySwapCount);
    }

    /// Re-walks to group `g`. The provider callbacks have no room in their
    /// ABIs for the group index, so it travels through transient storage and
    /// the group is recovered from calldata here.
    function groupAt(bytes calldata plan, uint256 g)
        internal pure returns (FlashGroup memory fg)
    {
        uint256 o = HEADER_LEN + 1;
        for (uint256 i; i < g; ++i) {
            (, o) = group(plan, o);
        }
        (fg, ) = group(plan, o);
    }

    // ───────────────────────────── liquidation legs ───────────────────────

    function tailLen(uint8 adapter) internal pure returns (uint256) {
        if (adapter == A_AAVE_V3) return TAIL_AAVE_V3;
        if (adapter == A_AAVE_V4) return TAIL_AAVE_V4;
        if (adapter == A_MORPHO)  return TAIL_MORPHO;
        revert UnknownAdapter(adapter);
    }

    function liqLeg(bytes calldata plan, uint256 o)
        internal pure returns (LiqLeg memory l, uint256 next)
    {
        l.adapter         = uint8(plan[o]);
        l.market          = address(bytes20(plan[o + 1  : o + 21]));
        l.borrower        = address(bytes20(plan[o + 21 : o + 41]));
        l.collateralAsset = address(bytes20(plan[o + 41 : o + 61]));
        l.repayAmount     = uint128(bytes16(plan[o + 61 : o + 77]));
        l.tailOffset      = o + LIQ_LEG_LEN;
        next = l.tailOffset + tailLen(l.adapter);
        // Bounds-check the tail now; `tailV4`/`tailMorpho` read it later.
        if (next > plan.length) revert BadPlanLength(next, plan.length);
    }

    /// Legs are variable length (adapter tail), so the only way past them
    /// is to walk them. One calldata byte per leg.
    function skipLiqLegs(bytes calldata plan, uint256 o, uint8 n)
        internal pure returns (uint256)
    {
        for (uint256 i; i < n; ++i) {
            o += LIQ_LEG_LEN + tailLen(uint8(plan[o]));
        }
        if (o > plan.length) revert BadPlanLength(o, plan.length);
        return o;
    }

    function tailV4(bytes calldata plan, uint256 o)
        internal pure returns (uint16 collateralReserveId, uint16 debtReserveId)
    {
        collateralReserveId = uint16(bytes2(plan[o     : o + 2]));
        debtReserveId       = uint16(bytes2(plan[o + 2 : o + 4]));
    }

    function tailMorpho(bytes calldata plan, uint256 o) internal pure returns (bytes32 id) {
        id = bytes32(plan[o : o + 32]);
    }

    // ───────────────────────────────── swap legs ─────────────────────────

    function swapLeg(bytes calldata plan, uint256 o)
        internal pure returns (SwapLeg memory s, uint256 next)
    {
        s.venue      = uint8(plan[o]);
        s.tokenIn    = address(bytes20(plan[o + 1  : o + 21]));
        s.tokenOut   = address(bytes20(plan[o + 21 : o + 41]));
        s.flags      = uint8(plan[o + 41]);
        s.amount     = uint128(bytes16(plan[o + 42 : o + 58]));
        s.dataLen    = uint16(bytes2(plan[o + 58 : o + 60]));
        s.dataOffset = o + SWAP_LEG_HEAD_LEN;
        next = s.dataOffset + s.dataLen;
        if (next > plan.length) revert BadPlanLength(next, plan.length);
    }

    function skipSwapLegs(bytes calldata plan, uint256 o, uint8 n)
        internal pure returns (uint256)
    {
        for (uint256 i; i < n; ++i) {
            o += SWAP_LEG_HEAD_LEN + uint16(bytes2(plan[o + 58 : o + 60]));
        }
        if (o > plan.length) revert BadPlanLength(o, plan.length);
        return o;
    }
}
