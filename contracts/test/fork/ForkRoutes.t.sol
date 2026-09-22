// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {IAavePool, IUniV3Pool} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20B {
    function balanceOf(address) external view returns (uint256);
    function allowance(address, address) external view returns (uint256);
    function decimals() external view returns (uint8);
}

interface IPoolEx {
    function supply(address asset, uint256 amount, address onBehalfOf, uint16) external;
    function borrow(address asset, uint256 amount, uint256 rateMode, uint16, address onBehalfOf) external;
    function ADDRESSES_PROVIDER() external view returns (address);
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

interface IAaveOracle {
    function getAssetPrice(address) external view returns (uint256);
}

interface IPoolAddressesProvider {
    function getPriceOracle() external view returns (address);
}

interface ISwapRouter02 {
    struct ExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountOut;
        uint256 amountInMaximum;
        uint160 sqrtPriceLimitX96;
    }

    function exactOutputSingle(ExactOutputSingleParams calldata) external payable returns (uint256);
}

/*
 * TESTING.md §2–§3, Executor row. Fork only. No mock token, no mock pool.
 *
 * Oracles used below, named on each assertion:
 *   chain     — balanceOf / allowance / the pool's own Swap log
 *   chain     — Aave `liquidationCall` sizes the repay; that is the protocol,
 *               not Executor
 *   invariant — after execute, every token the plan moved is gone from the
 *               executor; profit sits in the sink as WETH
 *   spec      — a router that is neither constructor address reverts
 *               RouterNotAllowed (TESTING.md: "a test that a non-allowlisted
 *               router reverts")
 *
 * Share redeem (Compound, Euler) and Liquity gas compensation are proved in
 * ForkShareRedeem.t.sol. This file is the router path only.
 */
contract ForkRoutesTest is Test {
    uint256 constant PINNED_BLOCK = 26_019_284;

    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant DAI = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;

    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;
    address constant DAI_WETH_005 = 0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8;
    address constant USDT_WETH_005 = 0x11b815efB8f581194ae79006d24E0d814B7697F6;
    address constant SWAP_ROUTER02 = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;

    /// keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
    bytes32 constant UNIV3_SWAP = 0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67;

    address operator = makeAddr("operator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string("https://ethereum-rpc.publicnode.com"));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = new Executor(operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, SWAP_ROUTER02, makeAddr("routerB"), WETH);
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    /// Two pool-direct legs, second leg's tokenIn is the first leg's tokenOut.
    /// Repay is the real USDT/WETH pool. Profit round-trips leftover WETH
    /// through the real USDC/WETH pool and back, so both legs have to run.
    function test_fork_two_hop_usdt_then_usdc() public onFork {
        address user = makeAddr("twohop");
        _openAaveV3(user, WETH, USDT, 5e18);
        // Chain: ask for more than Aave will take. `pulled` is what liquidationCall
        // actually transferred. The leg approves twice that, so a residual
        // allowance remains unless safeApprove zeroes it. Real USDT reverts
        // on a later non-zero approve if that zeroing write is missing
        // (TESTING.md mutation 9).
        (uint256 pulled,) = _probeAave(user, WETH, USDT, 1_000_000e6);
        uint128 buy = uint128(pulled + _aaveFee(pulled));
        uint128 approved = uint128(pulled * 2);

        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, USDT, uint128(pulled), 1, 1),
            PB.legV3(AAVE_V3_POOL, user, WETH, approved),
            PB.poolSwap(USDT_WETH_005, WETH, USDT, PB.L_EXACT_OUT, buy),
            PB.profit(
                2,
                bytes.concat(
                    PB.poolSwap(USDC_WETH_005, WETH, USDC, PB.L_TAKE_BALANCE, 0),
                    PB.poolSwap(USDC_WETH_005, USDC, WETH, PB.L_TAKE_BALANCE, 0)
                )
            )
        );

        uint256 sinkBefore = IERC20B(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(plan);

        // chain: the USDC/WETH pool logged two swaps whose sender is the executor
        // (pool-direct). One swap, or a sender that is not the executor, means a hop did not run.
        uint256 hops;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != USDC_WETH_005 || logs[i].topics[0] != UNIV3_SWAP) continue;
            if (address(uint160(uint256(logs[i].topics[1]))) == address(ex)) ++hops;
        }
        assertEq(hops, 2, "chain: USDC/WETH pool did not see both hops");

        // invariant + chain: profit is WETH at the sink, and no touched token remains
        assertGt(IERC20B(WETH).balanceOf(sink), sinkBefore, "chain: sink WETH did not increase");
        assertEq(IERC20B(WETH).balanceOf(address(ex)), 0, "chain: WETH left on executor");
        assertEq(IERC20B(USDT).balanceOf(address(ex)), 0, "chain: USDT left on executor");
        assertEq(IERC20B(USDC).balanceOf(address(ex)), 0, "chain: USDC left between hops");
        assertEq(IERC20B(USDT).allowance(address(ex), AAVE_V3_POOL), 0, "chain: USDT allowance survives");
    }

    /// Repay through the real SwapRouter02. Input cap is the collateral Aave
    /// actually seized on a reverted probe, not a number Executor computed.
    function test_fork_router_exact_out_repay() public onFork {
        address user = makeAddr("router");
        _openAaveV3(user, WETH, DAI, 5e18);
        uint256 repayAsk = _aaveDebt(user, DAI) / 5;
        (uint256 pulled, uint256 seized) = _probeAave(user, WETH, DAI, repayAsk);
        uint128 buy = uint128(pulled + _aaveFee(pulled));
        uint24 feeTier = IUniV3Pool(DAI_WETH_005).fee();

        bytes memory callData = abi.encodeCall(
            ISwapRouter02.exactOutputSingle,
            (ISwapRouter02.ExactOutputSingleParams({
                tokenIn: WETH,
                tokenOut: DAI,
                fee: feeTier,
                recipient: address(ex),
                amountOut: buy,
                amountInMaximum: seized,
                sqrtPriceLimitX96: 0
            }))
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, DAI, uint128(pulled), 1, 1),
            PB.legV3(AAVE_V3_POOL, user, WETH, uint128(pulled)),
            PB.routerSwap(SWAP_ROUTER02, WETH, DAI, PB.L_EXACT_OUT, uint128(seized), callData),
            PB.profit(1, PB.poolSwap(DAI_WETH_005, DAI, WETH, PB.L_TAKE_BALANCE, 0))
        );

        uint256 sinkBefore = IERC20B(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(plan);

        // chain: the pool's Swap log names the router as sender, not the executor
        bool routerSwapped;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != DAI_WETH_005 || logs[i].topics[0] != UNIV3_SWAP) continue;
            if (address(uint160(uint256(logs[i].topics[1]))) == SWAP_ROUTER02) routerSwapped = true;
        }
        assertTrue(routerSwapped, "chain: DAI/WETH swap was not sent by SwapRouter02");

        assertGt(IERC20B(WETH).balanceOf(sink), sinkBefore, "chain: sink WETH did not increase");
        assertEq(IERC20B(WETH).balanceOf(address(ex)), 0, "chain: WETH left on executor");
        assertEq(IERC20B(DAI).balanceOf(address(ex)), 0, "chain: DAI left on executor");
        assertEq(IERC20B(WETH).allowance(address(ex), SWAP_ROUTER02), 0, "chain: router allowance survives");
        assertEq(IERC20B(DAI).allowance(address(ex), AAVE_V3_POOL), 0, "chain: flash allowance survives");
    }

    /// Negative. A router outside the two constructor addresses reverts.
    /// Oracle: TESTING.md executor row, run on the fork against this Executor.
    function test_fork_unknown_router_reverts() public onFork {
        address user = makeAddr("badrouter");
        _openAaveV3(user, WETH, DAI, 5e18);
        uint256 repayAsk = _aaveDebt(user, DAI) / 5;
        (uint256 pulled, uint256 seized) = _probeAave(user, WETH, DAI, repayAsk);
        uint128 buy = uint128(pulled + _aaveFee(pulled));
        address stranger = makeAddr("stranger");
        bytes memory callData = abi.encodeCall(
            ISwapRouter02.exactOutputSingle,
            (ISwapRouter02.ExactOutputSingleParams({
                tokenIn: WETH,
                tokenOut: DAI,
                fee: 500,
                recipient: address(ex),
                amountOut: buy,
                amountInMaximum: seized,
                sqrtPriceLimitX96: 0
            }))
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, DAI, uint128(pulled), 1, 1),
            PB.legV3(AAVE_V3_POOL, user, WETH, uint128(pulled)),
            PB.routerSwap(stranger, WETH, DAI, PB.L_EXACT_OUT, uint128(seized), callData),
            PB.profit(0, "")
        );
        vm.prank(operator);
        vm.expectRevert(abi.encodeWithSelector(Executor.RouterNotAllowed.selector, stranger));
        ex.execute(plan);
    }

    function _openAaveV3(address user, address coll, address debt, uint256 collAmt) internal {
        deal(coll, user, collAmt);
        _approve(coll, user, AAVE_V3_POOL, collAmt);
        vm.prank(user);
        IPoolEx(AAVE_V3_POOL).supply(coll, collAmt, user, 0);
        (,, uint256 avail,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
        uint256 px = _aavePrice(debt);
        uint256 borrowAmt = avail * (10 ** uint256(IERC20B(debt).decimals())) / px;
        borrowAmt = borrowAmt * 97 / 100;
        require(borrowAmt > 0, "v3 borrow 0");
        vm.prank(user);
        IPoolEx(AAVE_V3_POOL).borrow(debt, borrowAmt, 2, 0, user);
        vm.warp(block.timestamp + 2500 days);
        (,,,,, uint256 hf) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
        require(hf < 1e18, "v3 still healthy");
    }

    function _approve(address token, address owner, address spender, uint256 amt) internal {
        vm.startPrank(owner);
        if (token == USDT) {
            (bool z,) = token.call(abi.encodeWithSelector(bytes4(keccak256("approve(address,uint256)")), spender, 0));
            require(z, "usdt0");
        }
        (bool ok,) = token.call(abi.encodeWithSelector(bytes4(keccak256("approve(address,uint256)")), spender, amt));
        require(ok, "approve");
        vm.stopPrank();
    }

    function _aavePrice(address asset) internal view returns (uint256) {
        address oracle = IPoolAddressesProvider(IPoolEx(AAVE_V3_POOL).ADDRESSES_PROVIDER()).getPriceOracle();
        uint256 px = IAaveOracle(oracle).getAssetPrice(asset);
        require(px != 0, "oracle");
        return px;
    }

    function _aaveDebt(address user, address debt) internal view returns (uint256) {
        (, uint256 debtBase,,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
        return debtBase * (10 ** uint256(IERC20B(debt).decimals())) / _aavePrice(debt);
    }

    function _aaveFee(uint256 amount) internal view returns (uint256) {
        uint256 bps = IPoolEx(AAVE_V3_POOL).FLASHLOAN_PREMIUM_TOTAL();
        return (amount * bps + 9_999) / 10_000;
    }

    /// Chain oracle for the repay size: Aave's own liquidationCall, reverted.
    function _probeAave(address user, address coll, address debt, uint256 repay)
        internal
        returns (uint256 pulled, uint256 seized)
    {
        address probe = makeAddr("probe");
        uint256 snap = vm.snapshotState();
        deal(debt, probe, repay * 2);
        _approve(debt, probe, AAVE_V3_POOL, repay * 2);
        uint256 d0 = IERC20B(debt).balanceOf(probe);
        uint256 c0 = IERC20B(coll).balanceOf(probe);
        vm.prank(probe);
        IAavePool(AAVE_V3_POOL).liquidationCall(coll, debt, user, repay, false);
        pulled = d0 - IERC20B(debt).balanceOf(probe);
        seized = IERC20B(coll).balanceOf(probe) - c0;
        vm.revertToState(snap);
        require(pulled > 0 && seized > 0, "probe empty");
    }
}
