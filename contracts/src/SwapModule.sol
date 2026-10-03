// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {SwapLeg, PlanDecoder} from "./lib/PlanDecoder.sol";
import {
    IERC20, IUniV3Pool, IUniV2Pair, ICurvePool, ICurveCryptoPool, IERC4626Unwrap,
    IPendlePT, IPendleYT, IPendleSY, IPendleMarket, IPendleMarketFactory, ICurveMetaRegistry
} from "./lib/Interfaces.sol";
import {MainnetVenues} from "./lib/MainnetVenues.sol";
import {T_ENTERED, T_SWAPPING, ModuleIds} from "./lib/ExecutorShared.sol";

/*
 * SwapModule — the Executor's swap and unwrap legs (GUIDE 10, GUIDE 12).
 *
 * Code only. The Executor DELEGATECALLs it, so every line here runs as the
 * Executor: its address, balances, allowances and transient storage. A
 * Uniswap V3 pool therefore calls back the Executor, whose
 * `uniswapV3SwapCallback` authenticates the pool by CREATE2 and pays. This
 * contract holds nothing, has no storage, and refuses to run any other way
 * (`onlyDelegated`). Its address is an Executor immutable (D55).
 */
contract SwapModule {
    using SafeTransfer for address;
    using PlanDecoder for bytes;

    /// What the Executor's constructor checks this address answers.
    bytes32 public constant MODULE_ID = ModuleIds.SWAP;

    /// Canonical WETH. Profit is denominated in ETH, so every plan converges here.
    address public immutable WETH;
    /// Allowlisted routers for Curve / aggregator legs. Fixed at construction:
    /// a mutable allowlist is an operator-reachable arbitrary call.
    address public immutable ROUTER_A;
    address public immutable ROUTER_B;
    /// Uniswap V2 and SushiSwap factories + pair init-code hashes: a V2 leg's
    /// pair must be the CREATE2 address one of them derives for the leg's
    /// tokens, so a plan cannot send funds to an arbitrary "pair".
    address public immutable UNIV2_FACTORY;
    bytes32 public immutable UNIV2_INIT_HASH;
    address public immutable SUSHI_FACTORY;
    bytes32 public immutable SUSHI_INIT_HASH;
    /// Curve MetaRegistry: a Curve leg's pool must be registered there and
    /// hold the leg's tokens at the encoded indices.
    address public immutable CURVE_REGISTRY;
    /// This contract's own address: running with `address(this) == SELF` is
    /// a direct call, not the Executor's delegatecall.
    address private immutable SELF;

    // Swap leg venues. Uniswap V4 is deliberately absent: hooks make swap
    // behaviour pool-specific, so it is excluded as a routing venue even though
    // it remains the preferred flashloan source (D08, GUIDE 12 Step 3).
    uint8 private constant S_UNIV3_POOL = 0;  // pool-direct, transfer-in-callback, no approval
    uint8 private constant S_ROUTER     = 1;  // allowlisted router
    /// Pair-direct V2: tokens in, `pair.swap` out, amounts from live reserves.
    /// data = pair (20) ‖ factory id (1: 0 = Uniswap V2, 1 = SushiSwap).
    uint8 private constant S_UNIV2_POOL = 2;
    /// Pool-direct Curve StableSwap plain pool: approve, `exchange`.
    /// data = pool (20) ‖ i (1) ‖ j (1). Exact-input only — Curve has no
    /// exact-output swap.
    uint8 private constant S_CURVE_POOL = 3;
    // Curve crypto pool, pool-direct, exact input: MetaRegistry-verified like
    // S_CURVE_POOL, `exchange(uint256,uint256,uint256,uint256)`.
    uint8 private constant S_CURVE_CRYPTO_POOL = 4;
    // Unwrap: redeem ERC-4626 shares (`tokenIn` is the vault) for its
    // `asset()` (`tokenOut`). Exact input; owner and receiver are this
    // contract, so there is no approval.
    uint8 private constant S_UNWRAP_4626 = 5;
    // Unwrap: redeem an expired Pendle PT (`tokenIn`) through its YT for SY,
    // then the SY for `tokenOut`. Exact input.
    uint8 private constant S_PENDLE_PT_REDEEM = 6;
    // Unwrap: withdraw a Curve StableSwap-NG LP (`tokenIn`, the pool itself)
    // as coin `i` (`tokenOut`). Exact input; the pool burns our LP, so there
    // is no approval.
    uint8 private constant S_CURVE_LP_ONE_COIN = 7;
    // Unwrap: sell a live Pendle PT (`tokenIn`) on its market for SY, then
    // redeem the SY for `tokenOut`. Exact input.
    uint8 private constant S_PENDLE_MARKET_SELL = 8;
    /// Uniswap V2 / SushiSwap swap fee, 0.30 %.
    uint256 private constant V2_FEE_KEEP = 997;

    /// Swap-leg flag: ignore the encoded amount and take this contract's whole
    /// balance of `tokenIn`. Set on the last leg for each collateral.
    uint8 private constant L_TAKE_BALANCE = 1 << 0;
    /// Swap-leg flag: `amount` is an exact OUTPUT target, not an input amount.
    /// Used for the repay leg so no debt-token dust is left behind.
    uint8 private constant L_EXACT_OUT    = 1 << 1;

    uint160 private constant TickMath_MIN_SQRT = 4295128739;
    uint160 private constant TickMath_MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    // The same declarations as the Executor's, which lists every error its
    // `execute` can surface (`ExecutorModules.t.sol` holds them equal).
    error NotDelegated();
    error ZeroAddress();
    error UnknownVenue(uint8 v);
    error RouterNotAllowed(address target);
    error RouterCallFailed(address target);
    /// A V2/Curve leg's data is malformed, names a pool that is not the
    /// verified one for its tokens, or asks more than the pool holds.
    error BadPool(uint8 venue, address pool);
    /// Curve cannot swap to an exact output.
    error ExactOutUnsupported(uint8 venue);

    constructor(
        address weth_, address routerA_, address routerB_,
        address univ2Factory_, bytes32 univ2InitHash_,
        address sushiFactory_, bytes32 sushiInitHash_,
        address curveRegistry_
    ) {
        if (weth_ == address(0) || routerA_ == address(0) || routerB_ == address(0)
            || univ2Factory_ == address(0) || sushiFactory_ == address(0)
            || curveRegistry_ == address(0)) {
            revert ZeroAddress();
        }
        WETH            = weth_;
        ROUTER_A        = routerA_;
        ROUTER_B        = routerB_;
        UNIV2_FACTORY   = univ2Factory_;
        UNIV2_INIT_HASH = univ2InitHash_;
        SUSHI_FACTORY   = sushiFactory_;
        SUSHI_INIT_HASH = sushiInitHash_;
        CURVE_REGISTRY  = curveRegistry_;
        SELF            = address(this);
    }

    /// Module code runs only as the Executor's own, inside an `execute`.
    /// Called directly it would run on this contract's empty state: no
    /// funds or allowances are reachable that way, and refusing makes that
    /// argument unnecessary. `T_ENTERED` is read from the caller's transient
    /// storage, which under delegatecall is the Executor's.
    modifier onlyDelegated() {
        uint256 entered;
        assembly { entered := tload(T_ENTERED) }
        if (address(this) == SELF || entered == 0) revert NotDelegated();
        _;
    }

    /// Split swap across N pools, over however many collaterals the batch
    /// seized. The off-chain water-fill (GUIDE 12) decided the allocations;
    /// this executes them and nothing more.
    ///
    /// `o` points at the first leg head; `legs` is the count (from the group
    /// head for repay blobs, from the count byte for the profit blob).
    ///
    /// Each leg carries its own `tokenIn` and `tokenOut` (a batch spans several
    /// collaterals and several debt assets). The last leg per collateral sets
    /// `L_TAKE_BALANCE` and spends the whole balance — which cannot disagree
    /// with what is held, however the solver rounded or a leg under-delivered.
    ///
    /// **Order is load-bearing.** `L_EXACT_OUT` legs come FIRST and buy exactly
    /// the debt asset needed to repay the flash; every later leg converts what
    /// is left to WETH. The encoder asserts the order (PLAN-ENCODING §2a).
    ///
    /// A leg whose `tokenIn` balance is zero is skipped silently — the normal
    /// consequence of a liquidation leg being beaten, not an error. No per-leg
    /// amountOutMin: `minProfit` in execute() is the real constraint and it is
    /// checked against the net balance change, which covers every leg at once.
    ///
    /// `payable` because delegatecall keeps the Executor's `msg.value`: a
    /// wallet-funded `execute` would otherwise revert here. No value moves.
    function runSwaps(uint256 o, uint8 legs, bytes calldata plan)
        external payable onlyDelegated
    {
        assembly { tstore(T_SWAPPING, 1) }

        for (uint256 i; i < legs; ++i) {
            (SwapLeg memory s, uint256 next) = plan.swapLeg(o);
            o = next;
            // Seized WETH is already the profit asset. A WETH→WETH leg has
            // no pool; executing it reverts the liquidation instead of
            // leaving the residual to be swept.
            if (s.tokenIn == WETH && s.tokenOut == WETH) continue;

            uint256 legAmt = (s.flags & L_TAKE_BALANCE != 0)
                ? IERC20(s.tokenIn).balanceOf(address(this))
                : s.amount;
            if (legAmt == 0) continue;

            _swapLeg(s, legAmt, plan[s.dataOffset : s.dataOffset + s.dataLen]);
        }

        assembly { tstore(T_SWAPPING, 0) }
    }

    function _swapLeg(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.venue == S_UNIV3_POOL) {
            // Pool-direct: we transfer inside uniswapV3SwapCallback, so there is
            // no approval on this path at all. Cheaper and a smaller surface.
            address pool = address(bytes20(data[0:20]));
            bool zeroForOne = s.tokenIn < s.tokenOut;
            // V3 encodes direction in the sign: positive = exact input,
            // negative = exact output. One call site, both modes.
            int256 specified = (s.flags & L_EXACT_OUT != 0) ? -int256(amount) : int256(amount);
            IUniV3Pool(pool).swap(
                address(this), zeroForOne, specified,
                zeroForOne ? TickMath_MIN_SQRT + 1 : TickMath_MAX_SQRT - 1,
                abi.encode(s.tokenIn, s.tokenOut, IUniV3Pool(pool).fee())
            );
        } else if (s.venue == S_ROUTER) {
            address target = address(bytes20(data[0:20]));
            if (target == address(0)) revert RouterNotAllowed(target);
            if (target != ROUTER_A && target != ROUTER_B) revert RouterNotAllowed(target);
            // Exact approval, then zeroed unconditionally after the call. A
            // router that does not consume the full amount would otherwise
            // leave a standing allowance between transactions. For an
            // exact-output router leg `amount` is the maximum input to approve;
            // the router's own calldata carries the exact output.
            s.tokenIn.safeApprove(target, amount);
            (bool ok, ) = target.call(data[20:]);
            if (!ok) revert RouterCallFailed(target);
            s.tokenIn.safeApprove(target, 0);
        } else if (s.venue == S_UNIV2_POOL) {
            _swapV2(s, amount, data);
        } else if (s.venue == S_CURVE_POOL) {
            _swapCurve(s, amount, data);
        } else if (s.venue == S_CURVE_CRYPTO_POOL) {
            _swapCurveCrypto(s, amount, data);
        } else if (s.venue == S_UNWRAP_4626) {
            _unwrap4626(s, amount, data);
        } else if (s.venue == S_PENDLE_PT_REDEEM) {
            _redeemPendlePt(s, amount, data);
        } else if (s.venue == S_CURVE_LP_ONE_COIN) {
            _withdrawCurveLp(s, amount, data);
        } else if (s.venue == S_PENDLE_MARKET_SELL) {
            _sellPendlePt(s, amount, data);
        } else {
            revert UnknownVenue(s.venue);
        }
    }

    /// Pair-direct V2 (UniswapV2Library math, 0.30 %). The pair is verified
    /// by CREATE2 against an immutable factory before any token moves.
    function _swapV2(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (data.length != 21) revert BadPool(S_UNIV2_POOL, address(0));
        address pair = address(bytes20(data[0:20]));
        uint8 fid = uint8(data[20]);
        address factory;
        bytes32 initHash;
        if (fid == 0) {
            (factory, initHash) = (UNIV2_FACTORY, UNIV2_INIT_HASH);
        } else if (fid == 1) {
            (factory, initHash) = (SUSHI_FACTORY, SUSHI_INIT_HASH);
        }
        bool zeroForOne = s.tokenIn < s.tokenOut;
        (address t0, address t1) = zeroForOne ? (s.tokenIn, s.tokenOut) : (s.tokenOut, s.tokenIn);
        address expected = address(uint160(uint256(keccak256(abi.encodePacked(
            hex"ff", factory, keccak256(abi.encodePacked(t0, t1)), initHash
        )))));
        if (factory == address(0) || pair != expected) revert BadPool(S_UNIV2_POOL, pair);

        (uint112 r0, uint112 r1,) = IUniV2Pair(pair).getReserves();
        (uint256 rIn, uint256 rOut) = zeroForOne ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        uint256 amountIn;
        uint256 amountOut;
        if (s.flags & L_EXACT_OUT != 0) {
            amountOut = amount;
            if (amountOut >= rOut) revert BadPool(S_UNIV2_POOL, pair);
            amountIn = (rIn * amountOut * 1000) / ((rOut - amountOut) * V2_FEE_KEEP) + 1;
        } else {
            amountIn = amount;
            uint256 inWithFee = amountIn * V2_FEE_KEEP;
            amountOut = (inWithFee * rOut) / (rIn * 1000 + inWithFee);
        }
        s.tokenIn.safeTransfer(pair, amountIn);
        (uint256 o0, uint256 o1) = zeroForOne ? (uint256(0), amountOut) : (amountOut, uint256(0));
        IUniV2Pair(pair).swap(o0, o1, address(this), "");
    }

    /// Pool-direct Curve StableSwap plain pool, exact input. The pool must
    /// be registered in the MetaRegistry and hold `tokenIn`/`tokenOut` at the
    /// encoded indices. Exact approval, zeroed after. No per-leg min_dy:
    /// `minProfit` is the constraint, as for every other leg.
    function _swapCurve(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_CURVE_POOL);
        if (data.length != 22) revert BadPool(S_CURVE_POOL, address(0));
        address pool = address(bytes20(data[0:20]));
        uint8 i = uint8(data[20]);
        uint8 j = uint8(data[21]);
        if (!ICurveMetaRegistry(CURVE_REGISTRY).is_registered(pool)
            || ICurvePool(pool).coins(i) != s.tokenIn
            || ICurvePool(pool).coins(j) != s.tokenOut) {
            revert BadPool(S_CURVE_POOL, pool);
        }
        s.tokenIn.safeApprove(pool, amount);
        // i, j are uint8: widening to uint128 then int128 is lossless.
        // forge-lint: disable-next-line(unsafe-typecast)
        ICurvePool(pool).exchange(int128(uint128(i)), int128(uint128(j)), amount, 0);
        s.tokenIn.safeApprove(pool, 0);
    }

    /// Pool-direct Curve crypto pool, exact input. Same checks as
    /// `_swapCurve` (MetaRegistry, coin indices), unsigned indices.
    function _swapCurveCrypto(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_CURVE_CRYPTO_POOL);
        if (data.length != 22) revert BadPool(S_CURVE_CRYPTO_POOL, address(0));
        address pool = address(bytes20(data[0:20]));
        uint256 i = uint8(data[20]);
        uint256 j = uint8(data[21]);
        if (!ICurveMetaRegistry(CURVE_REGISTRY).is_registered(pool)
            || ICurveCryptoPool(pool).coins(i) != s.tokenIn
            || ICurveCryptoPool(pool).coins(j) != s.tokenOut) {
            revert BadPool(S_CURVE_CRYPTO_POOL, pool);
        }
        s.tokenIn.safeApprove(pool, amount);
        ICurveCryptoPool(pool).exchange(i, j, amount, 0);
        s.tokenIn.safeApprove(pool, 0);
    }

    /// Redeem ERC-4626 shares held here into the vault's asset. The vault is
    /// the token being spent, and its `asset()` must be the leg's output.
    function _unwrap4626(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_UNWRAP_4626);
        if (data.length != 20) revert BadPool(S_UNWRAP_4626, address(0));
        address vault = address(bytes20(data[0:20]));
        if (vault != s.tokenIn || IERC4626Unwrap(vault).asset() != s.tokenOut) {
            revert BadPool(S_UNWRAP_4626, vault);
        }
        IERC4626Unwrap(vault).redeem(amount, address(this), address(this));
    }

    /// Withdraw a Curve NG LP held here as one coin. The pool must be in the
    /// MetaRegistry, be the LP token being spent, and hold `tokenOut` at `i`.
    function _withdrawCurveLp(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_CURVE_LP_ONE_COIN);
        if (data.length != 21) revert BadPool(S_CURVE_LP_ONE_COIN, address(0));
        address pool = address(bytes20(data[0:20]));
        uint8 i = uint8(data[20]);
        if (pool != s.tokenIn
            || !ICurveMetaRegistry(CURVE_REGISTRY).is_registered(pool)
            || ICurvePool(pool).coins(i) != s.tokenOut) {
            revert BadPool(S_CURVE_LP_ONE_COIN, pool);
        }
        // i is uint8: widening to uint128 then int128 is lossless.
        // forge-lint: disable-next-line(unsafe-typecast)
        ICurvePool(pool).remove_liquidity_one_coin(amount, int128(uint128(i)), 0);
    }

    /// Sell a Pendle PT held here on its market, then redeem the SY. The
    /// market must be one Pendle's V6 factory created and trade `tokenIn`;
    /// the SY is the market's own and refuses a token it cannot pay.
    function _sellPendlePt(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_PENDLE_MARKET_SELL);
        if (data.length != 20) revert BadPool(S_PENDLE_MARKET_SELL, address(0));
        address market = address(bytes20(data[0:20]));
        if (!IPendleMarketFactory(MainnetVenues.PENDLE_MARKET_FACTORY_V6).isValidMarket(market)) {
            revert BadPool(S_PENDLE_MARKET_SELL, market);
        }
        (address sy, address pt,) = IPendleMarket(market).readTokens();
        if (pt != s.tokenIn) revert BadPool(S_PENDLE_MARKET_SELL, market);
        s.tokenIn.safeTransfer(market, amount);
        (uint256 syOut,) = IPendleMarket(market).swapExactPtForSy(address(this), amount, "");
        IPendleSY(sy).redeem(address(this), syOut, s.tokenOut, 0, false);
    }

    /// Redeem an expired Pendle PT held here: PT → YT `redeemPY` → SY, then
    /// SY `redeem` → `tokenOut` (the SY refuses a token it cannot pay). The
    /// PT and YT must name each other, so the PT only ever goes to its own YT.
    function _redeemPendlePt(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_PENDLE_PT_REDEEM);
        if (data.length != 20) revert BadPool(S_PENDLE_PT_REDEEM, address(0));
        address yt = address(bytes20(data[0:20]));
        if (IPendlePT(s.tokenIn).YT() != yt || IPendleYT(yt).PT() != s.tokenIn || !IPendleYT(yt).isExpired()) {
            revert BadPool(S_PENDLE_PT_REDEEM, yt);
        }
        address sy = IPendleYT(yt).SY();
        s.tokenIn.safeTransfer(yt, amount);
        uint256 syOut = IPendleYT(yt).redeemPY(address(this));
        IPendleSY(sy).redeem(address(this), syOut, s.tokenOut, 0, false);
    }
}
