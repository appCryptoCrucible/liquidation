// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanDecoder} from "../../src/lib/PlanDecoder.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {ExecutorTestBase} from "./Base.sol";
import {
    MockEulerVault, MockSiloHook, MockTroveManager, MockFluidT1,
    MockCreditFacade, MockComptroller, MockCErc20
} from "./Mocks.sol";

/// 10E dispatch / approve / zero-allowance / unknown-adapter. V3/V4/Morpho
/// still decode. Does not invent live protocol state — mock counterparties
/// only, matching the existing 10A unit pattern.
contract ExecutorAdapters10ETest is ExecutorTestBase {
    MockEulerVault euler;
    MockSiloHook silo;
    MockTroveManager liquity;
    MockFluidT1 fluid;
    MockCreditFacade gearbox;
    MockComptroller comptroller;
    MockCErc20 cDebt;

    function setUp() public override {
        super.setUp();
        euler = new MockEulerVault();
        silo = new MockSiloHook();
        liquity = new MockTroveManager();
        fluid = new MockFluidT1();
        gearbox = new MockCreditFacade();
        comptroller = new MockComptroller();
        cDebt = new MockCErc20(comptroller);

        euler.setDebtToken(address(debt));
        silo.setDebtToken(address(debt));
        fluid.setDebtToken(address(debt));
        fluid.setCollToken(address(coll));
        gearbox.setDebtToken(address(debt));
        cDebt.setDebtToken(address(debt));
        liquity.setCollToken(address(coll));

        debt.mint(address(euler), 1e15);
        debt.mint(address(silo), 1e15);
        debt.mint(address(fluid), 1e15);
        debt.mint(address(gearbox), 1e15);
        debt.mint(address(cDebt), 1e15);
        coll.mint(address(euler), 1e12);
        coll.mint(address(silo), 1e12);
        coll.mint(address(liquity), 1e12);
        coll.mint(address(fluid), 1e12);
        coll.mint(address(gearbox), 1e12);
        coll.mint(address(cDebt), 1e12);

        euler.setPosition(borrower, REPAY, COLL_OUT);
        silo.setPosition(borrower, REPAY, COLL_OUT);
        liquity.setTrove(uint256(uint160(borrower)), 1, COLL_OUT);
        fluid.setPosition(address(fluid), REPAY, COLL_OUT);
        gearbox.setPosition(borrower, REPAY, COLL_OUT);
        cDebt.setPosition(borrower, REPAY, COLL_OUT);
        comptroller.setShortfall(borrower, 1);
    }

    function _execLeg(bytes memory leg) internal {
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1, leg));
        _assertClean();
        assertEq(debt.allowance(address(ex), address(euler)), 0);
        assertEq(debt.allowance(address(ex), address(silo)), 0);
        assertEq(debt.allowance(address(ex), address(fluid)), 0);
        assertEq(debt.allowance(address(ex), address(gearbox)), 0);
        assertEq(debt.allowance(address(ex), address(cDebt)), 0);
    }

    function test_v3_v4_morpho_still_decode() public pure {
        assertEq(PlanDecoder.A_AAVE_V3, 0);
        assertEq(PlanDecoder.A_AAVE_V4, 1);
        assertEq(PlanDecoder.A_MORPHO, 2);
        assertEq(PlanDecoder.tailLen(0), 0);
        assertEq(PlanDecoder.tailLen(1), 4);
        assertEq(PlanDecoder.tailLen(2), 32);
    }

    function test_unknown_adapter_9_reverts_at_decode() public {
        bytes memory bad = PB.legV3(address(pool), borrower, address(coll), REPAY);
        bad[0] = bytes1(uint8(9));
        vm.expectRevert(abi.encodeWithSelector(PlanDecoder.UnknownAdapter.selector, uint8(9)));
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, bad));
    }

    function test_euler_dispatch_approve_zero() public {
        _execLeg(PB.legEuler(address(euler), borrower, address(coll), REPAY, 1));
        assertEq(euler.lastViolator(), borrower);
        assertEq(euler.lastMinYield(), 1);
    }

    function test_euler_guard_and_revert_zero_allowance() public {
        euler.setPosition(borrower, 0, 0);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, PB.legEuler(address(euler), borrower, address(coll), REPAY, 1)));
        assertEq(debt.allowance(address(ex), address(euler)), 0);

        euler.setPosition(borrower, REPAY, COLL_OUT);
        euler.setRevertOnLiquidate(true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, PB.legEuler(address(euler), borrower, address(coll), REPAY, 1)));
        assertEq(debt.allowance(address(ex), address(euler)), 0);
    }

    function test_silo_dispatch_approve_zero() public {
        _execLeg(PB.legSilo(address(silo), borrower, address(coll), REPAY));
        assertEq(silo.lastBorrower(), borrower);
        assertEq(silo.lastReceiveS(), false);
    }

    function test_silo_reject_zeros_allowance() public {
        silo.setRevertOnLiquidate(true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, PB.legSilo(address(silo), borrower, address(coll), REPAY)));
        assertEq(debt.allowance(address(ex), address(silo)), 0);
    }

    function test_liquity_dispatch_and_nothing_to_liquidate() public {
        _execLeg(PB.legLiquity(address(liquity), borrower, address(coll), REPAY, uint256(uint160(borrower))));
        assertEq(liquity.lastId(), uint256(uint160(borrower)));

        liquity.setTrove(uint256(uint160(borrower)), 2, COLL_OUT); // closed by owner
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(
            PB.F_SWEEP, 0, GAS_COST, 0, 1,
            PB.legLiquity(address(liquity), borrower, address(coll), REPAY, uint256(uint160(borrower)))
        ));
    }

    function test_fluid_dispatch_approve_zero() public {
        _execLeg(PB.legFluid(address(fluid), address(fluid), address(coll), REPAY, 1e27));
        assertEq(fluid.lastAbsorb(), true);
        assertEq(fluid.lastTo(), address(ex));
        assertEq(fluid.lastColPer(), 1e27);
    }

    function test_fluid_reject_zeros_allowance() public {
        fluid.setRevertOnLiquidate(true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, PB.legFluid(address(fluid), address(fluid), address(coll), REPAY, 1e27)));
        assertEq(debt.allowance(address(ex), address(fluid)), 0);
    }

    function test_gearbox_dispatch_approve_zero() public {
        _execLeg(PB.legGearbox(address(gearbox), borrower, address(coll), REPAY, 1));
        assertEq(gearbox.lastAccount(), borrower);
        assertEq(gearbox.lastTo(), address(ex));
    }

    function test_gearbox_reject_zeros_allowance() public {
        gearbox.setRevertOnLiquidate(true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1, PB.legGearbox(address(gearbox), borrower, address(coll), REPAY, 1)));
        assertEq(debt.allowance(address(ex), address(gearbox)), 0);
    }

    function test_compound_dispatch_approve_zero() public {
        _execLeg(PB.legCompound(address(cDebt), borrower, address(coll), REPAY, address(coll), 0));
        assertEq(cDebt.lastBorrower(), borrower);
        assertEq(cDebt.lastCColl(), address(coll));
    }

    function test_compound_healthy_skips_and_revert_zeros_allowance() public {
        comptroller.setShortfall(borrower, 0);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(
            PB.F_SWEEP, 0, GAS_COST, 0, 1,
            PB.legCompound(address(cDebt), borrower, address(coll), REPAY, address(coll), 0)
        ));
        assertEq(debt.allowance(address(ex), address(cDebt)), 0);

        comptroller.setShortfall(borrower, 1);
        cDebt.setRevertOnLiquidate(true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(
            PB.F_SWEEP, 0, GAS_COST, 0, 1,
            PB.legCompound(address(cDebt), borrower, address(coll), REPAY, address(coll), 0)
        ));
        assertEq(debt.allowance(address(ex), address(cDebt)), 0);
    }
}
