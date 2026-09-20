// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {MarketParams} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {ExecutorTestBase} from "./Base.sol";
import {MockERC20, MockUniV3Pool, MockDssFlash, LazyFlashProvider} from "./Mocks.sol";

/// The other four flash providers, the Aave V4 and Morpho adapters, and a
/// sequential multi-group cascade (D32).
contract ProvidersAndAdaptersTest is ExecutorTestBase {
    uint128 constant COLL_SPENT_0FEE = 50_000_000;                 // 30_000e6 · 1e8 / 60_000e6
    uint128 constant GROSS_0FEE      = (COLL_OUT - COLL_SPENT_0FEE) * 2e11; // 1e18

    function _planWith(uint8 provider, address src, uint128 owed, bytes memory legs, uint8 n)
        internal view returns (bytes memory)
    {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(provider, src, address(debt), REPAY, n, 1),
            legs,
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, owed),
            PB.profit(1, _profitLeg())
        );
    }

    function _v3Leg() internal view returns (bytes memory) {
        return PB.legV3(address(pool), borrower, address(coll), REPAY);
    }

    // ── flash providers ───────────────────────────────────────────────

    function test_univ3_flash_repays_amount_plus_fee_by_transfer() public {
        // debt is token0 or token1 of pDebtWeth; the Executor picks the side.
        uint256 before = debt.balanceOf(address(pDebtWeth));
        _exec(_planWith(PB.P_UNIV3, address(pDebtWeth), OWED, _v3Leg(), 1)); // 5 bps fee
        assertEq(debt.balanceOf(address(pDebtWeth)), before + 15e6, "pool keeps the fee");
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    function test_univ4_unlock_take_settle() public {
        uint256 before = debt.balanceOf(address(pm));
        _exec(_planWith(PB.P_UNIV4, address(pm), REPAY, _v3Leg(), 1));
        assertEq(debt.balanceOf(address(pm)), before, "zero-fee, fully settled");
        assertEq(weth.balanceOf(sink), GROSS_0FEE);
        _assertClean();
    }

    function test_lazy_flash_provider_residual_allowance_is_zero() public {
        LazyFlashProvider lazy = new LazyFlashProvider();
        debt.mint(address(lazy), 1e15);
        _exec(_planWith(PB.P_AAVE, address(lazy), OWED, _v3Leg(), 1));
        assertEq(debt.allowance(address(ex), address(lazy)), 0, "D1: residual flash allowance");
    }

    function test_morpho_flash_repaid_by_pull() public {
        uint256 before = debt.balanceOf(address(morpho));
        _exec(_planWith(PB.P_MORPHO, address(morpho), REPAY, _v3Leg(), 1));
        assertEq(debt.balanceOf(address(morpho)), before);
        assertEq(weth.balanceOf(sink), GROSS_0FEE);
        _assertClean();
    }

    function test_sky_dss_flash_erc3156() public {
        MockERC20 dai = new MockERC20("DAI", 18);
        MockDssFlash dss = new MockDssFlash(address(dai));
        dai.mint(address(dss), 1e27);
        MockUniV3Pool pCollDai = MockUniV3Pool(factory.deploy(address(coll), address(dai), 3000));
        _price(pCollDai, address(coll), address(dai), 6e14, 1); // 60_000e18 / 1e8
        dai.mint(address(pCollDai), 1e27);
        address b = makeAddr("dai-borrower");
        pool.setPosition(b, 0.95e18, 30_000e18, COLL_OUT);

        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_SKY, address(dss), address(dai), 30_000e18, 1, 1),
            PB.legV3(address(pool), b, address(coll), 30_000e18),
            PB.poolSwap(address(pCollDai), address(coll), address(dai), PB.L_EXACT_OUT, 30_000e18),
            PB.profit(1, _profitLeg())
        );
        uint256 before = dai.balanceOf(address(dss));
        _exec(plan);
        assertEq(dai.balanceOf(address(dss)), before, "mint repaid (toll 0)");
        assertEq(dai.allowance(address(ex), address(dss)), 0);
        assertEq(weth.balanceOf(sink), GROSS_0FEE);
        _assertClean();
    }

    function test_sky_dss_wrong_token_reverts() public {
        MockERC20 dai = new MockERC20("DAI", 18);
        MockDssFlash dss = new MockDssFlash(address(dai));
        vm.expectRevert("DssFlash/token-unsupported");
        _exec(_planWith(PB.P_SKY, address(dss), REPAY, _v3Leg(), 1));
    }

    // ── Aave V4 adapter ───────────────────────────────────────────────

    function test_aave_v4_liquidates_by_reserve_id_from_tail() public {
        spoke.setReserve(3, address(coll));
        spoke.setReserve(7, address(debt));
        spoke.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legV4(address(spoke), borrower, address(coll), REPAY, 3, 7), 1));
        assertEq(spoke.lastCollId(), 3); assertEq(spoke.lastDebtId(), 7);
        assertEq(spoke.lastUser(), borrower); assertEq(spoke.lastDebtToCover(), REPAY);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    function test_aave_v4_healthy_skipped() public {
        spoke.setReserve(3, address(coll)); spoke.setReserve(7, address(debt));
        spoke.setPosition(borrower, 1.01e18, REPAY, COLL_OUT);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legV4(address(spoke), borrower, address(coll), REPAY, 3, 7), 1));
    }

    // ── Morpho Blue adapter ───────────────────────────────────────────

    uint128 constant TBA = 1_000_000e6;     // totalBorrowAssets before accrual
    uint128 constant TBS = 1_000_000e12;    // totalBorrowShares
    uint128 constant PENDING = 1_000e6;     // interest accrueInterest will add

    function _market() internal returns (bytes32 id, MarketParams memory mp) {
        mp = MarketParams(address(debt), address(coll), makeAddr("oracle"), makeAddr("irm"), 0.86e18);
        id = morpho.createMarket(mp);
        morpho.setTotals(id, TBA, TBS);
        morpho.setPendingInterest(id, PENDING);
        morpho.setCollateralOut(borrower, COLL_OUT);
    }

    /// `toSharesDown` with POST-accrual totals, exactly as `Executor` does.
    function _sharesDown(uint256 assets) internal pure returns (uint256) {
        return assets * (uint256(TBS) + 1e6) / (uint256(TBA) + PENDING + 1);
    }
    function _assetsUp(uint256 shares) internal pure returns (uint256) {
        uint256 d = uint256(TBS) + 1e6;
        return (shares * (uint256(TBA) + PENDING + 1) + d - 1) / d;
    }

    function test_morpho_accrues_then_repays_shares_from_assets() public {
        (bytes32 id, ) = _market();
        morpho.setPosition(id, borrower, 30_000e12, 1e8);

        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legMorpho(address(morpho), borrower, address(coll), REPAY, id), 1));

        uint256 shares = _sharesDown(REPAY);
        assertEq(morpho.accrueCalls(), 1, "accrued once before reading totals");
        assertEq(morpho.lastRepaidShares(), shares);
        assertEq(morpho.lastRepaidAssets(), _assetsUp(shares));
        assertLe(morpho.lastRepaidAssets(), REPAY, "exact approval always covers the pull");
        // Debt dust from rounding stays in the Executor, reachable only by sweep.
        uint256 dust = REPAY - morpho.lastRepaidAssets();
        assertEq(debt.balanceOf(address(ex)), dust);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
        address[] memory a = new address[](1); a[0] = address(debt);
        vm.prank(stranger); ex.sweep(a);
        assertEq(debt.balanceOf(sink), dust);
    }

    function test_morpho_caps_shares_at_borrower_position() public {
        (bytes32 id, ) = _market();
        morpho.setPosition(id, borrower, 1e12, 1e8); // ~1 DEBT of debt
        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legMorpho(address(morpho), borrower, address(coll), REPAY, id), 1));
        assertEq(morpho.lastRepaidShares(), 1e12, "capped: one share above owed would revert in Morpho");
        assertEq(morpho.lastRepaidAssets(), _assetsUp(1e12));
    }

    function test_morpho_market_mismatch_reverts_whole_plan() public {
        (bytes32 id, ) = _market();
        morpho.setPosition(id, borrower, 30_000e12, 1e8);
        vm.expectRevert(Executor.LegMismatch.selector);
        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legMorpho(address(morpho), borrower, address(weth), REPAY, id), 1)); // wrong collateral
    }

    function test_morpho_healthy_position_skipped() public {
        (bytes32 id, ) = _market();
        morpho.setPosition(id, borrower, 30_000e12, 1e8);
        morpho.setHealthy(borrower, true);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legMorpho(address(morpho), borrower, address(coll), REPAY, id), 1));
    }

    function test_morpho_no_debt_skipped_before_approval() public {
        (bytes32 id, ) = _market();
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_planWith(PB.P_AAVE, address(pool), OWED,
            PB.legMorpho(address(morpho), borrower, address(coll), REPAY, id), 1));
    }

    // ── multi-group cascade (D32: sequential, never nested) ───────────

    function test_two_groups_two_providers_one_profit_swap() public {
        address b1 = makeAddr("b1"); address b2 = makeAddr("b2");
        pool.setPosition(b1, 0.9e18, 15_000e6, 0.275e8);
        pool.setPosition(b2, 0.9e18, 15_000e6, 0.275e8);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 2),
            // group 0: Aave flash (5 bps)
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), 15_000e6, 1, 1),
            PB.legV3(address(pool), b1, address(coll), 15_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 15_007.5e6),
            // group 1: Morpho flash (0 fee)
            PB.groupHead(PB.P_MORPHO, address(morpho), address(debt), 15_000e6, 1, 1),
            PB.legV3(address(pool), b2, address(coll), 15_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 15_000e6),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        // spent: ceil(15_007.5e6·1e8/60_000e6) = 25_012_500 ; 25_000_000
        uint256 left = uint256(0.55e8) - 25_012_500 - 25_000_000;
        assertEq(weth.balanceOf(sink), left * 2e11);
        assertEq(debt.balanceOf(address(ex)), 0);
        _assertClean();
    }

    function test_second_group_failure_reverts_first_group_too() public {
        address b1 = makeAddr("b1"); address b2 = makeAddr("b2");
        pool.setPosition(b1, 0.9e18, 15_000e6, 0.275e8);
        pool.setPosition(b2, 0.9e18, 15_000e6, 0.275e8);
        uint256 poolDebt = debt.balanceOf(address(pool));
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 2),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), 15_000e6, 1, 1),
            PB.legV3(address(pool), b1, address(coll), 15_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 15_007.5e6),
            PB.groupHead(9, address(morpho), address(debt), 15_000e6, 1, 1), // unknown provider
            PB.legV3(address(pool), b2, address(coll), 15_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 15_000e6),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(abi.encodeWithSelector(Executor.UnknownProvider.selector, uint8(9)));
        _exec(plan);
        assertEq(debt.balanceOf(address(pool)), poolDebt, "group 0 rolled back with group 1");
        assertLt(pool.hf(b1), 1e18, "b1 still liquidatable: nothing partial");
    }
}
