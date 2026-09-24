// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {ExecutorTestBase} from "./Base.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {MockV2Pair, MockCurvePool} from "./Mocks.sol";

/// Pool-direct UniswapV2 / SushiSwap and Curve legs. The success paths run
/// the reference liquidation with the repay swap on the new venue; the
/// refusals prove a plan cannot send funds to a pool the Executor has not
/// verified on chain (CREATE2 for V2, MetaRegistry + coin indices for Curve).
contract ExecutorVenuesTest is ExecutorTestBase {
    uint128 constant MIN_PROFIT = 0.5e18;

    /// A V2 pair at the address `factory_` derives for (COLL, DEBT), priced
    /// at 1 COLL = 60_000 DEBT with deep reserves.
    function _pair(address factory_, bytes32 hash_) internal returns (MockV2Pair p) {
        address a = _v2PairAddr(factory_, hash_, address(coll), address(debt));
        vm.etch(a, address(new MockV2Pair()).code);
        p = MockV2Pair(a);
        p.init(address(coll), address(debt));
        coll.mint(a, 1_000e8);
        debt.mint(a, 60_000_000e6);
        p.sync();
    }

    function _curve(bool register) internal returns (MockCurvePool c) {
        address[] memory coins = new address[](2);
        coins[0] = address(coll);
        coins[1] = address(debt);
        c = new MockCurvePool(coins);
        c.setRate(600, 1); // 1 raw COLL → 600 raw DEBT = 60_000 DEBT / COLL
        debt.mint(address(c), 1e15);
        if (register) curveRegistry.register(address(c));
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

    function _collProfit() internal view returns (bytes memory) {
        return PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
    }

    // ── UniswapV2 / SushiSwap ─────────────────────────────────────────────

    function test_v2_exact_out_repay_via_verified_uniswap_pair() public {
        MockV2Pair p = _pair(v2Factory, V2_HASH);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            PB.v2Swap(address(p), 0, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(debt.balanceOf(address(ex)), 0, "exact-out bought exactly the flash owed");
        _assertClean();
    }

    function test_v2_sushi_pair_is_verified_by_its_own_factory() public {
        MockV2Pair p = _pair(sushiFactory, SUSHI_HASH);
        _exec(_planWith(
            PB.v2Swap(address(p), 1, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
        _assertClean();
    }

    function test_v2_pair_from_the_other_factory_is_refused() public {
        MockV2Pair p = _pair(v2Factory, V2_HASH);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(2), address(p)));
        _exec(_planWith(
            PB.v2Swap(address(p), 1, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
    }

    function test_v2_arbitrary_address_as_pair_is_refused() public {
        address rogue = makeAddr("rogue");
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(2), rogue));
        _exec(_planWith(
            PB.v2Swap(rogue, 0, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
    }

    function test_v2_unknown_factory_id_is_refused() public {
        MockV2Pair p = _pair(v2Factory, V2_HASH);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(2), address(p)));
        _exec(_planWith(
            PB.v2Swap(address(p), 7, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
    }

    // ── Curve ─────────────────────────────────────────────────────────────

    /// Curve has no exact output: the repay share is sold exact-in with a
    /// small overshoot and the surplus debt is swept to WETH.
    function test_curve_exact_in_repay_with_surplus_debt_swept() public {
        MockCurvePool c = _curve(true);
        uint128 collIn = 0.51e8; // → 30_600 DEBT ≥ 30_015 owed
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, address(coll), address(debt), 0, collIn),
            2,
            bytes.concat(
                _collProfit(),
                PB.poolSwap(address(pDebtWeth), address(debt), address(weth), PB.L_TAKE_BALANCE, 0)
            )
        ));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(debt.balanceOf(address(ex)), 0, "surplus debt swept");
        assertEq(coll.allowance(address(ex), address(c)), 0, "curve allowance zeroed");
        _assertClean();
    }

    function test_curve_unregistered_pool_is_refused() public {
        MockCurvePool c = _curve(false);
        vm.expectRevert(bytes("no registry"));
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    function test_curve_wrong_coin_index_is_refused() public {
        MockCurvePool c = _curve(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(3), address(c)));
        _exec(_planWith(
            PB.curveSwap(address(c), 1, 0, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    function test_curve_exact_out_is_refused() public {
        MockCurvePool c = _curve(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.ExactOutUnsupported.selector, uint8(3)));
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
    }

    // ── constructor ───────────────────────────────────────────────────────

    function test_constructor_rejects_zero_venue_anchors() public {
        bytes32 h = factory.initHash();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(factory), h, address(routerA), address(routerB), address(weth),
            address(0), V2_HASH, sushiFactory, SUSHI_HASH, address(curveRegistry));
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(factory), h, address(routerA), address(routerB), address(weth),
            v2Factory, V2_HASH, address(0), SUSHI_HASH, address(curveRegistry));
        vm.expectRevert(Executor.ZeroAddress.selector);
        new Executor(operator, sink, address(factory), h, address(routerA), address(routerB), address(weth),
            v2Factory, V2_HASH, sushiFactory, SUSHI_HASH, address(0));
    }
}
