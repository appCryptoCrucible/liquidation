// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../unit/Base.sol";
import {MockERC20, MockUniV3Pool, MockEulerVault} from "../unit/Mocks.sol";

/// Collateral that under-delivers only when paid TO `taxed` (the Executor).
contract FoTColl is MockERC20 {
    uint256 public immutable bps;
    address public immutable taxed;

    constructor(address taxed_, uint256 bps_) MockERC20("FOT", 8) {
        taxed = taxed_;
        bps = bps_;
    }

    function transfer(address to, uint256 a) public override returns (bool) {
        uint256 fee = to == taxed ? a * bps / 10_000 : 0;
        balanceOf[msg.sender] -= a;
        balanceOf[to] += a - fee;
        return true;
    }
}

/// V4 spoke that pulls `protocol_pull` but seizes strictly less collateral.
contract UnderSeizeSpoke {
    struct UserAccountData {
        uint256 riskPremium;
        uint256 avgCollateralFactor;
        uint256 healthFactor;
        uint256 totalCollateralValue;
        uint256 totalDebtValueRay;
        uint256 activeCollateralCount;
        uint256 borrowCount;
    }

    mapping(uint256 => address) public underlying;
    mapping(address => uint256) public hf;
    mapping(address => uint256) public maxDebt;
    mapping(address => uint256) public collateralOut;
    uint256 public lastCollId;
    uint256 public lastDebtId;
    address public lastUser;
    uint256 public lastDebtToCover;
    uint256 public seizeBps = 5_000;

    function setReserve(uint256 id, address token) external {
        underlying[id] = token;
    }

    function setPosition(address u, uint256 hf_, uint256 maxDebt_, uint256 collOut) external {
        hf[u] = hf_;
        maxDebt[u] = maxDebt_;
        collateralOut[u] = collOut;
    }

    function setSeizeBps(uint256 b) external {
        seizeBps = b;
    }

    function getUserAccountData(address u) external view returns (UserAccountData memory d) {
        d.healthFactor = hf[u];
    }

    function liquidationCall(uint256 collId, uint256 debtId, address user, uint256 debtToCover, bool receiveShares)
        external
    {
        require(!receiveShares && hf[user] < 1e18, "spoke");
        address debtT = underlying[debtId];
        address collT = underlying[collId];
        uint256 actual = debtToCover < maxDebt[user] ? debtToCover : maxDebt[user];
        lastCollId = collId;
        lastDebtId = debtId;
        lastUser = user;
        lastDebtToCover = actual;
        require(MockERC20(debtT).transferFrom(msg.sender, address(this), actual), "pull");
        uint256 out = collateralOut[user] * actual / maxDebt[user] * seizeBps / 10_000;
        require(MockERC20(collT).transfer(msg.sender, out), "push");
        hf[user] = 2e18;
    }
}

/// 10C paths that must not live in the frozen 10A unit file: surplus-borrow,
/// non-trivial V4 ids, FoT measured deltas, under-seize V4, multi-group, 3-group.
contract ExecutorCoverage10CTest is ExecutorTestBase {
    function test_surplus_borrow_take_balance_routes_leftover_debt() public {
        uint128 flash = REPAY * 2;
        uint128 owedFlash = flash + uint128(uint256(flash) * 5 / 10_000);
        uint128 buy = owedFlash - (flash - REPAY);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), flash, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, buy),
            PB.profit(
                2,
                bytes.concat(
                    _profitLeg(),
                    PB.poolSwap(address(pDebtWeth), address(debt), address(weth), PB.L_TAKE_BALANCE, 0)
                )
            )
        );
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(plan);
        assertEq(debt.balanceOf(address(pool)), poolDebt + (owedFlash - flash) + REPAY, "flash repaid exactly");
        assertEq(debt.balanceOf(address(ex)), 0);
        assertGt(weth.balanceOf(sink), 0);
        _assertClean();
    }

    function test_v4_collId_debtId_byte_order_1_2_3() public {
        spoke.setReserve(1, address(coll));
        spoke.setReserve(2, address(weth));
        spoke.setReserve(3, address(debt));
        spoke.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV4(address(spoke), borrower, address(coll), REPAY, 1, 3),
            _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(spoke.lastCollId(), 1);
        assertEq(spoke.lastDebtId(), 3);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    function test_fee_on_transfer_repay_flash_exactly() public {
        FoTColl fot = new FoTColl(address(ex), 200);
        fot.mint(address(pool), 1e12);
        MockUniV3Pool pFotDebt = MockUniV3Pool(factory.deploy(address(fot), address(debt), 3000));
        MockUniV3Pool pFotWeth = MockUniV3Pool(factory.deploy(address(fot), address(weth), 3000));
        _price(pFotDebt, address(fot), address(debt), 600, 1);
        _price(pFotWeth, address(fot), address(weth), 2e11, 1);
        debt.mint(address(pFotDebt), 1e15);
        weth.mint(address(pFotWeth), 1e24);
        address b = makeAddr("fot");
        uint128 collOut = 0.56e8;
        pool.setPosition(b, 0.9e18, REPAY, collOut);
        uint256 delivered = collOut - collOut * 200 / 10_000;
        require(delivered > COLL_SPENT, "fot still covers exact-out");
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), b, address(fot), REPAY),
            PB.poolSwap(address(pFotDebt), address(fot), address(debt), PB.L_EXACT_OUT, OWED),
            PB.profit(1, PB.poolSwap(address(pFotWeth), address(fot), address(weth), PB.L_TAKE_BALANCE, 0))
        );
        uint256 poolDebt = debt.balanceOf(address(pool));
        _exec(plan);
        assertEq(debt.balanceOf(address(pool)), poolDebt + OWED, "flash repaid exactly under FoT");
        assertEq(debt.balanceOf(address(ex)), 0);
        assertEq(fot.balanceOf(address(ex)), 0);
        assertGt(weth.balanceOf(sink), 0);
    }

    function test_under_seize_v4_reverts_whole_plan() public {
        UnderSeizeSpoke bad = new UnderSeizeSpoke();
        coll.mint(address(bad), 1e12);
        bad.setReserve(1, address(coll));
        bad.setReserve(3, address(debt));
        bad.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        bad.setSeizeBps(1);
        uint256 poolDebt = debt.balanceOf(address(pool));
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV4(address(bad), borrower, address(coll), REPAY, 1, 3),
            _repayLeg(),
            PB.profit(1, _profitLeg())
        );
        vm.prank(operator);
        vm.expectRevert();
        ex.execute(plan);
        assertEq(debt.balanceOf(address(pool)), poolDebt, "flash not kept");
        assertEq(debt.balanceOf(address(ex)), 0, "no dust");
        assertEq(coll.balanceOf(address(ex)), 0, "no coll dust");
    }

    function test_group1_all_fail_reverts_later_groups() public {
        address b1 = makeAddr("healthy");
        address b2 = makeAddr("live");
        pool.setPosition(b1, 1.2e18, REPAY, COLL_OUT);
        pool.setPosition(b2, 0.9e18, REPAY, COLL_OUT);
        uint256 poolDebt = debt.balanceOf(address(pool));
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 2),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), b1, address(coll), REPAY),
            _repayLeg(),
            PB.groupHead(PB.P_MORPHO, address(morpho), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), b2, address(coll), REPAY),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            PB.profit(1, _profitLeg())
        );
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(plan);
        assertEq(debt.balanceOf(address(pool)), poolDebt);
        assertLt(pool.hf(b2), 1e18);
    }

    function test_group1_partial_fail_group2_still_runs() public {
        address beaten = makeAddr("beaten");
        address live1 = makeAddr("live1");
        address live2 = makeAddr("live2");
        pool.setPosition(beaten, 1.2e18, 15_000e6, 0.275e8);
        pool.setPosition(live1, 0.9e18, 15_000e6, 0.275e8);
        pool.setPosition(live2, 0.9e18, 15_000e6, 0.275e8);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.4e18, 2),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), 15_000e6, 2, 1),
            PB.legV3(address(pool), beaten, address(coll), 15_000e6),
            PB.legV3(address(pool), live1, address(coll), 15_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 15_007.5e6),
            PB.groupHead(PB.P_MORPHO, address(morpho), address(debt), 15_000e6, 1, 1),
            PB.legV3(address(pool), live2, address(coll), 15_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 15_000e6),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(pool.hf(live1), 2e18);
        assertEq(pool.hf(live2), 2e18);
        assertEq(pool.hf(beaten), 1.2e18);
        assertGt(weth.balanceOf(sink), 0);
        _assertClean();
    }

    function test_three_group_rewalk_same_debt() public {
        address b0 = makeAddr("g0");
        address b1 = makeAddr("g1");
        address b2 = makeAddr("g2");
        pool.setPosition(b0, 0.9e18, 10_000e6, 0.20e8);
        pool.setPosition(b1, 0.9e18, 10_000e6, 0.20e8);
        pool.setPosition(b2, 0.9e18, 10_000e6, 0.20e8);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.3e18, 3),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), 10_000e6, 1, 1),
            PB.legV3(address(pool), b0, address(coll), 10_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 10_005e6),
            PB.groupHead(PB.P_MORPHO, address(morpho), address(debt), 10_000e6, 1, 1),
            PB.legV3(address(pool), b1, address(coll), 10_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 10_000e6),
            PB.groupHead(PB.P_UNIV4, address(pm), address(debt), 10_000e6, 1, 1),
            PB.legV3(address(pool), b2, address(coll), 10_000e6),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, 10_000e6),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(pool.hf(b0), 2e18);
        assertEq(pool.hf(b1), 2e18);
        assertEq(pool.hf(b2), 2e18);
        assertGt(weth.balanceOf(sink), 0);
        _assertClean();
    }

    function test_10e_euler_invariants_zero_dust_zero_allowance() public {
        MockEulerVault euler = new MockEulerVault();
        euler.setDebtToken(address(debt));
        euler.setPosition(borrower, REPAY, COLL_OUT);
        debt.mint(address(euler), 1e15);
        coll.mint(address(euler), 1e12);
        uint256 poolDebt = debt.balanceOf(address(pool));
        uint256 eulerDebt = debt.balanceOf(address(euler));
        _exec(_plan(
            PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1,
            PB.legEuler(address(euler), borrower, address(coll), REPAY, 1, address(coll))
        ));
        // Flash source is the Aave mock, not the Euler vault: out REPAY, in OWED,
        // net = premium. V3's `+ REPAY` does not apply — that repay lands on Euler.
        assertEq(debt.balanceOf(address(pool)), poolDebt + (OWED - REPAY), "flash repaid exactly");
        assertEq(debt.balanceOf(address(euler)), eulerDebt + REPAY, "euler pulled repay");
        assertEq(debt.allowance(address(ex), address(euler)), 0);
        assertEq(weth.balanceOf(address(ex)), 0);
        assertEq(coll.balanceOf(address(ex)), 0);
        assertGt(weth.balanceOf(sink), 0);
        _assertClean();
    }

    function test_min_profit_or_revert() public {
        vm.expectRevert(abi.encodeWithSelector(Executor.Unprofitable.selector, uint256(GROSS_WETH), uint256(GROSS_WETH) + 1));
        _exec(_plan(PB.F_SWEEP, 0, 0, uint128(uint256(GROSS_WETH) + 1), 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        _exec(_plan(PB.F_SWEEP, 0, 0, GROSS_WETH, 1, PB.legV3(address(pool), borrower, address(coll), REPAY)));
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }
}
