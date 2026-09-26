// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../unit/Base.sol";
import {MockERC20} from "../unit/Mocks.sol";

/**
 * Edge-value property fuzz for INV-01 / INV-08 / INV-11 (and related).
 * Stateful invariants live in ExecutorFocusInvariants.t.sol.
 */
contract ExecutorEdgeFuzzTest is ExecutorTestBase {
    address internal attacker = makeAddr("edge-attacker");

    function testFuzz_INV01_rogueCallback_alwaysReverts(address caller, uint8 which) public {
        vm.assume(caller != address(0));
        which = uint8(bound(which, 0, 5));
        bytes4 want = which == 5 ? Executor.BadSwapCallback.selector : Executor.BadCallback.selector;
        bytes memory call = which == 0
            ? abi.encodeCall(ex.executeOperation, (address(debt), type(uint256).max, 0, address(ex), hex"dead"))
            : which == 1
                ? abi.encodeCall(ex.uniswapV3FlashCallback, (type(uint256).max, type(uint256).max, hex"ab"))
                : which == 2
                    ? abi.encodeCall(ex.unlockCallback, (hex"cd"))
                    : which == 3
                        ? abi.encodeCall(ex.onMorphoFlashLoan, (type(uint256).max, hex"ef"))
                        : which == 4
                            ? abi.encodeCall(
                                ex.onFlashLoan, (address(ex), address(debt), type(uint256).max, 0, hex"11")
                            )
                            : abi.encodeCall(
                                ex.uniswapV3SwapCallback,
                                (type(int256).max, type(int256).min, abi.encode(address(coll), address(debt), uint24(3000)))
                            );
        vm.prank(caller);
        (bool ok, bytes memory r) = address(ex).call(call);
        assertFalse(ok, "INV-01: callback must revert");
        assertEq(bytes4(r), want, "INV-01: wrong revert selector");
    }

    function testFuzz_INV01_zeroAddressCaller_stillReverts(uint8 which) public {
        which = uint8(bound(which, 0, 4));
        bytes memory call = which == 0
            ? abi.encodeCall(ex.executeOperation, (address(debt), 0, 0, address(0), ""))
            : which == 1
                ? abi.encodeCall(ex.uniswapV3FlashCallback, (0, 0, ""))
                : which == 2
                    ? abi.encodeCall(ex.unlockCallback, (""))
                    : which == 3
                        ? abi.encodeCall(ex.onMorphoFlashLoan, (0, ""))
                        : abi.encodeCall(ex.onFlashLoan, (address(0), address(0), 0, 0, ""));
        vm.prank(address(0));
        (bool ok, bytes memory r) = address(ex).call(call);
        assertFalse(ok);
        assertEq(bytes4(r), Executor.BadCallback.selector);
    }

    function testFuzz_INV11_sweep_only_increases_sink(uint256 wethAmt, uint256 debtAmt, address caller) public {
        vm.assume(caller != sink && caller != address(ex));
        wethAmt = bound(wethAmt, 0, 100e18);
        debtAmt = bound(debtAmt, 0, 1_000_000e6);
        if (wethAmt > 0) weth.mint(address(ex), wethAmt);
        if (debtAmt > 0) debt.mint(address(ex), debtAmt);

        uint256 sinkW = weth.balanceOf(sink);
        uint256 sinkD = debt.balanceOf(sink);
        uint256 callerW = weth.balanceOf(caller);
        uint256 callerD = debt.balanceOf(caller);
        uint256 attW = weth.balanceOf(attacker);

        address[] memory assets = new address[](2);
        assets[0] = address(weth);
        assets[1] = address(debt);

        vm.prank(caller);
        ex.sweep(assets);

        assertEq(weth.balanceOf(address(ex)), 0, "ex WETH cleared");
        assertEq(debt.balanceOf(address(ex)), 0, "ex debt cleared");
        assertEq(weth.balanceOf(sink), sinkW + wethAmt, "INV-11: all WETH to sink");
        assertEq(debt.balanceOf(sink), sinkD + debtAmt, "INV-11: all debt to sink");
        assertEq(weth.balanceOf(caller), callerW, "INV-11: caller gained no WETH");
        assertEq(debt.balanceOf(caller), callerD, "INV-11: caller gained no debt");
        assertEq(weth.balanceOf(attacker), attW, "INV-11: attacker unchanged");
    }

    function testFuzz_INV11_sweep_empty_and_zero_balance_noop(address caller) public {
        vm.assume(caller != address(0));
        address[] memory empty = new address[](0);
        vm.prank(caller);
        ex.sweep(empty);

        address[] memory assets = new address[](1);
        assets[0] = address(weth);
        uint256 sinkBefore = weth.balanceOf(sink);
        vm.prank(caller);
        ex.sweep(assets);
        assertEq(weth.balanceOf(sink), sinkBefore);
    }

    function testFuzz_INV08_standing_weth_theft_reverts_when_above_profit(uint256 standingSeed) public {
        // Standing must exceed reference liq gross (~0.995e18) or steal is masked by profit.
        uint256 standing = bound(standingSeed, 2e18, 100e18);
        EdgeDrainPull pull = new EdgeDrainPull();
        Executor stealEx = new Executor(
            operator,
            sink,
            address(factory),
            factory.initHash(),
            address(pull),
            address(routerB),
            address(weth),
            v2Factory,
            V2_HASH,
            sushiFactory,
            SUSHI_HASH,
            address(curveRegistry)
        );
        weth.mint(address(stealEx), standing);
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);

        bytes memory stealLeg = PB.routerSwap(
            address(pull),
            address(weth),
            address(weth),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(EdgeDrainPull.steal, (address(weth), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        vm.prank(operator);
        vm.expectRevert();
        stealEx.execute(plan);
        assertEq(weth.balanceOf(address(stealEx)), standing, "INV-08: standing intact");
        assertEq(weth.balanceOf(attacker), 0, "INV-08: no attacker WETH");
    }

    /// Successful execute never finishes with wethAfter < wethBefore (gross underflow path).
    function testFuzz_INV08_successful_execute_weth_non_decreasing(uint16 bidBps, bool sweepFlag) public {
        bidBps = uint16(bound(bidBps, 0, 2000));
        uint256 standing = 3e18;
        weth.mint(address(ex), standing);
        uint256 before = weth.balanceOf(address(ex));
        bytes memory plan = _plan(sweepFlag ? PB.F_SWEEP : 0, bidBps, 0, 0, 1, PB.legV3(address(pool), borrower, address(coll), REPAY));
        _exec(plan);
        // After success: executor WETH + sink delta + coinbase >= before (accounting for bid/sweep).
        uint256 afterEx = weth.balanceOf(address(ex));
        uint256 afterSink = weth.balanceOf(sink);
        assertGe(afterEx + afterSink + coinbase.received(), before, "INV-08: net WETH accounting");
    }

    function testFuzz_nonOperator_execute_reverts(address caller, bytes memory junk) public {
        vm.assume(caller != operator);
        vm.prank(caller);
        vm.expectRevert(Executor.NotOperator.selector);
        ex.execute(junk);
    }
}

contract EdgeDrainPull {
    function steal(address token, address to, uint256 amount) external {
        require(MockERC20(token).transferFrom(msg.sender, to, amount), "drain");
    }
}
