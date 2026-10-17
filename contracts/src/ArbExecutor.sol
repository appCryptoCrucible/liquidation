// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {MainnetVenues} from "./lib/MainnetVenues.sol";
import {Plan, FlashGroup, PlanDecoder} from "./lib/PlanDecoder.sol";
import {IERC20, IWETH, IUniV3Pool, IMorpho, IPoolManager} from "./lib/Interfaces.sol";
import {
    T_EXPECTED_CALLER, T_ENTERED, T_SWAPPING, T_FILLED, T_FEE, T_V4_UNLOCKED,
    T_GROUP_SOURCE, T_GROUP_DEBT, T_GROUP_SPAN,
    ModuleIds, ISwapModule
} from "./lib/ExecutorShared.sol";

/*
 * ArbExecutor.sol — flash-funded backrun executor (the `arb` branch).
 *
 * The liquidation Executor without liquidations: borrow → swap → repay →
 * keep the rest. A backrun is one flash group whose repay blob is the whole
 * cycle (the anchor swap and the exit, `liq-arb`), then profit legs that
 * close what is left into WETH. Same plan wire format (PLAN-ENCODING), with
 * `liqCount == 0` in every group; same swap legs, run by DELEGATECALL to the
 * same deployed `SwapModule` the Executor uses. Its own contract so arb
 * funds, approvals and code stay apart from the liquidator's.
 *
 * SECURITY MODEL — the Executor's, unchanged where it applies:
 *  - OPERATOR is a hot key with one right: call execute(). It cannot move
 *    funds to an address of its choosing or make an arbitrary call.
 *  - More operator keys (parallel nonce lanes, so several backruns can land
 *    in one block) are added and revoked by ADMIN, a cold key that is never
 *    an operator. Each has OPERATOR's one right and nothing more; ADMIN's
 *    only right is that list. OPERATOR itself is immutable and costs no
 *    storage read; an added key costs one (2,100 gas, cold).
 *  - PROFIT_SINK is immutable (the liquidator's own sink). Profit is
 *    `gross = wethAfter - wethBefore`, which underflows if the standing
 *    balance falls, so a plan can never spend more than it earns.
 *  - sweep() is permissionless: the destination is fixed.
 *  - Every flash callback checks msg.sender is the provider just called AND
 *    that we are inside our own execute() (transient storage, EIP-1153).
 *  - The swap module's address is an immutable, checked at construction to
 *    answer the swap MODULE_ID and our WETH; delegatecall goes nowhere else.
 *  - No proxy. The operator list is the only setter.
 *
 * Providers: Uniswap V4 `take` (free), Morpho Blue (free), and a Uniswap V3
 * flash swap — the anchor pool itself pays the bought token first and is
 * paid in the callback, so the anchor swap funds the backrun.
 *
 * NOT AUDITED. Fork-test every provider and venue before mainnet.
 */
contract ArbExecutor {
    using SafeTransfer for address;
    using PlanDecoder for bytes;

    address public immutable OPERATOR;
    /// Cold key: adds and revokes operators, nothing else.
    address public immutable ADMIN;
    address public immutable PROFIT_SINK;
    /// Canonical WETH. Profit is denominated in ETH: every plan converges here.
    address public immutable WETH;
    /// Uniswap V3 factory — verifies swap callbacks by CREATE2. Public: the
    /// swap module reads it back through this contract (`IV3Anchors`).
    address public immutable UNIV3_FACTORY;
    bytes32 public immutable UNIV3_POOL_INIT_HASH;
    /// Swap and unwrap legs (delegatecall).
    address public immutable SWAP_MODULE;

    /// Operator keys beside OPERATOR, each with its one right.
    mapping(address => bool) public isOperator;

    uint256 private constant NO_CALLBACK = type(uint256).max;

    // Provider ids — the liquidation Executor's, so one encoder serves both.
    uint8 private constant P_UNIV4      = 2;
    uint8 private constant P_MORPHO     = 3;
    uint8 private constant P_UNIV3_SWAP = 6;

    uint160 private constant TICK_MIN_SQRT = 4295128739;
    uint160 private constant TICK_MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    uint8 private constant F_SWEEP = 1 << 0;

    error NotOperator();
    error NotAdmin();
    /// ADMIN, the profit sink, or the immutable OPERATOR as a listed key.
    error BadOperator(address key);
    error BadCallback();
    error Reentrant();
    error Unprofitable(uint256 gained, uint256 required);
    error UnknownProvider(uint8 p);
    error BadSwapCallback();
    /// A group with no swap legs, or with liquidation legs (an arb plan
    /// carries none).
    error NoLegs();
    error BidFailed(uint256 amount);
    error NoCallback(uint8 provider);
    error FlashMismatch();
    error ZeroAddress();
    error BadModule(address module);
    // Raised by the swap module, run as this contract.
    error UnknownVenue(uint8 v);
    error RouterNotAllowed(address target);
    error RouterCallFailed(address target);
    error BadPool(uint8 venue, address pool);
    error ExactOutUnsupported(uint8 venue);
    error NotDelegated();

    event OperatorSet(address indexed key, bool allowed);

    constructor(
        address operator_, address admin_, address profitSink_, address weth_,
        address univ3Factory_, bytes32 univ3InitHash_, address swapModule_
    ) {
        if (operator_ == address(0) || admin_ == address(0) || profitSink_ == address(0)
            || weth_ == address(0) || univ3Factory_ == address(0)) {
            revert ZeroAddress();
        }
        // The admin is cold: a hot operator key must not also hold the list.
        if (admin_ == operator_) revert BadOperator(admin_);
        _checkModule(swapModule_, ModuleIds.SWAP, weth_);
        OPERATOR             = operator_;
        ADMIN                = admin_;
        PROFIT_SINK          = profitSink_;
        WETH                 = weth_;
        UNIV3_FACTORY        = univ3Factory_;
        UNIV3_POOL_INIT_HASH = univ3InitHash_;
        SWAP_MODULE          = swapModule_;
    }

    /// The module is code that names itself `id` and works on our WETH.
    function _checkModule(address module, bytes32 id, address weth_) private view {
        if (module.code.length == 0) revert BadModule(module);
        (bool ok, bytes memory r) = module.staticcall(abi.encodeCall(ISwapModule.MODULE_ID, ()));
        if (!ok || r.length != 32 || abi.decode(r, (bytes32)) != id) revert BadModule(module);
        (ok, r) = module.staticcall(abi.encodeCall(ISwapModule.WETH, ()));
        if (!ok || r.length != 32 || abi.decode(r, (address)) != weth_) revert BadModule(module);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Entry point
    // ──────────────────────────────────────────────────────────────────────
    function execute(bytes calldata plan) external payable {
        // OPERATOR first: the immutable compare spares it the storage read.
        if (msg.sender != OPERATOR && !isOperator[msg.sender]) revert NotOperator();
        uint256 entered;
        assembly { entered := tload(T_ENTERED) }
        if (entered != 0) revert Reentrant();
        assembly { tstore(T_ENTERED, 1) }

        (Plan memory p, FlashGroup[] memory groups) = plan.header();

        uint256 wethBefore = IWETH(WETH).balanceOf(address(this));

        for (uint256 g; g < groups.length; ++g) {
            FlashGroup memory fg = groups[g];
            assembly { tstore(T_FILLED, not(0)) } // NO_CALLBACK sentinel
            _storeGroup(fg);
            _arm(fg.flashSource);
            _initiate(fg, plan);     // returns only after the callback settled
            if (fg.provider == P_MORPHO) {
                fg.debtAsset.safeApprove(fg.flashSource, 0);
            }
            _disarm();

            uint256 f;
            assembly { f := tload(T_FILLED) }
            if (f == NO_CALLBACK) revert NoCallback(fg.provider);
        }

        // What is left, in whatever token the cycle started, closes into WETH.
        uint8 profitLegs = uint8(plan[p.profitSwapOffset]);
        if (profitLegs != 0) _runSwaps(p.profitSwapOffset + 1, profitLegs, plan);

        uint256 gross = IWETH(WETH).balanceOf(address(this)) - wethBefore;
        // Underflow here is the correct failure: the cycle did not cover gas.
        uint256 net = gross - p.gasCostWei;
        // The bid is a fraction of REALIZED net, paid to coinbase: an
        // optimistic quote shrinks the bid rather than what we keep.
        uint256 bid = (net * p.bidBps) / 10_000;
        uint256 keep = net - bid;
        if (keep < p.minProfit) revert Unprofitable(keep, p.minProfit);

        if (bid != 0) {
            uint256 fromWallet = msg.value;
            if (fromWallet != 0) {
                if (bid > fromWallet) bid = fromWallet;
            } else {
                IWETH(WETH).withdraw(bid);
            }
            (bool ok, ) = block.coinbase.call{value: bid}("");
            if (!ok) revert BidFailed(bid);
            if (fromWallet > bid) {
                (bool r, ) = msg.sender.call{value: fromWallet - bid}("");
                if (!r) revert BidFailed(fromWallet - bid);
            }
        } else if (msg.value != 0) {
            (bool r, ) = msg.sender.call{value: msg.value}("");
            if (!r) revert BidFailed(msg.value);
        }

        if (p.flags & F_SWEEP != 0) {
            _sweep(WETH);
        }

        assembly { tstore(T_ENTERED, 0) }
    }

    /// ADMIN adds (`allowed`) or revokes an operator key. Never ADMIN itself,
    /// the profit sink, or OPERATOR (immutable, always allowed).
    function setOperator(address key, bool allowed) external {
        if (msg.sender != ADMIN) revert NotAdmin();
        if (key == address(0)) revert ZeroAddress();
        if (key == ADMIN || key == PROFIT_SINK || key == OPERATOR) revert BadOperator(key);
        isOperator[key] = allowed;
        emit OperatorSet(key, allowed);
    }

    /// Permissionless: the destination is immutable.
    function sweep(address[] calldata assets) external {
        for (uint256 i; i < assets.length; ++i) {
            _sweep(assets[i]);
        }
    }

    function _sweep(address asset) internal {
        uint256 bal = IERC20(asset).balanceOf(address(this));
        if (bal != 0) asset.safeTransfer(PROFIT_SINK, bal);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Flash initiation
    // ──────────────────────────────────────────────────────────────────────
    function _initiate(FlashGroup memory p, bytes calldata plan) internal {
        uint8 provider = p.provider;
        if (provider == P_UNIV4) {
            IPoolManager(p.flashSource).unlock(plan);
        } else if (provider == P_UNIV3_SWAP) {
            // Exact output of `debtAsset` (the anchor's bought token). The
            // pool pays it, the cycle runs in the callback, the pool is paid.
            bool zeroForOne = IUniV3Pool(p.flashSource).token0() != p.debtAsset;
            IUniV3Pool(p.flashSource).swap(
                address(this),
                zeroForOne,
                -int256(uint256(p.flashAmount)),
                zeroForOne ? TICK_MIN_SQRT + 1 : TICK_MAX_SQRT - 1,
                plan
            );
        } else if (provider == P_MORPHO) {
            IMorpho(p.flashSource).flashLoan(p.debtAsset, p.flashAmount, plan);
        } else {
            revert UnknownProvider(provider);
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Callback authentication
    // ──────────────────────────────────────────────────────────────────────
    function _arm(address expected) internal {
        assembly { tstore(T_EXPECTED_CALLER, expected) }
    }

    function _disarm() internal {
        assembly { tstore(T_EXPECTED_CALLER, 0) }
    }

    /// msg.sender is the provider we just called, inside our own execute().
    function _checkCallback() internal view {
        address expected;
        uint256 entered;
        assembly {
            expected := tload(T_EXPECTED_CALLER)
            entered  := tload(T_ENTERED)
        }
        if (msg.sender != expected || entered == 0) revert BadCallback();
    }

    function _storeGroup(FlashGroup memory fg) private {
        uint256 source = uint256(fg.provider) << 160 | uint256(uint160(fg.flashSource));
        uint256 debt = uint256(fg.repaySwapCount) << 168 | uint256(fg.liqCount) << 160
            | uint256(uint160(fg.debtAsset));
        uint256 span = fg.repaySwapOffset << 192 | fg.liqOffset << 128 | uint256(fg.flashAmount);
        assembly {
            tstore(T_GROUP_SOURCE, source)
            tstore(T_GROUP_DEBT, debt)
            tstore(T_GROUP_SPAN, span)
        }
    }

    function _currentGroup() internal view returns (FlashGroup memory fg) {
        uint256 source;
        uint256 debt;
        uint256 span;
        assembly {
            source := tload(T_GROUP_SOURCE)
            debt   := tload(T_GROUP_DEBT)
            span   := tload(T_GROUP_SPAN)
        }
        fg.provider        = uint8(source >> 160);
        fg.flashSource     = address(uint160(source));
        fg.debtAsset       = address(uint160(debt));
        fg.liqCount        = uint8(debt >> 160);
        fg.repaySwapCount  = uint8(debt >> 168);
        fg.flashAmount     = uint128(span);
        fg.liqOffset       = uint64(span >> 128);
        fg.repaySwapOffset = span >> 192;
    }

    // ──────────────────────────────────────────────────────────────────────
    // Provider callbacks
    // ──────────────────────────────────────────────────────────────────────

    /// Uniswap V4. Zero fee: take() to borrow, sync/transfer/settle to return.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        uint256 swapping;
        assembly { swapping := tload(T_SWAPPING) }
        // A V4 swap leg's own unlock: only the PoolManager, only mid-swap.
        if (swapping != 0) {
            if (msg.sender != MainnetVenues.V4_POOL_MANAGER) revert BadSwapCallback();
            _delegate(SWAP_MODULE, abi.encodeCall(ISwapModule.v4SwapCallback, (data)));
            return "";
        }
        _checkCallback();
        FlashGroup memory fg = _currentGroup();

        IPoolManager(msg.sender).take(fg.debtAsset, address(this), fg.flashAmount);
        if (msg.sender == MainnetVenues.V4_POOL_MANAGER) {
            assembly { tstore(T_V4_UNLOCKED, 1) }
        }
        _core(fg, data, 0);
        assembly { tstore(T_V4_UNLOCKED, 0) }
        IPoolManager(msg.sender).sync(fg.debtAsset);
        fg.debtAsset.safeTransfer(msg.sender, fg.flashAmount);
        IPoolManager(msg.sender).settle();
        return "";
    }

    /// Morpho Blue. Zero fee, pull-based.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        _checkCallback();
        FlashGroup memory fg = _currentGroup();
        if (assets != fg.flashAmount) revert FlashMismatch();
        _core(fg, data, 0);
        fg.debtAsset.safeApprove(msg.sender, assets);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Core: the cycle's swap legs. Repayment is the caller's job.
    // ──────────────────────────────────────────────────────────────────────
    function _core(FlashGroup memory fg, bytes calldata plan, uint256 fee) internal {
        if (fg.repaySwapCount == 0 || fg.liqCount != 0) revert NoLegs();
        // No liquidation filled (there are none), which also marks that the
        // provider called back: `execute` reads anything but NO_CALLBACK as
        // that. Arb swap legs are untied, so `runSwaps` runs them all.
        assembly {
            tstore(T_FILLED, 0)
            tstore(T_FEE, fee)
        }
        _runSwaps(fg.repaySwapOffset, fg.repaySwapCount, plan);
        assembly { tstore(T_FEE, 0) }
    }

    function _runSwaps(uint256 offset, uint8 legs, bytes calldata plan) internal {
        _delegate(SWAP_MODULE, abi.encodeCall(ISwapModule.runSwaps, (offset, legs, plan)));
    }

    function _delegate(address module, bytes memory data) private returns (bytes memory ret) {
        bool ok;
        (ok, ret) = module.delegatecall(data);
        if (!ok) {
            assembly { revert(add(ret, 0x20), mload(ret)) }
        }
    }

    /// Uniswap V3 swap callback (and SushiSwap V3's). Outside a swap leg the
    /// only one taken is the flash swap's, from the armed pool.
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        uint256 swapping;
        assembly { swapping := tload(T_SWAPPING) }
        if (swapping == 0) {
            _flashSwap(amount0Delta, amount1Delta, data);
            return;
        }
        _v3SwapCallback(amount0Delta, amount1Delta, data);
    }

    /// PancakeSwap V3's swap callback. Never a flash-swap source.
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        uint256 swapping;
        assembly { swapping := tload(T_SWAPPING) }
        if (swapping == 0) revert BadSwapCallback();
        _v3SwapCallback(amount0Delta, amount1Delta, data);
    }

    /// A V3-family swap callback inside a swap leg: the caller must be the
    /// pool the leg's deployer derives (the Executor's rule, verbatim).
    function _v3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) private {
        address tokenIn;
        address expected;
        if (data.length == 96) {
            uint24 fee;
            (tokenIn, expected, fee) = abi.decode(data, (address, address, uint24));
            (address t0, address t1) = tokenIn < expected ? (tokenIn, expected) : (expected, tokenIn);
            expected = address(uint160(uint256(keccak256(abi.encodePacked(
                hex"ff", UNIV3_FACTORY, keccak256(abi.encode(t0, t1, fee)), UNIV3_POOL_INIT_HASH
            )))));
        } else if (data.length == 128) {
            address tokenOut;
            uint24 fee;
            uint8 fid;
            (tokenIn, tokenOut, fee, fid) = abi.decode(data, (address, address, uint24, uint8));
            expected = MainnetVenues.v3ForkPool(fid, tokenIn, tokenOut, fee);
        } else {
            _delegate(SWAP_MODULE, abi.encodeCall(ISwapModule.v3ChainCallback, (amount0Delta, amount1Delta, data)));
            return;
        }
        if (msg.sender != expected) revert BadSwapCallback();
        uint256 owed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        tokenIn.safeTransfer(msg.sender, owed);
    }

    /// The flash swap's callback: the anchor pool has paid the bought token
    /// and is owed its other one, which the cycle produces.
    function _flashSwap(int256 amount0Delta, int256 amount1Delta, bytes calldata plan) internal {
        address expected;
        uint256 entered;
        assembly {
            expected := tload(T_EXPECTED_CALLER)
            entered  := tload(T_ENTERED)
        }
        if (msg.sender != expected || entered == 0) revert BadSwapCallback();
        FlashGroup memory fg = _currentGroup();
        if (fg.provider != P_UNIV3_SWAP) revert BadSwapCallback();

        address t0 = IUniV3Pool(msg.sender).token0();
        address t1 = IUniV3Pool(msg.sender).token1();
        int256 paid;
        int256 owed;
        address tokenIn;
        if (t0 == fg.debtAsset) {
            (paid, owed, tokenIn) = (amount0Delta, amount1Delta, t1);
        } else if (t1 == fg.debtAsset) {
            (paid, owed, tokenIn) = (amount1Delta, amount0Delta, t0);
        } else {
            revert FlashMismatch();
        }
        if (paid != -int256(uint256(fg.flashAmount)) || owed <= 0) revert FlashMismatch();

        _core(fg, plan, 0);
        tokenIn.safeTransfer(msg.sender, uint256(owed));
    }

    /// WETH unwrapped for a coinbase bid, or native ETH a leg pays out.
    receive() external payable {}
}
