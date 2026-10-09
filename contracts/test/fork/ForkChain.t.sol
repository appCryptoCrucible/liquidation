// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {SwapModule} from "../../src/SwapModule.sol";
import {DexModule} from "../../src/DexModule.sol";
import {ISwapModule, T_ENTERED} from "../../src/lib/ExecutorShared.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20C {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
}

interface ICurveGetDy {
    function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
}

struct V4KeyC {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

struct V4QuoteSingle {
    V4KeyC poolKey;
    bool zeroForOne;
    uint128 exactAmount;
    bytes hookData;
}

/// Uniswap's V4Quoter (periphery `quoteExactInputSingle`).
interface IV4QuoterC {
    function quoteExactInputSingle(V4QuoteSingle memory params)
        external returns (uint256 amountOut, uint256 gasEstimate);
}

interface IQuoterV2 {
    function quoteExactOutput(bytes memory path, uint256 amountOut)
        external returns (uint256 amountIn, uint160[] memory, uint32[] memory, uint256);
    function quoteExactInput(bytes memory path, uint256 amountIn)
        external returns (uint256 amountOut, uint160[] memory, uint32[] memory, uint256);
}

interface IUniV2PairC {
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
}

/// Plays the Executor's part around the swap module, as the Executor does
/// it: marks itself inside an `execute`, delegatecalls `runSwaps`, relays a
/// V3 swap callback that carries a chain to `v3ChainCallback`, and answers
/// its V3 anchors. Every line of the venue under test is the module's own.
contract ChainHarness {
    address immutable MOD;
    address public immutable UNIV3_FACTORY;
    bytes32 public immutable UNIV3_POOL_INIT_HASH;

    constructor(address mod, address factory, bytes32 initHash) {
        MOD = mod;
        UNIV3_FACTORY = factory;
        UNIV3_POOL_INIT_HASH = initHash;
    }

    function run(bytes calldata legs, uint8 n) external {
        assembly { tstore(T_ENTERED, 1) }
        _call(abi.encodeCall(ISwapModule.runSwaps, (0, n, legs)));
        assembly { tstore(T_ENTERED, 0) }
    }

    /// A stray chain callback from inside an `execute` (the harness itself
    /// calls, no hop in flight).
    function strayCallback(int256 a0, int256 a1, bytes calldata data) external {
        assembly { tstore(T_ENTERED, 1) }
        this.uniswapV3SwapCallback(a0, a1, data);
        assembly { tstore(T_ENTERED, 0) }
    }

    /// A V4 hop's own unlock, as the Executor relays it: only the
    /// PoolManager, only while a leg runs.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        require(msg.sender == MainnetVenues.V4_POOL_MANAGER, "harness: PoolManager only");
        _call(abi.encodeCall(ISwapModule.v4SwapCallback, (data)));
        return "";
    }

    /// Native ETH a V4 hop takes and wraps.
    receive() external payable {}

    function uniswapV3SwapCallback(int256 a0, int256 a1, bytes calldata data) external {
        // The Executor's routing: a chain hop's data is not a 96-byte triple.
        require(data.length != 96, "harness: chain callbacks only");
        _call(abi.encodeCall(ISwapModule.v3ChainCallback, (a0, a1, data)));
    }

    function _call(bytes memory data) internal {
        (bool ok, bytes memory ret) = MOD.delegatecall(data);
        if (!ok) {
            assembly { revert(add(ret, 0x20), mload(ret)) }
        }
    }
}

/*
 * Swap venue 10 (`S_CHAIN`, coverage plan 4F): an exact-output chain
 * through Uniswap V3 and V2 hops, every intermediate amount the pools' own
 * answer at execution.
 *
 * Oracle: Uniswap's QuoterV2 `quoteExactOutput` on the same fork state (it
 * runs the pools' own swaps and reverts with the input), and the V2 pair's
 * `getAmountIn` on its live reserves. The chain must spend exactly that
 * input and deliver exactly the output, leaving nothing in between.
 */
contract ForkChainTest is Test {
    uint256 constant BLOCK = 26_100_000;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant WBTC = 0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599;
    address constant LINK = 0x514910771AF9Ca656af840dff83E8264EcF986CA;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant QUOTER_V2 = 0x61fFE014bA17989E743c5F6cB21bF9697530B21e;
    address constant V4_QUOTER = 0x52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203;
    /// Uniswap V2 USDC/WETH.
    address constant V2_USDC_WETH = 0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc;

    uint8 constant S_CHAIN = 10;
    uint8 constant HOP_V3 = 0;
    uint8 constant HOP_V2 = 2;

    ChainHarness h;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, BLOCK);
        forked = true;
        SwapModule mod = new SwapModule(
            WETH, makeAddr("routerA"), makeAddr("routerB"),
            MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH,
            MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH,
            MainnetVenues.CURVE_META_REGISTRY, address(new DexModule(WETH))
        );
        h = new ChainHarness(address(mod), UNIV3_FACTORY, UNIV3_INIT_HASH);
    }

    modifier onFork() {
        if (!forked) {
            vm.skip(true);
            return;
        }
        _;
    }

    /// A chain leg: hops of (kind, param), the intermediate tokens.
    function chain(
        uint8[] memory kinds, uint24[] memory params, address[] memory mids,
        address tIn, address tOut, uint128 amountOut
    ) internal pure returns (bytes memory) {
        bytes memory data = abi.encodePacked(uint8(kinds.length));
        for (uint256 i; i < kinds.length; ++i) data = abi.encodePacked(data, kinds[i], params[i]);
        for (uint256 i; i < mids.length; ++i) data = abi.encodePacked(data, mids[i]);
        return PB.swap(S_CHAIN, tIn, tOut, PB.L_EXACT_OUT, amountOut, data);
    }

    function bal(address t) internal view returns (uint256) {
        return IERC20C(t).balanceOf(address(h));
    }

    function v3Path2(uint24 f0, uint24 f1) internal pure returns (uint8[] memory k, uint24[] memory p) {
        k = new uint8[](2);
        p = new uint24[](2);
        (k[0], k[1], p[0], p[1]) = (HOP_V3, HOP_V3, f0, f1);
    }

    /// WBTC → WETH (0.30 %) → USDC (0.05 %), exactly 10 000 USDC.
    function test_twoV3Hops_spendExactlyTheQuoterInput() public onFork {
        uint128 out = 10_000e6;
        (uint256 want,,,) = IQuoterV2(QUOTER_V2).quoteExactOutput(
            abi.encodePacked(USDC, uint24(500), WETH, uint24(3000), WBTC), out
        );
        deal(WBTC, address(h), 1e8);
        (uint8[] memory k, uint24[] memory p) = v3Path2(3000, 500);
        address[] memory mids = new address[](1);
        mids[0] = WETH;
        h.run(chain(k, p, mids, WBTC, USDC, out), 1);
        assertEq(bal(USDC), out, "exactly the output");
        assertEq(1e8 - bal(WBTC), want, "exactly the quoter's input");
        assertEq(bal(WETH), 0, "nothing left in between");
    }

    /// Gas of a chain leg (`run`, the module's whole leg), exact output,
    /// along one path cut at one, two and three V3 hops: WBTC → WETH (0.30 %)
    /// → USDC (0.05 %) → USDT (0.01 %). Each length is its own test, so its
    /// pools and tokens are cold, as in a liquidation; the step from one
    /// length to the next is a chained hop's cost. The router charges a
    /// chain the sum of its pools' `[swap]` figures (`config/liq-gas.toml`).
    function _gasChain(uint256 hops) internal returns (uint256 used) {
        uint8[] memory k = new uint8[](hops);
        uint24[] memory p = new uint24[](hops);
        uint24[3] memory fees = [uint24(3000), 500, 100];
        address[3] memory outs = [WETH, USDC, USDT];
        for (uint256 i; i < hops; ++i) {
            (k[i], p[i]) = (HOP_V3, fees[i]);
        }
        address[] memory mids = new address[](hops - 1);
        for (uint256 i; i + 1 < hops; ++i) {
            mids[i] = outs[i];
        }
        deal(WBTC, address(h), 1e8);
        uint128 out = hops == 1 ? 1e18 : 1_000e6;
        bytes memory leg = chain(k, p, mids, WBTC, outs[hops - 1], out);
        uint256 g = gasleft();
        h.run(leg, 1);
        used = g - gasleft();
        emit log_named_uint("chain gas, V3 hops", hops);
        emit log_named_uint("  used", used);
    }

    function test_gas_chainOneV3Hop() public onFork {
        _gasChain(1);
    }

    function test_gas_chainTwoV3Hops() public onFork {
        _gasChain(2);
    }

    function test_gas_chainThreeV3Hops() public onFork {
        _gasChain(3);
    }

    /// USDT → USDC on Curve 3pool (coins USDC = 1, USDT = 2, base-registry
    /// handler 0), then USDC → WETH on the V3 0.05 % pool, selling exactly
    /// 10,000 USDT. Oracle: 3pool's own `get_dy`, then QuoterV2 on that
    /// output. A chain with a Curve hop sells exact input; exact output
    /// through it is refused.
    function test_curveThenV3_exactInputReceivesTheComposedQuote() public onFork {
        address pool3 = 0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7;
        uint256 dy = ICurveGetDy(pool3).get_dy(int128(2), int128(1), 10_000e6);
        (uint256 want,,,) = IQuoterV2(QUOTER_V2).quoteExactInput(
            abi.encodePacked(USDC, uint24(500), WETH), dy
        );
        // base 1 + 2 * 4 + 20 = 29: the Curve extra at 29.
        bytes memory data = abi.encodePacked(
            uint8(2), uint8(3), uint24(29), HOP_V3, uint24(500), USDC,
            pool3, uint8(2), uint8(1), uint8(0)
        );
        deal(USDT, address(h), 10_000e6);
        h.run(PB.swap(S_CHAIN, USDT, WETH, 0, 10_000e6, data), 1);
        assertEq(bal(WETH), want, "the composed quote");
        assertEq(bal(USDT) + bal(USDC), 0, "nothing left in between");

        vm.expectRevert();
        h.run(PB.swap(S_CHAIN, USDT, WETH, PB.L_EXACT_OUT, 1e18, data), 1);
    }

    /// WBTC → WETH on the V3 0.30 % pool, then WETH → USDC on the hookless
    /// V4 ETH/USDC pool (0.05 %, tick spacing 10, native ETH), selling
    /// exactly 0.1 WBTC. Oracle: QuoterV2 for the V3 hop, then Uniswap's
    /// V4Quoter on its output.
    function test_v3ThenV4_exactInputReceivesTheComposedQuote() public onFork {
        (uint256 weth,,,) = IQuoterV2(QUOTER_V2).quoteExactInput(
            abi.encodePacked(WBTC, uint24(3000), WETH), 1e7
        );
        (uint256 want,) = IV4QuoterC(V4_QUOTER).quoteExactInputSingle(
            V4QuoteSingle({
                poolKey: V4KeyC(address(0), USDC, 500, 10, address(0)),
                zeroForOne: true,
                exactAmount: uint128(weth),
                hookData: ""
            })
        );
        bytes memory data = abi.encodePacked(
            uint8(2), HOP_V3, uint24(3000), uint8(1), uint24(29), WETH,
            address(0), USDC, uint24(500), int24(10), address(0)
        );
        deal(WBTC, address(h), 1e7);
        h.run(PB.swap(S_CHAIN, WBTC, USDC, 0, 1e7, data), 1);
        assertEq(bal(USDC), want, "the composed quote");
        assertEq(bal(WBTC) + bal(WETH), 0, "nothing left in between");
    }

    /// LINK → WETH (0.30 %) → USDC (0.05 %) → USDT (0.01 %), exactly 5 000
    /// USDT: three nested V3 callbacks.
    function test_threeV3Hops_spendExactlyTheQuoterInput() public onFork {
        uint128 out = 5_000e6;
        (uint256 want,,,) = IQuoterV2(QUOTER_V2).quoteExactOutput(
            abi.encodePacked(USDT, uint24(100), USDC, uint24(500), WETH, uint24(3000), LINK), out
        );
        deal(LINK, address(h), 10_000e18);
        uint8[] memory k = new uint8[](3);
        uint24[] memory p = new uint24[](3);
        (k[0], k[1], k[2]) = (HOP_V3, HOP_V3, HOP_V3);
        (p[0], p[1], p[2]) = (3000, 500, 100);
        address[] memory mids = new address[](2);
        (mids[0], mids[1]) = (WETH, USDC);
        h.run(chain(k, p, mids, LINK, USDT, out), 1);
        assertEq(bal(USDT), out);
        assertEq(10_000e18 - bal(LINK), want);
        assertEq(bal(WETH) + bal(USDC), 0);
    }

    /// WBTC → WETH (V3 0.30 %) → USDC (Uniswap V2): the V2 hop's input is
    /// its `getAmountIn` on live reserves, which the V3 hop buys exactly
    /// and delivers to the pair.
    function test_v3ThenV2Hop_spendExactlyTheComposedQuote() public onFork {
        uint128 out = 2_000e6;
        (uint112 r0, uint112 r1,) = IUniV2PairC(V2_USDC_WETH).getReserves();
        (uint256 rIn, uint256 rOut) = IUniV2PairC(V2_USDC_WETH).token0() == USDC
            ? (uint256(r1), uint256(r0)) : (uint256(r0), uint256(r1));
        uint256 wethIn = (rIn * out * 1000) / ((rOut - out) * 997) + 1;
        (uint256 want,,,) = IQuoterV2(QUOTER_V2).quoteExactOutput(
            abi.encodePacked(WETH, uint24(3000), WBTC), wethIn
        );
        deal(WBTC, address(h), 1e8);
        uint8[] memory k = new uint8[](2);
        uint24[] memory p = new uint24[](2);
        (k[0], k[1], p[0], p[1]) = (HOP_V3, HOP_V2, 3000, 0);
        address[] memory mids = new address[](1);
        mids[0] = WETH;
        h.run(chain(k, p, mids, WBTC, USDC, out), 1);
        assertEq(bal(USDC), out);
        assertEq(1e8 - bal(WBTC), want);
        assertEq(bal(WETH), 0);
    }

    /// An exact-input chain (the profit closer's shape: the whole balance,
    /// `L_TAKE_BALANCE`): WBTC → WETH (0.30 %) → USDC (0.05 %) sells all the
    /// WBTC and receives exactly the quoter's output.
    function test_exactInputV3Chain_receivesExactlyTheQuoterOutput() public onFork {
        deal(WBTC, address(h), 1e7);
        (uint256 want,,,) = IQuoterV2(QUOTER_V2).quoteExactInput(
            abi.encodePacked(WBTC, uint24(3000), WETH, uint24(500), USDC), 1e7
        );
        (uint8[] memory k, uint24[] memory p) = v3Path2(3000, 500);
        address[] memory mids = new address[](1);
        mids[0] = WETH;
        bytes memory leg = chain(k, p, mids, WBTC, USDC, 0);
        // `chain` sets exact output; this leg sells the whole balance.
        leg[41] = bytes1(PB.L_TAKE_BALANCE);
        h.run(leg, 1);
        assertEq(bal(WBTC), 0, "all of it sold");
        assertEq(bal(USDC), want, "exactly the quoter's output");
        assertEq(bal(WETH), 0);
    }

    /// Exact input through a V3 hop then a V2 hop: the V2 hop sells what
    /// the V3 hop bought, at its `getAmountOut` on live reserves.
    function test_exactInputV3ThenV2_receivesTheComposedQuote() public onFork {
        deal(WBTC, address(h), 1e7);
        (uint256 weth,,,) = IQuoterV2(QUOTER_V2).quoteExactInput(abi.encodePacked(WBTC, uint24(3000), WETH), 1e7);
        (uint112 r0, uint112 r1,) = IUniV2PairC(V2_USDC_WETH).getReserves();
        (uint256 rIn, uint256 rOut) = IUniV2PairC(V2_USDC_WETH).token0() == USDC
            ? (uint256(r1), uint256(r0)) : (uint256(r0), uint256(r1));
        uint256 want = (weth * 997 * rOut) / (rIn * 1000 + weth * 997);
        uint8[] memory k = new uint8[](2);
        uint24[] memory p = new uint24[](2);
        (k[0], k[1], p[0], p[1]) = (HOP_V3, HOP_V2, 3000, 0);
        address[] memory mids = new address[](1);
        mids[0] = WETH;
        bytes memory leg = chain(k, p, mids, WBTC, USDC, 1e7);
        leg[41] = bytes1(0); // exact input of the encoded amount
        h.run(leg, 1);
        assertEq(bal(WBTC), 0);
        assertEq(bal(USDC), want);
        assertEq(bal(WETH), 0);
    }

    /// Only the pool of the hop in flight may continue a chain. Outside an
    /// `execute` the module refuses to run at all; inside one, with no hop
    /// in flight, a chain callback is refused before anything is paid.
    function test_aCallbackFromAnyoneButTheHopInFlightIsRefused() public onFork {
        deal(WBTC, address(h), 1e8);
        bytes memory cb = abi.encode(abi.encodePacked(uint8(1), HOP_V3, uint24(3000)), WBTC, USDC, uint256(0));
        vm.expectRevert(SwapModule.NotDelegated.selector);
        h.uniswapV3SwapCallback(1e8, -1, cb);
        vm.expectRevert(SwapModule.BadSwapCallback.selector);
        h.strayCallback(1e8, -1, cb);
        assertEq(bal(WBTC), 1e8, "nothing paid");
    }

    /// A hop that cannot deliver exactly what is asked reverts the chain:
    /// more USDT than the 0.01 % USDC/USDT pool holds in range.
    function test_aHopShortOfItsOutputReverts() public onFork {
        deal(USDC, address(h), 1e15);
        uint8[] memory k = new uint8[](1);
        uint24[] memory p = new uint24[](1);
        (k[0], p[0]) = (HOP_V3, 100);
        vm.expectRevert();
        h.run(chain(k, p, new address[](0), USDC, USDT, type(uint128).max / 2), 1);
    }

    /// Malformed chains are refused before any token moves: no hops, too
    /// many, a length the hop count does not imply, an unknown hop kind.
    function test_malformedChainsAreRefused() public onFork {
        deal(WBTC, address(h), 1e8);
        bytes memory zero = PB.swap(S_CHAIN, WBTC, USDC, PB.L_EXACT_OUT, 1e6, abi.encodePacked(uint8(0)));
        vm.expectRevert(SwapModule.BadChain.selector);
        h.run(zero, 1);
        bytes memory five = PB.swap(S_CHAIN, WBTC, USDC, PB.L_EXACT_OUT, 1e6, abi.encodePacked(uint8(5)));
        vm.expectRevert(SwapModule.BadChain.selector);
        h.run(five, 1);
        bytes memory short = PB.swap(
            S_CHAIN, WBTC, USDC, PB.L_EXACT_OUT, 1e6, abi.encodePacked(uint8(2), HOP_V3, uint24(3000), HOP_V3, uint24(500))
        );
        vm.expectRevert(SwapModule.BadChain.selector);
        h.run(short, 1);
        bytes memory kind = PB.swap(S_CHAIN, WBTC, USDC, PB.L_EXACT_OUT, 1e6, abi.encodePacked(uint8(1), uint8(7), uint24(3000)));
        vm.expectRevert(SwapModule.BadChain.selector);
        h.run(kind, 1);
        assertEq(bal(WBTC), 1e8, "nothing moved");
    }

    /// A fee tier with no pool for the pair: the CREATE2 address has no
    /// code, and the call fails before anything is paid.
    function test_aHopWithNoPoolReverts() public onFork {
        deal(WBTC, address(h), 1e8);
        uint8[] memory k = new uint8[](1);
        uint24[] memory p = new uint24[](1);
        (k[0], p[0]) = (HOP_V3, 77);
        vm.expectRevert();
        h.run(chain(k, p, new address[](0), WBTC, USDC, 1e6), 1);
        assertEq(bal(WBTC), 1e8);
    }
}
