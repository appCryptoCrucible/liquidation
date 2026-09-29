// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {ExecutorTestBase} from "./Base.sol";
import {MockFluidVault, MockERC20} from "./Mocks.sol";

/// Fluid leg, all four vault types (pin `9496626f`): the ABI each type is
/// called with, the one-token choice on each smart side, debt shares derived
/// from the repay, and native ETH unwrapped for `msg.value` and wrapped back.
contract ExecutorFluidTest is ExecutorTestBase {
    address constant NATIVE = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;
    MockERC20 other;

    function setUp() public override {
        super.setUp();
        other = new MockERC20("OTHER", 18);
        weth.mint(address(pool), 1e24);
    }

    function _vault(uint8 t) internal returns (MockFluidVault v) {
        v = new MockFluidVault(t);
        debt.mint(address(v), 1e15);
        coll.mint(address(v), 1e12);
        other.mint(address(v), 1e30);
    }

    function _run(MockFluidVault v, uint8 t, uint8 flags, uint256 colPer, uint256 debtPerShare, uint256 colPerShare)
        internal
    {
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1,
            PB.legFluidT(address(v), address(v), address(coll), REPAY, t, flags, colPer, debtPerShare, colPerShare)));
        _assertClean();
        assertEq(debt.allowance(address(ex), address(v)), 0, "vault allowance");
        assertEq(address(ex).balance, 0, "eth stuck");
    }

    function test_t1_liquidate_absorb_and_slippage_floor() public {
        MockFluidVault v = _vault(1);
        v.setTokens(address(debt), address(0), address(coll), address(0));
        v.setPosition(REPAY, COLL_OUT);
        uint256 colPer = uint256(COLL_OUT) * 1e18 / REPAY;
        _run(v, 1, PB.FL_ABSORB, colPer, 0, 0);
        assertEq(v.lastDebtArg(), REPAY);
        assertEq(v.lastColPer(), colPer);
        assertTrue(v.lastAbsorb());
        assertEq(v.lastTo(), address(ex));
    }

    function test_t1_absorb_off_when_flag_clear() public {
        MockFluidVault v = _vault(1);
        v.setTokens(address(debt), address(0), address(coll), address(0));
        v.setPosition(REPAY, COLL_OUT);
        _run(v, 1, 0, 1, 0, 0);
        assertFalse(v.lastAbsorb());
    }

    /// Smart collateral taken in token1 only: token0's per-share minimum is 0.
    function test_t2_takes_col_in_token1() public {
        MockFluidVault v = _vault(2);
        v.setTokens(address(debt), address(0), address(other), address(coll));
        v.setPosition(REPAY, COLL_OUT);            // col shares
        v.setRates(0, 0, 3e18, 1e18);              // 1 coll per share on token1
        uint256 colPer = uint256(COLL_OUT) * 1e18 / REPAY;
        _run(v, 2, PB.FL_COL1 | PB.FL_ABSORB, colPer, 0, 1e18);
        assertEq(v.lastDebtArg(), REPAY);
        assertEq(v.lastC0(), 0);
        assertEq(v.lastC1(), 1e18);
    }

    /// Smart debt repaid in token0 only: exactly the repay, and the shares it
    /// burns must reach repay · debtSharesMinPerToken / 1e18.
    function test_t3_repays_exact_token0_with_a_share_floor() public {
        MockFluidVault v = _vault(3);
        v.setTokens(address(debt), address(other), address(coll), address(0));
        v.setPosition(REPAY / 2, COLL_OUT);        // debt shares at 2 DEBT each
        v.setRates(2e18, 0, 0, 0);
        uint256 colPer = uint256(COLL_OUT) * 1e18 / (REPAY / 2);
        _run(v, 3, PB.FL_ABSORB, colPer, 0.5e18, 0);
        assertEq(v.lastT0(), REPAY);
        assertEq(v.lastT1(), 0);
        assertEq(v.lastSharesMin(), REPAY / 2);
        assertEq(v.lastPaid(), REPAY);
    }

    function test_t4_repays_token1_and_takes_token0() public {
        MockFluidVault v = _vault(4);
        v.setTokens(address(other), address(debt), address(coll), address(other));
        v.setPosition(REPAY / 2, COLL_OUT);        // debt shares, col shares
        v.setRates(0, 2e18, 1e18, 0);
        uint256 colPer = uint256(COLL_OUT) * 1e18 / (REPAY / 2);
        _run(v, 4, PB.FL_DEBT1 | PB.FL_ABSORB, colPer, 0.5e18, 1e18);
        assertEq(v.lastT0(), 0);
        assertEq(v.lastT1(), REPAY);
        assertEq(v.lastC0(), 1e18);
        assertEq(v.lastC1(), 0);
    }

    /// Native debt: the flash is WETH, unwrapped for `msg.value == repay`.
    function test_t1_native_debt_pays_eth_from_weth() public {
        MockFluidVault v = _vault(1);
        v.setTokens(NATIVE, address(0), address(coll), address(0));
        uint128 repayWeth = 10e18;                 // 0.5 COLL at 20 WETH
        v.setPosition(repayWeth, COLL_OUT);
        uint256 colPer = uint256(COLL_OUT) * 1e18 / repayWeth;
        uint128 owed = repayWeth + repayWeth * 5 / 10_000;
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(weth), repayWeth, 1, 1),
            PB.legFluidT(address(v), address(v), address(coll), repayWeth, 1, PB.FL_NATIVE_DEBT | PB.FL_ABSORB, colPer, 0, 0),
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_EXACT_OUT, owed),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        _assertClean();
        assertEq(v.lastValue(), repayWeth);
        assertEq(address(v).balance, repayWeth);
        assertEq(address(ex).balance, 0, "eth stuck");
    }

    /// Native collateral arrives as ETH and is wrapped before the swaps.
    function test_t1_native_collateral_is_wrapped() public {
        MockFluidVault v = _vault(1);
        v.setTokens(address(debt), address(0), NATIVE, address(0));
        uint256 colEth = 11e18;
        vm.deal(address(v), colEth);
        v.setPosition(REPAY, colEth);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legFluidT(address(v), address(v), address(weth), REPAY, 1, PB.FL_NATIVE_COL, colEth * 1e18 / REPAY, 0, 0),
            PB.poolSwap(address(pDebtWeth), address(weth), address(debt), PB.L_EXACT_OUT, OWED),
            PB.profit(0, "")
        );
        _exec(plan);
        _assertClean();
        assertEq(address(v).balance, 0);
        assertEq(address(ex).balance, 0, "eth stuck");
    }

    /// Native smart debt: exactly the repay in ETH, from WETH.
    function test_t3_native_debt_pays_exact_eth() public {
        MockFluidVault v = _vault(3);
        v.setTokens(NATIVE, address(other), address(coll), address(0));
        uint128 repayWeth = 10e18;
        v.setPosition(5e18, COLL_OUT);             // 5 shares at 2 ETH
        v.setRates(2e18, 0, 0, 0);
        uint128 owed = repayWeth + repayWeth * 5 / 10_000;
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(weth), repayWeth, 1, 1),
            PB.legFluidT(address(v), address(v), address(coll), repayWeth, 3, PB.FL_NATIVE_DEBT, 1, 0.5e18, 0),
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_EXACT_OUT, owed),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        _assertClean();
        assertEq(v.lastT0(), repayWeth);
        assertEq(v.lastValue(), repayWeth);
        assertEq(address(v).balance, repayWeth);
        assertEq(address(ex).balance, 0, "eth stuck");
    }

    function test_revert_fails_the_leg_and_clears_the_allowance() public {
        MockFluidVault v = _vault(1);
        v.setTokens(address(debt), address(0), address(coll), address(0));
        v.setPosition(REPAY, COLL_OUT);
        v.setRevert(true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1,
            PB.legFluidT(address(v), address(v), address(coll), REPAY, 1, 0, 1, 0, 0)));
    }

    function test_unknown_type_fails_the_leg() public {
        MockFluidVault v = _vault(1);
        v.setTokens(address(debt), address(0), address(coll), address(0));
        v.setPosition(REPAY, COLL_OUT);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1,
            PB.legFluidT(address(v), address(v), address(coll), REPAY, 5, 0, 1, 0, 0)));
    }

    /// A repay that buys fewer debt shares than the floor fails the leg.
    function test_t3_shares_below_floor_fail_the_leg() public {
        MockFluidVault v = _vault(3);
        v.setTokens(address(debt), address(other), address(coll), address(0));
        v.setPosition(REPAY, COLL_OUT);
        v.setRates(2e18, 0, 0, 0);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0, 1,
            PB.legFluidT(address(v), address(v), address(coll), REPAY, 3, 0, 1, 0.6e18, 0)));
    }
}
