// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20V4 {
    function balanceOf(address) external view returns (uint256);
    function allowance(address, address) external view returns (uint256);
}

interface ICTokenV4 {
    function accrueInterest() external returns (uint256);
}

interface IComptrollerV4 {
    function getAccountLiquidity(address) external view returns (uint256, uint256, uint256);
    function liquidateCalculateSeizeTokens(address cBorrowed, address cCollateral, uint256 repay)
        external view returns (uint256, uint256);
}

/*
 * Swap venue 9: a Uniswap V4 pool by its key (decision 7: hookless or an
 * allowlisted hook).
 *
 * The position is real: Capyfi borrower 0x677f6990…, short at block
 * 25,586,282, was liquidated for 100 USDC against its LAC collateral at
 * 25,586,283, and the winner sold the LAC on the hookless LAC/USDC 1 % V4
 * pool, the only pool LAC has. Our Executor does the same liquidation and
 * pays it back through that pool:
 *  - outside any unlock (a Morpho flash): the leg unlocks the PoolManager
 *    itself, exact output for the repay and exact input for the rest;
 *  - inside a V4 flash's unlock: the leg swaps and settles directly.
 * Oracle: the Comptroller's own `liquidateCalculateSeizeTokens` on the same
 * state, and every balance the Executor held returning to zero.
 *
 * At this block the pool prices LAC about 3.8 % under Capyfi's oracle, more
 * than the bonus left after the 2.8 % protocol seize share: buying the 100
 * USDC back takes 10,552 LAC and the liquidation seizes 10,543.6. The test
 * is of the venue, not of this position's margin, so the Executor starts
 * with 1,000 LAC; the profit leg sells what is left.
 */
contract ForkUniV4SwapTest is Test {
    uint256 constant PARENT = 25_586_282;
    address constant COMPTROLLER = 0x0b9af1fd73885aD52680A1aeAa7A3f17AC702afA;
    address constant CA_USDC = 0xc3aD34De18B59A24BD0877e454Fb924181F09C8f;
    address constant CA_LAC = 0x0568F6cb5A0E84FACa107D02f81ddEB1803f3B50;
    address constant BORROWER = 0x677f699053987fA3F6C52506b4C9317bBF63aF47;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant LAC = 0x0Df3a853e4B604fC2ac0881E9Dc92db27fF7f51b;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    /// LAC/USDC V4 pool: LAC < USDC, 1 % LP fee, tick spacing 200, no hook.
    uint24 constant LAC_FEE = 10_000;
    int24 constant LAC_SPACING = 200;
    uint128 constant REPAY = 100e6;
    bytes32 constant LIQUIDATE_BORROW =
        keccak256("LiquidateBorrow(address,address,uint256,address,uint256)");

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrunOperator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PARENT);
        forked = true;
        ex = ExecutorStack.deploy(operator, backrunOperator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH, MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY);
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    function _lacSwap(address hooks, address tIn, address tOut, uint8 flags, uint128 amount)
        internal pure returns (bytes memory)
    {
        return PB.v4Swap(LAC, USDC, LAC_FEE, LAC_SPACING, hooks, tIn, tOut, flags, amount);
    }

    function _plan(uint8 provider, address source, address hooks) internal pure returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(provider, source, USDC, REPAY, 1, 1),
            PB.legCompound(CA_USDC, BORROWER, LAC, REPAY, CA_LAC, 0),
            _lacSwap(hooks, LAC, USDC, PB.L_EXACT_OUT, REPAY),
            PB.profit(2, bytes.concat(
                _lacSwap(hooks, LAC, USDC, PB.L_TAKE_BALANCE, 0),
                PB.poolSwap(USDC_WETH_005, USDC, WETH, PB.L_TAKE_BALANCE, 0)
            ))
        );
    }

    /// The seize the Comptroller computes for our repay on this state, with
    /// both markets accrued as `liquidateBorrow` accrues them.
    function _expectedSeize() internal returns (uint256) {
        (, , uint256 shortfall) = IComptrollerV4(COMPTROLLER).getAccountLiquidity(BORROWER);
        assertGt(shortfall, 0, "chain: the borrower is short at the parent block");
        ICTokenV4(CA_USDC).accrueInterest();
        ICTokenV4(CA_LAC).accrueInterest();
        (uint256 err, uint256 seize) =
            IComptrollerV4(COMPTROLLER).liquidateCalculateSeizeTokens(CA_USDC, CA_LAC, REPAY);
        assertEq(err, 0, "chain: seize computable");
        return seize;
    }

    function _run(bytes memory plan, uint256 expectedSeize) internal {
        deal(LAC, address(ex), 1_000e18);
        uint256 sinkBefore = IERC20V4(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(plan);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != CA_USDC || logs[i].topics[0] != LIQUIDATE_BORROW) continue;
            (address liquidator, address borrower, uint256 repay, address cColl, uint256 seize) =
                abi.decode(logs[i].data, (address, address, uint256, address, uint256));
            assertEq(liquidator, address(ex));
            assertEq(borrower, BORROWER);
            assertEq(repay, REPAY);
            assertEq(cColl, CA_LAC);
            assertEq(seize, expectedSeize, "the Comptroller's own seize");
            found = true;
        }
        assertTrue(found, "liquidated");
        assertGt(IERC20V4(WETH).balanceOf(sink), sinkBefore, "WETH profit");
        assertEq(IERC20V4(LAC).balanceOf(address(ex)), 0, "LAC left");
        assertEq(IERC20V4(USDC).balanceOf(address(ex)), 0, "USDC left");
        assertEq(IERC20V4(WETH).balanceOf(address(ex)), 0, "WETH left");
        assertEq(address(ex).balance, 0, "ETH left");
    }

    function test_fork_v4_legs_unlock_on_their_own_under_a_morpho_flash() public onFork {
        uint256 seize = _expectedSeize();
        _run(_plan(PB.P_MORPHO, MORPHO, address(0)), seize);
    }

    function test_fork_v4_legs_swap_inside_a_v4_flash_unlock() public onFork {
        uint256 seize = _expectedSeize();
        _run(_plan(PB.P_UNIV4, MainnetVenues.V4_POOL_MANAGER, address(0)), seize);
    }

    /// The hook rule reads the swap permission bits of the address: none,
    /// or only liquidity / initialize / donate permissions, may swap; any of
    /// `beforeSwap`, `afterSwap` or their delta returns may not. Oracle:
    /// v4-core `Hooks.sol` flag values.
    function test_v4_hook_rule_reads_the_swap_permission_bits() public pure {
        assertTrue(MainnetVenues.v4HookAllowed(address(0)));
        // beforeRemoveLiquidity | afterRemoveLiquidity (1 << 9 | 1 << 8)
        assertTrue(MainnetVenues.v4HookAllowed(address(0xaBcd000000000000000000000000000000000300)));
        // beforeInitialize (1 << 13)
        assertTrue(MainnetVenues.v4HookAllowed(address(0xabCD000000000000000000000000000000002000)));
        assertFalse(MainnetVenues.v4HookAllowed(address(0xaBcD000000000000000000000000000000000080))); // beforeSwap
        assertFalse(MainnetVenues.v4HookAllowed(address(0xaBcd000000000000000000000000000000000040))); // afterSwap
        assertFalse(MainnetVenues.v4HookAllowed(address(0xAbcD000000000000000000000000000000000008))); // beforeSwapReturnsDelta
        assertFalse(MainnetVenues.v4HookAllowed(address(0xaBCd000000000000000000000000000000000004))); // afterSwapReturnsDelta
    }

    /// A pool whose hook is not allowlisted is refused before any token
    /// moves; the leg names it in the error.
    function test_fork_v4_hooked_pool_is_refused() public onFork {
        address hook = address(0x1234000000000000000000000000000000000080);
        bytes memory plan = _plan(PB.P_MORPHO, MORPHO, hook);
        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Executor.BadPool.selector, uint8(9), hook));
        ex.execute(plan);
    }
}
