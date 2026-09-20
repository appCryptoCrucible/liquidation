// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/*
 * Executor.sol — flashloan-funded liquidation executor.
 *
 * Funds are in transit only: borrow → liquidate → swap → repay → (maybe) sweep.
 *
 * SECURITY MODEL
 *  - OPERATOR is a hot key. It can only call execute(). It cannot move funds to
 *    any address of its choosing, cannot change PROFIT_SINK, and cannot make an
 *    arbitrary external call.
 *  - PROFIT_SINK is immutable. Every token that leaves this contract, other than
 *    a flashloan repayment or a liquidation repay, goes there. A compromised
 *    OPERATOR key therefore cannot steal; the worst it can do is waste gas.
 *  - sweep() is permissionless for exactly that reason: the destination is fixed,
 *    so letting anyone push funds out is a backstop, not a risk.
 *  - Callback authentication uses transient storage (EIP-1153). Every flashloan
 *    callback verifies BOTH that msg.sender is the provider we just called AND
 *    that we are inside our own execute(). This is the critical bug class for
 *    this contract type: an unauthenticated callback is a free-money function.
 *
 * NOT AUDITED. Fork-test every adapter × in-scope provider pair before mainnet, and get
 * an independent review of the callback auth and approval logic specifically.
 */

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}

/*
 * SafeTransfer — mandatory, not defensive.
 *
 * USDT (and BNB, OMG, and others) predate the finalized ERC-20 and return NO
 * data from transfer/approve. Calling them through the bool-returning interface
 * above succeeds at the EVM level and then reverts in Solidity's ABI decoder,
 * which expects 32 bytes. USDT is one of the largest debt assets on Aave, so
 * the naive interface silently excludes a large share of the opportunity set —
 * and it presents as "those liquidations never work," not as an obvious bug.
 *
 * USDT additionally reverts on a non-zero -> non-zero approve, so every approval
 * is zeroed first. When the allowance is already zero — the normal case, since
 * approvals here are exact and consumed — that write is same-value and cheap.
 * When it is not, this is the difference between working and being permanently
 * stuck at that allowance.
 */
library SafeTransfer {
    /*
     * Deliberate deviation from OpenZeppelin's SafeERC20: no extcodesize check.
     * A call to an address with no code returns success with empty returndata,
     * which would pass these checks. That hole is closed OFF-chain instead —
     * every token here comes from the verified registry, and the boot assertion
     * (REGISTRY.md §4) has already called decimals() on it, which an EOA
     * cannot answer. Paying ~2600 gas per transfer on the hot path to re-prove
     * something startup already proved is not worth it. If you ever make token
     * addresses reachable from an unverified source, add the check back.
     */
    error TransferFailed(address token, address to, uint256 amount);
    error ApproveFailed(address token, address spender, uint256 amount);

    function safeTransfer(address token, address to, uint256 amount) internal {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
            revert TransferFailed(token, to, amount);
        }
    }

    function safeApprove(address token, address spender, uint256 amount) internal {
        _approve(token, spender, 0);
        if (amount != 0) _approve(token, spender, amount);
    }

    function _approve(address token, address spender, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
            revert ApproveFailed(token, spender, amount);
        }
    }
}

interface IWETH {
    function balanceOf(address) external view returns (uint256);
    function withdraw(uint256) external;
}

interface IAavePool {
    function flashLoanSimple(
        address receiver, address asset, uint256 amount,
        bytes calldata params, uint16 referralCode
    ) external;
    function liquidationCall(
        address collateral, address debt, address user,
        uint256 debtToCover, bool receiveAToken
    ) external;
    function getUserAccountData(address user) external view returns (
        uint256, uint256, uint256, uint256, uint256, uint256 healthFactor
    );
}

interface IUniV3Pool {
    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external;
    function token0() external view returns (address);
}

interface IPoolManager {
    function unlock(bytes calldata data) external returns (bytes memory);
    function take(address currency, address to, uint256 amount) external;
    function sync(address currency) external;
    function settle() external payable returns (uint256);
}

interface IMorpho {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

/// Sky DSS Flash (MCD Flash) — ERC-3156 Dai flash-mint. Mainnet:
/// 0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA. DAI only; fee via flashFee (0 today).
interface IDssFlash {
    function flashLoan(
        address receiver, address token, uint256 amount, bytes calldata data
    ) external returns (bool);
    function flashFee(address token, uint256 amount) external view returns (uint256);
    function maxFlashLoan(address token) external view returns (uint256);
    function dai() external view returns (address);
}

interface IUniV3PoolSwap {
    function swap(
        address recipient, bool zeroForOne, int256 amountSpecified,
        uint160 sqrtPriceLimitX96, bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
}

interface IAllowlistedRouter {
    // Curve / aggregator legs go through an allowlisted target with
    // venue-specific calldata. Never an arbitrary call — see _swapLeg.
}

contract Executor {
    using SafeTransfer for address;

    // ──────────────────────────────────────────────────────────────────────
    // Immutables. No storage variables at all — every SLOAD avoided is gas
    // saved on the hot path, and every absent setter is an attack surface
    // that does not exist.
    // ──────────────────────────────────────────────────────────────────────
    address public immutable OPERATOR;
    address public immutable PROFIT_SINK;
    /// Uniswap V3 factory — used to verify swap callbacks by CREATE2 address.
    address public immutable UNIV3_FACTORY;
    bytes32 public immutable UNIV3_POOL_INIT_HASH;
    /// Allowlisted routers for Curve / aggregator legs. Fixed at construction:
    /// a mutable allowlist is an operator-reachable arbitrary call.
    address public immutable ROUTER_A;
    address public immutable ROUTER_B;
    /// Canonical WETH. Profit is denominated in ETH, so every plan converges here.
    address public immutable WETH;

    // Transient storage slots (EIP-1153). ~100 gas vs 20k/5k for SSTORE.
    uint256 private constant T_EXPECTED_CALLER = 0x00;
    uint256 private constant T_ENTERED         = 0x01;
    uint256 private constant T_SWAPPING        = 0x02;
    /// Which flash group is executing, so a callback can find its own legs.
    uint256 private constant T_GROUP           = 0x03;
    /// Successful liquidation legs in the group currently executing.
    uint256 private constant T_FILLED          = 0x04;

    // Swap leg venues. Uniswap V4 is deliberately absent: hooks make swap
    // behaviour pool-specific, so it is excluded as a routing venue even though
    // it remains the preferred flashloan source. See GUIDE 12 Step 3.
    uint8 private constant S_UNIV3_POOL = 0;  // pool-direct, transfer-in-callback, no approval
    uint8 private constant S_ROUTER     = 1;  // allowlisted router (Curve, …)

    // Provider ids
    uint8 private constant P_AAVE      = 0;
    uint8 private constant P_UNIV3     = 1;
    uint8 private constant P_UNIV4     = 2;
    uint8 private constant P_MORPHO    = 3;
    uint8 private constant P_SKY_DSS   = 4; // Sky DSS Flash (ERC-3156); reclaimed from Balancer reservation

    // Adapter ids
    uint8 private constant A_AAVE_V3   = 0;
    uint8 private constant A_AAVE_V4   = 1;
    uint8 private constant A_MORPHO    = 2;
    // A_COMPOUND_V3, … added per GUIDE 15 / 10R-n

    // Flags
    uint8 private constant F_SWEEP = 1 << 0; // sweep after this plan
    // F_SKIP_SWAP and F_RECEIVE_ATOKEN are gone. Skipping is expressed by
    // emitting zero swap legs, and aTokens are never taken — profit has to
    // converge on WETH, and a receipt token cannot.

    error NotOperator();
    error BadCallback();
    error Reentrant();
    error NotLiquidatable(uint256 hf);
    error Unprofitable(uint256 gained, uint256 required);
    error UnknownProvider(uint8 p);
    error UnknownAdapter(uint8 a);

    error RouterNotAllowed(address target);
    error BadSwapCallback();
    error NoLegs();
    error NoGroups();
    error BidFailed(uint256 amount);
    /// Every leg was taken by someone else between simulation and inclusion.
    /// Reverting drops the bundle, which costs nothing — see GUIDE 13.
    error AllLegsFailed();

    constructor(
        address operator_, address profitSink_,
        address univ3Factory_, bytes32 univ3InitHash_,
        address routerA_, address routerB_, address weth_
    ) {
        OPERATOR             = operator_;
        PROFIT_SINK          = profitSink_;
        UNIV3_FACTORY        = univ3Factory_;
        UNIV3_POOL_INIT_HASH = univ3InitHash_;
        ROUTER_A             = routerA_;
        ROUTER_B             = routerB_;
        WETH                 = weth_;
    }

    // ──────────────────────────────────────────────────────────────────────
    // Plan — decoded from packed calldata. See PLAN-ENCODING.md; the Rust
    // encoder in liq-exec MUST match this byte-for-byte.
    // ──────────────────────────────────────────────────────────────────────
    /// Shared across the whole plan — and note what is NOT here: the flashloan.
    /// One oracle update makes positions liquidatable across many debt assets, so
    /// a plan carries several flash groups executed in sequence (PLAN-ENCODING §1b).
    ///
    /// What is shared: one profit floor, one bid fraction, one gas estimate.
    /// Profit converges on WETH from every group, so the guard stays a single wei
    /// check however many assets were borrowed. That is what makes multi-asset
    /// batching tractable at all.
    struct Plan {
        uint8   flags;
        uint16  bidBps;           // fraction of net paid to block.coinbase
        uint128 gasCostWei;       // predicted total gas cost, from the off-chain
                                  // oracle. Exact base fee is known a block ahead.
        uint128 minProfit;        // **in wei**, what we keep AFTER the bid
        uint256 groupOffset;      // calldata offset of the flash-group blob
        uint256 profitSwapOffset; // calldata offset of the profit-swap blob
    }

    /// One flashloan and everything it funds. Run to completion — borrow,
    /// liquidate, swap to cover the repay, settle — before the next group starts.
    /// Sequential, never nested: nesting deepens the call stack for no benefit and
    /// makes every callback's authentication harder to reason about.
    struct FlashGroup {
        uint8   provider;
        address flashSource;
        address debtAsset;
        uint128 flashAmount;
        uint8   liqCount;
        uint8   repaySwapCount;
        uint256 liqOffset;
        uint256 repaySwapOffset;
    }

    /// One position. A single-position plan is one group with `liqCount == 1` —
    /// there is no special case, which is the point: the batched path is the only
    /// path, so it is exercised by every test rather than by a rare one.
    struct LiqLeg {
        uint8   adapter;          // per-leg: a batch may span protocols
        address market;           // Aave Pool / V4 Spoke / Morpho market
        address borrower;
        address collateralAsset;
        uint128 repayAmount;
    }

    uint256 private constant HEADER_LEN   = 35;
    uint256 private constant GROUP_HEAD_LEN = 59;
    uint256 private constant LIQ_LEG_LEN = 77;
    /// venue 1 | tokenIn 20 | tokenOut 20 | flags 1 | amount 16 | dataLen 2
    uint256 private constant SWAP_LEG_HEAD_LEN = 60;

    /// Swap-leg flag: ignore the encoded amount and take this contract's whole
    /// balance of `tokenIn`. Set it on the last leg for each collateral.
    uint8 private constant L_TAKE_BALANCE = 1 << 0;
    /// Swap-leg flag: `amountIn` is an exact OUTPUT target, not an input amount.
    /// Used for the repay leg so no debt-token dust is left behind.
    uint8 private constant L_EXACT_OUT    = 1 << 1;

    // ──────────────────────────────────────────────────────────────────────
    // Entry point
    // ──────────────────────────────────────────────────────────────────────
    function execute(bytes calldata plan) external payable {
        if (msg.sender != OPERATOR) revert NotOperator();
        assembly {
            if tload(T_ENTERED) { mstore(0x00, 0x2f3bef71) revert(0x1c, 0x04) } // Reentrant()
            tstore(T_ENTERED, 1)
        }

        Plan memory p = _decode(plan);

        // Profit is denominated in ETH, always, whatever was borrowed or seized.
        // Snapshot WETH before any borrowing, and measure after every group has
        // repaid. Anything left is ours — including the case where some group's
        // debt asset was WETH itself and repay and profit shared a balance.
        uint256 wethBefore = IWETH(WETH).balanceOf(address(this));

        uint8 groups = uint8(plan[p.groupOffset]);
        if (groups == 0) revert NoGroups();

        // Sequential, not nested (D32). Each group borrows, liquidates, swaps enough
        // to cover its own repay, and settles before the next one starts.
        // Multi-source cascade = sibling groups with the same debtAsset and different
        // providers (PLAN-ENCODING §1b′) — never nested flash callbacks.
        uint256 cursor = p.groupOffset + 1;
        uint256 filled;
        for (uint8 g; g < groups; ++g) {
            (FlashGroup memory fg, uint256 next) = _decodeGroup(plan, cursor);
            cursor = next;

            assembly { tstore(T_GROUP, g) }
            _arm(fg.flashSource);
            _initiate(fg, plan);     // returns only after the callback settled
            _disarm();

            uint256 f;
            assembly { f := tload(T_FILLED) }
            filled += f;
        }
        if (filled == 0) revert AllLegsFailed();

        // Everything that is left, across every group, converges on WETH here.
        _swap(p.profitSwapOffset, plan);

        uint256 gross = IWETH(WETH).balanceOf(address(this)) - wethBefore;

        // Underflow here is the correct failure: gross ETH did not cover gas, so
        // the liquidation was never worth doing and the bundle should drop.
        uint256 net = gross - p.gasCostWei;

        // The bid is a fraction of REALIZED net, not of what we predicted. That
        // is the whole reason to pay via coinbase rather than priority fee: if
        // the quote was optimistic, the bid shrinks with it and the 1% we keep
        // survives. A priority fee is committed before the swap and cannot.
        uint256 bid = (net * p.bidBps) / 10_000;
        uint256 keep = net - bid;
        if (keep < p.minProfit) revert Unprofitable(keep, p.minProfit);

        if (bid != 0) {
            // Wallet-funded bidding, off by default. `msg.value` is a CEILING,
            // never the bid itself — the bid is still computed from realized net
            // so an optimistic quote shrinks it rather than overpaying. Anything
            // unspent goes straight back to the operator.
            //
            // Deliberately built and deliberately unused: the executor is
            // immutable, so this costs a branch now and a redeploy later. See
            // GUIDE 10 §4d.
            uint256 fromWallet = msg.value;
            if (fromWallet != 0) {
                if (bid > fromWallet) bid = fromWallet;
            } else {
                IWETH(WETH).withdraw(bid);
            }

            // `call`, not `transfer`: a fee recipient that is a contract will
            // fail on the 2300-gas stipend. EIP-3651 pre-warms COINBASE, so
            // this costs ~9k rather than paying cold access on top.
            (bool ok, ) = block.coinbase.call{value: bid}("");
            if (!ok) revert BidFailed(bid);

            // Never read address(this).balance here — `receive()` is open, so a
            // donation would be indistinguishable from the bid budget and would
            // be bid away. Explicit accounting only.
            if (fromWallet > bid) {
                (bool r, ) = msg.sender.call{value: fromWallet - bid}("");
                if (!r) revert BidFailed(fromWallet - bid);
            }
        } else if (msg.value != 0) {
            (bool r, ) = msg.sender.call{value: msg.value}("");
            if (!r) revert BidFailed(msg.value);
        }

        // Residual is WETH and only WETH. Sweeping is now a pure gas question —
        // is the balance worth a transfer — not a risk budget, because nothing
        // else accumulates here.
        if (p.flags & F_SWEEP != 0) {
            _sweep(WETH);
        }

        assembly { tstore(T_ENTERED, 0) }
    }

    /// Permissionless: destination is immutable, so anyone may push funds out.
    /// Backstop against an operator that never sets F_SWEEP.
    function sweep(address[] calldata assets) external {
        for (uint256 i; i < assets.length; ++i) {
            _sweep(assets[i]);
        }
    }

    function _sweep(address asset) internal {
        uint256 bal = IERC20(asset).balanceOf(address(this)); // warm here: ~100 gas
        if (bal != 0) asset.safeTransfer(PROFIT_SINK, bal);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Flashloan initiation
    // ──────────────────────────────────────────────────────────────────────
    function _initiate(FlashGroup memory p, bytes calldata plan) internal {
        uint8 provider = p.provider;

        if (provider == P_UNIV4) {
            // V4 hands us nothing up front; we take() inside unlockCallback.
            IPoolManager(p.flashSource).unlock(plan);
        } else if (provider == P_AAVE) {
            IAavePool(p.flashSource).flashLoanSimple(
                address(this), p.debtAsset, p.flashAmount, plan, 0
            );
        } else if (provider == P_UNIV3) {
            bool zeroForOne = IUniV3Pool(p.flashSource).token0() == p.debtAsset;
            IUniV3Pool(p.flashSource).flash(
                address(this),
                zeroForOne ? p.flashAmount : 0,
                zeroForOne ? 0 : p.flashAmount,
                plan
            );
        } else if (provider == P_MORPHO) {
            IMorpho(p.flashSource).flashLoan(p.debtAsset, p.flashAmount, plan);
        } else if (provider == P_SKY_DSS) {
            // ERC-3156: Flash pulls repayment via transferFrom after onFlashLoan.
            // debtAsset must be Flash.dai(); off-chain encoder enforces that.
            IDssFlash(p.flashSource).flashLoan(
                address(this), p.debtAsset, p.flashAmount, plan
            );
        } else {
            revert UnknownProvider(provider);
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Callback authentication — the critical security boundary.
    // ──────────────────────────────────────────────────────────────────────
    function _arm(address expected) internal {
        assembly { tstore(T_EXPECTED_CALLER, expected) }
    }

    function _disarm() internal {
        assembly { tstore(T_EXPECTED_CALLER, 0) }
    }

    /// Reverts unless msg.sender is the provider we just called, inside our own
    /// execute(). Both conditions are required: the first stops an arbitrary
    /// contract from calling our callback, the second stops a legitimate
    /// provider's callback being triggered by someone else's flashloan.
    function _checkCallback() internal view {
        address expected;
        uint256 entered;
        assembly {
            expected := tload(T_EXPECTED_CALLER)
            entered  := tload(T_ENTERED)
        }
        if (msg.sender != expected || entered == 0) revert BadCallback();
    }

    // ──────────────────────────────────────────────────────────────────────
    // Provider callbacks — each settles in its provider's own idiom
    // ──────────────────────────────────────────────────────────────────────

    /// Aave. Pull-based: approve exactly what is owed. The pull consumes the
    /// allowance to zero, so safeApprove's leading zeroing write is same-value
    /// and cheap in the normal case — and is what keeps USDT working.
    function executeOperation(
        address asset, uint256 amount, uint256 premium, address initiator, bytes calldata params
    ) external returns (bool) {
        _checkCallback();
        if (initiator != address(this)) revert BadCallback();
        _core(params);
        asset.safeApprove(msg.sender, amount + premium);
        return true;
    }

    /// The provider callbacks each need the group they belong to, and none of
    /// their ABIs has room to carry it — so the index goes through transient
    /// storage and the group is re-walked from calldata here.
    function _currentGroup(bytes calldata plan) internal view returns (FlashGroup memory fg) {
        uint256 g;
        assembly { g := tload(T_GROUP) }
        uint256 o = HEADER_LEN + 1;
        for (uint256 i; i < g; ++i) {
            (, uint256 next) = _decodeGroup(plan, o);
            o = next;
        }
        (fg, ) = _decodeGroup(plan, o);
    }

    /// Uniswap V3. Transfer-based: no approval anywhere in this path.
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _checkCallback();
        FlashGroup memory p = _currentGroup(data);
        _core(data);
        uint256 fee = fee0 != 0 ? fee0 : fee1;
        p.debtAsset.safeTransfer(msg.sender, uint256(p.flashAmount) + fee);
    }

    /// Uniswap V4. Zero fee. take() to borrow, sync/transfer/settle to return.
    /// unlock() reverts unless every currency delta nets to zero before it
    /// returns, so a missed settle fails closed rather than stealing.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        _checkCallback();
        FlashGroup memory p = _currentGroup(data);

        IPoolManager(msg.sender).take(p.debtAsset, address(this), p.flashAmount);
        _core(data);
        IPoolManager(msg.sender).sync(p.debtAsset);
        p.debtAsset.safeTransfer(msg.sender, p.flashAmount);
        IPoolManager(msg.sender).settle();

        return "";
    }

    /// Morpho Blue. Zero fee, pull-based.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        _checkCallback();
        FlashGroup memory p = _currentGroup(data);
        _core(data);
        p.debtAsset.safeApprove(msg.sender, assets);
    }

    /// Sky DSS Flash (ERC-3156). Pull-based repay: approve Flash for amount+fee.
    /// Must return the ERC-3156 success magic or the mint reverts.
    function onFlashLoan(
        address initiator, address token, uint256 amount, uint256 fee, bytes calldata data
    ) external returns (bytes32) {
        _checkCallback();
        if (initiator != address(this)) revert BadCallback();
        _core(data);
        token.safeApprove(msg.sender, amount + fee);
        return keccak256("ERC3156FlashBorrower.onFlashLoan");
    }

    // ──────────────────────────────────────────────────────────────────────
    // Core: guard → liquidate → swap. Repayment is the caller's job.
    // ──────────────────────────────────────────────────────────────────────
    function _core(bytes calldata plan) internal {
        uint256 g;
        assembly { g := tload(T_GROUP) }

        // Re-walk to this group rather than passing it through the provider's
        // callback ABI, which differs per provider and has no room for it.
        uint256 o = HEADER_LEN + 1;
        for (uint256 i; i < g; ++i) {
            (, uint256 next) = _decodeGroup(plan, o);
            o = next;
        }
        (FlashGroup memory fg, ) = _decodeGroup(plan, o);
        if (fg.liqCount == 0) revert NoLegs();

        // Each leg stands or falls alone. A competitor taking one position
        // between simulation and inclusion must not cost us the others — that is
        // the whole risk batching introduces, and per-leg tolerance is the whole
        // mitigation. The profit floor still judges the plan as a whole.
        uint256 filled;
        for (uint256 i; i < fg.liqCount; ++i) {
            LiqLeg memory l = _decodeLiqLeg(plan, fg.liqOffset + i * LIQ_LEG_LEN);
            if (_liquidateLeg(fg, l)) ++filled;
        }
        assembly { tstore(T_FILLED, filled) }

        // Only this group's repay swaps run here — exact-output into its debt
        // asset, sized to what it owes. Profit swaps run once, after every group
        // has settled, so they see the whole batch's leftover collateral at once
        // and can be solved jointly (GUIDE 12 §4c).
        if (fg.repaySwapCount != 0) {
            _swap(fg.repaySwapOffset, plan);
        }
    }

    /// On-chain recheck. Off-chain state can be one block stale; this converts
    /// that into a skipped leg (or, if every leg skips, a dropped bundle)
    /// instead of a loss. A reverting bundle is simply not included and costs
    /// nothing, so failing closed here is free.
    function _isLiquidatable(LiqLeg memory l) internal view returns (bool) {
        if (l.adapter == A_AAVE_V3) {
            (,,,,, uint256 hf) = IAavePool(l.market).getUserAccountData(l.borrower);
            return hf < 1e18;
        } else if (l.adapter == A_AAVE_V4) {
            // V4 exposes health at the Spoke. Wire to the real view function
            // once GUIDE 04 has confirmed its signature against the deployed
            // contracts; do not guess it from documentation.
            revert UnknownAdapter(l.adapter);
        } else {
            revert UnknownAdapter(l.adapter);
        }
    }

    /// Attempts one leg. Returns false — rather than reverting — when the
    /// position is already gone or the protocol rejects the call, so the rest
    /// of the batch survives.
    ///
    /// Note there is no `seized` return. With several collaterals in flight,
    /// sizing the swap from per-leg deltas means tracking a map; the swap blob
    /// takes whole balances instead (see `_swap`), which is both simpler and
    /// strictly more correct — it cannot disagree with what is actually held.
    function _liquidateLeg(FlashGroup memory p, LiqLeg memory l) internal returns (bool) {
        if (!_isLiquidatable(l)) return false;

        p.debtAsset.safeApprove(l.market, l.repayAmount);

        // `market` comes from the plan, so it is only ever a registry address
        // the operator encoded. try/catch does not bound gas, and a market that
        // burns the call's gas would take the batch down with it — acceptable
        // only because that set is curated. Do not widen it to arbitrary input.
        try IAavePool(l.market).liquidationCall(
            l.collateralAsset, p.debtAsset, l.borrower, l.repayAmount,
            false   // never receive aTokens: profit must converge on WETH
        ) {
            return true;
        } catch {
            // Beaten to it. Drop the allowance we just set rather than leaving
            // it standing against a protocol we did not transact with.
            p.debtAsset.safeApprove(l.market, 0);
            return false;
        }
    }

    /// Split swap across N pools, over however many collaterals the batch
    /// seized. The off-chain water-fill (GUIDE 12) decided the allocations —
    /// jointly across collaterals, since they share output pools; this executes
    /// them and nothing more.
    ///
    /// Blob layout, from `plan[blobOffset..]`:
    ///   1 byte   legCount
    ///   per leg: 1 venue | 20 tokenIn | 20 tokenOut | 1 legFlags | 16 amount
    ///            | 2 dataLen | dataLen bytes
    ///
    /// `tokenOut` is encoded rather than derived, because a plan spans several
    /// debt assets now and there is no single one to fall back to. 20 bytes a leg
    /// is a few hundred gas; a leg that guesses its own output is a silent
    /// misliquidation.
    ///
    /// Each leg carries its own `tokenIn`, because a batch can hold several
    /// collaterals at once and "the remainder" is meaningless without saying
    /// remainder *of what*. Instead of positional remainder logic, the last leg
    /// for each collateral sets `L_TAKE_BALANCE` and sweeps that token's whole
    /// balance — which cannot disagree with what is actually held, however the
    /// solver rounded or however much a partial fill under-delivered.
    ///
    /// **Two outputs, and the order is load-bearing.** `L_EXACT_OUT` legs come
    /// FIRST and buy exactly the debt asset needed to repay the flash — exact
    /// output, so no debt-token dust is stranded. Every later leg converts what
    /// is left to WETH, because profit is denominated in ETH: it is the unit the
    /// bid must be paid in, the unit gas is priced in, and therefore the unit the
    /// viability band (GUIDE 12 §4b) already works in. One numeraire end to end.
    ///
    /// The encoder must emit exact-output legs before balance-taking legs; an
    /// exact-output leg consumes an unknown amount of collateral, so anything
    /// sweeping a balance has to run after it.
    function _swap(uint256 blobOffset, bytes calldata plan) internal {
        bytes calldata blob = plan[blobOffset:];
        uint8 legs = uint8(blob[0]);
        uint256 cursor = 1;

        assembly { tstore(T_SWAPPING, 1) }

        for (uint8 i; i < legs; ++i) {
            uint8   venue    = uint8(blob[cursor]);
            address tokenIn  = address(bytes20(blob[cursor + 1  : cursor + 21]));
            address tokenOut = address(bytes20(blob[cursor + 21 : cursor + 41]));
            uint8   legFlags = uint8(blob[cursor + 41]);
            uint256 amt      = uint256(uint128(bytes16(blob[cursor + 42 : cursor + 58])));
            uint16  dataLen  = uint16(bytes2(blob[cursor + 58 : cursor + 60]));
            bytes calldata legData = blob[cursor + 60 : cursor + 60 + dataLen];
            cursor += 60 + dataLen;

            bool exactOut = legFlags & L_EXACT_OUT != 0;

            uint256 legAmt = (legFlags & L_TAKE_BALANCE != 0)
                ? IERC20(tokenIn).balanceOf(address(this))
                : amt;

            // A skipped liquidation leg means its collateral never arrived.
            // Nothing to swap is not an error — the guard judges the result.
            if (legAmt == 0) continue;

            _swapLeg(venue, tokenIn, tokenOut, legAmt, exactOut, legData);
        }

        assembly { tstore(T_SWAPPING, 0) }

        // No assertion that every collateral reached zero. A collateral the
        // encoder forgot to give a TAKE_BALANCE leg simply stays here and goes
        // to PROFIT_SINK on the next sweep — a leak of profit into the sink,
        // not a loss. The encoder round-trip test (PLAN-ENCODING.md §4) is
        // where that is supposed to be caught.

        // No per-leg amountOutMin: `minProfit` in execute() is the real
        // constraint and it is checked against the net balance change, which
        // covers every leg at once.
    }

    function _swapLeg(
        uint8 venue, address tokenIn, address tokenOut,
        uint256 amount, bool exactOut, bytes calldata data
    ) internal {
        if (venue == S_UNIV3_POOL) {
            // Pool-direct: we transfer inside uniswapV3SwapCallback, so there is
            // no approval on this path at all. Cheaper and a smaller surface.
            address pool = address(bytes20(data[0:20]));
            bool zeroForOne = tokenIn < tokenOut;
            // V3 encodes direction in the sign: positive = exact input,
            // negative = exact output. One call site, both modes.
            int256 specified = exactOut ? -int256(amount) : int256(amount);
            IUniV3PoolSwap(pool).swap(
                address(this), zeroForOne, specified,
                zeroForOne ? TickMath_MIN_SQRT + 1 : TickMath_MAX_SQRT - 1,
                abi.encode(tokenIn, tokenOut, IUniV3PoolSwap(pool).fee())
            );
        } else if (venue == S_ROUTER) {
            address target = address(bytes20(data[0:20]));
            if (target != ROUTER_A && target != ROUTER_B) revert RouterNotAllowed(target);
            // Exact approval, then zeroed unconditionally after the call. A
            // router that does not consume the full amount would otherwise
            // leave this contract with a standing allowance between
            // transactions — bounded, since routers are immutable and
            // allowlisted, but there is no reason to carry it.
            // For an exact-output router leg `amount` is the maximum input to
            // approve; the router's own calldata carries the exact output.
            tokenIn.safeApprove(target, amount);
            (bool ok, ) = target.call(data[20:]);
            if (!ok) revert RouterNotAllowed(target);
            tokenIn.safeApprove(target, 0);
        } else {
            revert UnknownProvider(venue);
        }
    }

    uint160 private constant TickMath_MIN_SQRT = 4295128739;
    uint160 private constant TickMath_MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    /// Uniswap V3 swap callback. Distinct selector from the flash callback, and
    /// it needs its own authentication: verify the caller IS the canonical pool
    /// for (token0, token1, fee) by CREATE2, and that we are mid-swap.
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        uint256 swapping;
        assembly { swapping := tload(T_SWAPPING) }
        if (swapping == 0) revert BadSwapCallback();

        (address tokenIn, address tokenOut, uint24 fee) = abi.decode(data, (address, address, uint24));
        (address t0, address t1) = tokenIn < tokenOut ? (tokenIn, tokenOut) : (tokenOut, tokenIn);
        address expected = address(uint160(uint256(keccak256(abi.encodePacked(
            hex"ff", UNIV3_FACTORY, keccak256(abi.encode(t0, t1, fee)), UNIV3_POOL_INIT_HASH
        )))));
        if (msg.sender != expected) revert BadSwapCallback();

        uint256 owed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        tokenIn.safeTransfer(msg.sender, owed);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Packed calldata decode. Layout is fixed; see PLAN-ENCODING.md.
    // ──────────────────────────────────────────────────────────────────────
    function _decode(bytes calldata plan) internal pure returns (Plan memory p) {
        p.flags      = uint8(plan[0]);
        p.bidBps     = uint16(bytes2(plan[1:3]));
        p.gasCostWei = uint128(bytes16(plan[3:19]));
        p.minProfit  = uint128(bytes16(plan[19:35]));

        // Offsets are derived by walking, never encoded. An encoded offset is a
        // second source of truth for a fact already in the data, and the two drift.
        p.groupOffset = HEADER_LEN;
        uint256 cur = HEADER_LEN + 1;
        uint8 groups = uint8(plan[HEADER_LEN]);
        for (uint8 g; g < groups; ++g) {
            uint8 liqs   = uint8(plan[cur + 57]);
            uint8 rSwaps = uint8(plan[cur + 58]);
            cur += GROUP_HEAD_LEN + uint256(liqs) * LIQ_LEG_LEN;
            cur  = _skipSwapLegs(plan, cur, rSwaps);
        }
        p.profitSwapOffset = cur;
    }

    function _decodeGroup(bytes calldata plan, uint256 o)
        internal pure returns (FlashGroup memory fg, uint256 next)
    {
        fg.provider       = uint8(plan[o]);
        fg.flashSource    = address(bytes20(plan[o + 1  : o + 21]));
        fg.debtAsset      = address(bytes20(plan[o + 21 : o + 41]));
        fg.flashAmount    = uint128(bytes16(plan[o + 41 : o + 57]));
        fg.liqCount       = uint8(plan[o + 57]);
        fg.repaySwapCount = uint8(plan[o + 58]);

        fg.liqOffset       = o + GROUP_HEAD_LEN;
        fg.repaySwapOffset = fg.liqOffset + uint256(fg.liqCount) * LIQ_LEG_LEN;
        next = _skipSwapLegs(plan, fg.repaySwapOffset, fg.repaySwapCount);
    }

    /// Swap legs are variable length, so the only way past them is to walk them.
    function _skipSwapLegs(bytes calldata plan, uint256 o, uint8 n)
        internal pure returns (uint256)
    {
        for (uint8 i; i < n; ++i) {
            uint16 dataLen = uint16(bytes2(plan[o + 58 : o + 60]));
            o += SWAP_LEG_HEAD_LEN + dataLen;
        }
        return o;
    }

    function _decodeLiqLeg(bytes calldata plan, uint256 o)
        internal pure returns (LiqLeg memory l)
    {
        l.adapter         = uint8(plan[o]);
        l.market          = address(bytes20(plan[o + 1  : o + 21]));
        l.borrower        = address(bytes20(plan[o + 21 : o + 41]));
        l.collateralAsset = address(bytes20(plan[o + 41 : o + 61]));
        l.repayAmount     = uint128(bytes16(plan[o + 61 : o + 77]));
    }

    /// Receives ETH from `IWETH.withdraw` when funding a coinbase bid.
    receive() external payable {}
}
