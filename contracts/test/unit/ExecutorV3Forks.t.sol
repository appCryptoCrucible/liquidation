// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {ExecutorTestBase} from "./Base.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {MockForkPool, MockUniV3Pool} from "./Mocks.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";

/// Pool-direct legs on SushiSwap V3 (factory id 1) and PancakeSwap V3 (id 2).
/// The anchors are mainnet constants, so each pool is a mock etched at the
/// address `MainnetVenues.v3ForkPool` derives for it: the Executor's
/// CREATE2 authentication then runs as on mainnet. The refusals prove a pool
/// is accepted only under the id of the deployer that derives its address,
/// whatever it calls back as, and whatever data it hands back.
contract ExecutorV3ForksTest is ExecutorTestBase {
    uint128 constant MIN_PROFIT = 0.5e18;
    uint24 constant FEE = 3000;
    uint8 constant SUSHI = 1;
    uint8 constant PANCAKE = 2;

    /// A fork pool for (a, b, FEE) at the address deployer `fid` derives;
    /// `price`: raw `b` per raw `a`, num/den.
    function _fork(uint8 fid, bool pancakeName, address a, address b, uint256 num, uint256 den)
        internal returns (MockForkPool p)
    {
        address at = MainnetVenues.v3ForkPool(fid, a, b, FEE);
        vm.etch(at, address(new MockForkPool()).code);
        p = MockForkPool(at);
        p.init(a, b, FEE, pancakeName);
        if (p.token0() == a) p.setRate(num, den); else p.setRate(den, num);
    }

    /// COLL/DEBT at 1 COLL = 60_000 DEBT, funded to pay out.
    function _collDebt(uint8 fid, bool pancakeName) internal returns (MockForkPool p) {
        p = _fork(fid, pancakeName, address(coll), address(debt), 600, 1);
        debt.mint(address(p), 1e15);
    }

    function _planWith(bytes memory repay, uint8 profitCount, bytes memory profitLegs)
        internal view returns (bytes memory)
    {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, MIN_PROFIT, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            repay,
            PB.profit(profitCount, profitLegs)
        );
    }

    function _repay(address pl, uint8 fid) internal view returns (bytes memory) {
        return PB.forkSwap(pl, fid, address(coll), address(debt), PB.L_EXACT_OUT, REPAY);
    }

    // ── success ───────────────────────────────────────────────────────────

    /// Oracle: the reference liquidation's own arithmetic (Base.sol) — the
    /// repay buys exactly the pull plus the flash premium and leaves the
    /// rest of the collateral to the profit leg, so the sink gets the same
    /// WETH as with a Uniswap pool (`test_an_explicit_zero_id_is_uniswaps` is
    /// that control); nothing stays in the Executor.
    function _assertRepaysLikeUniswap(MockForkPool p) internal {
        uint256 sinkBefore = weth.balanceOf(sink);
        uint256 collBefore = coll.balanceOf(address(p));
        _exec(_planWith(_repay(address(p), p.pancakeName() ? PANCAKE : SUSHI), 1, _profitLeg()));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH, "the sink gets the reference gross");
        assertEq(coll.balanceOf(address(p)) - collBefore, COLL_SPENT, "the pool took exactly what buys the debt");
        assertEq(p.swaps(), 1, "one swap");
        assertEq(debt.balanceOf(address(ex)), 0, "exact-out bought the pull and the premium, no more");
        _assertClean();
    }

    function test_sushi_pool_exact_out_repay() public {
        _assertRepaysLikeUniswap(_collDebt(SUSHI, false));
    }

    function test_pancake_pool_exact_out_repay() public {
        _assertRepaysLikeUniswap(_collDebt(PANCAKE, true));
    }

    /// The profit leg sells the leftover collateral exact-in (TAKE_BALANCE)
    /// through a fork pool: the same WETH the Uniswap pool would pay.
    function _assertSellsLikeUniswap(uint8 fid, bool pancakeName) internal {
        MockForkPool cw = _fork(fid, pancakeName, address(coll), address(weth), 2e11, 1);
        weth.mint(address(cw), 1e24);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1,
            PB.forkSwap(address(cw), fid, address(coll), address(weth), PB.L_TAKE_BALANCE, 0)
        ));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH, "the leftover sold at the pool rate");
        assertEq(cw.swaps(), 1);
        _assertClean();
    }

    function test_sushi_pool_exact_in_sale_of_the_leftover() public {
        _assertSellsLikeUniswap(SUSHI, false);
    }

    function test_pancake_pool_exact_in_sale_of_the_leftover() public {
        _assertSellsLikeUniswap(PANCAKE, true);
    }

    /// An explicit 0 is Uniswap's id: the same pool, the same triple.
    function test_an_explicit_zero_id_is_uniswaps() public {
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(_repay(address(pCollDebt), 0), 1, _profitLeg()));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH);
        _assertClean();
    }

    /// The callback's name decides nothing: a pool is accepted by the
    /// address its deployer derives for the triple it was handed, and a
    /// genuine pool calls back under its own name. Pinned so changing it is
    /// a decision, not an accident.
    function test_the_callback_name_does_not_decide() public {
        MockForkPool p = _collDebt(PANCAKE, true);
        p.setCrossName(true); // a Pancake-derived pool calling uniswapV3SwapCallback
        _exec(_planWith(_repay(address(p), PANCAKE), 1, _profitLeg()));
        assertEq(p.swaps(), 1);
        _assertClean();
    }

    // ── refusals ──────────────────────────────────────────────────────────

    /// A Sushi pool named as Pancake's (and the reverse): the callback derives
    /// the other deployer's address for the triple and the caller is not it.
    function test_a_pool_named_with_the_other_forks_id_is_refused() public {
        MockForkPool s = _collDebt(SUSHI, false);
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(s), PANCAKE), 1, _profitLeg()));

        MockForkPool c = _collDebt(PANCAKE, true);
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(c), SUSHI), 1, _profitLeg()));
    }

    /// A Uniswap pool named as a fork's: it calls back with the fork's id and
    /// the fork's deployer derives another address.
    function test_a_uniswap_pool_named_as_a_fork_is_refused() public {
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(pCollDebt), SUSHI), 1, _profitLeg()));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(pCollDebt), PANCAKE), 1, _profitLeg()));
    }

    /// A fork pool named with no id is taken for Uniswap's, whose factory
    /// derives another address for it.
    function test_a_fork_pool_without_an_id_is_refused() public {
        MockForkPool s = _collDebt(SUSHI, false);
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(PB.poolSwap(address(s), address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        MockForkPool c = _collDebt(PANCAKE, true);
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(PB.poolSwap(address(c), address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
    }

    /// Any contract at any other address, whatever id it is named with.
    function test_an_arbitrary_contract_as_a_fork_pool_is_refused() public {
        MockForkPool rogue = new MockForkPool();
        rogue.init(address(coll), address(debt), FEE, true);
        rogue.setRate(600, 1);
        debt.mint(address(rogue), 1e15);
        for (uint8 fid = SUSHI; fid <= PANCAKE; fid++) {
            vm.expectRevert(Executor.BadSwapCallback.selector);
            _exec(_planWith(_repay(address(rogue), fid), 1, _profitLeg()));
        }
    }

    /// A genuine pool of the named deployer that hands back a callback built
    /// for another pair, fee or id is not the pool of that triple.
    function test_a_forged_callback_triple_is_refused() public {
        MockForkPool p = _collDebt(SUSHI, false);
        // Another pair (COLL/WETH), same fee and id.
        p.setForged(abi.encode(address(coll), address(weth), FEE, SUSHI));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(p), SUSHI), 1, _profitLeg()));
        // Another fee tier.
        p.setForged(abi.encode(address(coll), address(debt), uint24(500), SUSHI));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(p), SUSHI), 1, _profitLeg()));
        // Another id.
        p.setForged(abi.encode(address(coll), address(debt), FEE, PANCAKE));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(p), SUSHI), 1, _profitLeg()));
        // An id no deployer has (derives the zero address).
        p.setForged(abi.encode(address(coll), address(debt), FEE, uint8(0x7f)));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        _exec(_planWith(_repay(address(p), SUSHI), 1, _profitLeg()));
    }

    /// The leg itself refuses an id past Pancake's, and data that is neither
    /// the pool nor the pool and one id byte.
    function test_unknown_ids_and_malformed_leg_data_are_refused() public {
        MockForkPool p = _collDebt(SUSHI, false);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(0), address(p)));
        _exec(_planWith(_repay(address(p), 3), 1, _profitLeg()));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(0), address(p)));
        _exec(_planWith(_repay(address(p), 255), 1, _profitLeg()));
        // 22 bytes: a stray second byte.
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(0), address(0)));
        _exec(_planWith(
            PB.swap(0, address(coll), address(debt), PB.L_EXACT_OUT, REPAY, abi.encodePacked(address(p), SUSHI, uint8(0))),
            1, _profitLeg()
        ));
        // 19 bytes: a truncated address.
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(0), address(0)));
        _exec(_planWith(
            PB.swap(0, address(coll), address(debt), PB.L_EXACT_OUT, REPAY, abi.encodePacked(bytes19(bytes20(address(p))))),
            1, _profitLeg()
        ));
    }

    // ── the callback entry points ─────────────────────────────────────────

    /// Outside a swap leg Pancake's callback has no flash-swap meaning at
    /// all: it reverts, from anyone, with any data.
    function test_pancake_callback_outside_a_swap_leg_is_refused() public {
        MockForkPool p = _collDebt(PANCAKE, true);
        bytes memory triple = abi.encode(address(coll), address(debt), FEE, PANCAKE);
        // Even the genuine pool's address, called by hand rather than by a swap.
        vm.prank(address(p));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        ex.pancakeV3SwapCallback(int256(1), int256(-1), triple);
        vm.prank(stranger);
        vm.expectRevert(Executor.BadSwapCallback.selector);
        ex.pancakeV3SwapCallback(int256(1), int256(-1), "");
    }

    /// The same for the Uniswap name outside a leg and outside a flash swap.
    function test_uniswap_callback_outside_a_swap_leg_is_still_refused() public {
        MockForkPool p = _collDebt(SUSHI, false);
        vm.prank(address(p));
        vm.expectRevert(Executor.BadSwapCallback.selector);
        ex.uniswapV3SwapCallback(int256(1), int256(-1), abi.encode(address(coll), address(debt), FEE, SUSHI));
    }
}
