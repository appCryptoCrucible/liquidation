// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {Plan, FlashGroup, LiqLeg, SwapLeg, PlanDecoder} from "../../src/lib/PlanDecoder.sol";
import {SafeTransfer} from "../../src/lib/SafeTransfer.sol";

/// Test-only surface (PLAN-ENCODING §4): exposes the decoder walk so the
/// Rust encoder can be checked field-for-field, including walked offsets.
/// Never deployed — lives under `test/`.
contract ExecutorHarness is Executor {
    using PlanDecoder for bytes;

    constructor(
        address operator_, address profitSink_,
        address univ3Factory_, bytes32 univ3InitHash_,
        address routerA_, address routerB_, address weth_
    ) Executor(
        operator_, profitSink_, univ3Factory_, univ3InitHash_, routerA_, routerB_, weth_,
        // Decode-only harness: the V2/Curve anchors are never exercised here.
        address(0x21), bytes32(0), address(0x22), bytes32(0), address(0x23)
    ) {}

    function debugHeader(bytes calldata plan) external pure returns (Plan memory) {
        return plan.header();
    }

    function debugGroup(bytes calldata plan, uint256 g) external pure returns (FlashGroup memory fg, uint256 next) {
        uint256 o = PlanDecoder.HEADER_LEN + 1;
        for (uint256 i; i < g; ++i) {
            (, o) = plan.group(o);
        }
        return plan.group(o);
    }

    /// Leg `i` of group `g`, plus its raw tail bytes.
    function debugLiqLeg(bytes calldata plan, uint256 g, uint256 i)
        external pure returns (LiqLeg memory l, bytes memory tail)
    {
        FlashGroup memory fg = plan.groupAt(g);
        uint256 o = fg.liqOffset;
        for (uint256 k; k < i; ++k) {
            (, o) = plan.liqLeg(o);
        }
        (l, ) = plan.liqLeg(o);
        tail = plan[l.tailOffset : l.tailOffset + PlanDecoder.tailLen(l.adapter)];
    }

    function debugTailV4(bytes calldata plan, uint256 o) external pure returns (uint16, uint16) {
        return plan.tailV4(o);
    }

    function debugTailMorpho(bytes calldata plan, uint256 o) external pure returns (bytes32) {
        return plan.tailMorpho(o);
    }

    /// Repay swap leg `i` of group `g`.
    function debugRepaySwap(bytes calldata plan, uint256 g, uint256 i)
        external pure returns (SwapLeg memory s, bytes memory data)
    {
        FlashGroup memory fg = plan.groupAt(g);
        require(i < fg.repaySwapCount, "repay swap index");
        return _swapAt(plan, fg.repaySwapOffset, i);
    }

    /// Profit swap leg `i`.
    function debugProfitSwap(bytes calldata plan, uint256 i)
        external pure returns (SwapLeg memory s, bytes memory data)
    {
        Plan memory p = plan.header();
        require(i < uint8(plan[p.profitSwapOffset]), "profit swap index");
        return _swapAt(plan, p.profitSwapOffset + 1, i);
    }

    function _swapAt(bytes calldata plan, uint256 o, uint256 i)
        internal pure returns (SwapLeg memory s, bytes memory data)
    {
        for (uint256 k; k < i; ++k) {
            (, o) = plan.swapLeg(o);
        }
        (s, ) = plan.swapLeg(o);
        data = plan[s.dataOffset : s.dataOffset + s.dataLen];
    }

    /// Direct access to the library for the USDT approve tests.
    function debugSafeApprove(address token, address spender, uint256 amount) external {
        SafeTransfer.safeApprove(token, spender, amount);
    }

    function debugSafeTransfer(address token, address to, uint256 amount) external {
        SafeTransfer.safeTransfer(token, to, amount);
    }
}
