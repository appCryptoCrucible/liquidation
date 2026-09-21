// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {ExecutorHarness} from "./ExecutorHarness.sol";
import {Plan, FlashGroup, LiqLeg, SwapLeg, PlanDecoder} from "../../src/lib/PlanDecoder.sol";

/// Solidity half of the wire round trip (PLAN-ENCODING §4). The fixtures
/// under `test/fixtures/` are produced by the Rust test-local encoder in
/// `crates/liq-exec/tests/wire_roundtrip.rs`; the field constants below are
/// hard-coded here independently. Oracle: PLAN-ENCODING §1 by hand.
contract PlanDecodeTest is Test {
    ExecutorHarness h;

    // `Address::repeat_byte(b)` in the Rust fixture.
    address immutable WETH     = _rep(0xEE);
    address immutable POOL_V3  = _rep(0xA3);
    address immutable SPOKE_V4 = _rep(0xA4);
    address immutable MORPHO   = _rep(0xBB);
    address immutable DEBT0    = _rep(0xD1);
    address immutable DEBT1    = _rep(0xD2);
    address immutable DAI      = _rep(0xDA);
    address immutable COLL0    = _rep(0xC0);
    address immutable COLL1    = _rep(0xC1);
    address immutable COLL2    = _rep(0xC2);

    function setUp() public {
        h = new ExecutorHarness(address(1), address(2), address(3), bytes32(0), address(4), address(5), WETH);
    }

    function _fixture(string memory name) internal view returns (bytes memory) {
        return vm.parseBytes(vm.trim(vm.readFile(string.concat("test/fixtures/", name, ".hex"))));
    }

    function _rep(uint8 b) internal pure returns (address) {
        return address(uint160(bytes20(abi.encodePacked(
            b, b, b, b, b, b, b, b, b, b, b, b, b, b, b, b, b, b, b, b
        ))));
    }

    function test_repeat_byte_address_helper() public pure {
        assertEq(_rep(0xEE), address(uint160(uint256(bytes32(hex"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")) >> 96)));
    }

    function test_full_fixture_header_and_walked_offsets() public view {
        bytes memory plan = _fixture("plan_v1_full");
        Plan memory p = h.debugHeader(plan);
        assertEq(p.flags, 1, "SWEEP");
        assertEq(p.bidBps, 9800);
        assertEq(p.gasCostWei, 0x0102030405060708090a0b0c0d0e0f10);
        assertEq(p.minProfit, 12345678901234567890);
        assertEq(p.groupCount, 3);

        (FlashGroup memory g0, uint256 n0) = h.debugGroup(plan, 0);
        assertEq(g0.provider, 0, "Aave");
        assertEq(g0.flashSource, _rep(0x11));
        assertEq(g0.debtAsset, DEBT0);
        assertEq(g0.flashAmount, 1_000_000_000_000);
        assertEq(g0.liqCount, 2);
        assertEq(g0.repaySwapCount, 1);
        assertEq(g0.liqOffset, 36 + 59);
        // V3 leg (77) + V4 leg (77 + 4 tail).
        assertEq(g0.repaySwapOffset, g0.liqOffset + 77 + 81);
        assertEq(n0, g0.repaySwapOffset + 60 + 20);

        (FlashGroup memory g1, uint256 n1) = h.debugGroup(plan, 1);
        assertEq(g1.provider, 2, "UniV4");
        assertEq(g1.flashSource, _rep(0x22));
        assertEq(g1.debtAsset, DEBT1);
        assertEq(g1.flashAmount, 7e18);
        assertEq(g1.liqCount, 1);
        assertEq(g1.repaySwapCount, 0, "minimum repay swaps");
        assertEq(g1.liqOffset, n0 + 59);
        assertEq(g1.repaySwapOffset, g1.liqOffset + 77 + 32, "Morpho tail");
        assertEq(n1, g1.repaySwapOffset);

        (FlashGroup memory g2, uint256 n2) = h.debugGroup(plan, 2);
        assertEq(g2.provider, 4, "Sky DSS");
        assertEq(g2.flashSource, _rep(0x44));
        assertEq(g2.debtAsset, DAI);
        assertEq(g2.flashAmount, 3e18);
        assertEq(g2.liqCount, 1);
        assertEq(g2.repaySwapCount, 2);
        assertEq(g2.liqOffset, n1 + 59);
        assertEq(g2.repaySwapOffset, g2.liqOffset + 77);
        // router leg 60 + 56, pool leg 60 + 20
        assertEq(n2, g2.repaySwapOffset + 116 + 80);

        assertEq(p.profitSwapOffset, n2);
        assertEq(uint8(plan[p.profitSwapOffset]), 2);
        // count + two pool legs, then the end.
        assertEq(p.profitSwapOffset + 1 + 2 * 80, plan.length, "walk lands on the end");
    }

    function test_full_fixture_liq_legs_and_tails() public view {
        bytes memory plan = _fixture("plan_v1_full");

        (LiqLeg memory l, bytes memory tail) = h.debugLiqLeg(plan, 0, 0);
        assertEq(l.adapter, PlanDecoder.A_AAVE_V3);
        assertEq(l.market, POOL_V3);
        assertEq(l.borrower, _rep(0xB0));
        assertEq(l.collateralAsset, COLL0);
        assertEq(l.repayAmount, 500_000_000_000);
        assertEq(tail.length, 0);

        (l, tail) = h.debugLiqLeg(plan, 0, 1);
        assertEq(l.adapter, PlanDecoder.A_AAVE_V4);
        assertEq(l.market, SPOKE_V4);
        assertEq(l.borrower, _rep(0xB1));
        assertEq(l.collateralAsset, COLL1);
        assertEq(l.repayAmount, 400_000_000_000);
        assertEq(tail, hex"00070003");
        (uint16 collId, uint16 debtId) = h.debugTailV4(plan, l.tailOffset);
        assertEq(collId, 7);
        assertEq(debtId, 3);

        (l, tail) = h.debugLiqLeg(plan, 1, 0);
        assertEq(l.adapter, PlanDecoder.A_MORPHO);
        assertEq(l.market, MORPHO);
        assertEq(l.borrower, _rep(0xB2));
        assertEq(l.collateralAsset, COLL2);
        assertEq(l.repayAmount, 6e18);
        assertEq(tail.length, 32);
        assertEq(h.debugTailMorpho(plan, l.tailOffset), bytes32(uint256(0xe1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1)));

        (l, ) = h.debugLiqLeg(plan, 2, 0);
        assertEq(l.borrower, _rep(0xB3));
        assertEq(l.repayAmount, 2_999e15);
    }

    function test_full_fixture_swap_legs() public {
        bytes memory plan = _fixture("plan_v1_full");

        (SwapLeg memory s, bytes memory data) = h.debugRepaySwap(plan, 0, 0);
        assertEq(s.venue, 0);
        assertEq(s.tokenIn, COLL0);
        assertEq(s.tokenOut, DEBT0);
        assertEq(s.flags, 2, "EXACT_OUT");
        assertEq(s.amount, 1_000_500_000_000);
        assertEq(s.dataLen, 20);
        assertEq(data, abi.encodePacked(_rep(0xF0)));

        (s, data) = h.debugRepaySwap(plan, 2, 0);
        assertEq(s.venue, 1, "router");
        assertEq(s.tokenIn, COLL0);
        assertEq(s.tokenOut, DAI);
        assertEq(s.amount, 1.5e18);
        assertEq(s.dataLen, 56);
        assertEq(address(bytes20(data)), _rep(0x77));
        for (uint256 i = 20; i < 56; i++) assertEq(uint8(data[i]), 0x5A);

        (s, data) = h.debugRepaySwap(plan, 2, 1);
        assertEq(s.venue, 0);
        assertEq(data, abi.encodePacked(_rep(0xF1)));

        (s, data) = h.debugProfitSwap(plan, 0);
        assertEq(s.tokenIn, COLL0);
        assertEq(s.tokenOut, WETH);
        assertEq(s.flags, 1, "TAKE_BALANCE");
        assertEq(s.amount, 0);
        assertEq(data, abi.encodePacked(_rep(0xF2)));
        (s, data) = h.debugProfitSwap(plan, 1);
        assertEq(s.tokenIn, COLL2);
        assertEq(data, abi.encodePacked(_rep(0xF3)));

        vm.expectRevert("profit swap index");
        h.debugProfitSwap(plan, 2);
        vm.expectRevert("repay swap index");
        h.debugRepaySwap(plan, 1, 0);
    }

    function test_min_fixture_every_minimum() public view {
        bytes memory plan = _fixture("plan_v1_min");
        assertEq(plan.length, 35 + 1 + 59 + 77 + 4 + 1);
        Plan memory p = h.debugHeader(plan);
        assertEq(p.flags, 0);
        assertEq(p.bidBps, 0);
        assertEq(p.gasCostWei, 0);
        assertEq(p.minProfit, 1);
        assertEq(p.groupCount, 1);
        assertEq(p.profitSwapOffset, 176);
        assertEq(uint8(plan[176]), 0);
        (FlashGroup memory g, uint256 n) = h.debugGroup(plan, 0);
        assertEq(g.provider, 3, "Morpho");
        assertEq(g.flashSource, MORPHO);
        assertEq(g.debtAsset, WETH);
        assertEq(g.flashAmount, 1);
        assertEq(g.repaySwapCount, 0);
        assertEq(n, 176);
        (LiqLeg memory l, bytes memory tail) = h.debugLiqLeg(plan, 0, 0);
        assertEq(l.adapter, PlanDecoder.A_AAVE_V4);
        assertEq(l.borrower, address(0x0101010101010101010101010101010101010101));
        assertEq(l.repayAmount, 1);
        assertEq(tail, hex"ffff0000");
    }

    /// Mutation #2 (TESTING.md §4): a stride error must be rejected, never
    /// misread. Same three mutations as the Rust test.
    function test_stride_mutations_revert() public {
        bytes memory good = _fixture("plan_v1_full");
        h.debugHeader(good);

        // Drop one byte from the V4 tail (group 0 leg 1).
        bytes memory short = _without(good, 36 + 59 + 77 + 77 + 3);
        vm.expectRevert();
        h.debugHeader(short);

        // Extra byte inside the last profit leg's data.
        bytes memory long = _insert(good, good.length - 5, 0);
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.BadPlanLength.selector, good.length, good.length + 1));
        h.debugHeader(long);

        // Unknown adapter id fails at the leg.
        bytes memory bad = good;
        bad[36 + 59] = 0x09;
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.UnknownAdapter.selector, uint8(9)));
        h.debugHeader(bad);

        // Trailing garbage.
        bytes memory trailing = abi.encodePacked(_fixture("plan_v1_full"), hex"00");
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.BadPlanLength.selector, good.length, good.length + 1));
        h.debugHeader(trailing);

        // Zero groups.
        bytes memory zero = new bytes(36);
        vm.expectRevert(PlanDecoder.NoGroups.selector);
        h.debugHeader(zero);
    }

    function _without(bytes memory b, uint256 at) internal pure returns (bytes memory out) {
        out = new bytes(b.length - 1);
        for (uint256 i; i < at; i++) out[i] = b[i];
        for (uint256 i = at + 1; i < b.length; i++) out[i - 1] = b[i];
    }

    function test_tail_len_10e_and_unknown_9() public {
        // Existing fixtures still decode (V3/V4/Morpho).
        h.debugHeader(_fixture("plan_v1_full"));
        h.debugHeader(_fixture("plan_v1_min"));
        assertEq(PlanDecoder.A_AAVE_V3, 0);
        assertEq(PlanDecoder.A_AAVE_V4, 1);
        assertEq(PlanDecoder.A_MORPHO, 2);
        assertEq(PlanDecoder.A_EULER, 3);
        assertEq(PlanDecoder.A_SILO, 4);
        assertEq(PlanDecoder.A_LIQUITY, 5);
        assertEq(PlanDecoder.A_FLUID, 6);
        assertEq(PlanDecoder.A_GEARBOX, 7);
        assertEq(PlanDecoder.A_COMPOUND, 8);
        assertEq(PlanDecoder.tailLen(0), 0);
        assertEq(PlanDecoder.tailLen(1), 4);
        assertEq(PlanDecoder.tailLen(2), 32);
        assertEq(PlanDecoder.tailLen(3), 32);
        assertEq(PlanDecoder.tailLen(4), 0);
        assertEq(PlanDecoder.tailLen(5), 32);
        assertEq(PlanDecoder.tailLen(6), 32);
        assertEq(PlanDecoder.tailLen(7), 32);
        assertEq(PlanDecoder.tailLen(8), 21);
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.UnknownAdapter.selector, uint8(9)));
        this.tailLen9();
    }

    function tailLen9() external pure returns (uint256) {
        return PlanDecoder.tailLen(9);
    }

    function _insert(bytes memory b, uint256 at, bytes1 v) internal pure returns (bytes memory out) {
        out = new bytes(b.length + 1);
        for (uint256 i; i < at; i++) out[i] = b[i];
        out[at] = v;
        for (uint256 i = at; i < b.length; i++) out[i + 1] = b[i];
    }
}
