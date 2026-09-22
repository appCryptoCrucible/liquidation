// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {ExecutorHarness} from "../unit/ExecutorHarness.sol";
import {Plan, FlashGroup, LiqLeg, SwapLeg} from "../../src/lib/PlanDecoder.sol";

/// Solidity half of WP 10B: decode Rust-encoded plans and re-pack byte-for-bit.
/// Reads `generated.bin` written by `cargo test -p liq-plan`. No RPC / no fork.
contract PlanEncodingRoundTrip is Test {
    ExecutorHarness h;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    function setUp() public {
        h = new ExecutorHarness(address(1), address(2), address(3), bytes32(0), address(4), address(5), WETH);
    }

    function test_generated_plans_decode_and_repack() public {
        if (!vm.exists("test/encoding/generated.bin")) {
            revert("generated.bin missing - run cargo test -p liq-plan write_solidity_roundtrip_cases");
        }
        bytes memory blob = vm.readFileBinary("test/encoding/generated.bin");
        require(blob.length >= 4, "empty generated.bin");
        uint256 n = uint256(uint32(bytes4(blob)));
        require(n >= 256, "need >=256 cases");
        uint256 o = 4;
        for (uint256 i; i < n; ++i) {
            require(o + 4 <= blob.length, "len prefix");
            uint32 len;
            assembly {
                len := shr(224, mload(add(add(blob, 32), o)))
            }
            o += 4;
            require(o + len <= blob.length, "plan bytes");
            bytes memory plan = new bytes(len);
            for (uint256 k; k < len; ++k) {
                plan[k] = blob[o + k];
            }
            o += len;
            bytes memory rebuilt = _rebuild(plan);
            assertEq(rebuilt, plan, "solidity decode+repack != rust encode");
        }
    }

    function _rebuild(bytes memory plan) internal view returns (bytes memory out) {
        Plan memory hdr = h.debugHeader(plan);
        out = abi.encodePacked(hdr.flags, hdr.bidBps, hdr.gasCostWei, hdr.minProfit, hdr.groupCount);
        for (uint256 g; g < hdr.groupCount; ++g) {
            (FlashGroup memory fg,) = h.debugGroup(plan, g);
            out = bytes.concat(
                out,
                abi.encodePacked(
                    fg.provider, fg.flashSource, fg.debtAsset, fg.flashAmount, fg.liqCount, fg.repaySwapCount
                )
            );
            for (uint256 i; i < fg.liqCount; ++i) {
                (LiqLeg memory l, bytes memory tail) = h.debugLiqLeg(plan, g, i);
                out = bytes.concat(
                    out,
                    abi.encodePacked(l.adapter, l.market, l.borrower, l.collateralAsset, l.repayAmount),
                    tail
                );
            }
            for (uint256 i; i < fg.repaySwapCount; ++i) {
                (SwapLeg memory s, bytes memory data) = h.debugRepaySwap(plan, g, i);
                out = bytes.concat(
                    out, abi.encodePacked(s.venue, s.tokenIn, s.tokenOut, s.flags, s.amount, s.dataLen), data
                );
            }
        }
        uint8 pc = uint8(plan[hdr.profitSwapOffset]);
        out = bytes.concat(out, abi.encodePacked(pc));
        for (uint256 i; i < pc; ++i) {
            (SwapLeg memory s, bytes memory data) = h.debugProfitSwap(plan, i);
            out = bytes.concat(
                out, abi.encodePacked(s.venue, s.tokenIn, s.tokenOut, s.flags, s.amount, s.dataLen), data
            );
        }
    }
}
