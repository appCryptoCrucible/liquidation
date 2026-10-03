// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {LiquidationModule} from "../../src/LiquidationModule.sol";
import {SwapModule} from "../../src/SwapModule.sol";
import {ILiquidationModule, ISwapModule} from "../../src/lib/ExecutorShared.sol";
import {ExecutorTestBase} from "./Base.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {MockTroveManager} from "./Mocks.sol";

/// Delegatecalls a module from outside any `execute` of ours.
contract ForeignDelegator {
    function run(address module, bytes calldata data) external returns (bool ok, bytes memory ret) {
        (ok, ret) = module.delegatecall(data);
    }
}

/// The Executor split into a core and two delegatecalled modules: who can
/// run module code, which code the core will run, and that errors, events,
/// value and size are what the single contract had.
contract ExecutorModulesTest is ExecutorTestBase {

    // ── who can run module code ──────────────────────────────────────────

    /// Called directly, a module would run on its own empty state. It refuses.
    function test_modules_refuse_a_direct_call() public {
        bytes memory plan = _refPlan();
        vm.expectRevert(Executor.NotDelegated.selector);
        liqModule.runLegs(address(debt), 0, 1, plan);
        vm.expectRevert(Executor.NotDelegated.selector);
        liqModule.govExec(0, 0, address(0));
        vm.expectRevert(Executor.NotDelegated.selector);
        swapModule.runSwaps(0, 1, plan);
    }

    /// Someone else's contract delegatecalling a module runs it on that
    /// contract's own state, outside any `execute` of ours: the transient
    /// `entered` flag it reads is unset there, and the module refuses.
    function test_modules_refuse_a_foreign_delegatecall() public {
        ForeignDelegator d = new ForeignDelegator();
        (bool ok, bytes memory ret) =
            d.run(address(liqModule), abi.encodeCall(ILiquidationModule.govExec, (0, 0, address(0))));
        assertFalse(ok);
        assertEq(bytes4(ret), Executor.NotDelegated.selector);
        (ok, ret) = d.run(address(swapModule), abi.encodeCall(ISwapModule.runSwaps, (0, 0, "")));
        assertFalse(ok);
        assertEq(bytes4(ret), Executor.NotDelegated.selector);
    }

    /// The core has no module entry point of its own: module code runs only
    /// through `execute`, even for an operator.
    function test_core_exposes_no_module_entry_point() public {
        bytes memory plan = _refPlan();
        vm.startPrank(operator);
        (bool ok,) = address(ex).call(abi.encodeCall(ILiquidationModule.runLegs, (address(debt), 0, 1, plan)));
        assertFalse(ok, "runLegs");
        (ok,) = address(ex).call(abi.encodeCall(ILiquidationModule.govExec, (0, 0, address(0))));
        assertFalse(ok, "govExec");
        (ok,) = address(ex).call(abi.encodeCall(ISwapModule.runSwaps, (0, 1, plan)));
        assertFalse(ok, "runSwaps");
        vm.stopPrank();
    }

    // ── which code the core runs ─────────────────────────────────────────

    function test_core_runs_the_modules_it_was_built_with() public view {
        assertEq(ex.LIQUIDATION_MODULE(), address(liqModule));
        assertEq(ex.SWAP_MODULE(), address(swapModule));
        assertEq(liqModule.MODULE_ID(), keccak256("liq-executor/liquidation-module/v1"));
        assertEq(swapModule.MODULE_ID(), keccak256("liq-executor/swap-module/v1"));
        assertEq(liqModule.WETH(), ex.WETH());
        assertEq(swapModule.WETH(), ex.WETH());
    }

    function test_constructor_refuses_an_address_without_code() public {
        bytes32 h = factory.initHash();
        address eoa = makeAddr("not-a-module");
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, eoa));
        new Executor(operator, backrunOperator, sink, address(weth), address(factory), h, eoa, address(swapModule));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, eoa));
        new Executor(operator, backrunOperator, sink, address(weth), address(factory), h, address(liqModule), eoa);
    }

    function test_constructor_refuses_swapped_modules() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, address(swapModule)));
        new Executor(
            operator, backrunOperator, sink, address(weth), address(factory), h,
            address(swapModule), address(liqModule)
        );
    }

    function test_constructor_refuses_a_contract_that_is_not_a_module() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, address(weth)));
        new Executor(operator, backrunOperator, sink, address(weth), address(factory), h, address(weth), address(swapModule));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, address(pool)));
        new Executor(operator, backrunOperator, sink, address(weth), address(factory), h, address(liqModule), address(pool));
    }

    /// A module built for other anchors (here another WETH) would measure
    /// profit in a token the core does not: refused at deploy.
    function test_constructor_refuses_a_module_built_for_another_weth() public {
        bytes32 h = factory.initHash();
        address otherWeth = makeAddr("other-weth");
        LiquidationModule otherLiq = new LiquidationModule(otherWeth);
        SwapModule otherSwaps = new SwapModule(
            otherWeth, address(routerA), address(routerB), v2Factory, V2_HASH, sushiFactory, SUSHI_HASH, address(curveRegistry)
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, address(otherLiq)));
        new Executor(operator, backrunOperator, sink, address(weth), address(factory), h, address(otherLiq), address(swapModule));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadModule.selector, address(otherSwaps)));
        new Executor(operator, backrunOperator, sink, address(weth), address(factory), h, address(liqModule), address(otherSwaps));
    }

    // ── what the single contract had ─────────────────────────────────────

    /// Every module error and event has the Executor's selector, so a revert
    /// or a log decodes against the Executor's ABI exactly as before.
    function test_module_errors_and_events_are_the_executors() public pure {
        assertEq(LiquidationModule.NotDelegated.selector, Executor.NotDelegated.selector);
        assertEq(LiquidationModule.ZeroAddress.selector, Executor.ZeroAddress.selector);
        assertEq(LiquidationModule.UnknownAdapter.selector, Executor.UnknownAdapter.selector);
        assertEq(LiquidationModule.LegMismatch.selector, Executor.LegMismatch.selector);
        assertEq(LiquidationModule.RedeemFailed.selector, Executor.RedeemFailed.selector);
        assertEq(LiquidationModule.SeizedBelowMin.selector, Executor.SeizedBelowMin.selector);
        assertEq(LiquidationModule.LegFailed.selector, Executor.LegFailed.selector);
        assertEq(LiquidationModule.GovExecSkipped.selector, Executor.GovExecSkipped.selector);
        assertEq(LiquidationModule.GovSpellSkipped.selector, Executor.GovSpellSkipped.selector);
        assertEq(SwapModule.NotDelegated.selector, Executor.NotDelegated.selector);
        assertEq(SwapModule.ZeroAddress.selector, Executor.ZeroAddress.selector);
        assertEq(SwapModule.UnknownVenue.selector, Executor.UnknownVenue.selector);
        assertEq(SwapModule.RouterNotAllowed.selector, Executor.RouterNotAllowed.selector);
        assertEq(SwapModule.RouterCallFailed.selector, Executor.RouterCallFailed.selector);
        assertEq(SwapModule.BadPool.selector, Executor.BadPool.selector);
        assertEq(SwapModule.ExactOutUnsupported.selector, Executor.ExactOutUnsupported.selector);
    }

    /// A skipped leg's `LegFailed` is logged at the Executor's address: the
    /// module emits as the Executor, so receipts read as before.
    function test_module_events_come_from_the_executor() public {
        address b2 = makeAddr("b2");
        pool.setPosition(b2, 1.5e18, REPAY, COLL_OUT); // healthy: someone got there first
        bytes memory legs = bytes.concat(
            PB.legV3(address(pool), b2, address(coll), REPAY),
            PB.legV3(address(pool), borrower, address(coll), REPAY)
        );
        vm.expectEmit(true, true, true, true, address(ex));
        emit Executor.LegFailed(PB.A_V3, address(pool), b2, 2, ""); // stage 2: not liquidatable
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 2, legs));
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    /// A wallet-funded `execute` keeps its `msg.value` through the
    /// delegatecalls that run in its own frame (a reward-only group and the
    /// profit swap): the modules accept it, the bid is paid from the
    /// ceiling and the rest goes back to the operator.
    function test_msg_value_passes_through_the_modules() public {
        MockTroveManager liquity = new MockTroveManager();
        liquity.setCollToken(address(coll));
        coll.mint(address(liquity), 1e12);
        uint256 trove = uint256(uint160(borrower));
        liquity.setTrove(trove, 1, COLL_OUT);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 10, GAS_COST, 0.1e18, 1),
            PB.groupHead(PB.P_NONE, address(0), address(debt), 0, 1, 0),
            PB.legLiquity(address(liquity), borrower, address(coll), 0, trove),
            PB.profit(1, PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0))
        );
        vm.deal(operator, 1 ether);
        vm.prank(operator);
        ex.execute{value: 1 ether}(plan);
        uint256 bid = coinbase.received();
        assertGt(bid, 0, "bid paid");
        assertLt(bid, 1 ether, "inside the ceiling");
        assertEq(operator.balance, 1 ether - bid, "unspent ceiling refunded");
        assertEq(address(ex).balance, 0);
        _assertClean();
    }

    /// EIP-170: each deployed contract's code fits 24,576 bytes.
    function test_each_contract_fits_eip170() public view {
        assertLe(address(ex).code.length, 24_576, "core");
        assertLe(address(liqModule).code.length, 24_576, "liquidation module");
        assertLe(address(swapModule).code.length, 24_576, "swap module");
    }
}
