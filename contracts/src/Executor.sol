// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {Plan, FlashGroup, LiqLeg, SwapLeg, PlanDecoder} from "./lib/PlanDecoder.sol";
import {
    IERC20, IWETH, IAavePool, IAaveV4Spoke, IMorpho, MarketParams,
    IUniV3Pool, IUniV2Pair, ICurvePool, ICurveMetaRegistry, IPoolManager, IDssFlash,
    IEVault, IEVC, ISiloHook, ITroveManager, IFluidT1, ICreditFacadeV3, ICreditFacadeV3Multicall, MultiCall, PriceUpdate,
    ICToken, IComptroller, ICErc20, ICEther
} from "./lib/Interfaces.sol";

/*
 * Executor.sol — flashloan-funded liquidation executor (GUIDE 10, WP 10A).
 *
 * Funds are in transit only: borrow → liquidate → swap → repay → (maybe) sweep.
 *
 * SECURITY MODEL
 *  - OPERATOR is a hot key. It can only call execute(). It cannot move funds to
 *    any address of its choosing, cannot change PROFIT_SINK, and cannot make an
 *    arbitrary external call.
 *  - PROFIT_SINK is immutable. Every token that leaves this contract, other than
 *    a flashloan repayment or a liquidation repay, goes there. An operator can
 *    route a transaction's realized proceeds to an arbitrary recipient via the
 *    allowlisted routers (plan-controlled calldata + exact approval), but cannot
 *    touch the standing balance: `gross = wethAfter - wethBefore` underflows if
 *    that balance falls, so theft is capped at the transaction's own earnings.
 *  - sweep() is permissionless for exactly that reason: the destination is fixed,
 *    so letting anyone push funds out is a backstop, not a risk.
 *  - Callback authentication uses transient storage (EIP-1153). Every flashloan
 *    callback verifies BOTH that msg.sender is the provider we just called AND
 *    that we are inside our own execute(). This is the critical bug class for
 *    this contract type: an unauthenticated callback is a free-money function.
 *  - Immutable, no proxy, no allowlist setter (D55 = A). A new liquidation ABI
 *    is a redeploy (D48, `10R-n`).
 *
 * NOT AUDITED. Fork-test every adapter × provider pair before mainnet (10C), and
 * get an independent review of the callback auth and approval logic (H3).
 */
contract Executor {
    using SafeTransfer for address;
    using PlanDecoder for bytes;

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

    // Transient storage slots (EIP-1153). ~100 gas vs 20k/5k for SSTORE.
    uint256 private constant T_EXPECTED_CALLER = 0x00;
    uint256 private constant T_ENTERED         = 0x01;
    uint256 private constant T_SWAPPING        = 0x02;
    /// Which flash group is executing, so a callback can find its own legs.
    uint256 private constant T_GROUP           = 0x03;
    /// Successful liquidation legs in the group currently executing, or
    /// `NO_CALLBACK` if the provider returned without ever calling back.
    uint256 private constant T_FILLED          = 0x04;
    uint256 private constant NO_CALLBACK       = type(uint256).max;

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
    /// Uniswap V2 / SushiSwap swap fee, 0.30 %.
    uint256 private constant V2_FEE_KEEP = 997;

    // Provider ids — `liq_types::FlashProvider` discriminants (D09).
    uint8 private constant P_AAVE    = 0;
    uint8 private constant P_UNIV3   = 1;
    uint8 private constant P_UNIV4   = 2;
    uint8 private constant P_MORPHO  = 3;
    uint8 private constant P_SKY_DSS = 4;

    // Flags
    uint8 private constant F_SWEEP = 1 << 0; // sweep WETH after this plan
    /// Swap-leg flag: ignore the encoded amount and take this contract's whole
    /// balance of `tokenIn`. Set on the last leg for each collateral.
    uint8 private constant L_TAKE_BALANCE = 1 << 0;
    /// Swap-leg flag: `amount` is an exact OUTPUT target, not an input amount.
    /// Used for the repay leg so no debt-token dust is left behind.
    uint8 private constant L_EXACT_OUT    = 1 << 1;

    uint256 private constant HF_THRESHOLD = 1e18;
    /// Morpho `SharesMathLib` virtual shares/assets (pin 8e26ca6a).
    uint256 private constant MORPHO_VIRTUAL_SHARES = 1e6;
    uint256 private constant MORPHO_VIRTUAL_ASSETS = 1;

    uint160 private constant TickMath_MIN_SQRT = 4295128739;
    uint160 private constant TickMath_MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    error NotOperator();
    error BadCallback();
    error Reentrant();
    error Unprofitable(uint256 gained, uint256 required);
    error UnknownProvider(uint8 p);
    error UnknownAdapter(uint8 a);
    error UnknownVenue(uint8 v);
    error RouterNotAllowed(address target);
    error RouterCallFailed(address target);
    /// A V2/Curve leg's data is malformed, names a pool that is not the
    /// verified one for its tokens, or asks more than the pool holds.
    error BadPool(uint8 venue, address pool);
    /// Curve cannot swap to an exact output.
    error ExactOutUnsupported(uint8 venue);
    error BadSwapCallback();
    error NoLegs();
    error BidFailed(uint256 amount);
    /// Every leg of a flash group was taken by someone else between
    /// simulation and inclusion. Reverting drops the bundle, which costs
    /// nothing — see GUIDE 13. Per group, not per plan: a group that seized
    /// nothing has nothing to pay its flash premium from.
    error AllLegsFailed();
    /// The provider returned from `_initiate` without invoking our callback.
    error NoCallback(uint8 provider);
    /// The provider's callback arguments disagree with the group we encoded.
    error FlashMismatch();
    /// A Morpho leg whose market `Id` does not resolve to the encoded
    /// `(loanToken, collateralToken)`. Encoder bug, not a race: revert all.
    error LegMismatch();
    error FlashLoanRejected();
    error ZeroAddress();
    /// Seized cTokens did not redeem. The whole `execute` reverts so the
    /// liquidation and the flash roll back together.
    error RedeemFailed(address token, uint256 code);
    /// Gearbox full liquidation delivered less collateral than the plan's
    /// minimum.
    error SeizedBelowMin(uint256 got, uint256 minimum);

    /// Why a liquidation leg did not fill. Every `catch` in this contract
    /// emits one before returning false, so a skipped leg is diagnosable from
    /// the receipt instead of being indistinguishable from "no opportunity".
    /// `stage` names which call failed; `reason` is the raw revert data,
    /// truncated to the first 256 bytes (a custom-error selector plus args,
    /// or an ABI-encoded `Error(string)`).
    event LegFailed(
        uint8 indexed adapter,
        address indexed market,
        address indexed borrower,
        uint8 stage,
        bytes reason
    );

    // `stage` values for `LegFailed`. Guard stages are pre-call views; the
    // LIQUIDATE stage is the protocol call itself.
    uint8 private constant ST_GUARD      = 1; // health / max-liquidation view reverted
    uint8 private constant ST_NOT_LIQ    = 2; // guard succeeded, position is not liquidatable
    uint8 private constant ST_LIQUIDATE  = 3; // the liquidation call reverted
    uint8 private constant ST_SIZING     = 4; // pre-call sizing made the leg a no-op
    uint8 private constant ST_TAIL       = 5; // the leg tail is malformed or unset

    constructor(
        address operator_, address profitSink_,
        address univ3Factory_, bytes32 univ3InitHash_,
        address routerA_, address routerB_, address weth_,
        address univ2Factory_, bytes32 univ2InitHash_,
        address sushiFactory_, bytes32 sushiInitHash_,
        address curveRegistry_
    ) {
        if (operator_ == address(0) || profitSink_ == address(0) || univ3Factory_ == address(0)
            || routerA_ == address(0) || routerB_ == address(0) || weth_ == address(0)
            || univ2Factory_ == address(0) || sushiFactory_ == address(0)
            || curveRegistry_ == address(0)) {
            revert ZeroAddress();
        }
        UNIV2_FACTORY        = univ2Factory_;
        UNIV2_INIT_HASH      = univ2InitHash_;
        SUSHI_FACTORY        = sushiFactory_;
        SUSHI_INIT_HASH      = sushiInitHash_;
        CURVE_REGISTRY       = curveRegistry_;
        OPERATOR             = operator_;
        PROFIT_SINK          = profitSink_;
        UNIV3_FACTORY        = univ3Factory_;
        UNIV3_POOL_INIT_HASH = univ3InitHash_;
        ROUTER_A             = routerA_;
        ROUTER_B             = routerB_;
        WETH                 = weth_;
    }

    // ──────────────────────────────────────────────────────────────────────
    // Entry point
    // ──────────────────────────────────────────────────────────────────────
    function execute(bytes calldata plan) external payable {
        if (msg.sender != OPERATOR) revert NotOperator();
        uint256 entered;
        assembly { entered := tload(T_ENTERED) }
        // Compiler-derived selector, not a hand-written literal: the previous
        // draft carried a wrong constant here, which the unit suite caught.
        if (entered != 0) revert Reentrant();
        assembly { tstore(T_ENTERED, 1) }

        // Decodes the header and bounds-checks the whole plan before any
        // external call (unknown adapter, truncated blob → revert here).
        Plan memory p = plan.header();

        // Profit is denominated in ETH, always, whatever was borrowed or seized.
        // Snapshot WETH before any borrowing, and measure after every group has
        // repaid. Anything left is ours — including the case where some group's
        // debt asset was WETH itself and repay and profit shared a balance.
        uint256 wethBefore = IWETH(WETH).balanceOf(address(this));

        // Sequential, not nested (D32). Each group borrows, liquidates, swaps
        // enough to cover its own repay, and settles before the next one starts.
        // Multi-source cascade = sibling groups with the same debtAsset and
        // different providers (PLAN-ENCODING §1b′) — never nested callbacks.
        uint256 cursor = PlanDecoder.HEADER_LEN + 1;
        for (uint256 g; g < p.groupCount; ++g) {
            (FlashGroup memory fg, uint256 next) = plan.group(cursor);
            cursor = next;

            assembly {
                tstore(T_GROUP, g)
                tstore(T_FILLED, not(0)) // NO_CALLBACK sentinel
            }
            _arm(fg.flashSource);
            _initiate(fg, plan);     // returns only after the callback settled
            if (fg.provider != P_UNIV3 && fg.provider != P_UNIV4) {
                fg.debtAsset.safeApprove(fg.flashSource, 0);
            }
            _disarm();

            uint256 f;
            assembly { f := tload(T_FILLED) }
            // A provider that returns without calling back has not lent, so
            // nothing was liquidated; the plan's semantics are broken — fail.
            if (f == NO_CALLBACK) revert NoCallback(fg.provider);
        }

        // Everything that is left, across every group, converges on WETH here.
        _swap(p.profitSwapOffset + 1, uint8(plan[p.profitSwapOffset]), plan);

        uint256 gross = IWETH(WETH).balanceOf(address(this)) - wethBefore;

        // Underflow here is the correct failure: gross ETH did not cover gas, so
        // the liquidation was never worth doing and the bundle should drop.
        uint256 net = gross - p.gasCostWei;

        // The bid is a fraction of REALIZED net, not of what we predicted. That
        // is the whole reason to pay via coinbase rather than priority fee: if
        // the quote was optimistic, the bid shrinks with it and what we keep
        // survives. A priority fee is committed before the swap and cannot.
        uint256 bid = (net * p.bidBps) / 10_000;
        uint256 keep = net - bid;
        if (keep < p.minProfit) revert Unprofitable(keep, p.minProfit);

        if (bid != 0) {
            // Wallet-funded bidding, off by default (D37). `msg.value` is a
            // CEILING, never the bid itself — the bid is still computed from
            // realized net so an optimistic quote shrinks it rather than
            // overpaying. Anything unspent goes straight back to the operator.
            uint256 fromWallet = msg.value;
            if (fromWallet != 0) {
                if (bid > fromWallet) bid = fromWallet;
            } else {
                IWETH(WETH).withdraw(bid);
            }

            // `call`, not `transfer`: a fee recipient that is a contract will
            // fail on the 2300-gas stipend. EIP-3651 pre-warms COINBASE.
            (bool ok, ) = block.coinbase.call{value: bid}("");
            if (!ok) revert BidFailed(bid);

            // Never read address(this).balance here — `receive()` is open, so a
            // donation would be indistinguishable from the bid budget and would
            // be bid away. Explicit accounting only (mutation #11).
            if (fromWallet > bid) {
                (bool r, ) = msg.sender.call{value: fromWallet - bid}("");
                if (!r) revert BidFailed(fromWallet - bid);
            }
        } else if (msg.value != 0) {
            (bool r, ) = msg.sender.call{value: msg.value}("");
            if (!r) revert BidFailed(msg.value);
        }

        // Residual is WETH and only WETH (D29). The flag is decided off-chain
        // (D12 risk budget); the contract does no storage read for it.
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
        uint256 bal = IERC20(asset).balanceOf(address(this));
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
            // debtAsset must be Flash.dai(); the off-chain encoder enforces that.
            if (!IDssFlash(p.flashSource).flashLoan(address(this), p.debtAsset, p.flashAmount, plan)) {
                revert FlashLoanRejected();
            }
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

    /// The provider callbacks each need the group they belong to, and none of
    /// their ABIs has room to carry it — so the index goes through transient
    /// storage and the group is re-walked from calldata here.
    function _currentGroup(bytes calldata plan) internal view returns (FlashGroup memory fg) {
        uint256 g;
        assembly { g := tload(T_GROUP) }
        fg = plan.groupAt(g);
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
        FlashGroup memory fg = _currentGroup(params);
        if (asset != fg.debtAsset || amount != fg.flashAmount) revert FlashMismatch();
        _core(fg, params);
        asset.safeApprove(msg.sender, amount + premium);
        return true;
    }

    /// Uniswap V3. Transfer-based: no approval anywhere in this path. Exactly
    /// one side was borrowed, so the other side's fee is zero and the sum is
    /// the fee owed without branching on direction.
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _checkCallback();
        FlashGroup memory fg = _currentGroup(data);
        _core(fg, data);
        fg.debtAsset.safeTransfer(msg.sender, uint256(fg.flashAmount) + fee0 + fee1);
    }

    /// Uniswap V4. Zero fee. take() to borrow, sync/transfer/settle to return.
    /// unlock() reverts unless every currency delta nets to zero before it
    /// returns, so a missed settle fails closed rather than stealing.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        _checkCallback();
        FlashGroup memory fg = _currentGroup(data);

        IPoolManager(msg.sender).take(fg.debtAsset, address(this), fg.flashAmount);
        _core(fg, data);
        IPoolManager(msg.sender).sync(fg.debtAsset);
        fg.debtAsset.safeTransfer(msg.sender, fg.flashAmount);
        IPoolManager(msg.sender).settle();

        return "";
    }

    /// Morpho Blue. Zero fee, pull-based.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        _checkCallback();
        FlashGroup memory fg = _currentGroup(data);
        if (assets != fg.flashAmount) revert FlashMismatch();
        _core(fg, data);
        fg.debtAsset.safeApprove(msg.sender, assets);
    }

    /// Sky DSS Flash (ERC-3156). Pull-based repay: approve Flash for amount+fee.
    /// Must return the ERC-3156 success magic or the mint reverts.
    function onFlashLoan(
        address initiator, address token, uint256 amount, uint256 fee, bytes calldata data
    ) external returns (bytes32) {
        _checkCallback();
        if (initiator != address(this)) revert BadCallback();
        FlashGroup memory fg = _currentGroup(data);
        if (token != fg.debtAsset || amount != fg.flashAmount) revert FlashMismatch();
        _core(fg, data);
        token.safeApprove(msg.sender, amount + fee);
        return keccak256("ERC3156FlashBorrower.onFlashLoan");
    }

    // ──────────────────────────────────────────────────────────────────────
    // Core: liquidate legs → repay swaps. Repayment is the caller's job.
    // ──────────────────────────────────────────────────────────────────────
    function _core(FlashGroup memory fg, bytes calldata plan) internal {
        if (fg.liqCount == 0) revert NoLegs();

        // Each leg stands or falls alone. A competitor taking one position
        // between simulation and inclusion must not cost us the others — that
        // is the whole risk batching introduces, and per-leg tolerance is the
        // whole mitigation. The profit floor still judges the plan as a whole.
        uint256 filled;
        uint256 o = fg.liqOffset;
        for (uint256 i; i < fg.liqCount; ++i) {
            (LiqLeg memory l, uint256 next) = plan.liqLeg(o);
            o = next;
            if (_liquidateLeg(fg.debtAsset, l, plan)) ++filled;
        }
        // Nothing seized in this group means nothing to swap and no premium
        // to pay from: the flash could not be settled anyway. Revert here,
        // by name, before a repay swap fails with a misleading transfer error.
        if (filled == 0) revert AllLegsFailed();
        assembly { tstore(T_FILLED, filled) }

        // Only this group's repay swaps run here — exact-output into its debt
        // asset, sized to what it owes. Profit swaps run once, after every
        // group has settled, so they see the whole batch's leftover collateral
        // at once and can be solved jointly (GUIDE 12 §4c). The repay blob has
        // NO count prefix — its count lives in the group head (PLAN-ENCODING §1c).
        if (fg.repaySwapCount != 0) {
            _swap(fg.repaySwapOffset, fg.repaySwapCount, plan);
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // On-chain adapters. Each: guard (protocol's own health view where one
    // exists) → exact approval → try/catch the real liquidation call → zero
    // the allowance unconditionally. Returns false — rather than reverting —
    // when the position is gone or the protocol rejects, so the rest of the
    // batch survives.
    //
    // No `seized` return anywhere. With several collaterals in flight, sizing
    // swaps from per-leg deltas means a map; the swap blob takes whole balances
    // instead (`L_TAKE_BALANCE`), which cannot disagree with what is held —
    // that is also how "read back actual repaid/seized" (GUIDE 10 §4) is met:
    // nothing downstream trusts the requested amount.
    //
    // The allowance is zeroed after EVERY call, success or failure: V3 clamps
    // `debtToCover` to its close factor and V4 to its target-HF maximum, so a
    // successful pull can consume less than approved. GUIDE 10 §5.
    //
    // `market` comes from the plan, so it is only ever a registry address the
    // operator encoded. try/catch does not bound gas, and a market that burns
    // the call's gas would take the batch down with it — acceptable only
    // because that set is curated. Do not widen it to arbitrary input.
    // ──────────────────────────────────────────────────────────────────────
    /// Cap revert data so a protocol returning a huge blob cannot make the
    /// log dominate the gas cost of the leg it describes.
    function _clip(bytes memory r) internal pure returns (bytes memory) {
        if (r.length <= 256) return r;
        bytes memory out = new bytes(256);
        for (uint256 i; i < 256; ++i) out[i] = r[i];
        return out;
    }

    function _liquidateLeg(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool)
    {
        if (l.adapter == PlanDecoder.A_AAVE_V3)  return _liquidateAaveV3(debtAsset, l);
        if (l.adapter == PlanDecoder.A_AAVE_V4)  return _liquidateAaveV4(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_MORPHO)   return _liquidateMorpho(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_EULER)    return _liquidateEulerV2(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_SILO)     return _liquidateSiloV2(debtAsset, l);
        if (l.adapter == PlanDecoder.A_LIQUITY)  return _liquidateLiquityV2(l, plan);
        if (l.adapter == PlanDecoder.A_FLUID)    return _liquidateFluid(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_GEARBOX)  return _liquidateGearbox(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_COMPOUND) return _liquidateCompoundV2(debtAsset, l, plan);
        revert UnknownAdapter(l.adapter); // unreachable: the decoder rejected it
    }

    /// Aave V3 `Pool.liquidationCall(collateral, debt, user, debtToCover, false)`
    /// pin 8305565ae. Guard: `getUserAccountData(user).healthFactor < 1e18`.
    function _liquidateAaveV3(address debtAsset, LiqLeg memory l) internal returns (bool ok) {
        (,,,,, uint256 hf) = IAavePool(l.market).getUserAccountData(l.borrower);
        if (hf >= HF_THRESHOLD) {
            emit LegFailed(PlanDecoder.A_AAVE_V3, l.market, l.borrower, ST_NOT_LIQ, "");
            return false;
        }

        debtAsset.safeApprove(l.market, l.repayAmount);
        try IAavePool(l.market).liquidationCall(
            l.collateralAsset, debtAsset, l.borrower, l.repayAmount,
            false   // never receive aTokens: profit must converge on WETH
        ) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_AAVE_V3, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Aave V4 `Spoke.liquidationCall(collateralReserveId, debtReserveId, user,
    /// debtToCover, false)` pin 40232a0a. Reserve ids come from the leg tail;
    /// the encoder (`liq-plan::validate`) pins them to the leg's addresses via
    /// the adapter config — the contract has no address→id view to check
    /// against and the profit guard bounds any mismatch. Guard:
    /// `getUserAccountData(user).healthFactor < 1e18`. The protocol clamps
    /// `debtToCover` to its target-HF maximum; the unconditional zeroing below
    /// and the balance-based swaps are what make that safe.
    function _liquidateAaveV4(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        IAaveV4Spoke.UserAccountData memory d = IAaveV4Spoke(l.market).getUserAccountData(l.borrower);
        if (d.healthFactor >= HF_THRESHOLD) {
            emit LegFailed(PlanDecoder.A_AAVE_V4, l.market, l.borrower, ST_NOT_LIQ, "");
            return false;
        }

        (uint16 collId, uint16 debtId) = plan.tailV4(l.tailOffset);
        debtAsset.safeApprove(l.market, l.repayAmount);
        try IAaveV4Spoke(l.market).liquidationCall(
            collId, debtId, l.borrower, l.repayAmount,
            false   // never receive shares
        ) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_AAVE_V4, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Morpho Blue `liquidate(marketParams, borrower, 0, repaidShares, "")`
    /// pin 8e26ca6a. Morpho has no health view; its own `_isHealthy` inside
    /// `liquidate` is the guard (`HEALTHY_POSITION` → caught → leg skipped).
    ///
    /// `repayAmount` is loan assets; Morpho takes shares. Convert with the
    /// post-accrual totals Morpho itself will use — `accrueInterest` is
    /// idempotent within a block, so the totals read here are exactly the ones
    /// `liquidate` sees. `toSharesDown(a)` then `toAssetsUp(shares) <= a`, so
    /// the exact approval always covers the pull. Cap at the borrower's shares:
    /// a full-close quote is `toAssetsUp(borrowShares)`, and converting that
    /// back down can land one share above what is owed — which would revert
    /// inside Morpho and skip a liquidatable position.
    function _liquidateMorpho(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        bytes32 id = plan.tailMorpho(l.tailOffset);
        IMorpho morpho = IMorpho(l.market);

        MarketParams memory mp = morpho.idToMarketParams(id);
        if (mp.loanToken != debtAsset || mp.collateralToken != l.collateralAsset) revert LegMismatch();

        IMorpho.Market memory m;
        IMorpho.Position memory pos;
        try morpho.accrueInterest(mp) {
            try morpho.market(id) returns (IMorpho.Market memory m_) {
                m = m_;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_GUARD, _clip(r));
                return false;
            }
            try morpho.position(id, l.borrower) returns (IMorpho.Position memory p_) {
                pos = p_;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_GUARD, _clip(r));
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        if (pos.borrowShares == 0) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_NOT_LIQ, "");
            return false;
        }

        // SharesMathLib.toSharesDown: same expression, same operands as Morpho.
        uint256 shares = (uint256(l.repayAmount) * (uint256(m.totalBorrowShares) + MORPHO_VIRTUAL_SHARES))
            / (uint256(m.totalBorrowAssets) + MORPHO_VIRTUAL_ASSETS);
        if (shares > pos.borrowShares) shares = pos.borrowShares;
        if (shares == 0) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_SIZING, "");
            return false;
        }

        debtAsset.safeApprove(l.market, l.repayAmount);
        try morpho.liquidate(mp, l.borrower, 0, shares, "") returns (uint256, uint256) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Euler V2 `IEVault.liquidate(violator, collateral, repayAssets, minYieldBalance)`
    /// pin `bfb325a6`. Target = debt vault (`market`). Tail vault is the
    /// collateral vault (shares). `collateralAsset` is the underlying the
    /// swaps sell. Guard: `checkLiquidation` returns `(0,0)` when healthy;
    /// HF==1 is liquidatable. The ABI has no receive-underlying flag, so the
    /// seized shares are redeemed before the swap.
    function _liquidateEulerV2(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (uint256 minYield, address vault) = plan.tailEuler(l.tailOffset);
        if (vault == address(0)) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_TAIL, "");
            return false;
        }
        try IEVault(l.market).checkLiquidation(address(this), l.borrower, vault)
            returns (uint256 maxRepay, uint256)
        {
            if (maxRepay == 0) {
                emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_NOT_LIQ, "");
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }

        address connector;
        try IEVault(l.market).EVC() returns (address e) {
            connector = e;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        if (connector == address(0)) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_GUARD, "");
            return false;
        }

        // `liquidate` transfers the violator's debt onto the caller and seizes
        // shares (`transferBorrow`). `enableCollateral` puts the seized vault
        // into the account's collateral set BEFORE `liquidate` runs, so the
        // controller's deferred account-status check (fired by
        // `enableController` and resolved at the end of the batch) sees it.
        // The caller's account check reverts `E_AccountLiquidity` unless the
        // assumed debt is repaid first — `repay(uint256.max)` pulls the
        // underlying and clears it. The controller is released in the same
        // batch so a later debt vault can be enabled, and `disableController`
        // itself reverts `E_OutstandingDebt` if the repay left anything open,
        // so the transaction cannot end holding Euler debt. `redeem` last:
        // it only needs the shares this contract already holds, not
        // controller/collateral state, and running it after `disableController`
        // keeps the batch in the sequence the mechanism was verified against.
        IEVC.BatchItem[] memory items = new IEVC.BatchItem[](6);
        items[0] = IEVC.BatchItem({
            targetContract: connector,
            onBehalfOfAccount: address(0),
            value: 0,
            data: abi.encodeCall(IEVC.enableController, (address(this), l.market))
        });
        items[1] = IEVC.BatchItem({
            targetContract: connector,
            onBehalfOfAccount: address(0),
            value: 0,
            data: abi.encodeCall(IEVC.enableCollateral, (address(this), vault))
        });
        items[2] = IEVC.BatchItem({
            targetContract: l.market,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.liquidate, (l.borrower, vault, l.repayAmount, minYield))
        });
        items[3] = IEVC.BatchItem({
            targetContract: l.market,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.repay, (type(uint256).max, address(this)))
        });
        items[4] = IEVC.BatchItem({
            targetContract: l.market,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.disableController, ())
        });
        items[5] = IEVC.BatchItem({
            targetContract: vault,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.redeem, (type(uint256).max, address(this), address(this)))
        });

        uint256 sharesBefore = IERC20(vault).balanceOf(address(this));
        debtAsset.safeApprove(l.market, l.repayAmount);
        try IEVC(connector).batch(items) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
        if (ok) {
            uint256 sharesNow = IERC20(vault).balanceOf(address(this));
            if (sharesNow > sharesBefore) {
                IEVault(vault).redeem(sharesNow - sharesBefore, address(this), address(this));
            }
        }
    }

    /// Silo V2 `IPartialLiquidation.liquidationCall` pin `570a668a` topic0
    /// `0x3a84f644…`. Target = hook receiver. Guard: `maxLiquidation`
    /// `debtToRepay == 0`.
    ///
    /// S3. `maxLiquidation` names whether the withdraw must land as sTokens
    /// (insufficient underlying liquidity in the collateral silo) via its
    /// third return, `sTokenRequired`. Calling `liquidationCall` with
    /// `_receiveSToken = false` when the protocol requires `true` reverts the
    /// leg; passing `true` back would "succeed" but leave the Executor
    /// holding Silo shares this contract has no redeem path for, and the
    /// downstream swap is built to sell the underlying it does not have —
    /// stuck funds, not a skipped opportunity. Reading the flag and skipping
    /// the leg when it is set is the fail-closed choice until an sToken
    /// redeem path exists.
    function _liquidateSiloV2(address debtAsset, LiqLeg memory l) internal returns (bool ok) {
        try ISiloHook(l.market).maxLiquidation(l.borrower)
            returns (uint256, uint256 debtToRepay, bool sTokenRequired)
        {
            if (debtToRepay == 0) {
                emit LegFailed(PlanDecoder.A_SILO, l.market, l.borrower, ST_NOT_LIQ, "");
                return false;
            }
            if (sTokenRequired) {
                emit LegFailed(PlanDecoder.A_SILO, l.market, l.borrower, ST_SIZING, "");
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_SILO, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }

        debtAsset.safeApprove(l.market, l.repayAmount);
        try ISiloHook(l.market).liquidationCall(
            l.collateralAsset, debtAsset, l.borrower, l.repayAmount, false
        ) returns (uint256, uint256) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_SILO, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Liquity V2 `batchLiquidateTroves(uint256[])` selector `0xef49a6b4`
    /// pin `c8a5a4ee`. No token repay (Stability Pool is the counterparty).
    /// Guard: `getTroveStatus` ∈ {active=1, zombie=4}. `NothingToLiquidate`
    /// / `EmptyData` → catch → skip. Gas compensation is a WETH
    /// `transferFrom` from the gas pool plus `sendColl` of the branch
    /// collateral (pin `c8a5a4ee`). A native-ETH delta, if one arrives, is
    /// wrapped; the WETH transfer is already in the WETH balance `gross` reads.
    function _liquidateLiquityV2(LiqLeg memory l, bytes calldata plan) internal returns (bool ok) {
        uint256 id = plan.tailU256(l.tailOffset);
        if (id == 0) {
            emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_TAIL, "");
            return false;
        }
        try ITroveManager(l.market).getTroveStatus(id) returns (uint8 status) {
            if (status != 1 && status != 4) {
                emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_NOT_LIQ, "");
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }

        uint256[] memory ids = new uint256[](1);
        ids[0] = id;
        uint256 ethBefore = address(this).balance;
        try ITroveManager(l.market).batchLiquidateTroves(ids) {
            ok = true;
            uint256 ethNow = address(this).balance;
            if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
    }

    /// Fluid T1 `liquidate(debtAmt_, colPerUnitDebt_, to_, absorb_)` pin
    /// `9496626f`. Tail is **1e18** min coll/debt (slip check); passed
    /// through with no conversion. `to_` = this Executor. `absorb_ = true`
    /// (quote includes absorbed). T2/T3/T4 must not reach this path. No HF
    /// view — the call is the guard (Morpho-style).
    function _liquidateFluid(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        uint256 colPer = plan.tailU256(l.tailOffset);
        debtAsset.safeApprove(l.market, l.repayAmount);
        try IFluidT1(l.market).liquidate(l.repayAmount, colPer, address(this), true)
            returns (uint256, uint256)
        {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_FLUID, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Gearbox V3 (`market` = facade, `borrower` = credit account,
    /// `debtAsset` = the manager's underlying). Tail = minimum collateral
    /// received ‖ mode. **Approvals go to the credit manager**, which does
    /// every pull as spender; an allowance held by the facade is never used.
    ///
    /// Mode 0 — `partiallyLiquidateCreditAccount` (v3.1): repay
    /// `repayAmount`, seize `collateralAsset` at the discount; the facade
    /// enforces `minSeized` and requires the account to end healthy.
    ///
    /// Mode 1 — full `liquidateCreditAccount(account, this, calls)` (the
    /// 3-arg form: v3.0, and v3.1's wrapper with empty loss-policy data).
    /// Multicall: `addCollateral(underlying, repayAmount)` then
    /// `withdrawCollateral(collateralAsset, max, this)`. The manager pays the
    /// pool from the account's underlying, keeps the borrower's share on the
    /// account and returns the rest of the underlying to us, so over-adding
    /// comes back. Too little reverts in Gearbox → the leg fails. The facade
    /// has no slip check on withdrawn collateral, so `minSeized` is enforced
    /// here after the call — short is a whole-plan revert.
    function _liquidateGearbox(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (uint256 minSeized, uint8 mode) = plan.tailGearbox(l.tailOffset);
        if (mode > 1) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_TAIL, "");
            return false;
        }

        address puller;
        try ICreditFacadeV3(l.market).creditManager() returns (address m) {
            puller = m;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        if (puller == address(0)) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_GUARD, "");
            return false;
        }

        debtAsset.safeApprove(puller, l.repayAmount);
        if (mode == 0) {
            PriceUpdate[] memory none;
            try ICreditFacadeV3(l.market).partiallyLiquidateCreditAccount(
                l.borrower, l.collateralAsset, l.repayAmount, minSeized, address(this), none
            ) returns (uint256) {
                ok = true;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
            }
        } else {
            ok = _liquidateGearboxFull(debtAsset, l, minSeized);
        }
        debtAsset.safeApprove(puller, 0);
    }

    function _liquidateGearboxFull(address debtAsset, LiqLeg memory l, uint256 minSeized)
        internal returns (bool ok)
    {
        MultiCall[] memory calls = new MultiCall[](2);
        calls[0] = MultiCall({
            target: l.market,
            callData: abi.encodeCall(ICreditFacadeV3Multicall.addCollateral, (debtAsset, l.repayAmount))
        });
        calls[1] = MultiCall({
            target: l.market,
            callData: abi.encodeCall(
                ICreditFacadeV3Multicall.withdrawCollateral, (l.collateralAsset, type(uint256).max, address(this))
            )
        });
        uint256 collBefore = IERC20(l.collateralAsset).balanceOf(address(this));
        try ICreditFacadeV3(l.market).liquidateCreditAccount(l.borrower, address(this), calls) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        if (ok) {
            uint256 got = IERC20(l.collateralAsset).balanceOf(address(this)) - collBefore;
            if (got < minSeized) revert SeizedBelowMin(got, minSeized);
        }
    }

    /// Compound V2 official Unitroller pin `a3214f67`. `market` = debt
    /// cToken. Tail = cTokenCollateral ‖ isCEther. Guard:
    /// `getAccountLiquidity` shortfall or `isDeprecated`. Never receive
    /// cTokens as a flag — seize lands as cTokens. This leg redeems that
    /// delta to underlying (CEther: ETH, then wrapped) before the swaps.
    /// CEther debt: unwrap WETH, official 2-arg payable
    /// `liquidateBorrow`, wrap only ETH gained by this leg. Wrong
    /// `isCEther` / repay > WETH / withdraw-or-liq revert skips the **leg**.
    function _liquidateCompoundV2(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (address cTokenColl, uint8 isCEther) = plan.tailCompound(l.tailOffset);
        if (isCEther > 1) {
            emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_TAIL, "");
            return false;
        }

        address unitroller;
        try ICToken(l.market).comptroller() returns (address c) {
            unitroller = c;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        try IComptroller(unitroller).getAccountLiquidity(l.borrower)
            returns (uint256 err, uint256, uint256 shortfall)
        {
            if (err != 0) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_GUARD, "");
                return false;
            }
            bool deprecated;
            try IComptroller(unitroller).isDeprecated(l.market) returns (bool d) {
                deprecated = d;
            } catch {}
            if (shortfall == 0 && !deprecated) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_NOT_LIQ, "");
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }

        uint256 seizedBefore = IERC20(cTokenColl).balanceOf(address(this));
        if (isCEther != 0) {
            if (debtAsset != WETH) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_TAIL, "");
                return false;
            }
            uint256 need = l.repayAmount;
            if (IERC20(WETH).balanceOf(address(this)) < need) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_SIZING, "");
                return false;
            }
            uint256 ethBefore = address(this).balance;
            try IWETH(WETH).withdraw(need) {} catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_SIZING, _clip(r));
                return false;
            }
            try ICEther(l.market).liquidateBorrow{value: need}(l.borrower, cTokenColl) {
                ok = true;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
            }
            uint256 ethNow = address(this).balance;
            if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
        } else {
            debtAsset.safeApprove(l.market, l.repayAmount);
            try ICErc20(l.market).liquidateBorrow(l.borrower, l.repayAmount, cTokenColl)
                returns (uint256 errCode)
            {
                ok = errCode == 0;
                if (!ok) {
                    // Compound signals failure by return code, not revert.
                    emit LegFailed(
                        PlanDecoder.A_COMPOUND, l.market, l.borrower,
                        ST_LIQUIDATE, abi.encode(errCode)
                    );
                }
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
            }
            debtAsset.safeApprove(l.market, 0);
        }
        if (ok) _redeemSeizedCToken(cTokenColl, seizedBefore);
    }

    /// Redeem only the cTokens this leg seized. CEther pays ETH; wrap that
    /// delta so `gross` sees WETH. A non-zero Compound error code reverts
    /// the transaction: the liquidation must not stand with cTokens stuck.
    function _redeemSeizedCToken(address cToken, uint256 seizedBefore) internal {
        uint256 seizedNow = IERC20(cToken).balanceOf(address(this));
        if (seizedNow <= seizedBefore) return;
        uint256 ethBefore = address(this).balance;
        // cETH at older implementations returns no data on success. A newer
        // cToken returns the Compound error code. A non-zero code reverts;
        // empty return data does not.
        (bool redeemed, bytes memory ret) =
            cToken.call(abi.encodeWithSelector(ICToken.redeem.selector, seizedNow - seizedBefore));
        if (!redeemed) {
            uint256 code;
            if (ret.length >= 32) code = abi.decode(ret, (uint256));
            revert RedeemFailed(cToken, code);
        }
        if (ret.length >= 32) {
            uint256 code = abi.decode(ret, (uint256));
            if (code != 0) revert RedeemFailed(cToken, code);
        }
        uint256 ethNow = address(this).balance;
        if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
    }

    // ──────────────────────────────────────────────────────────────────────
    // Swaps
    // ──────────────────────────────────────────────────────────────────────

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
    function _swap(uint256 o, uint8 legs, bytes calldata plan) internal {
        assembly { tstore(T_SWAPPING, 1) }

        for (uint256 i; i < legs; ++i) {
            (SwapLeg memory s, uint256 next) = plan.swapLeg(o);
            o = next;

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

    /// Uniswap V3 swap callback. Distinct selector from the flash callback, and
    /// it needs its own authentication: verify the caller IS the canonical pool
    /// for (token0, token1, fee) by CREATE2, and that we are mid-swap. Storing
    /// "the pool we called" does not generalize to N legs; derivation does.
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

        // We called swap(zeroForOne = tokenIn < tokenOut), so the positive
        // delta is always tokenIn's side.
        uint256 owed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        tokenIn.safeTransfer(msg.sender, owed);
    }

    /// Receives ETH from `IWETH.withdraw` when funding a coinbase bid.
    receive() external payable {}
}
