// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {ISiloHook, MultiCall} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20G {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function allowance(address, address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
}

interface ISiloV2 {
    function deposit(uint256 assets, address receiver, uint8 collateralType) external returns (uint256);
    function borrow(uint256 assets, address receiver, address borrower) external returns (uint256);
    function maxBorrow(address borrower) external view returns (uint256);
    function isSolvent(address borrower) external view returns (bool);
    function accrueInterest() external returns (uint256);
}

interface IAavePoolFee {
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

/// `CollateralDebtData` (core-v3 `510fc654`).
struct CollateralDebtData {
    uint256 debt;
    uint256 cumulativeIndexNow;
    uint256 cumulativeIndexLastUpdate;
    uint128 cumulativeQuotaInterest;
    uint256 accruedInterest;
    uint256 accruedFees;
    uint256 totalDebtUSD;
    uint256 totalValue;
    uint256 totalValueUSD;
    uint256 twvUSD;
    uint256 enabledTokensMask;
    uint256 quotedTokensMask;
    address[] quotedTokens;
    address _poolQuotaKeeper;
}

interface IGbFacade {
    function openCreditAccount(address onBehalfOf, MultiCall[] calldata calls, uint256 referralCode)
        external payable returns (address);
}

interface IGbMulticall {
    function addCollateral(address token, uint256 amount) external;
    function increaseDebt(uint256 amount) external;
    function updateQuota(address token, int96 quotaChange, uint96 minQuota) external;
    function withdrawCollateral(address token, uint256 amount, address to) external;
}

interface IGbManager {
    function calcDebtAndCollateral(address creditAccount, uint8 task)
        external view returns (CollateralDebtData memory);
    function fees() external view returns (uint16, uint16, uint16, uint16, uint16);
}

interface IGbConfigurator {
    function setLiquidationThreshold(address token, uint16 liquidationThreshold) external;
}

/*
 * Fork proofs + gas measurement for the two adapters that had none: Silo V2
 * and Gearbox V3. Same shape as ForkLiveLiquidations: a real position is
 * opened on the fork and made liquidatable by accruing interest (vm.warp);
 * prices come from the protocols' own oracles, never a chosen number.
 *
 *   forge test --match-contract ForkSiloGearbox --isolate -vvvv > trace
 *   python tools/gas-measure/fork_decompose.py trace
 */
contract ForkSiloGearboxTest is Test {
    uint256 constant PIN = 26_019_284;

    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant WSTETH = 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0;
    address constant WSTETH_WHALE = 0x3e40D73EB977Dc6a537aF587D48316feE66E9C8c;

    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant WSTETH_WETH_001 = 0x109830a1AAaD605BbF02a9dFA7B0B92EC2FB7dAa;

    /// Silo V2 wstETH/WETH pair (SiloConfig 0xe7a7…5f70): wstETH silo LT
    /// 0.96 / max LTV 0.93 / fee 1.5 %, oracle = wstETH exchange rate.
    address constant SILO_WSTETH = 0x1a132e4e90D66E2f4FCDc99420F204D46F907aDB;
    address constant SILO_WETH = 0x02AE6A64a0DC17ffFDC5722Ad8270a7B32Be44db;
    address constant SILO_HOOK = 0x863F7830884aFa6d061100ba3C223A159B5A57Fc;
    uint8 constant COLLATERAL = 1; // ISilo.CollateralType.Collateral

    address operator = makeAddr("operator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PIN);
        forked = true;
        ex = new Executor(
            operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH,
            MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY,
            MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY
        );
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    // ── Silo V2 ───────────────────────────────────────────────────────────

    /// wstETH collateral / WETH debt on the real pair and hook. A lender
    /// supplies the WETH silo so the borrow drives utilization up; interest
    /// then takes LTV past LT. Aave flashes the WETH, the hook seizes
    /// wstETH, UniV3 buys back the WETH owed, the rest is profit.
    function test_fork_silo_v2_wsteth_weth_liquidation() public onFork {
        address lender = makeAddr("silo-lender");
        address user = makeAddr("silo-user");

        deal(WETH, lender, 3.5 ether);
        vm.startPrank(lender);
        IERC20G(WETH).approve(SILO_WETH, type(uint256).max);
        ISiloV2(SILO_WETH).deposit(3.5 ether, lender, COLLATERAL);
        vm.stopPrank();

        uint256 coll = 4 ether;
        vm.prank(WSTETH_WHALE);
        require(IERC20G(WSTETH).transfer(user, coll), "wst whale");
        vm.startPrank(user);
        IERC20G(WSTETH).approve(SILO_WSTETH, type(uint256).max);
        ISiloV2(SILO_WSTETH).deposit(coll, user, COLLATERAL);
        uint256 borrow = ISiloV2(SILO_WETH).maxBorrow(user);
        require(borrow > 0, "chain: maxBorrow 0");
        ISiloV2(SILO_WETH).borrow(borrow, user, user);
        vm.stopPrank();
        assertTrue(ISiloV2(SILO_WETH).isSolvent(user), "chain: open landed insolvent");

        // Healthy: the leg is skipped, the plan reverts as a whole.
        bytes memory early = _siloPlan(user, 1 ether);
        vm.prank(operator);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        ex.execute(early);

        _warpUntilInsolvent(user);
        (, uint256 debtToRepay, bool sTokenRequired) = ISiloHook(SILO_HOOK).maxLiquidation(user);
        assertGt(debtToRepay, 0, "chain: hook reports nothing to repay");
        assertFalse(sTokenRequired, "chain: hook requires sToken");

        bytes memory plan = _siloPlan(user, debtToRepay);
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        vm.prank(operator);
        ex.execute(plan);

        assertGt(IERC20G(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        assertEq(IERC20G(WETH).balanceOf(address(ex)), 0, "weth left");
        assertEq(IERC20G(WSTETH).balanceOf(address(ex)), 0, "wsteth left");
        assertEq(IERC20G(WETH).allowance(address(ex), SILO_HOOK), 0, "hook allowance");
        assertEq(IERC20G(WETH).allowance(address(ex), AAVE_V3_POOL), 0, "aave allowance");
    }

    /// Step time forward, accruing interest on chain each step, until the
    /// position crosses LT. Monotonic: the position lands one step past LT,
    /// not deep in bad debt.
    function _warpUntilInsolvent(address user) internal {
        // A local clock: under via-IR `block.timestamp` can be read once and
        // reused, so `warp(block.timestamp + dt)` in a loop never advances.
        uint256 t = block.timestamp;
        for (uint256 i; i < 200; ++i) {
            t += 7 days;
            vm.warp(t);
            ISiloV2(SILO_WETH).accrueInterest();
            if (!ISiloV2(SILO_WETH).isSolvent(user)) return;
        }
        revert("chain: interest never took LTV past LT");
    }

    // ── Gearbox V3.1 ──────────────────────────────────────────────────────

    uint256 constant GB_PIN = 26_049_000;
    /// kpk WETH market: manager / facade / configurator, and the market
    /// configurator that administers them (`acl().getConfigurator()`).
    address constant GB_MANAGER = 0x79C6C1ce5B12abCC3E407ce8C160eE1160250921;
    address constant GB_FACADE = 0x9515AB9BB73A9642F1a93Ba7C2790e9d08227f9a;
    address constant GB_CONFIGURATOR = 0x2be90B890e7009E24f2b99D6679968be453e76CB;
    address constant GB_MARKET_CONFIGURATOR = 0x1B265B97EB169fB6668E3258007C3b0242C7bDBE;
    uint8 constant DEBT_COLLATERAL = 3;
    uint16 constant LOWERED_LT = 7_000;

    function _gbFork() internal returns (Executor gx) {
        vm.createSelectFork(vm.envString("MAINNET_RPC_URL"), GB_PIN);
        gx = new Executor(
            operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH,
            MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY,
            MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY
        );
    }

    /// A real credit account on the kpk WETH manager: `size` wstETH
    /// collateral (quoted), `size` WETH debt, the borrowed WETH withdrawn, healthy at the
    /// market LT (95 %). The market admin then lowers wstETH's LT to 70 %
    /// (fixture only: prices and payments stay the protocol's own), which
    /// makes it liquidatable without bad debt.
    function _gbOpenAndBreak(Executor gx, address user, uint256 size) internal returns (address ca) {
        deal(WSTETH, user, size);
        int96 quota = int96(int256(size * 4 / 3));
        MultiCall[] memory calls = new MultiCall[](4);
        calls[0] = MultiCall(GB_FACADE, abi.encodeCall(IGbMulticall.addCollateral, (WSTETH, size)));
        calls[1] = MultiCall(GB_FACADE, abi.encodeCall(IGbMulticall.increaseDebt, (size)));
        calls[2] = MultiCall(GB_FACADE, abi.encodeCall(IGbMulticall.updateQuota, (WSTETH, quota, 0)));
        calls[3] = MultiCall(GB_FACADE, abi.encodeCall(IGbMulticall.withdrawCollateral, (WETH, size, user)));
        vm.startPrank(user);
        IERC20G(WSTETH).approve(GB_MANAGER, type(uint256).max);
        ca = IGbFacade(GB_FACADE).openCreditAccount(user, calls, 0);
        vm.stopPrank();
        CollateralDebtData memory c = IGbManager(GB_MANAGER).calcDebtAndCollateral(ca, DEBT_COLLATERAL);
        assertGe(c.twvUSD, c.totalDebtUSD, "chain: account opened unhealthy");

        // Healthy: the leg is skipped, the plan reverts as a whole.
        bytes memory early = _gbPlan(ca, 1 ether, 1, PB.GB_FULL);
        vm.prank(operator);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        gx.execute(early);

        // Gearbox refuses a second debt change in the block that opened the
        // account (`DebtUpdatedTwiceInOneBlockException`).
        vm.roll(block.number + 1);
        vm.prank(GB_MARKET_CONFIGURATOR);
        IGbConfigurator(GB_CONFIGURATOR).setLiquidationThreshold(WSTETH, LOWERED_LT);
        c = IGbManager(GB_MANAGER).calcDebtAndCollateral(ca, DEBT_COLLATERAL);
        assertLt(c.twvUSD, c.totalDebtUSD, "chain: still healthy after LT change");
    }

    /// Full liquidation: add `totalValue * discount` (+ 0.5 % margin) of
    /// WETH, withdraw all wstETH; the manager returns the unused WETH.
    function test_fork_gearbox_v31_full_liquidation() public onFork {
        Executor gx = _gbFork();
        address ca = _gbOpenAndBreak(gx, makeAddr("gb-full"), 12 ether);

        CollateralDebtData memory c = IGbManager(GB_MANAGER).calcDebtAndCollateral(ca, DEBT_COLLATERAL);
        (,, uint16 discount,,) = IGbManager(GB_MANAGER).fees();
        uint256 u0 = IERC20G(WETH).balanceOf(ca);
        uint256 add = c.totalValue * discount / 10_000 + c.totalValue * 50 / 10_000 - u0;
        uint256 coll = IERC20G(WSTETH).balanceOf(ca);

        bytes memory plan = _gbPlan(ca, add, coll - 1, PB.GB_FULL);
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        vm.prank(operator);
        gx.execute(plan);

        c = IGbManager(GB_MANAGER).calcDebtAndCollateral(ca, DEBT_COLLATERAL);
        assertEq(c.debt, 0, "chain: account still has debt");
        assertGt(IERC20G(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        _gbAssertClean(gx);
    }

    /// Partial liquidation: repay 17 of 30 WETH, seize wstETH at the
    /// discount. Gearbox then requires the account healthy (restored at LT
    /// 70 %) and its remaining debt at or above the manager's minimum
    /// (10 WETH) — `BorrowAmountOutOfLimitsException` otherwise.
    function test_fork_gearbox_v31_partial_liquidation() public onFork {
        Executor gx = _gbFork();
        address ca = _gbOpenAndBreak(gx, makeAddr("gb-partial"), 30 ether);

        bytes memory plan = _gbPlan(ca, 17 ether, 1, PB.GB_PARTIAL);
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        vm.prank(operator);
        gx.execute(plan);

        CollateralDebtData memory c = IGbManager(GB_MANAGER).calcDebtAndCollateral(ca, DEBT_COLLATERAL);
        assertLt(c.debt, 30 ether, "chain: debt not reduced");
        assertGe(c.twvUSD, c.totalDebtUSD, "chain: account not healthy after partial");
        assertGt(IERC20G(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        _gbAssertClean(gx);
    }

    function _gbAssertClean(Executor gx) internal view {
        assertEq(IERC20G(WETH).balanceOf(address(gx)), 0, "weth left");
        assertEq(IERC20G(WSTETH).balanceOf(address(gx)), 0, "wsteth left");
        assertEq(IERC20G(WETH).allowance(address(gx), GB_MANAGER), 0, "manager allowance");
        assertEq(IERC20G(WETH).allowance(address(gx), GB_FACADE), 0, "facade allowance");
    }

    /// Aave flashes `repay` WETH; the leg adds/repays it; UniV3 buys back
    /// the WETH owed with wstETH; leftover wstETH goes to WETH as profit.
    function _gbPlan(address ca, uint256 repay, uint256 minSeized, uint8 mode)
        internal view returns (bytes memory)
    {
        uint256 bps = IAavePoolFee(AAVE_V3_POOL).FLASHLOAN_PREMIUM_TOTAL();
        uint256 fee = (repay * bps + 10_000 - 1) / 10_000;
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, WETH, uint128(repay), 1, 1),
            PB.legGearbox(GB_FACADE, ca, WSTETH, uint128(repay), minSeized, mode),
            PB.poolSwap(WSTETH_WETH_001, WSTETH, WETH, PB.L_EXACT_OUT, uint128(repay + fee)),
            PB.profit(1, PB.poolSwap(WSTETH_WETH_001, WSTETH, WETH, PB.L_TAKE_BALANCE, 0))
        );
    }

    function _siloPlan(address user, uint256 repay) internal view returns (bytes memory) {
        uint256 bps = IAavePoolFee(AAVE_V3_POOL).FLASHLOAN_PREMIUM_TOTAL();
        uint256 fee = (repay * bps + 10_000 - 1) / 10_000;
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, WETH, uint128(repay), 1, 1),
            PB.legSilo(SILO_HOOK, user, WSTETH, uint128(repay)),
            PB.poolSwap(WSTETH_WETH_001, WSTETH, WETH, PB.L_EXACT_OUT, uint128(repay + fee)),
            PB.profit(1, PB.poolSwap(WSTETH_WETH_001, WSTETH, WETH, PB.L_TAKE_BALANCE, 0))
        );
    }
}
