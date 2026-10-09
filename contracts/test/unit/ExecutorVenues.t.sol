// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {SwapModule} from "../../src/SwapModule.sol";
import {ExecutorTestBase} from "./Base.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {
    MockV2Pair, MockCurvePool, MockCurveCryptoPool, MockVault4626, MockPendlePT, MockPendleYT, MockPendleSY,
    MockCurveNgLp, MockPendleMarket, MockPendleFactory
} from "./Mocks.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {DexModule} from "../../src/DexModule.sol";

/// Pool-direct UniswapV2 / SushiSwap and Curve legs. The success paths run
/// the reference liquidation with the repay swap on the new venue; the
/// refusals prove a plan cannot send funds to a pool the Executor has not
/// verified on chain (CREATE2 for V2; for Curve the MetaRegistry handler the
/// leg names, and the coin indices).
contract ExecutorVenuesTest is ExecutorTestBase {
    uint128 constant MIN_PROFIT = 0.5e18;
    /// The MetaRegistry handler the Curve doubles are registered in. Not 0,
    /// so a leg that dropped its handler byte cannot pass by default.
    uint8 constant H = 1;
    /// A handler that exists and does not hold them.
    uint8 constant H_OTHER = 0;
    /// Past `registry_length`: `get_registry` reads the zero address.
    uint8 constant H_NONE = 200;

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
        if (register) curveRegistry.register(address(c), H);
    }

    function _crypto(bool register) internal returns (MockCurveCryptoPool c) {
        address[] memory coins = new address[](2);
        coins[0] = address(coll);
        coins[1] = address(debt);
        c = new MockCurveCryptoPool(coins);
        c.setRate(600, 1);
        debt.mint(address(c), 1e15);
        if (register) curveRegistry.register(address(c), H);
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
            PB.v2Swap(address(p), 0, address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1, _collProfit()
        ));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(debt.balanceOf(address(ex)), 0, "exact-out bought the pull and the premium, no more");
        _assertClean();
    }

    function test_v2_sushi_pair_is_verified_by_its_own_factory() public {
        MockV2Pair p = _pair(sushiFactory, SUSHI_HASH);
        _exec(_planWith(
            PB.v2Swap(address(p), 1, address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1, _collProfit()
        ));
        _assertClean();
    }

    function test_v2_pair_from_the_other_factory_is_refused() public {
        MockV2Pair p = _pair(v2Factory, V2_HASH);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(2), address(p)));
        _exec(_planWith(
            PB.v2Swap(address(p), 1, address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1, _collProfit()
        ));
    }

    function test_v2_arbitrary_address_as_pair_is_refused() public {
        address rogue = makeAddr("rogue");
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(2), rogue));
        _exec(_planWith(
            PB.v2Swap(rogue, 0, address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1, _collProfit()
        ));
    }

    function test_v2_unknown_factory_id_is_refused() public {
        MockV2Pair p = _pair(v2Factory, V2_HASH);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(2), address(p)));
        _exec(_planWith(
            PB.v2Swap(address(p), 7, address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
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
            PB.curveSwap(address(c), 0, 1, H, address(coll), address(debt), 0, collIn),
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
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(3), address(c)));
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, H, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    /// The leg names the handler; only that one is asked. Oracle: the
    /// registry double's own table, where the pool sits in handler `H` and
    /// nowhere else (the deployed MetaRegistry keeps one handler per index
    /// and the zero address past its list).
    function test_curve_pool_named_with_another_handler_is_refused() public {
        MockCurvePool c = _curve(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(3), address(c)));
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, H_OTHER, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(3), address(c)));
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, H_NONE, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    /// A leg in the layout before the handler byte (pool, i, j: 22 bytes)
    /// is refused whole, not read with a handler of its own invention.
    function test_curve_leg_without_a_handler_byte_is_refused() public {
        MockCurvePool c = _curve(true);
        bytes memory old = PB.swap(3, address(coll), address(debt), 0, 0.51e8, abi.encodePacked(address(c), uint8(0), uint8(1)));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(3), address(0)));
        _exec(_planWith(old, 1, _collProfit()));
    }

    function test_curve_wrong_coin_index_is_refused() public {
        MockCurvePool c = _curve(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(3), address(c)));
        _exec(_planWith(
            PB.curveSwap(address(c), 1, 0, H, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    function test_curve_exact_out_is_refused() public {
        MockCurvePool c = _curve(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.ExactOutUnsupported.selector, uint8(3)));
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, H, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
    }

    // ── Curve crypto ──────────────────────────────────────────────────────

    /// A crypto pool repays exact-in through `exchange(uint256,uint256,…)`,
    /// with the same surplus sweep and allowance reset as a plain pool.
    function test_curve_crypto_exact_in_repay_with_surplus_debt_swept() public {
        MockCurveCryptoPool c = _crypto(true);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            PB.curveCryptoSwap(address(c), 0, 1, H, address(coll), address(debt), 0, 0.51e8),
            2,
            bytes.concat(
                _collProfit(),
                PB.poolSwap(address(pDebtWeth), address(debt), address(weth), PB.L_TAKE_BALANCE, 0)
            )
        ));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(debt.balanceOf(address(ex)), 0, "surplus debt swept");
        assertEq(coll.allowance(address(ex), address(c)), 0, "crypto allowance zeroed");
        _assertClean();
    }

    function test_curve_crypto_unregistered_pool_is_refused() public {
        MockCurveCryptoPool c = _crypto(false);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(4), address(c)));
        _exec(_planWith(
            PB.curveCryptoSwap(address(c), 0, 1, H, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    /// Same handler check as the plain venue, on the crypto venue.
    function test_curve_crypto_pool_named_with_another_handler_is_refused() public {
        MockCurveCryptoPool c = _crypto(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(4), address(c)));
        _exec(_planWith(
            PB.curveCryptoSwap(address(c), 0, 1, H_OTHER, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(4), address(c)));
        _exec(_planWith(
            PB.curveCryptoSwap(address(c), 0, 1, H_NONE, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    function test_curve_crypto_wrong_coin_index_is_refused() public {
        MockCurveCryptoPool c = _crypto(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(4), address(c)));
        _exec(_planWith(
            PB.curveCryptoSwap(address(c), 1, 0, H, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    function test_curve_crypto_exact_out_is_refused() public {
        MockCurveCryptoPool c = _crypto(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.ExactOutUnsupported.selector, uint8(4)));
        _exec(_planWith(
            PB.curveCryptoSwap(address(c), 0, 1, H, address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            1, _collProfit()
        ));
    }

    /// A plain Curve leg against a crypto pool (or the reverse) fails: the
    /// signed and unsigned `exchange` selectors differ.
    function test_curve_plain_venue_on_a_crypto_pool_reverts() public {
        MockCurveCryptoPool c = _crypto(true);
        vm.expectRevert();
        _exec(_planWith(
            PB.curveSwap(address(c), 0, 1, H, address(coll), address(debt), 0, 0.51e8),
            1, _collProfit()
        ));
    }

    // ── ERC-4626 unwrap ───────────────────────────────────────────────────

    /// Seized vault shares are redeemed into the asset first; the repay and
    /// closer legs then spend the asset as usual.
    function _vaultPlan(MockVault4626 v, bytes memory unwrapLeg) internal view returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, MIN_PROFIT, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 2),
            PB.legV3(address(pool), borrower, address(v), REPAY),
            unwrapLeg,
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            PB.profit(1, _collProfit())
        );
    }

    function _vault() internal returns (MockVault4626 v) {
        v = new MockVault4626(address(coll), 8);
        v.mint(address(pool), 1e12);
        coll.mint(address(v), 1e12);
    }

    function test_unwrap_4626_then_repay_from_the_asset() public {
        MockVault4626 v = _vault();
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_vaultPlan(v, PB.unwrap4626(address(v), address(coll))));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(v.balanceOf(address(ex)), 0, "shares all redeemed");
        _assertClean();
    }

    function test_unwrap_4626_wrong_asset_is_refused() public {
        MockVault4626 v = _vault();
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(5), address(v)));
        _exec(_vaultPlan(v, PB.unwrap4626(address(v), address(debt))));
    }

    function test_unwrap_4626_vault_must_be_the_spent_token() public {
        MockVault4626 v = _vault();
        MockVault4626 other = _vault();
        bytes memory leg = PB.swap(5, address(v), address(coll), PB.L_TAKE_BALANCE, 0, abi.encodePacked(address(other)));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(5), address(other)));
        _exec(_vaultPlan(v, leg));
    }

    // ── Curve NG LP one-coin withdrawal ───────────────────────────────────

    /// An NG pool of (COLL, DEBT) whose LP pays 1 COLL per LP.
    function _ngLp(bool register) internal returns (MockCurveNgLp lp) {
        address[] memory coins = new address[](2);
        coins[0] = address(coll);
        coins[1] = address(debt);
        lp = new MockCurveNgLp(coins);
        lp.mint(address(pool), 1e12);
        coll.mint(address(lp), 1e12);
        if (register) curveRegistry.register(address(lp), H);
    }

    function _lpPlan(address lp, bytes memory withdrawLeg) internal view returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, MIN_PROFIT, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 2),
            PB.legV3(address(pool), borrower, lp, REPAY),
            withdrawLeg,
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            PB.profit(1, _collProfit())
        );
    }

    function test_curve_lp_withdraws_one_coin_then_repays() public {
        MockCurveNgLp lp = _ngLp(true);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_lpPlan(address(lp), PB.curveLpOneCoin(address(lp), 0, H, address(coll))));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(lp.balanceOf(address(ex)), 0, "LP all withdrawn");
        _assertClean();
    }

    function test_curve_lp_unregistered_pool_is_refused() public {
        MockCurveNgLp lp = _ngLp(false);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(7), address(lp)));
        _exec(_lpPlan(address(lp), PB.curveLpOneCoin(address(lp), 0, H, address(coll))));
    }

    /// Same handler check as the swap venues, on the LP withdrawal.
    function test_curve_lp_named_with_another_handler_is_refused() public {
        MockCurveNgLp lp = _ngLp(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(7), address(lp)));
        _exec(_lpPlan(address(lp), PB.curveLpOneCoin(address(lp), 0, H_OTHER, address(coll))));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(7), address(lp)));
        _exec(_lpPlan(address(lp), PB.curveLpOneCoin(address(lp), 0, H_NONE, address(coll))));
    }

    function test_curve_lp_wrong_coin_is_refused() public {
        MockCurveNgLp lp = _ngLp(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(7), address(lp)));
        _exec(_lpPlan(address(lp), PB.curveLpOneCoin(address(lp), 1, H, address(coll))));
    }

    /// The pool must be the LP being spent: a registered pool cannot be named
    /// to withdraw some other token.
    function test_curve_lp_pool_must_be_the_spent_token() public {
        MockCurveNgLp lp = _ngLp(true);
        MockCurveNgLp other = _ngLp(true);
        bytes memory leg = PB.swap(7, address(lp), address(coll), PB.L_TAKE_BALANCE, 0, abi.encodePacked(address(other), uint8(0), H));
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(7), address(other)));
        _exec(_lpPlan(address(lp), leg));
    }

    // ── Pendle PT redeem (expired) ────────────────────────────────────────

    /// PT at index 1.25 (1 PT = 0.8 SY), SY at 1.25 of the collateral token:
    /// 1 PT redeems to 1 COLL.
    function _pendle() internal returns (MockPendlePT pt, MockPendleYT yt, MockPendleSY sy) {
        pt = new MockPendlePT();
        sy = new MockPendleSY(address(coll));
        yt = new MockPendleYT(address(pt), address(sy));
        pt.setYT(address(yt));
        yt.setIndex(1.25e18);
        sy.setRate(5, 4);
        yt.setExpired(true);
        pt.mint(address(pool), 1e12);
        coll.mint(address(sy), 1e12);
    }

    function _ptPlan(address pt, bytes memory redeemLeg) internal view returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, MIN_PROFIT, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 2),
            PB.legV3(address(pool), borrower, pt, REPAY),
            redeemLeg,
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            PB.profit(1, _collProfit())
        );
    }

    function test_pendle_pt_redeems_through_yt_and_sy_then_repays() public {
        (MockPendlePT pt, MockPendleYT yt, MockPendleSY sy) = _pendle();
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_ptPlan(address(pt), PB.pendlePtRedeem(address(pt), address(yt), address(coll))));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(pt.balanceOf(address(ex)), 0, "PT all redeemed");
        assertEq(sy.balanceOf(address(ex)), 0, "SY all redeemed");
        _assertClean();
    }

    function test_pendle_pt_before_expiry_is_refused() public {
        (MockPendlePT pt, MockPendleYT yt,) = _pendle();
        yt.setExpired(false);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(6), address(yt)));
        _exec(_ptPlan(address(pt), PB.pendlePtRedeem(address(pt), address(yt), address(coll))));
    }

    /// A YT that does not name the PT back (or a PT naming another YT) never
    /// receives the PT.
    function test_pendle_yt_must_pair_with_the_pt() public {
        (MockPendlePT pt, MockPendleYT yt, MockPendleSY sy) = _pendle();
        MockPendleYT rogue = new MockPendleYT(address(pt), address(sy));
        rogue.setExpired(true);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(6), address(rogue)));
        _exec(_ptPlan(address(pt), PB.pendlePtRedeem(address(pt), address(rogue), address(coll))));
        MockPendlePT otherPt = new MockPendlePT();
        otherPt.setYT(address(yt));
        otherPt.mint(address(pool), 1e12);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(6), address(yt)));
        _exec(_ptPlan(address(otherPt), PB.pendlePtRedeem(address(otherPt), address(yt), address(coll))));
    }

    function test_pendle_sy_refuses_a_token_it_cannot_pay() public {
        (MockPendlePT pt, MockPendleYT yt,) = _pendle();
        vm.expectRevert(bytes("sy: token out"));
        _exec(_ptPlan(address(pt), PB.pendlePtRedeem(address(pt), address(yt), address(debt))));
    }

    // ── Pendle market sale (live PT) ──────────────────────────────────────

    /// A live PT sold on a V6-factory market at 0.8 SY per PT; the SY
    /// redeems at 1.25 COLL: 1 PT sells for 1 COLL.
    function _pendleMarket(bool valid) internal returns (MockPendlePT pt, MockPendleMarket market) {
        MockPendleSY sy;
        MockPendleYT yt;
        (pt, yt, sy) = _pendle();
        yt.setExpired(false);
        market = new MockPendleMarket(address(sy), address(pt), address(yt));
        market.setRate(4, 5);
        vm.etch(MainnetVenues.PENDLE_MARKET_FACTORY_V6, address(new MockPendleFactory()).code);
        if (valid) MockPendleFactory(MainnetVenues.PENDLE_MARKET_FACTORY_V6).add(address(market));
    }

    function test_pendle_market_sale_then_redeem_then_repay() public {
        (MockPendlePT pt, MockPendleMarket market) = _pendleMarket(true);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_ptPlan(address(pt), PB.pendleMarketSell(address(pt), address(market), address(coll))));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertEq(pt.balanceOf(address(ex)), 0, "PT all sold");
        _assertClean();
    }

    function test_pendle_market_not_from_the_factory_is_refused() public {
        (MockPendlePT pt, MockPendleMarket market) = _pendleMarket(false);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(8), address(market)));
        _exec(_ptPlan(address(pt), PB.pendleMarketSell(address(pt), address(market), address(coll))));
    }

    /// A valid market for another PT cannot be handed this PT.
    function test_pendle_market_must_trade_the_spent_pt() public {
        (, MockPendleMarket market) = _pendleMarket(true);
        (MockPendlePT otherPt,,) = _pendle();
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(8), address(market)));
        _exec(_ptPlan(address(otherPt), PB.pendleMarketSell(address(otherPt), address(market), address(coll))));
    }

    // ── constructor ───────────────────────────────────────────────────────

    /// The venue anchors are the swap module's.
    function test_constructor_rejects_zero_venue_anchors() public {
        address dexm = swapModule.DEX_MODULE();
        vm.expectRevert(Executor.ZeroAddress.selector);
        new SwapModule(address(weth), address(routerA), address(routerB),
            address(0), V2_HASH, sushiFactory, SUSHI_HASH, address(curveRegistry), dexm);
        vm.expectRevert(Executor.ZeroAddress.selector);
        new SwapModule(address(weth), address(routerA), address(routerB),
            v2Factory, V2_HASH, address(0), SUSHI_HASH, address(curveRegistry), dexm);
        vm.expectRevert(Executor.ZeroAddress.selector);
        new SwapModule(address(weth), address(routerA), address(routerB),
            v2Factory, V2_HASH, sushiFactory, SUSHI_HASH, address(0), dexm);
        vm.expectRevert(Executor.ZeroAddress.selector);
        new SwapModule(address(weth), address(routerA), address(routerB),
            v2Factory, V2_HASH, sushiFactory, SUSHI_HASH, address(curveRegistry), address(0));
    }

    /// The dex module must be a `DexModule` for this WETH: an EOA, any other
    /// contract, or one built for another WETH is refused at deploy.
    function test_constructor_refuses_a_dex_module_that_is_not_one_for_this_weth() public {
        for (uint256 k; k < 3; ++k) {
            address bad = k == 0 ? makeAddr("eoa") : k == 1 ? address(weth) : address(new DexModule(makeAddr("other-weth")));
            vm.expectRevert(abi.encodeWithSelector(SwapModule.BadDexModule.selector, bad));
            new SwapModule(address(weth), address(routerA), address(routerB),
                v2Factory, V2_HASH, sushiFactory, SUSHI_HASH, address(curveRegistry), bad);
        }
    }
}
