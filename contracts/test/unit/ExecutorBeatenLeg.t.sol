// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "../../src/lib/SafeTransfer.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {ExecutorTestBase} from "./Base.sol";
import {MockERC20, MockUniV3Pool} from "./Mocks.sol";

/// Per-leg tolerance on plans shaped the way the off-chain assembler shapes
/// them (`liq-router` `assemble`): the flash is the sum of the group's
/// pulls, and each liquidation leg has its own EXACT_OUT repay swap for its
/// own pull, tied to it (swap-leg flags bits 2–7). A competitor takes one
/// position between simulation and inclusion; the other must still land.
/// The beaten leg's swap is skipped, and the flash fee, which the Executor
/// adds to the first exact-output pool leg that runs, is bought by the
/// live leg's (GUIDE 12 "per-leg tolerance"; `SwapModule.runSwaps`).
///
/// Oracle: the mocks' fixed rates (1 COLL = 60_000 DEBT = 20 WETH, an
/// exact output's input rounded up as the mock pool rounds it) and the Aave
/// mock's premium (5 bps unless set), worked by hand in each test.
contract ExecutorBeatenLegTest is ExecutorTestBase {
    uint128 constant FLASH   = 2 * REPAY;              // both legs' pulls
    uint128 constant PREMIUM = FLASH * 5 / 10_000;     // 30e6

    MockERC20 other;
    MockUniV3Pool pOtherDebt;
    MockUniV3Pool pOtherWeth;
    address b0 = makeAddr("b0");

    function setUp() public override {
        super.setUp();
        // OTHER: a second collateral at COLL's rates, with its own pools.
        other = new MockERC20("OTHER", 8);
        pOtherDebt = MockUniV3Pool(factory.deploy(address(other), address(debt), 3000));
        pOtherWeth = MockUniV3Pool(factory.deploy(address(other), address(weth), 3000));
        _price(pOtherDebt, address(other), address(debt), 600, 1);
        _price(pOtherWeth, address(other), address(weth), 2e11, 1);
        debt.mint(address(pOtherDebt), 1e15);
        weth.mint(address(pOtherWeth), 1e24);
        other.mint(address(pool), 1e12);
    }

    /// Leg 0 is `b0` on `c0` (repaid through `p0`), leg 1 the reference
    /// borrower on COLL; one Aave flash of both pulls. Each repay swap buys
    /// its own leg's pull, tied to it or not.
    function _plan(address c0, MockUniV3Pool p0, bool tied) internal view returns (bytes memory) {
        uint8 f0 = tied ? PB.tie(PB.L_EXACT_OUT, 0) : PB.L_EXACT_OUT;
        uint8 f1 = tied ? PB.tie(PB.L_EXACT_OUT, 1) : PB.L_EXACT_OUT;
        bytes memory profits = c0 == address(coll)
            ? PB.profit(1, _profitLeg())
            : PB.profit(2, bytes.concat(
                PB.poolSwap(address(pOtherWeth), address(other), address(weth), PB.L_TAKE_BALANCE, 0),
                _profitLeg()
            ));
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.1e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), FLASH, 2, 2),
            PB.legV3(address(pool), b0, c0, REPAY),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            PB.poolSwap(address(p0), c0, address(debt), f0, REPAY),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), f1, REPAY),
            profits
        );
    }

    /// Two positions on one collateral; the first is beaten. Leg 1's swap
    /// buys its 30_000 pull and the 30 premium on the 60_000 flash: 30_030
    /// DEBT for exactly 0.5005 COLL. 0.0495 COLL is left: 0.99 WETH.
    function test_same_collateral_first_leg_beaten_the_live_leg_lands() public {
        pool.setPosition(b0, 1.5e18, REPAY, COLL_OUT); // healthy: someone got there first
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(_plan(address(coll), pCollDebt, true));
        assertEq(pool.hf(borrower), 2e18, "the live position was liquidated");
        assertEq(pool.hf(b0), 1.5e18, "the beaten one untouched");
        assertEq(pCollDebt.swaps(), 1, "the beaten leg's swap skipped");
        // Lent 60_000; paid 30_000 by the liquidation, then 60_030.
        assertEq(debt.balanceOf(address(pool)), poolDebt + REPAY + PREMIUM, "flash repaid with its premium");
        assertEq(debt.balanceOf(address(ex)), 0, "nothing bought beyond what was owed");
        assertEq(weth.balanceOf(sink), 0.99e18);
        _assertClean();
    }

    /// The same with the second leg beaten: the first leg's swap runs first
    /// and buys the premium. Same outcome.
    function test_same_collateral_second_leg_beaten_the_live_leg_lands() public {
        pool.setPosition(b0, 0.95e18, REPAY, COLL_OUT);
        pool.setPosition(borrower, 1.5e18, REPAY, COLL_OUT);
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(_plan(address(coll), pCollDebt, true));
        assertEq(pool.hf(b0), 2e18, "the live position was liquidated");
        assertEq(pool.hf(borrower), 1.5e18, "the beaten one untouched");
        assertEq(pCollDebt.swaps(), 1, "the beaten leg's swap skipped");
        assertEq(debt.balanceOf(address(pool)), poolDebt + REPAY + PREMIUM, "flash repaid with its premium");
        assertEq(debt.balanceOf(address(ex)), 0);
        assertEq(weth.balanceOf(sink), 0.99e18);
        _assertClean();
    }

    /// Both live: the swaps buy 30_030 (with the premium) and 30_000, for
    /// 0.5005 and 0.5 COLL of the 1.1 seized. 0.0995 COLL is left: 1.99 WETH.
    function test_same_collateral_both_live_each_swap_buys_its_pull() public {
        pool.setPosition(b0, 0.95e18, REPAY, COLL_OUT);
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(_plan(address(coll), pCollDebt, true));
        assertEq(pool.hf(b0), 2e18);
        assertEq(pool.hf(borrower), 2e18);
        assertEq(pCollDebt.swaps(), 2);
        assertEq(debt.balanceOf(address(pool)), poolDebt + 2 * REPAY + PREMIUM, "flash repaid with its premium");
        assertEq(debt.balanceOf(address(ex)), 0);
        assertEq(weth.balanceOf(sink), 1.99e18);
        _assertClean();
    }

    /// Two collaterals; the first is beaten, so OTHER never arrives and its
    /// swap is skipped. COLL buys the pull and the premium as above.
    function test_two_collaterals_first_leg_beaten_the_live_leg_lands() public {
        pool.setPosition(b0, 1.5e18, REPAY, COLL_OUT);
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(_plan(address(other), pOtherDebt, true));
        assertEq(pool.hf(borrower), 2e18, "the live position was liquidated");
        assertEq(pOtherDebt.swaps(), 0, "the beaten leg's swap skipped");
        assertEq(pOtherWeth.swaps(), 0, "and no OTHER to close");
        assertEq(pCollDebt.swaps(), 1);
        assertEq(debt.balanceOf(address(pool)), poolDebt + REPAY + PREMIUM, "flash repaid with its premium");
        assertEq(weth.balanceOf(sink), 0.99e18);
        _assertClean();
    }

    /// Both live on two collaterals: OTHER buys 30_030 for 0.5005 and closes
    /// 0.0495 (0.99 WETH); COLL buys 30_000 for 0.5 and closes 0.05 (1 WETH).
    function test_two_collaterals_both_live() public {
        pool.setPosition(b0, 0.95e18, REPAY, COLL_OUT);
        _exec(_plan(address(other), pOtherDebt, true));
        assertEq(pool.hf(b0), 2e18);
        assertEq(pool.hf(borrower), 2e18);
        assertEq(other.balanceOf(address(ex)), 0, "OTHER closed");
        assertEq(weth.balanceOf(sink), 1.99e18);
        _assertClean();
    }

    /// Untied, the beaten leg's swap still runs: it takes 0.5005 COLL for
    /// the first exact output, and the live leg's swap cannot pay its 0.5
    /// from the 0.0495 left. The tie decides, not a balance heuristic.
    function test_untied_swaps_still_revert_on_a_beaten_leg() public {
        pool.setPosition(b0, 1.5e18, REPAY, COLL_OUT);
        vm.expectRevert(abi.encodeWithSelector(
            SafeTransfer.TransferFailed.selector, address(coll), address(pCollDebt), 50_000_000
        ));
        _exec(_plan(address(coll), pCollDebt, false));
    }

    /// The premium the provider charges at inclusion is the one bought, not
    /// the one simulated: at 9 bps the reference swap buys 30_027 DEBT for
    /// exactly 0.50045 COLL, and 0.04955 COLL is left: 0.991 WETH.
    function test_a_fee_that_moved_after_simulation_is_bought_as_charged() public {
        pool.setPremiumBps(9);
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(_refPlan());
        assertEq(debt.balanceOf(address(pool)), poolDebt + REPAY + 27e6, "flash repaid at 9 bps");
        assertEq(debt.balanceOf(address(ex)), 0);
        assertEq(weth.balanceOf(sink), 0.991e18);
        _assertClean();
    }
}
