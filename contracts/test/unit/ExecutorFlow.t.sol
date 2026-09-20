// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {stdError} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {PlanDecoder} from "../../src/lib/PlanDecoder.sol";
import {SafeTransfer} from "../../src/lib/SafeTransfer.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {ExecutorTestBase, OperatorRelay} from "./Base.sol";
import {
    MockERC20, MockUSDT, MockAavePool, MockUniV3Pool, MockRouter,
    SilentFlashProvider, HijackedFlashProvider, LyingFlashProvider, ReentrantFlashProvider, IRelay
} from "./Mocks.sol";

/// Aave V3 flash × Aave V3 adapter × UniV3-pool swaps: the reference path,
/// then every guard around it (GUIDE 10 Steps 2, 3, 5, 6, 7 and mutations).
contract ExecutorFlowTest is ExecutorTestBase {

    // ── happy path ────────────────────────────────────────────────────

    function test_constructor_rejects_zero_operator() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(address(0), sink, address(factory), h, address(routerA), address(routerB), address(weth));
    }
    function test_constructor_rejects_zero_sink() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, address(0), address(factory), h, address(routerA), address(routerB), address(weth));
    }
    function test_constructor_rejects_zero_factory() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(0), h, address(routerA), address(routerB), address(weth));
    }
    function test_constructor_rejects_zero_router_a() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(factory), h, address(0), address(routerB), address(weth));
    }
    function test_constructor_rejects_zero_router_b() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(factory), h, address(routerA), address(0), address(weth));
    }
    function test_constructor_rejects_zero_weth() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(factory), h, address(routerA), address(routerB), address(0));
    }

    function test_zero_address_router_target_reverts() public {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            PB.routerSwap(address(0), address(coll), address(debt), PB.L_EXACT_OUT, OWED, ""),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.RouterNotAllowed.selector, address(0)));
        _exec(plan);
    }

    function test_reference_liquidation_pays_gross_to_sink() public {
        _exec(_refPlan());
        assertEq(weth.balanceOf(sink), GROSS_WETH, "sink receives gross WETH");
        assertEq(pool.lastDebtToCover(), REPAY);
        assertEq(pool.lastCollateral(), address(coll));
        assertEq(pCollDebt.swaps(), 1); assertEq(pCollWeth.swaps(), 1);
        assertEq(debt.balanceOf(address(ex)), 0, "no debt dust: exact-out repay");
        _assertClean();
    }

    function test_no_sweep_flag_leaves_weth_for_permissionless_sweep() public {
        _exec(_plan(0, 0, GAS_COST, 0.9e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        assertEq(weth.balanceOf(address(ex)), GROSS_WETH);
        assertEq(weth.balanceOf(sink), 0);
        address[] memory a = new address[](1); a[0] = address(weth);
        vm.prank(stranger);
        ex.sweep(a);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    function test_sweep_destination_is_immutable_sink() public {
        coll.mint(address(ex), 123);
        address[] memory a = new address[](1); a[0] = address(coll);
        vm.prank(stranger);
        ex.sweep(a);
        assertEq(coll.balanceOf(sink), 123);
        assertEq(coll.balanceOf(stranger), 0);
    }

    // ── batching: per-leg tolerance, whole-plan floor ──────────────────

    function test_beaten_leg_is_skipped_and_batch_survives() public {
        address b2 = makeAddr("b2");
        pool.setPosition(b2, 1.5e18, REPAY, COLL_OUT); // healthy: someone got there first
        bytes memory legs = bytes.concat(
            PB.legV3(address(pool), b2, address(coll), REPAY),
            PB.legV3(address(pool), borrower, address(coll), REPAY)
        );
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 2, legs));
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        assertEq(pool.lastDebtToCover(), REPAY, "only the live leg was pulled");
        _assertClean();
    }

    function test_all_legs_beaten_reverts_whole_plan() public {
        pool.setPosition(borrower, 1.2e18, REPAY, COLL_OUT);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_refPlan());
    }

    function test_protocol_revert_is_caught_and_allowance_zeroed() public {
        MockAavePool broken = new MockAavePool();
        broken.setRevertOnLiquidate(true);
        broken.setPosition(borrower, 0.5e18, REPAY, COLL_OUT);
        bytes memory legs = bytes.concat(
            PB.legV3(address(broken), borrower, address(coll), REPAY),
            PB.legV3(address(pool), borrower, address(coll), REPAY)
        );
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 2, legs));
        assertEq(debt.allowance(address(ex), address(broken)), 0, "allowance zeroed after caught revert");
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    /// The protocol pulls less than approved (close-factor clamp): the
    /// remainder of the allowance must not survive the call.
    function test_clamped_pull_zeroes_residual_allowance() public {
        pool.setPosition(borrower, 0.95e18, 20_000e6, COLL_OUT);
        uint128 owed = 20_010e6;
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.6e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), 20_000e6, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), // asks 30k, gets 20k
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, owed),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(pool.lastDebtToCover(), 20_000e6, "protocol clamped the pull");
        // ceil(20_010e6·1e8/60_000e6) = 33_350_000 spent of the 0.55e8 seized
        assertEq(weth.balanceOf(sink), uint256(COLL_OUT - 33_350_000) * 2e11);
        assertEq(debt.balanceOf(address(ex)), 0);
        _assertClean();
    }

    /// Encoder sized the repay swap for a seizure the protocol did not give:
    /// the swap callback cannot pay and the whole plan reverts. No partial
    /// state, no stranded flash.
    function test_undersized_seizure_reverts_atomically() public {
        pool.setPosition(borrower, 0.95e18, REPAY, 0.3e8); // 30_000_000 < COLL_SPENT
        uint256 poolDebt = debt.balanceOf(address(pool));
        vm.expectRevert(abi.encodeWithSelector(
            SafeTransfer.TransferFailed.selector, address(coll), address(pCollDebt), COLL_SPENT
        ));
        _exec(_refPlan());
        assertEq(debt.balanceOf(address(pool)), poolDebt, "flash provider untouched");
    }

    // ── profit floor and bid ──────────────────────────────────────────

    function test_unprofitable_reverts_with_realized_numbers() public {
        vm.expectRevert(abi.encodeWithSelector(Executor.Unprofitable.selector, NET, 1e18));
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 1e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
    }

    function test_gas_cost_above_gross_underflows_and_reverts() public {
        vm.expectRevert(stdError.arithmeticError);
        _exec(_plan(PB.F_SWEEP, 0, 2e18, 0, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
    }

    function test_bid_is_fraction_of_realized_net_paid_from_weth() public {
        uint256 bid = uint256(NET) * 2500 / 10_000;
        _exec(_plan(PB.F_SWEEP, 2500, GAS_COST, 0.7e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        assertEq(coinbase.received(), bid, "coinbase paid (contract recipient, >2300 gas)");
        assertEq(weth.balanceOf(sink), GROSS_WETH - bid);
        assertEq(address(ex).balance, 0);
        _assertClean();
    }

    function test_bid_below_min_profit_reverts() public {
        uint256 bid = uint256(NET) * 5000 / 10_000;
        vm.expectRevert(abi.encodeWithSelector(Executor.Unprofitable.selector, NET - bid, 0.7e18));
        _exec(_plan(PB.F_SWEEP, 5000, GAS_COST, 0.7e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
    }

    function test_wallet_funded_bid_refunds_unspent_ceiling() public {
        uint256 bid = uint256(NET) * 2500 / 10_000;
        vm.deal(operator, 1 ether);
        vm.prank(operator);
        ex.execute{value: 1 ether}(_plan(PB.F_SWEEP, 2500, GAS_COST, 0.7e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        assertEq(coinbase.received(), bid);
        assertEq(operator.balance, 1 ether - bid, "unspent ceiling refunded");
        assertEq(weth.balanceOf(sink), GROSS_WETH, "WETH untouched when the wallet funds the bid");
        assertEq(address(ex).balance, 0);
    }

    function test_wallet_ceiling_caps_bid() public {
        uint256 bid = uint256(NET) * 2500 / 10_000;
        assertGt(bid, 0.1 ether);
        vm.deal(operator, 0.1 ether);
        vm.prank(operator);
        ex.execute{value: 0.1 ether}(_plan(PB.F_SWEEP, 2500, GAS_COST, 0.7e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        assertEq(coinbase.received(), 0.1 ether);
        assertEq(operator.balance, 0);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }

    function test_msg_value_refunded_when_bid_is_zero() public {
        vm.deal(operator, 1 ether);
        vm.prank(operator);
        ex.execute{value: 1 ether}(_refPlan());
        assertEq(operator.balance, 1 ether);
        assertEq(coinbase.received(), 0);
    }

    /// Mutation #11: a donation to `receive()` must never be bid away.
    function test_donated_eth_is_not_bid() public {
        vm.deal(address(ex), 5 ether);
        uint256 bid = uint256(NET) * 2500 / 10_000;
        _exec(_plan(PB.F_SWEEP, 2500, GAS_COST, 0.7e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        assertEq(coinbase.received(), bid);
        assertEq(address(ex).balance, 5 ether, "donation untouched");
    }

    // ── callback authentication ───────────────────────────────────────

    function test_callbacks_revert_outside_execute() public {
        vm.expectRevert(Executor.BadCallback.selector);
        ex.executeOperation(address(debt), 1, 0, address(ex), "");
        vm.expectRevert(Executor.BadCallback.selector);
        ex.uniswapV3FlashCallback(0, 0, "");
        vm.expectRevert(Executor.BadCallback.selector);
        ex.unlockCallback("");
        vm.expectRevert(Executor.BadCallback.selector);
        ex.onMorphoFlashLoan(1, "");
        vm.expectRevert(Executor.BadCallback.selector);
        ex.onFlashLoan(address(ex), address(debt), 1, 0, "");
        vm.expectRevert(Executor.BadSwapCallback.selector);
        ex.uniswapV3SwapCallback(1, -1, abi.encode(address(coll), address(debt), uint24(3000)));
    }

    function test_third_party_callback_mid_flash_reverts() public {
        HijackedFlashProvider hj = new HijackedFlashProvider();
        debt.mint(address(hj), REPAY);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(hj), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(Executor.BadCallback.selector);
        _exec(plan);
    }

    function test_provider_that_never_calls_back_reverts() public {
        SilentFlashProvider s = new SilentFlashProvider();
        debt.mint(address(s), REPAY);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(s), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.NoCallback.selector, uint8(0)));
        _exec(plan);
    }

    function test_callback_arguments_must_match_group() public {
        LyingFlashProvider ly = new LyingFlashProvider();
        debt.mint(address(ly), REPAY);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(ly), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(Executor.FlashMismatch.selector);
        _exec(plan);
    }

    function test_reentry_through_operator_reverts() public {
        // A fresh Executor whose operator is a relay the provider can call.
        OperatorRelay relay;
        Executor ex2;
        {
            // Resolve the circular dependency: the relay needs the Executor,
            // the Executor needs the relay as OPERATOR.
            address predicted = vm.computeCreateAddress(address(this), vm.getNonce(address(this)) + 1);
            relay = new OperatorRelay(Executor(payable(predicted)));
            ex2 = new Executor(address(relay), sink, address(factory), factory.initHash(),
                               address(routerA), address(routerB), address(weth));
            assertEq(address(ex2), predicted);
        }
        ReentrantFlashProvider re = new ReentrantFlashProvider(IRelay(address(relay)));
        debt.mint(address(re), REPAY);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(re), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(Executor.Reentrant.selector);
        relay.run(plan);
    }

    function test_swap_callback_requires_canonical_pool() public {
        // A pool the factory did not deploy for (coll, weth, 500) calls back
        // mid-swap: the CREATE2 derivation rejects it.
        vm.expectRevert(Executor.BadSwapCallback.selector);
        ex.uniswapV3SwapCallback(1, -1, abi.encode(address(coll), address(weth), uint24(500)));
    }

    // ── plan-level guards ─────────────────────────────────────────────

    function test_not_operator() public {
        vm.expectRevert(Executor.NotOperator.selector);
        vm.prank(stranger);
        ex.execute(_refPlan());
    }

    function test_zero_groups_rejected_before_any_call() public {
        vm.expectRevert(PlanDecoder.NoGroups.selector);
        _exec(bytes.concat(PB.header(0, 0, 0, 0, 0), PB.profit(0, "")));
    }

    function test_unknown_adapter_rejected_at_decode() public {
        bytes memory bad = PB.legV3(address(pool), borrower, address(coll), REPAY);
        bad[0] = bytes1(uint8(7));
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.UnknownAdapter.selector, uint8(7)));
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, bad));
    }

    function test_unknown_provider_rejected() public {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(9, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.UnknownProvider.selector, uint8(9)));
        _exec(plan);
    }

    function test_group_without_legs_rejected() public {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 0, 0),
            PB.profit(0, "")
        );
        vm.expectRevert(Executor.NoLegs.selector);
        _exec(plan);
    }

    function test_trailing_bytes_rejected() public {
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.BadPlanLength.selector, _refPlan().length, _refPlan().length + 1));
        _exec(bytes.concat(_refPlan(), hex"00"));
    }

    // ── swap venues ───────────────────────────────────────────────────

    function test_router_leg_exact_allowance_then_zeroed() public {
        routerA.setPullBps(5000); // consumes only half of what was approved
        bytes memory profitLeg = PB.routerSwap(
            address(routerA), address(coll), address(weth), PB.L_TAKE_BALANCE, 0,
            abi.encodeCall(MockRouter.swapExact, (address(coll), address(weth), COLL_LEFT, GROSS_WETH))
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, profitLeg)
        );
        _exec(plan);
        assertEq(coll.allowance(address(ex), address(routerA)), 0, "no standing allowance");
        assertEq(coll.balanceOf(address(ex)), COLL_LEFT / 2, "unconsumed input stays (sweepable)");
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }

    function test_router_not_allowlisted_reverts() public {
        bytes memory profitLeg = PB.routerSwap(
            stranger, address(coll), address(weth), PB.L_TAKE_BALANCE, 0,
            abi.encodeCall(MockRouter.swapExact, (address(coll), address(weth), COLL_LEFT, GROSS_WETH))
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, profitLeg)
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.RouterNotAllowed.selector, stranger));
        _exec(plan);
    }

    function test_router_failure_reverts_plan() public {
        bytes memory profitLeg = PB.routerSwap(
            address(routerB), address(coll), address(weth), PB.L_TAKE_BALANCE, 0,
            abi.encodeWithSignature("doesNotExist()")
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, profitLeg)
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.RouterCallFailed.selector, address(routerB)));
        _exec(plan);
    }

    function test_unknown_venue_reverts() public {
        bytes memory profitLeg = PB.swap(5, address(coll), address(weth), PB.L_TAKE_BALANCE, 0, abi.encodePacked(address(pCollWeth)));
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(1, profitLeg)
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.UnknownVenue.selector, uint8(5)));
        _exec(plan);
    }

    /// A swap leg whose input balance is zero (its liquidation leg was
    /// beaten) is skipped, not an error.
    function test_zero_balance_swap_leg_skipped() public {
        MockERC20 other = new MockERC20("OTHER", 18);
        MockUniV3Pool pOther = MockUniV3Pool(factory.deploy(address(other), address(weth), 3000));
        bytes memory profit = bytes.concat(
            PB.poolSwap(address(pOther), address(other), address(weth), PB.L_TAKE_BALANCE, 0),
            _profitLeg()
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY), _repayLeg(),
            PB.profit(2, profit)
        );
        _exec(plan);
        assertEq(pOther.swaps(), 0);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }

    // ── USDT semantics ────────────────────────────────────────────────

    function test_usdt_debt_asset_round_trip() public {
        MockUSDT usdt = new MockUSDT();
        MockUniV3Pool pCollUsdt = MockUniV3Pool(factory.deploy(address(coll), address(usdt), 3000));
        _price(pCollUsdt, address(coll), address(usdt), 600, 1);
        usdt.mint(address(pool), 1e15); usdt.mint(address(pCollUsdt), 1e15);
        // Pre-existing non-zero allowance: the naive approve would revert.
        vm.prank(address(ex)); usdt.approve(address(pool), 1);

        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(usdt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            PB.poolSwap(address(pCollUsdt), address(coll), address(usdt), PB.L_EXACT_OUT, OWED),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        assertEq(usdt.allowance(address(ex), address(pool)), 0);
        assertEq(usdt.balanceOf(address(ex)), 0);
    }
}
