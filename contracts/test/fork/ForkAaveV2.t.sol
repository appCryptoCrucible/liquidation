// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20V2 {
    function balanceOf(address) external view returns (uint256);
    function allowance(address, address) external view returns (uint256);
}

interface IAaveV2Oracle {
    function getAssetPrice(address asset) external view returns (uint256);
}

interface IAaveV2Pool {
    function getUserAccountData(address user)
        external
        view
        returns (uint256, uint256, uint256, uint256, uint256, uint256 healthFactor);
}

/*
 * Plan Phase 2, step 1: does the Executor's Aave V3 leg liquidate an Aave V2
 * position unchanged? V2's `liquidationCall(collateral, debt, user,
 * debtToCover, receiveAToken)` has V3's selector and argument order.
 *
 * The position is real: the V2 liquidation in tx 0x0f8b8300… (block
 * 26,104,558, index 211). The borrower crossed inside that block, when tx
 * 0x7a4c412a… (index 197) moved V2's ENJ price from 13,965,324,765,728 to
 * 14,385,123,000,000 wei. The fork stands at block N−1, moved to block N's
 * number and time, with only that price update replayed (`vm.transact`;
 * replaying all 211 transactions over RPC times out). The seizure depends on
 * the two prices and the bonus, not on the other transactions. Our Executor
 * repays exactly what the winner repaid. Oracle: the winner's own
 * `LiquidationCall` — the same `debtToCover` must seize the same collateral.
 */
contract ForkAaveV2Test is Test {
    uint256 constant BLOCK_N = 26_104_558;
    uint256 constant BLOCK_N_TIME = 1_790_943_107;
    bytes32 constant PRICE_UPDATE_TX = 0x7a4c412a454220bbb080979b358fb1ab218dd30faf3d646cc63bed533d8e9d1e;
    /// V2's `AaveOracle` and ENJ's price in it at the end of block N.
    address constant V2_ORACLE = 0xA50ba011c48153De246E5192C8f9258A2ba79Ca9;
    uint256 constant ENJ_PRICE_AT_N = 14_385_123_000_000;
    address constant V2_POOL = 0x7d2768dE32b0b80b7a3454c06BdAc94A69DDc7A9;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant ENJ = 0xF629cBd94d3791C9250152BD8dfBDF380E2a3B9c;
    /// Uniswap V3 ENJ/WETH 0.3%: the flash swap's pool and the profit leg's.
    address constant ENJ_WETH_03 = 0xe16Be1798F860bC1EB0FEb64cD67Ca00AE9b6E58;
    address constant BORROWER = 0x63376496bF14Eeb84CD742ae19f2e15308bbABF3;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    /// The winner's `LiquidationCall`: debtToCover and liquidatedCollateralAmount.
    uint128 constant REPAID = 118_699_794_509_643_889_672;
    uint256 constant SEIZED = 1_792_886_701_300_749;
    bytes32 constant LIQUIDATION_CALL =
        keccak256("LiquidationCall(address,address,address,uint256,uint256,address,bool)");

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrunOperator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, BLOCK_N - 1);
        vm.roll(BLOCK_N);
        vm.warp(BLOCK_N_TIME);
        vm.transact(PRICE_UPDATE_TX);
        forked = true;
        ex = ExecutorStack.deploy(operator, backrunOperator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH, MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY);
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    function test_fork_v2_pool_liquidates_through_the_v3_leg() public onFork {
        assertEq(IAaveV2Oracle(V2_ORACLE).getAssetPrice(ENJ), ENJ_PRICE_AT_N, "chain: block N's ENJ price");
        (,,,,, uint256 hf) = IAaveV2Pool(V2_POOL).getUserAccountData(BORROWER);
        assertLt(hf, 1e18, "chain: liquidatable after the price update");

        // Flash-swap the ENJ out of the ENJ/WETH pool, liquidate on the V2
        // pool through the V3 leg, pay the pool in seized WETH; any ENJ left
        // is sold to WETH.
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_UNIV3_SWAP, ENJ_WETH_03, ENJ, REPAID, 1, 0),
            PB.legV3(V2_POOL, BORROWER, WETH, REPAID),
            PB.profit(1, PB.poolSwap(ENJ_WETH_03, ENJ, WETH, PB.L_TAKE_BALANCE, 0))
        );
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(plan);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != V2_POOL || logs[i].topics[0] != LIQUIDATION_CALL) continue;
            (uint256 debtToCover, uint256 seized, address liquidator, bool receiveAToken) =
                abi.decode(logs[i].data, (uint256, uint256, address, bool));
            assertEq(address(uint160(uint256(logs[i].topics[1]))), WETH, "collateral");
            assertEq(address(uint160(uint256(logs[i].topics[2]))), ENJ, "debt");
            assertEq(address(uint160(uint256(logs[i].topics[3]))), BORROWER, "user");
            assertEq(debtToCover, REPAID, "the winner's repay");
            assertEq(seized, SEIZED, "the same repay seizes what the winner's did");
            assertEq(liquidator, address(ex), "liquidated by our Executor");
            assertFalse(receiveAToken, "seized as the underlying");
            found = true;
        }
        assertTrue(found, "the V2 pool emitted LiquidationCall");
        assertEq(IERC20V2(WETH).balanceOf(address(ex)), 0, "weth left");
        assertEq(IERC20V2(ENJ).balanceOf(address(ex)), 0, "enj left");
        assertEq(IERC20V2(ENJ).allowance(address(ex), V2_POOL), 0, "pool allowance");
    }
}
