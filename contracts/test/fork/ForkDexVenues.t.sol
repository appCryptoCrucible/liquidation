// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IAavePool} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20D {
    function balanceOf(address) external view returns (uint256);
    function decimals() external view returns (uint8);
}

interface IPoolD {
    function supply(address asset, uint256 amount, address onBehalfOf, uint16) external;
    function borrow(address asset, uint256 amount, uint256 rateMode, uint16, address onBehalfOf) external;
    function ADDRESSES_PROVIDER() external view returns (address);
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

interface IAaveOracleD {
    function getAssetPrice(address) external view returns (uint256);
}

interface IProviderD {
    function getPriceOracle() external view returns (address);
}

interface IWeightedPool {
    function getPoolId() external view returns (bytes32);
}

interface IFluidDex {
    function swapIn(bool swap0to1, uint256 amountIn, uint256 amountOutMin, address to) external payable returns (uint256);
}

/*
 * The venues added with the third swap module — SushiSwap V3, PancakeSwap V3
 * (pools with a live `lmPool`), Balancer V2 weighted pools and Fluid DEX —
 * as the exit of a real Aave V3 liquidation on a mainnet fork, through the
 * real Executor and the real pools: no mock token, no mock pool.
 *
 * Oracles, named on each assertion:
 *   chain — the venue's own log shows the swap ran from the executor (V3
 *           forks: Swap with the executor as sender; Balancer: the Vault's
 *           Swap for the pool id; Fluid: the Liquidity layer's LogOperate by
 *           the pool)
 *   chain — Aave's `liquidationCall` sizes the repay (probed, reverted); the
 *           flash repay is Aave's own check
 *   invariant — after execute nothing the plan moved stays on the executor
 *   chain — the sink gained WETH profit
 *
 * Gas: run with `forge test --isolate --match-contract ForkDexVenues -vv`;
 * every test logs `gas execute`, and `test_gas_baseline_univ3` the same plan
 * through Uniswap V3. A venue's hop is priced as the baseline hop plus the
 * difference (config/liq-gas.toml [swap]).
 *
 * The plans here repay with the venue's pool as a hand-assembled leg; the
 * searcher-built plans over the same pools run in the historical replay
 * (`LIQ_REGISTRY_FILE=data/review/…-registry.candidate.json`).
 */
contract ForkDexVenuesTest is Test {
    uint256 constant PINNED_BLOCK = 26_149_895;

    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant DAI = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant WSTETH = 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0;

    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant SWAP_ROUTER02 = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;
    address constant UNIV3_USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;

    // candidate pools (data/review/2026-10-08-dex-candidates.json)
    address constant SUSHI_USDC_WETH_3000 = 0x763d3b7296e7C9718AD5B058aC2692A19E5b3638;
    address constant SUSHI_WETH_USDT_3000 = 0x6a11ED98B1a3ac36A768ebbbbA36DED101Da5a3f;
    address constant PANCAKE_USDC_WETH_500 = 0x1ac1A8FEaAEa1900C4166dEeed0C11cC10669D36;
    address constant PANCAKE_WETH_USDT_500 = 0x6CA298D2983aB03Aa1dA7679389D955A4eFEE15C;
    address constant BAL_USDC_WETH = 0x96646936b91d6B9D7D0c47C496AfBF3D6ec7B6f8;
    address constant BAL_DAI_WETH = 0x0b09deA16768f0799065C475bE02919503cB2a35;
    address constant FLUID_USDC_ETH = 0x836951EB21F3Df98273517B7249dCEFF270d34bf;
    address constant FLUID_WSTETH_ETH = 0x0B1a513ee24972DAEf112bC777a5610d4325C9e7;
    address constant FLUID_USDC_USDT = 0x667701e51B4D1Ca244F17C78F7aB8744B4C99F9B;
    address constant FLUID_LIQUIDITY = 0x52Aa899454998Be5b000Ad077a46Bbe360F4e497;
    address constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;

    uint8 constant S_BALANCER = 11;
    uint8 constant S_FLUID = 12;

    bytes32 constant UNIV3_SWAP = keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)");
    /// Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)
    bytes32 constant PANCAKE_SWAP = keccak256("Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)");
    /// Swap(bytes32,address,address,uint256,uint256)
    bytes32 constant BALANCER_SWAP = keccak256("Swap(bytes32,address,address,uint256,uint256)");
    /// LogOperate(address,address,int256,int256,address,address,uint256,uint256)
    bytes32 constant LOG_OPERATE = keccak256("LogOperate(address,address,int256,int256,address,address,uint256,uint256)");

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrunOperator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string("https://ethereum-rpc.publicnode.com"));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = ExecutorStack.deploy(
            operator, backrunOperator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, SWAP_ROUTER02, makeAddr("routerB"), WETH,
            MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY,
            MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY
        );
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    // ── the venues, each as the exit of a real liquidation ──────────────────

    address constant UNIV3_DAI_WETH_005 = 0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8;
    address constant UNIV3_USDT_WETH_005 = 0x11b815efB8f581194ae79006d24E0d814B7697F6;
    address constant UNIV3_WSTETH_WETH_001 = 0x109830a1AAaD605BbF02a9dFA7B0B92EC2FB7dAa;
    address constant UNIV3_USDC_USDT_001 = 0x3416cF6C708Da44DB2624D63ea0AAef7113527C6;

    function test_fork_sushi_v3_usdc_weth() public onFork {
        (uint256 pulled,) = _position(WETH, USDC);
        _exactOutVenue(
            USDC, pulled, UNIV3_USDC_WETH_005,
            PB.forkSwap(SUSHI_USDC_WETH_3000, 1, WETH, USDC, PB.L_EXACT_OUT, uint128(pulled)),
            SUSHI_USDC_WETH_3000, UNIV3_SWAP
        );
    }

    function test_fork_sushi_v3_weth_usdt() public onFork {
        (uint256 pulled,) = _position(WETH, USDT);
        _exactOutVenue(
            USDT, pulled, UNIV3_USDT_WETH_005,
            PB.forkSwap(SUSHI_WETH_USDT_3000, 1, WETH, USDT, PB.L_EXACT_OUT, uint128(pulled)),
            SUSHI_WETH_USDT_3000, UNIV3_SWAP
        );
    }

    /// Pancake's pool calls its `lmPool` inside `swap` (the pool's gas).
    function test_fork_pancake_v3_usdc_weth_with_lm_pool() public onFork {
        assertTrue(_lmPool(PANCAKE_USDC_WETH_500) != address(0), "chain: the pool has an lmPool");
        (uint256 pulled,) = _position(WETH, USDC);
        _exactOutVenue(
            USDC, pulled, UNIV3_USDC_WETH_005,
            PB.forkSwap(PANCAKE_USDC_WETH_500, 2, WETH, USDC, PB.L_EXACT_OUT, uint128(pulled)),
            PANCAKE_USDC_WETH_500, PANCAKE_SWAP
        );
    }

    function test_fork_pancake_v3_weth_usdt_with_lm_pool() public onFork {
        assertTrue(_lmPool(PANCAKE_WETH_USDT_500) != address(0), "chain: the pool has an lmPool");
        (uint256 pulled,) = _position(WETH, USDT);
        _exactOutVenue(
            USDT, pulled, UNIV3_USDT_WETH_005,
            PB.forkSwap(PANCAKE_WETH_USDT_500, 2, WETH, USDT, PB.L_EXACT_OUT, uint128(pulled)),
            PANCAKE_WETH_USDT_500, PANCAKE_SWAP
        );
    }

    function test_fork_balancer_usdc_weth_legacy() public onFork {
        // The legacy pools are thin: a small position.
        (uint256 pulled,) = _positionSized(WETH, USDC, 1e18);
        _balancer(USDC, pulled, UNIV3_USDC_WETH_005, IWeightedPool(BAL_USDC_WETH).getPoolId());
    }

    function test_fork_balancer_dai_weth_legacy() public onFork {
        (uint256 pulled,) = _positionSized(WETH, DAI, 1e18);
        _balancer(DAI, pulled, UNIV3_DAI_WETH_005, IWeightedPool(BAL_DAI_WETH).getPoolId());
    }

    /// Native ETH in: the module unwraps WETH and pays `msg.value`.
    function test_fork_fluid_native_in_weth_to_usdc() public onFork {
        (uint256 pulled, uint256 seized) = _position(WETH, USDC);
        // USDC is token0, ETH token1: WETH to USDC is 1 to 0. Leftover USDC
        // goes back to WETH through Uniswap.
        bytes memory profit = PB.profit(1, PB.poolSwap(UNIV3_USDC_WETH_005, USDC, WETH, PB.L_TAKE_BALANCE, 0));
        _fluid(WETH, USDC, pulled, seized, FLUID_USDC_ETH, false, UNIV3_USDC_WETH_005, profit);
    }

    /// Native ETH out: the module wraps what the pool paid.
    function test_fork_fluid_native_out_wsteth_to_weth() public onFork {
        (uint256 pulled, uint256 seized) = _position(WSTETH, WETH);
        // wstETH is token0, ETH token1: wstETH to WETH is 0 to 1. Leftover
        // wstETH goes to WETH through Uniswap.
        bytes memory profit = PB.profit(1, PB.poolSwap(UNIV3_WSTETH_WETH_001, WSTETH, WETH, PB.L_TAKE_BALANCE, 0));
        _fluid(WSTETH, WETH, pulled, seized, FLUID_WSTETH_ETH, true, UNIV3_WSTETH_WETH_001, profit);
    }

    /// Token for token, both ERC-20: approve exactly, cleared after.
    function test_fork_fluid_usdc_to_usdt() public onFork {
        (uint256 pulled, uint256 seized) = _position(USDC, USDT);
        // USDC is token0, USDT token1: USDC to USDT is 0 to 1. Leftovers of
        // both go to WETH through Uniswap.
        bytes memory profit = PB.profit(
            2,
            bytes.concat(
                PB.poolSwap(UNIV3_USDC_WETH_005, USDC, WETH, PB.L_TAKE_BALANCE, 0),
                PB.poolSwap(UNIV3_USDT_WETH_005, USDT, WETH, PB.L_TAKE_BALANCE, 0)
            )
        );
        _fluid(USDC, USDT, pulled, seized, FLUID_USDC_USDT, true, UNIV3_USDC_USDT_001, profit);
    }

    // ── plan assembly ───────────────────────────────────────────────────────

    /// Collateral WETH sold exact-out for `debt` through `swapV`; the same
    /// plan through Uniswap V3 (`basePool`) is the gas baseline.
    function _exactOutVenue(
        address debt, uint256 pulled, address basePool, bytes memory swapV, address pool, bytes32 topic
    ) internal {
        bytes memory base = PB.poolSwap(basePool, WETH, debt, PB.L_EXACT_OUT, uint128(pulled));
        Vm.Log[] memory logs = _compare(WETH, debt, pulled, swapV, base, PB.profit(0, ""));
        uint256 n;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != pool || logs[i].topics[0] != topic) continue;
            if (address(uint160(uint256(logs[i].topics[1]))) == address(ex)) ++n;
        }
        assertEq(n, 1, "chain: the pool logged one swap from the executor");
    }

    function _balancer(address debt, uint256 pulled, address basePool, bytes32 id) internal {
        bytes memory swapV = PB.swap(S_BALANCER, WETH, debt, PB.L_EXACT_OUT, uint128(pulled), abi.encodePacked(id));
        bytes memory base = PB.poolSwap(basePool, WETH, debt, PB.L_EXACT_OUT, uint128(pulled));
        Vm.Log[] memory logs = _compare(WETH, debt, pulled, swapV, base, PB.profit(0, ""));
        uint256 n;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == BALANCER_VAULT && logs[i].topics[0] == BALANCER_SWAP && logs[i].topics[1] == id) ++n;
        }
        assertEq(n, 1, "chain: the Vault logged the swap for the pool id");
    }

    function _fluid(
        address coll, address debt, uint256 pulled, uint256 seized, address pool, bool zeroToOne,
        address basePool, bytes memory profit
    ) internal {
        // The pool is exact-input: sell enough collateral that the output
        // covers the pull and Aave's premium, sized on the pool's own
        // estimate (`to = ADDRESS_DEAD` reverts with the result).
        uint256 want = pulled + _aaveFee(pulled);
        uint256 probeIn = seized / 4;
        uint256 probeOut = _fluidEstimate(pool, zeroToOne, probeIn, coll == WETH);
        require(probeOut > 0, "estimate");
        uint256 sell = probeIn * want * 10_020 / probeOut / 10_000;
        require(sell <= seized, "collateral cannot cover the pool price");
        require(_fluidEstimate(pool, zeroToOne, sell, coll == WETH) >= want, "sized below the debt");
        bytes memory swapV =
            PB.swap(S_FLUID, coll, debt, 0, uint128(sell), abi.encodePacked(pool, uint8(zeroToOne ? 1 : 0)));
        // the baseline buys the pull from Uniswap, exact output (the profit
        // legs the leftovers need are the same in both plans; a TAKE_BALANCE
        // leg with nothing to take is skipped)
        bytes memory base = PB.poolSwap(basePool, coll, debt, PB.L_EXACT_OUT, uint128(pulled));
        Vm.Log[] memory logs = _compare(coll, debt, pulled, swapV, base, profit);
        uint256 n;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == FLUID_LIQUIDITY && logs[i].topics[0] == LOG_OPERATE
                && address(uint160(uint256(logs[i].topics[1]))) == pool) ++n;
        }
        assertGe(n, 2, "chain: the pool operated on the Liquidity layer for both tokens");
    }

    /// Run the venue's plan (with its logs and checks), then the baseline's
    /// from the same state; log both gas figures and their difference.
    function _compare(
        address coll, address debt, uint256 pulled, bytes memory swapV, bytes memory swapBase, bytes memory profit
    ) internal returns (Vm.Log[] memory logs) {
        uint256 snap = vm.snapshotState();
        uint256 sinkBefore = IERC20D(WETH).balanceOf(sink);
        vm.recordLogs();
        uint256 gV = _exec(coll, debt, pulled, swapV, profit);
        logs = vm.getRecordedLogs();
        assertGt(IERC20D(WETH).balanceOf(sink), sinkBefore, "chain: the sink gained WETH profit");
        assertEq(IERC20D(coll).balanceOf(address(ex)), 0, "invariant: collateral left on the executor");
        assertEq(IERC20D(debt).balanceOf(address(ex)), 0, "invariant: debt token left on the executor");
        assertEq(IERC20D(WETH).balanceOf(address(ex)), 0, "invariant: WETH left on the executor");
        vm.revertToState(snap);
        uint256 gB = _exec(coll, debt, pulled, swapBase, profit);
        emit log_named_uint("gas execute (venue)", gV);
        emit log_named_uint("gas execute (uniswap v3 baseline)", gB);
        emit log_named_int("gas hop vs uniswap v3", int256(gV) - int256(gB));
    }

    function _exec(address coll, address debt, uint256 pulled, bytes memory swap, bytes memory profit)
        internal
        returns (uint256 used)
    {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, debt, uint128(pulled), 1, 1),
            PB.legV3(AAVE_V3_POOL, _user, coll, uint128(pulled)),
            swap,
            profit
        );
        uint256 g = gasleft();
        vm.prank(operator);
        ex.execute(plan);
        used = g - gasleft();
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    address _user;

    function _position(address coll, address debt) internal returns (uint256 pulled, uint256 seized) {
        return _positionSized(coll, debt, coll == WETH || coll == WSTETH ? 5e18 : 15_000e6);
    }

    function _positionSized(address coll, address debt, uint256 collAmt) internal returns (uint256 pulled, uint256 seized) {
        _user = makeAddr("borrower");
        deal(coll, _user, collAmt);
        _approve(coll, _user, AAVE_V3_POOL, collAmt);
        vm.prank(_user);
        IPoolD(AAVE_V3_POOL).supply(coll, collAmt, _user, 0);
        (,, uint256 avail,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(_user);
        uint256 borrowAmt = avail * (10 ** uint256(IERC20D(debt).decimals())) / _aavePrice(debt);
        borrowAmt = borrowAmt * 99 / 100;
        vm.prank(_user);
        IPoolD(AAVE_V3_POOL).borrow(debt, borrowAmt, 2, 0, _user);
        vm.warp(block.timestamp + 9000 days);
        (,,,,, uint256 hf) = IAavePool(AAVE_V3_POOL).getUserAccountData(_user);
        require(hf < 1e18, "still healthy");
        (, uint256 debtBase,,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(_user);
        uint256 total = debtBase * (10 ** uint256(IERC20D(debt).decimals())) / _aavePrice(debt);
        (pulled, seized) = _probeAave(_user, coll, debt, total / 5);
    }

    function _probeAave(address user, address coll, address debt, uint256 repay)
        internal
        returns (uint256 pulled, uint256 seized)
    {
        address probe = makeAddr("probe");
        uint256 snap = vm.snapshotState();
        deal(debt, probe, repay * 2);
        _approve(debt, probe, AAVE_V3_POOL, repay * 2);
        uint256 d0 = IERC20D(debt).balanceOf(probe);
        uint256 c0 = IERC20D(coll).balanceOf(probe);
        vm.prank(probe);
        IAavePool(AAVE_V3_POOL).liquidationCall(coll, debt, user, repay, false);
        pulled = d0 - IERC20D(debt).balanceOf(probe);
        seized = IERC20D(coll).balanceOf(probe) - c0;
        vm.revertToState(snap);
        require(pulled > 0 && seized > 0, "probe empty");
    }

    function _fluidEstimate(address pool, bool zeroToOne, uint256 amountIn, bool nativeIn) internal returns (uint256 out) {
        uint256 snap = vm.snapshotState();
        vm.deal(address(this), amountIn + 1 ether);
        (bool ok, bytes memory r) = pool.call{value: nativeIn ? amountIn : 0}(
            abi.encodeCall(IFluidDex.swapIn, (zeroToOne, amountIn, 0, address(0xdEaD)))
        );
        vm.revertToState(snap);
        require(!ok && r.length == 36, "estimate shape");
        assembly { out := mload(add(r, 36)) }
    }

    function _lmPool(address pool) internal view returns (address lm) {
        (bool ok, bytes memory r) = pool.staticcall(abi.encodeWithSignature("lmPool()"));
        require(ok && r.length == 32, "lmPool");
        lm = abi.decode(r, (address));
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
        address oracle = IProviderD(IPoolD(AAVE_V3_POOL).ADDRESSES_PROVIDER()).getPriceOracle();
        uint256 px = IAaveOracleD(oracle).getAssetPrice(asset);
        require(px != 0, "oracle");
        return px;
    }

    function _aaveFee(uint256 amount) internal view returns (uint256) {
        uint256 bps = IPoolD(AAVE_V3_POOL).FLASHLOAN_PREMIUM_TOTAL();
        return (amount * bps + 9_999) / 10_000;
    }
}
