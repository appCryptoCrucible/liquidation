// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {
    MockERC20, MockWETH, MockAavePool, MockV4Spoke, MockMorpho, MockUniV3Factory, MockUniV3Pool,
    MockPoolManager, MockDssFlash, MockRouter, ExpensiveCoinbase
} from "./Mocks.sol";

/// Relays `execute` so a counterparty can attempt re-entry as the operator.
contract OperatorRelay {
    Executor public immutable ex;
    constructor(Executor e) { ex = e; }
    function run(bytes calldata plan) external payable { ex.execute{value: msg.value}(plan); }
}

/*
 * Shared world for the Executor unit suite.
 *
 *   DEBT  6 dp (USDC-like)      COLL 8 dp (WBTC-like)      WETH 18 dp
 *   1 COLL = 60_000 DEBT        1 COLL = 20 WETH
 *
 * Reference position: 30_000 DEBT of debt, 0.55 COLL seized (10 % bonus).
 * Flash 30_000 DEBT from Aave at 5 bps → owe 30_015. Repay swap buys exactly
 * 30_015 DEBT with 0.50025 COLL; 0.04975 COLL is left → 0.995 WETH gross.
 */
abstract contract ExecutorTestBase is Test {
    uint128 constant REPAY      = 30_000e6;
    uint128 constant OWED       = 30_015e6;          // + 5 bps
    uint128 constant COLL_OUT   = 0.55e8;
    uint128 constant COLL_SPENT = 50_025_000;        // ceil(30_015e6 · 1e8 / 60_000e6)
    uint128 constant COLL_LEFT  = COLL_OUT - COLL_SPENT;
    uint128 constant GROSS_WETH = 0.995e18;          // COLL_LEFT · 2e11
    uint128 constant GAS_COST   = 0.05e18;
    uint128 constant NET        = GROSS_WETH - GAS_COST;

    address public operator = makeAddr("operator");
    address public sink     = makeAddr("sink");
    address borrower = makeAddr("borrower");
    address stranger = makeAddr("stranger");

    MockWETH public weth; MockERC20 public debt; MockERC20 public coll;
    MockAavePool public pool; MockV4Spoke public spoke; MockMorpho public morpho; MockPoolManager public pm;
    MockUniV3Factory public factory; MockUniV3Pool public pCollDebt; MockUniV3Pool public pCollWeth; MockUniV3Pool public pDebtWeth;
    MockRouter public routerA; MockRouter public routerB;
    ExpensiveCoinbase public coinbase;
    Executor public ex;

    function setUp() public virtual {
        weth = new MockWETH();
        debt = new MockERC20("DEBT", 6);
        coll = new MockERC20("COLL", 8);

        factory   = new MockUniV3Factory();
        pCollDebt = MockUniV3Pool(factory.deploy(address(coll), address(debt), 3000));
        pCollWeth = MockUniV3Pool(factory.deploy(address(coll), address(weth), 3000));
        pDebtWeth = MockUniV3Pool(factory.deploy(address(debt), address(weth), 500));
        _price(pCollDebt, address(coll), address(debt), 600, 1);       // 60_000e6 / 1e8
        _price(pCollWeth, address(coll), address(weth), 2e11, 1);      // 20e18 / 1e8
        _price(pDebtWeth, address(debt), address(weth), 1e12, 3000);   // 1e18 / 3000e6

        pool = new MockAavePool(); spoke = new MockV4Spoke(); morpho = new MockMorpho(); pm = new MockPoolManager();
        routerA = new MockRouter(); routerB = new MockRouter();
        coinbase = new ExpensiveCoinbase();
        vm.coinbase(address(coinbase));

        ex = new Executor(
            operator, sink, address(factory), factory.initHash(),
            address(routerA), address(routerB), address(weth)
        );

        // Liquidity everywhere a counterparty must pay out.
        debt.mint(address(pool), 1e15); debt.mint(address(pCollDebt), 1e15); debt.mint(address(pDebtWeth), 1e15);
        debt.mint(address(pm), 1e15);   debt.mint(address(morpho), 1e15);
        coll.mint(address(pool), 1e12); coll.mint(address(spoke), 1e12); coll.mint(address(morpho), 1e12);
        weth.mint(address(pCollWeth), 1e24); weth.mint(address(pDebtWeth), 1e24); weth.mint(address(routerA), 1e24);
        vm.deal(address(weth), 3e24); // backs the minted WETH so withdraw() can pay

        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
    }

    /// raw `b` per raw `a` = num/den, whatever the token ordering.
    function _price(MockUniV3Pool p, address a, address, uint256 num, uint256 den) internal {
        if (p.token0() == a) p.setRate(num, den); else p.setRate(den, num);
    }

    // ── plan assembly ──────────────────────────────────────────────────

    function _repayLeg() internal view returns (bytes memory) {
        return PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, OWED);
    }

    function _profitLeg() internal view returns (bytes memory) {
        return PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
    }

    /// One Aave-flash group over `legs`, one repay leg, one profit leg.
    function _plan(uint8 flags, uint16 bidBps, uint128 gasCost, uint128 minProfit, uint8 liqCount, bytes memory legs)
        internal view returns (bytes memory)
    {
        return bytes.concat(
            PB.header(flags, bidBps, gasCost, minProfit, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, liqCount, 1),
            legs, _repayLeg(),
            PB.profit(1, _profitLeg())
        );
    }

    function _refPlan() internal view returns (bytes memory) {
        return _plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1, PB.legV3(address(pool), borrower, address(coll), REPAY));
    }

    function _exec(bytes memory plan) internal {
        vm.prank(operator);
        ex.execute(plan);
    }

    /// Nothing may remain in the Executor and no counterparty may keep an
    /// allowance — the post-conditions every successful run must satisfy.
    function _assertClean() internal view {
        assertEq(weth.balanceOf(address(ex)), 0, "weth stuck");
        assertEq(coll.balanceOf(address(ex)), 0, "coll stuck");
        assertEq(debt.allowance(address(ex), address(pool)),   0, "pool allowance");
        assertEq(debt.allowance(address(ex), address(spoke)),  0, "spoke allowance");
        assertEq(debt.allowance(address(ex), address(morpho)), 0, "morpho allowance");
        assertEq(coll.allowance(address(ex), address(routerA)), 0, "router allowance");
    }
}
