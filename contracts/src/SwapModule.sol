// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {SwapLeg, PlanDecoder} from "./lib/PlanDecoder.sol";
import {
    IERC20, IUniV3Pool, IUniV2Pair, ICurvePool, ICurveCryptoPool, IERC4626Unwrap,
    IPendlePT, IPendleYT, IPendleSY, IPendleMarket, IPendleMarketFactory, ICurveMetaRegistry,
    ICurveRegistryHandler, IWETH, IPoolManager, V4PoolKey, V4SwapParams
} from "./lib/Interfaces.sol";
import {MainnetVenues} from "./lib/MainnetVenues.sol";
import {
    T_ENTERED, T_SWAPPING, T_FILLED, T_FEE, T_V4_UNLOCKED, T_CHAIN_POOL, ModuleIds, IV3Anchors, IDexModule
} from "./lib/ExecutorShared.sol";

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
    /// Curve MetaRegistry: a Curve leg's pool must be registered there, in
    /// the handler the leg names, and hold the leg's tokens at the encoded
    /// indices.
    address public immutable CURVE_REGISTRY;
    /// Balancer and Fluid DEX legs, and their chain hops, run in this module
    /// by delegatecall: this one is at the contract size limit.
    address public immutable DEX_MODULE;
    /// This contract's own address: running with `address(this) == SELF` is
    /// a direct call, not the Executor's delegatecall.
    address private immutable SELF;

    // Swap leg venues.
    uint8 private constant S_UNIV3_POOL = 0;  // pool-direct, transfer-in-callback, no approval
    uint8 private constant S_ROUTER     = 1;  // allowlisted router
    /// Pair-direct V2: tokens in, `pair.swap` out, amounts from live reserves.
    /// data = pair (20) ‖ factory id (1: 0 = Uniswap V2, 1 = SushiSwap).
    uint8 private constant S_UNIV2_POOL = 2;
    /// Pool-direct Curve StableSwap plain pool: approve, `exchange`.
    /// data = pool (20) ‖ i (1) ‖ j (1) ‖ MetaRegistry handler index (1).
    /// Exact-input only — Curve has no exact-output swap.
    uint8 private constant S_CURVE_POOL = 3;
    // Curve crypto pool, pool-direct, exact input: same data and the same
    // MetaRegistry check as S_CURVE_POOL,
    // `exchange(uint256,uint256,uint256,uint256)`.
    uint8 private constant S_CURVE_CRYPTO_POOL = 4;
    // Unwrap: redeem ERC-4626 shares (`tokenIn` is the vault) for its
    // `asset()` (`tokenOut`). Exact input; owner and receiver are this
    // contract, so there is no approval.
    uint8 private constant S_UNWRAP_4626 = 5;
    // Unwrap: redeem an expired Pendle PT (`tokenIn`) through its YT for SY,
    // then the SY for `tokenOut`. Exact input.
    uint8 private constant S_PENDLE_PT_REDEEM = 6;
    // Unwrap: withdraw a Curve StableSwap-NG LP (`tokenIn`, the pool itself)
    // as coin `i` (`tokenOut`). data = pool (20) ‖ i (1) ‖ MetaRegistry
    // handler index (1). Exact input; the pool burns our LP, so there is no
    // approval.
    uint8 private constant S_CURVE_LP_ONE_COIN = 7;
    // Unwrap: sell a live Pendle PT (`tokenIn`) on its market for SY, then
    // redeem the SY for `tokenOut`. Exact input.
    uint8 private constant S_PENDLE_MARKET_SELL = 8;
    // Uniswap V4 pool, by its key. data = currency0 (20) ‖ currency1 (20) ‖
    // fee (3) ‖ tickSpacing (3, int24) ‖ hooks (20). Exact input or output.
    // The hook must be none or allowlisted (`MainnetVenues.v4HookAllowed`):
    // hooks make a pool's behaviour its own (decision 7). Currency `0` is
    // native ETH, which the plan names as WETH: unwrapped to pay, wrapped
    // when received.
    uint8 private constant S_UNIV4_POOL = 9;
    uint256 private constant V4_KEY_LEN = 66;
    // Chain through Uniswap V3 / V2 hops (coverage plan 4F). Exact output
    // (`L_EXACT_OUT`): buys exactly `amount` of `tokenOut` with `tokenIn`,
    // every intermediate amount the pools' own answer at execution. Exact
    // input: sells `amount` of `tokenIn` (the whole balance with
    // `L_TAKE_BALANCE`) along the path into `tokenOut`.
    // data = hops (1) ‖ per hop: kind (1) ‖ param (3) ‖ the hops − 1
    // intermediate tokens (20 each), in path order ‖ extras. Kinds: CHAIN_V3
    // (param: fee tier), CHAIN_V2 (param: factory id in the low byte),
    // CHAIN_V4 (param: offset of its 66-byte pool key in the extras),
    // CHAIN_CURVE / CHAIN_CURVE_CRYPTO (param: offset of pool ‖ i ‖ j ‖
    // MetaRegistry handler). A chain with a V4 or Curve hop sells an exact
    // input only: neither can be nested inside the exact-output recursion.
    uint8 private constant S_CHAIN = 10;
    // Balancer V2 (data = poolId, 32) and Fluid DEX T1 (data = pool (20) ‖
    // swap0to1 (1)): `DexModule`. Exact input or output.
    uint8 private constant S_BALANCER = 11;
    uint8 private constant S_FLUID = 12;
    uint8 private constant CHAIN_V3 = 0;
    uint8 private constant CHAIN_V4 = 1;
    uint8 private constant CHAIN_V2 = 2;
    uint8 private constant CHAIN_CURVE = 3;
    uint8 private constant CHAIN_CURVE_CRYPTO = 4;
    // A SushiSwap V3 and a PancakeSwap V3 hop: CHAIN_V3 (param: fee tier) on
    // that factory's pool, exact output or exact input.
    uint8 private constant CHAIN_V3_SUSHI = 5;
    uint8 private constant CHAIN_V3_PANCAKE = 6;
    // A Balancer hop (extra: poolId, 32) and a Fluid hop (extra: pool ‖
    // swap0to1, 21), exact input only like a Curve hop.
    uint8 private constant CHAIN_BALANCER = 7;
    uint8 private constant CHAIN_FLUID = 8;
    uint256 private constant CHAIN_BALANCER_EXTRA = 32;
    uint256 private constant CHAIN_FLUID_EXTRA = 21;
    /// A Curve hop's extra: pool (20) ‖ i ‖ j ‖ MetaRegistry handler index.
    uint256 private constant CHAIN_CURVE_EXTRA = 23;
    uint256 private constant CHAIN_MAX_HOPS = 4;
    /// Uniswap V2 / SushiSwap swap fee, 0.30 %.
    uint256 private constant V2_FEE_KEEP = 997;

    /// Swap-leg flag: ignore the encoded amount and take this contract's whole
    /// balance of `tokenIn`. Set on the last leg for each collateral.
    uint8 private constant L_TAKE_BALANCE = 1 << 0;
    /// Swap-leg flag: `amount` is an exact OUTPUT target, not an input amount.
    /// Used for the repay leg so no debt-token dust is left behind.
    uint8 private constant L_EXACT_OUT    = 1 << 1;
    /// Swap-leg flags, bits 2–7: the liquidation leg this swap serves, as its
    /// index in the group plus one (0: none). A swap tied to a leg that did
    /// not fill is skipped: what it would sell never arrived, and its exact
    /// output would be paid for with another leg's collateral.
    uint8 private constant L_TIE_SHIFT    = 2;

    uint160 private constant TickMath_MIN_SQRT = 4295128739;
    uint160 private constant TickMath_MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    // The same declarations as the Executor's, which lists every error its
    // `execute` can surface (`ExecutorModules.t.sol` holds them equal).
    error NotDelegated();
    error ZeroAddress();
    /// The module given for Balancer and Fluid legs is not a `DexModule` for
    /// this WETH.
    error BadDexModule(address module);
    error UnknownVenue(uint8 v);
    error RouterNotAllowed(address target);
    error RouterCallFailed(address target);
    /// A V2/Curve leg's data is malformed, names a pool that is not the
    /// verified one for its tokens, or asks more than the pool holds.
    error BadPool(uint8 venue, address pool);
    /// Curve cannot swap to an exact output.
    error ExactOutUnsupported(uint8 venue);
    /// An `S_CHAIN` leg's data is malformed, it is not exact output, or a
    /// hop delivered other than exactly what was asked.
    error BadChain();
    /// A swap callback from anyone but the pool expected.
    error BadSwapCallback();

    constructor(
        address weth_, address routerA_, address routerB_,
        address univ2Factory_, bytes32 univ2InitHash_,
        address sushiFactory_, bytes32 sushiInitHash_,
        address curveRegistry_, address dexModule_
    ) {
        if (weth_ == address(0) || routerA_ == address(0) || routerB_ == address(0)
            || univ2Factory_ == address(0) || sushiFactory_ == address(0)
            || curveRegistry_ == address(0) || dexModule_ == address(0)) {
            revert ZeroAddress();
        }
        // The module must be one that names itself so and works on our WETH.
        (bool ok, bytes memory r) = dexModule_.staticcall(abi.encodeCall(IDexModule.MODULE_ID, ()));
        if (!ok || r.length != 32 || abi.decode(r, (bytes32)) != ModuleIds.DEX) revert BadDexModule(dexModule_);
        (ok, r) = dexModule_.staticcall(abi.encodeCall(IDexModule.WETH, ()));
        if (!ok || r.length != 32 || abi.decode(r, (address)) != weth_) revert BadDexModule(dexModule_);
        WETH            = weth_;
        ROUTER_A        = routerA_;
        ROUTER_B        = routerB_;
        UNIV2_FACTORY   = univ2Factory_;
        UNIV2_INIT_HASH = univ2InitHash_;
        SUSHI_FACTORY   = sushiFactory_;
        SUSHI_INIT_HASH = sushiInitHash_;
        CURVE_REGISTRY  = curveRegistry_;
        DEX_MODULE      = dexModule_;
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
    /// A repay leg tied to a liquidation leg that did not fill is skipped
    /// (`L_TIE_SHIFT`), and a TAKE_BALANCE leg whose `tokenIn` balance is zero
    /// is skipped silently — both the normal consequence of a liquidation leg
    /// being beaten, not an error. The first exact-output pool leg to run
    /// also buys the flash fee the provider charged (`T_FEE`). No per-leg
    /// amountOutMin: `minProfit` in execute() is the real constraint and it is
    /// checked against the net balance change, which covers every leg at once.
    ///
    /// `payable` because delegatecall keeps the Executor's `msg.value`: a
    /// wallet-funded `execute` would otherwise revert here. No value moves.
    function runSwaps(uint256 o, uint8 legs, bytes calldata plan)
        external payable onlyDelegated
    {
        uint256 filled;
        uint256 fee;
        assembly {
            tstore(T_SWAPPING, 1)
            filled := tload(T_FILLED)
            fee    := tload(T_FEE)
        }

        for (uint256 i; i < legs; ++i) {
            (SwapLeg memory s, uint256 next) = plan.swapLeg(o);
            o = next;
            // Seized WETH is already the profit asset. A WETH→WETH leg has
            // no pool; executing it reverts the liquidation instead of
            // leaving the residual to be swept.
            if (s.tokenIn == WETH && s.tokenOut == WETH) continue;
            uint256 tie = s.flags >> L_TIE_SHIFT;
            if (tie != 0 && ((filled >> (tie - 1)) & 1) == 0) continue;

            uint256 legAmt = (s.flags & L_TAKE_BALANCE != 0)
                ? IERC20(s.tokenIn).balanceOf(address(this))
                : s.amount;
            // The group owes the fee once, whichever legs filled: the first
            // exact-output pool leg buys it. A router's output is fixed in
            // its own calldata, and Curve has no exact output.
            if (fee != 0 && s.flags & L_EXACT_OUT != 0
                && (s.venue == S_UNIV3_POOL || s.venue == S_UNIV2_POOL || s.venue == S_UNIV4_POOL
                    || s.venue == S_CHAIN || s.venue == S_BALANCER || s.venue == S_FLUID)) {
                legAmt += fee;
                fee = 0;
            }
            if (legAmt == 0) continue;

            _swapLeg(s, legAmt, plan[s.dataOffset : s.dataOffset + s.dataLen]);
        }

        assembly {
            tstore(T_SWAPPING, 0)
            tstore(T_FEE, fee)
        }
    }

    function _swapLeg(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.venue == S_UNIV3_POOL) {
            // Pool-direct: we transfer inside uniswapV3SwapCallback, so there is
            // no approval on this path at all. Cheaper and a smaller surface.
            // data = pool (20), then for a fork the factory id (1, 2); the
            // pool is authenticated in the callback, by the id's deployer.
            if (data.length != 20 && data.length != 21) revert BadPool(S_UNIV3_POOL, address(0));
            address pool = address(bytes20(data[0:20]));
            uint8 fid = data.length == 21 ? uint8(data[20]) : 0;
            if (fid > MainnetVenues.V3_FACTORY_PANCAKE) revert BadPool(S_UNIV3_POOL, pool);
            bool zeroForOne = s.tokenIn < s.tokenOut;
            // V3 encodes direction in the sign: positive = exact input,
            // negative = exact output. One call site, both modes.
            int256 specified = (s.flags & L_EXACT_OUT != 0) ? -int256(amount) : int256(amount);
            uint24 poolFee = IUniV3Pool(pool).fee();
            IUniV3Pool(pool).swap(
                address(this), zeroForOne, specified,
                zeroForOne ? TickMath_MIN_SQRT + 1 : TickMath_MAX_SQRT - 1,
                fid == 0
                    ? abi.encode(s.tokenIn, s.tokenOut, poolFee)
                    : abi.encode(s.tokenIn, s.tokenOut, poolFee, fid)
            );
        } else if (s.venue == S_ROUTER) {
            address target = address(bytes20(data[0:20]));
            if (target == address(0)) revert RouterNotAllowed(target);
            if (target != ROUTER_A && target != ROUTER_B) revert RouterNotAllowed(target);
            // Exact approval, then cleared after the call. A router that
            // does not consume the full amount would otherwise leave a
            // standing allowance between transactions. For an
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
        } else if (s.venue == S_UNIV4_POOL) {
            _swapV4(s, amount, data);
        } else if (s.venue == S_CHAIN) {
            bool exactOutOk = _chainCheck(data);
            if (s.flags & L_EXACT_OUT != 0) {
                // V4 and Curve hops sell an exact input only.
                if (!exactOutOk) revert BadChain();
                _chainOut(data, s.tokenIn, s.tokenOut, uint8(data[0]) - 1, amount, address(this));
            } else {
                _chainIn(data, s.tokenIn, s.tokenOut, amount);
            }
        } else if (s.venue == S_BALANCER || s.venue == S_FLUID) {
            _dex(s.venue, s.tokenIn, s.tokenOut, s.flags & L_EXACT_OUT != 0, amount, data);
        } else {
            revert UnknownVenue(s.venue);
        }
    }

    /// One Balancer or Fluid swap, by the dex module running as this contract.
    /// Returns how much `tokenOut` it received; a revert comes back as the
    /// module's own error.
    function _dex(uint8 venue, address tokenIn, address tokenOut, bool exactOut, uint256 amount, bytes memory data)
        internal returns (uint256 received)
    {
        (bool ok, bytes memory ret) = DEX_MODULE.delegatecall(
            abi.encodeCall(IDexModule.swapLeg, (venue, tokenIn, tokenOut, exactOut, amount, data))
        );
        if (!ok) {
            assembly { revert(add(ret, 0x20), mload(ret)) }
        }
        received = abi.decode(ret, (uint256));
    }

    /// Uniswap V4: one swap on the PoolManager, settled at once. Inside a
    /// V4 flash group's callback this contract already holds the unlock
    /// (`T_V4_UNLOCKED`), so it swaps there; otherwise it unlocks, and the
    /// PoolManager calls the Executor's `unlockCallback`, which hands the
    /// swap back here (`v4SwapCallback`). Either way every delta of the swap
    /// is paid and taken before the unlock returns, or V4 reverts.
    function _swapV4(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (data.length != V4_KEY_LEN) revert BadPool(S_UNIV4_POOL, address(0));
        address hooks = address(bytes20(data[46:66]));
        if (!MainnetVenues.v4HookAllowed(hooks)) revert BadPool(S_UNIV4_POOL, hooks);
        bytes memory args = abi.encode(
            data[0:66], s.tokenIn, s.tokenOut, s.flags & L_EXACT_OUT != 0, amount
        );
        uint256 unlocked;
        assembly { unlocked := tload(T_V4_UNLOCKED) }
        if (unlocked != 0) {
            _v4SwapAndSettle(args);
        } else {
            IPoolManager(MainnetVenues.V4_POOL_MANAGER).unlock(args);
        }
    }

    /// The PoolManager's callback for a swap leg's own unlock, as the
    /// Executor relays it (only from the PoolManager, only while swap legs
    /// run).
    function v4SwapCallback(bytes calldata data) external payable onlyDelegated {
        _v4SwapAndSettle(data);
    }

    function _v4SwapAndSettle(bytes memory args) internal {
        (bytes memory k, address tokenIn, address tokenOut, bool exactOut, uint256 amount) =
            abi.decode(args, (bytes, address, address, bool, uint256));
        V4PoolKey memory key;
        assembly {
            let p := add(k, 0x20)
            mstore(key, shr(96, mload(p)))
            mstore(add(key, 0x20), shr(96, mload(add(p, 20))))
            mstore(add(key, 0x40), shr(232, mload(add(p, 40))))
            mstore(add(key, 0x60), signextend(2, shr(232, mload(add(p, 43)))))
            mstore(add(key, 0x80), shr(96, mload(add(p, 46))))
        }
        // The plan names native ETH as WETH.
        address cin = (tokenIn == WETH && key.currency0 == address(0)) ? address(0) : tokenIn;
        address cout = (tokenOut == WETH && key.currency0 == address(0)) ? address(0) : tokenOut;
        bool zeroForOne = cin == key.currency0;
        if (!(zeroForOne ? cout == key.currency1 : (cin == key.currency1 && cout == key.currency0))) {
            revert BadPool(S_UNIV4_POOL, key.hooks);
        }
        int256 delta = IPoolManager(MainnetVenues.V4_POOL_MANAGER).swap(
            key,
            V4SwapParams({
                zeroForOne: zeroForOne,
                amountSpecified: exactOut ? int256(amount) : -int256(amount),
                sqrtPriceLimitX96: zeroForOne ? TickMath_MIN_SQRT + 1 : TickMath_MAX_SQRT - 1
            }),
            ""
        );
        int128 d0 = int128(delta >> 128);
        int128 d1 = int128(delta);
        (int128 dIn, int128 dOut) = zeroForOne ? (d0, d1) : (d1, d0);
        // Owed to the pool is negative, owed to us positive.
        uint256 pay = uint256(uint128(-dIn));
        uint256 got = uint256(uint128(dOut));
        address pm = MainnetVenues.V4_POOL_MANAGER;
        if (cin == address(0)) {
            IWETH(WETH).withdraw(pay);
            IPoolManager(pm).settle{value: pay}();
        } else {
            IPoolManager(pm).sync(cin);
            cin.safeTransfer(pm, pay);
            IPoolManager(pm).settle();
        }
        IPoolManager(pm).take(cout, address(this), got);
        if (cout == address(0)) IWETH(WETH).deposit{value: got}();
    }

    // ───────────────────────────── chains (S_CHAIN) ─────────────────────────────

    /// The chain's data is well formed: 1 to `CHAIN_MAX_HOPS` hops, each a
    /// known kind; the length its hop count implies, plus the extras a V4
    /// hop (its 66-byte pool key) or a Curve hop (`CHAIN_CURVE_EXTRA`) names
    /// by offset in its param, each inside the data. Returns whether every
    /// hop can buy an exact output (V3 and V2 only).
    function _chainCheck(bytes calldata data) internal pure returns (bool exactOutOk) {
        if (data.length == 0) revert BadChain();
        uint256 n = uint8(data[0]);
        if (n == 0 || n > CHAIN_MAX_HOPS) revert BadChain();
        uint256 base = 1 + n * 4 + (n - 1) * 20;
        if (data.length < base) revert BadChain();
        exactOutOk = true;
        uint256 extras;
        for (uint256 h; h < n; ++h) {
            uint256 len = _chainExtraLen(data, h, base);
            if (len != 0) {
                extras += len;
                exactOutOk = false;
            }
        }
        // Nothing beyond the hops' own extras.
        if (data.length != base + extras) revert BadChain();
    }

    /// Hop `h`'s extra length (0 for V3 and V2), its kind known and its
    /// extra inside `data`.
    function _chainExtraLen(bytes calldata data, uint256 h, uint256 base) internal pure returns (uint256 len) {
        uint8 kind = uint8(data[1 + h * 4]);
        if (kind == CHAIN_V3 || kind == CHAIN_V2 || kind == CHAIN_V3_SUSHI || kind == CHAIN_V3_PANCAKE) return 0;
        if (kind == CHAIN_V4) {
            len = V4_KEY_LEN;
        } else if (kind == CHAIN_CURVE || kind == CHAIN_CURVE_CRYPTO) {
            len = CHAIN_CURVE_EXTRA;
        } else if (kind == CHAIN_BALANCER) {
            len = CHAIN_BALANCER_EXTRA;
        } else if (kind == CHAIN_FLUID) {
            len = CHAIN_FLUID_EXTRA;
        } else {
            revert BadChain();
        }
        uint256 at = uint256(uint24(bytes3(data[2 + h * 4:5 + h * 4])));
        if (at < base || at + len > data.length) revert BadChain();
    }

    /// Token `i` of the path (0 = `tokenIn`, hops = `tokenOut`).
    function _chainToken(bytes memory data, address tokenIn, address tokenOut, uint256 i)
        internal pure returns (address t)
    {
        uint256 n = uint8(data[0]);
        if (i == 0) return tokenIn;
        if (i == n) return tokenOut;
        uint256 at = 1 + n * 4 + (i - 1) * 20;
        assembly { t := shr(96, mload(add(add(data, 0x20), at))) }
    }

    /// Deliver exactly `amountOut` of path token `h + 1` to `recipient`
    /// through hops 0..=h. A V3 hop swaps exact-out first and is paid in its
    /// callback (`v3ChainCallback`), by the hops before it delivering to the
    /// pool; a V2 hop takes its input from the hops before it (delivered to
    /// the pair: its exact `getAmountIn` on live reserves), then swaps. Hop
    /// 0 is paid from this contract's `tokenIn`.
    function _chainOut(
        bytes memory data, address tokenIn, address tokenOut, uint256 h,
        uint256 amountOut, address recipient
    ) internal {
        address a = _chainToken(data, tokenIn, tokenOut, h);
        address b = _chainToken(data, tokenIn, tokenOut, h + 1);
        (uint8 kind, uint24 param) = _chainHop(data, h);
        bool zeroForOne = a < b;
        if (kind == CHAIN_V3 || kind == CHAIN_V3_SUSHI || kind == CHAIN_V3_PANCAKE) {
            address pool = _v3Pool(a, b, param, kind);
            uint256 prev;
            assembly {
                prev := tload(T_CHAIN_POOL)
                tstore(T_CHAIN_POOL, pool)
            }
            (int256 d0, int256 d1) = IUniV3Pool(pool).swap(
                recipient, zeroForOne, -int256(amountOut),
                zeroForOne ? TickMath_MIN_SQRT + 1 : TickMath_MAX_SQRT - 1,
                abi.encode(data, tokenIn, tokenOut, h, false)
            );
            assembly { tstore(T_CHAIN_POOL, prev) }
            // Exactly what was asked: a pool that ran out of range pays less.
            if ((zeroForOne ? d1 : d0) != -int256(amountOut)) revert BadChain();
        } else {
            address pair = _v2Pair(a, b, uint8(param));
            (uint112 r0, uint112 r1,) = IUniV2Pair(pair).getReserves();
            (uint256 rIn, uint256 rOut) = zeroForOne ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
            if (amountOut >= rOut) revert BadPool(S_UNIV2_POOL, pair);
            uint256 amountIn = (rIn * amountOut * 1000) / ((rOut - amountOut) * V2_FEE_KEEP) + 1;
            if (h == 0) {
                a.safeTransfer(pair, amountIn);
            } else {
                _chainOut(data, tokenIn, tokenOut, h - 1, amountIn, pair);
            }
            (uint256 o0, uint256 o1) = zeroForOne ? (uint256(0), amountOut) : (amountOut, uint256(0));
            IUniV2Pair(pair).swap(o0, o1, recipient, "");
        }
    }

    /// Hop `h`'s kind and param (V3 fee tier, or V2 factory id).
    function _chainHop(bytes memory data, uint256 h) internal pure returns (uint8 kind, uint24 param) {
        kind = uint8(data[1 + h * 4]);
        param = uint24(uint8(data[2 + h * 4])) << 16 | uint24(uint8(data[3 + h * 4])) << 8
            | uint24(uint8(data[4 + h * 4]));
    }

    /// Sell `amountIn` of `tokenIn` along the path, hop by hop, into this
    /// contract: a V3 hop is paid in its callback (`v3ChainCallback`) from
    /// what this contract holds of its input; a V2 hop is paid, then swaps
    /// its `getAmountOut` on live reserves. Each hop sells all the previous
    /// one bought.
    function _chainIn(bytes calldata raw, address tokenIn, address tokenOut, uint256 amountIn) internal {
        bytes memory data = raw;
        uint256 n = uint8(data[0]);
        uint256 amount = amountIn;
        for (uint256 h; h < n; ++h) {
            address a = _chainToken(data, tokenIn, tokenOut, h);
            address b = _chainToken(data, tokenIn, tokenOut, h + 1);
            (uint8 kind, uint24 param) = _chainHop(data, h);
            bool zeroForOne = a < b;
            if (kind == CHAIN_V3 || kind == CHAIN_V3_SUSHI || kind == CHAIN_V3_PANCAKE) {
                address pool = _v3Pool(a, b, param, kind);
                uint256 prev;
                assembly {
                    prev := tload(T_CHAIN_POOL)
                    tstore(T_CHAIN_POOL, pool)
                }
                (int256 d0, int256 d1) = IUniV3Pool(pool).swap(
                    address(this), zeroForOne, int256(amount),
                    zeroForOne ? TickMath_MIN_SQRT + 1 : TickMath_MAX_SQRT - 1,
                    abi.encode(data, tokenIn, tokenOut, h, true)
                );
                assembly { tstore(T_CHAIN_POOL, prev) }
                int256 got = zeroForOne ? d1 : d0;
                if (got >= 0) revert BadChain();
                amount = uint256(-got);
            } else if (kind == CHAIN_V2) {
                address pair = _v2Pair(a, b, uint8(param));
                (uint112 r0, uint112 r1,) = IUniV2Pair(pair).getReserves();
                (uint256 rIn, uint256 rOut) = zeroForOne ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
                uint256 inWithFee = amount * V2_FEE_KEEP;
                uint256 out = (inWithFee * rOut) / (rIn * 1000 + inWithFee);
                a.safeTransfer(pair, amount);
                (uint256 o0, uint256 o1) = zeroForOne ? (uint256(0), out) : (out, uint256(0));
                IUniV2Pair(pair).swap(o0, o1, address(this), "");
                amount = out;
            } else {
                // V4 and Curve: what the hop delivered here.
                uint256 before = IERC20(b).balanceOf(address(this));
                if (kind == CHAIN_V4) {
                    _chainV4(data, param, a, b, amount);
                } else if (kind == CHAIN_BALANCER || kind == CHAIN_FLUID) {
                    bool bal = kind == CHAIN_BALANCER;
                    bytes memory extra = new bytes(bal ? CHAIN_BALANCER_EXTRA : CHAIN_FLUID_EXTRA);
                    for (uint256 k; k < extra.length; ++k) extra[k] = data[param + k];
                    _dex(bal ? S_BALANCER : S_FLUID, a, b, false, amount, extra);
                } else {
                    _chainCurve(data, param, kind == CHAIN_CURVE_CRYPTO, a, b, amount);
                }
                amount = IERC20(b).balanceOf(address(this)) - before;
            }
        }
    }

    /// A V4 hop, exact input: the pool key at `at`, the same hook rule and
    /// settlement as a V4 leg (`_swapV4`), inside the unlock this contract
    /// holds or one it takes.
    function _chainV4(bytes memory data, uint24 at, address a, address b, uint256 amount) internal {
        bytes memory key = new bytes(V4_KEY_LEN);
        address hooks;
        assembly {
            let src := add(add(data, 0x20), at)
            let dst := add(key, 0x20)
            mstore(dst, mload(src))
            mstore(add(dst, 0x20), mload(add(src, 0x20)))
            mstore(add(dst, 0x40), mload(add(src, 0x40)))
            hooks := shr(96, mload(add(src, 46)))
        }
        // `new bytes` length is 66: the third word's trailing bytes are past it.
        if (!MainnetVenues.v4HookAllowed(hooks)) revert BadPool(S_UNIV4_POOL, hooks);
        bytes memory args = abi.encode(key, a, b, false, amount);
        uint256 unlocked;
        assembly { unlocked := tload(T_V4_UNLOCKED) }
        if (unlocked != 0) {
            _v4SwapAndSettle(args);
        } else {
            IPoolManager(MainnetVenues.V4_POOL_MANAGER).unlock(args);
        }
    }

    /// A Curve hop, exact input: pool ‖ i ‖ j ‖ handler at `at`, the same
    /// MetaRegistry and coin checks as a Curve leg, exact approval cleared
    /// after.
    function _chainCurve(bytes memory data, uint24 at, bool crypto, address a, address b, uint256 amount)
        internal
    {
        address pool;
        uint8 i;
        uint8 j;
        uint8 handler;
        assembly {
            let src := add(add(data, 0x20), at)
            pool := shr(96, mload(src))
            i := byte(0, mload(add(src, 20)))
            j := byte(0, mload(add(src, 21)))
            handler := byte(0, mload(add(src, 22)))
        }
        if (!_curveRegistered(pool, handler)) revert BadPool(crypto ? S_CURVE_CRYPTO_POOL : S_CURVE_POOL, pool);
        a.safeApprove(pool, amount);
        if (crypto) {
            if (ICurveCryptoPool(pool).coins(i) != a || ICurveCryptoPool(pool).coins(j) != b) {
                revert BadPool(S_CURVE_CRYPTO_POOL, pool);
            }
            ICurveCryptoPool(pool).exchange(i, j, amount, 0);
        } else {
            if (ICurvePool(pool).coins(i) != a || ICurvePool(pool).coins(j) != b) {
                revert BadPool(S_CURVE_POOL, pool);
            }
            // i, j are uint8: widening to uint128 then int128 is lossless.
            // forge-lint: disable-next-line(unsafe-typecast)
            ICurvePool(pool).exchange(int128(uint128(i)), int128(uint128(j)), amount, 0);
        }
        a.safeApprove(pool, 0);
    }

    /// A chain hop's V3 swap callback, relayed by the Executor: only the
    /// pool of the hop in flight (`T_CHAIN_POOL`) may call it. It owes the
    /// pool its input: selling forward, from this contract; buying an exact
    /// output, from the hops before it, delivered straight to the pool, or,
    /// at hop 0, from this contract.
    function v3ChainCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata cb)
        external payable onlyDelegated
    {
        address expected;
        assembly { expected := tload(T_CHAIN_POOL) }
        if (expected == address(0) || msg.sender != expected) revert BadSwapCallback();
        (bytes memory data, address tokenIn, address tokenOut, uint256 h, bool exactIn) =
            abi.decode(cb, (bytes, address, address, uint256, bool));
        uint256 owed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        // Selling forward: the hop's input is already here.
        if (exactIn) {
            _chainToken(data, tokenIn, tokenOut, h).safeTransfer(msg.sender, owed);
            return;
        }
        if (h == 0) {
            tokenIn.safeTransfer(msg.sender, owed);
        } else {
            _chainOut(data, tokenIn, tokenOut, h - 1, owed, msg.sender);
        }
    }

    /// The V3 pool for (a, b, fee) of a hop of `kind`: a Uniswap pool from
    /// the Executor's own factory and init-code hash, a fork's from its
    /// deployer's ([`MainnetVenues.v3ForkPool`]).
    function _v3Pool(address a, address b, uint24 fee, uint8 kind) internal view returns (address) {
        if (kind != CHAIN_V3) {
            return MainnetVenues.v3ForkPool(
                kind == CHAIN_V3_SUSHI ? MainnetVenues.V3_FACTORY_SUSHI : MainnetVenues.V3_FACTORY_PANCAKE,
                a, b, fee
            );
        }
        (address t0, address t1) = a < b ? (a, b) : (b, a);
        address factory = IV3Anchors(address(this)).UNIV3_FACTORY();
        bytes32 initHash = IV3Anchors(address(this)).UNIV3_POOL_INIT_HASH();
        return address(uint160(uint256(keccak256(abi.encodePacked(
            hex"ff", factory, keccak256(abi.encode(t0, t1, fee)), initHash
        )))));
    }

    /// The V2 pair for (a, b) of factory `fid` (0 = Uniswap V2, 1 =
    /// SushiSwap), by CREATE2.
    function _v2Pair(address a, address b, uint8 fid) internal view returns (address pair) {
        address factory;
        bytes32 initHash;
        if (fid == 0) {
            (factory, initHash) = (UNIV2_FACTORY, UNIV2_INIT_HASH);
        } else if (fid == 1) {
            (factory, initHash) = (SUSHI_FACTORY, SUSHI_INIT_HASH);
        } else {
            revert BadChain();
        }
        (address t0, address t1) = a < b ? (a, b) : (b, a);
        pair = address(uint160(uint256(keccak256(abi.encodePacked(
            hex"ff", factory, keccak256(abi.encodePacked(t0, t1)), initHash
        )))));
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

    /// `pool` is registered in handler `h` of the Curve MetaRegistry.
    ///
    /// The MetaRegistry's own `is_registered(pool)` asks every handler in
    /// turn, each an external call: 122,873 gas with the eight handlers on
    /// mainnet, whichever pool. This asks the one handler the plan names:
    /// 11k–16k gas, by handler. A pool that handler holds is a pool the
    /// MetaRegistry holds, so nothing is accepted that was refused before; a
    /// pool named with another handler's index, or one past the list (the
    /// zero address), is refused.
    function _curveRegistered(address pool, uint8 h) internal view returns (bool) {
        address handler = ICurveMetaRegistry(CURVE_REGISTRY).get_registry(h);
        return handler != address(0) && ICurveRegistryHandler(handler).is_registered(pool);
    }

    /// Pool-direct Curve StableSwap plain pool, exact input. The pool must
    /// be registered in the MetaRegistry (`_curveRegistered`) and hold
    /// `tokenIn`/`tokenOut` at the encoded indices. Exact approval, cleared
    /// after. No per-leg min_dy: `minProfit` is the constraint, as for every
    /// other leg.
    function _swapCurve(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_CURVE_POOL);
        if (data.length != 23) revert BadPool(S_CURVE_POOL, address(0));
        address pool = address(bytes20(data[0:20]));
        uint8 i = uint8(data[20]);
        uint8 j = uint8(data[21]);
        if (!_curveRegistered(pool, uint8(data[22]))
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
        if (data.length != 23) revert BadPool(S_CURVE_CRYPTO_POOL, address(0));
        address pool = address(bytes20(data[0:20]));
        uint256 i = uint8(data[20]);
        uint256 j = uint8(data[21]);
        if (!_curveRegistered(pool, uint8(data[22]))
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
    /// MetaRegistry (`_curveRegistered`), be the LP token being spent, and
    /// hold `tokenOut` at `i`.
    function _withdrawCurveLp(SwapLeg memory s, uint256 amount, bytes calldata data) internal {
        if (s.flags & L_EXACT_OUT != 0) revert ExactOutUnsupported(S_CURVE_LP_ONE_COIN);
        if (data.length != 22) revert BadPool(S_CURVE_LP_ONE_COIN, address(0));
        address pool = address(bytes20(data[0:20]));
        uint8 i = uint8(data[20]);
        if (pool != s.tokenIn
            || !_curveRegistered(pool, uint8(data[21]))
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
