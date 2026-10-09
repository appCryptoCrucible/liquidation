// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {MainnetVenues} from "./lib/MainnetVenues.sol";
import {Plan, FlashGroup, PlanDecoder} from "./lib/PlanDecoder.sol";
import {IERC20, IWETH, IAavePool, IUniV3Pool, IMorpho, IPoolManager, IDssFlash} from "./lib/Interfaces.sol";
import {
    T_EXPECTED_CALLER, T_ENTERED, T_SWAPPING, T_FILLED, T_FEE, T_V4_UNLOCKED,
    T_GROUP_SOURCE, T_GROUP_DEBT, T_GROUP_SPAN,
    ModuleIds, ILiquidationModule, ISwapModule
} from "./lib/ExecutorShared.sol";

/*
 * Executor.sol — flashloan-funded liquidation executor (GUIDE 10, WP 10A).
 *
 * Funds are in transit only: borrow → liquidate → swap → repay → (maybe) sweep.
 *
 * Three contracts, one account. This core holds every balance and allowance,
 * is the only address the operators call and the only one any provider or
 * pool calls back. The protocol legs and the swap legs live in two modules
 * (`LiquidationModule`, `SwapModule`) that it DELEGATECALLs: their code runs
 * as this contract, on its balances, allowances and transient storage, and
 * their events are this contract's. The split exists for EIP-170's 24,576-
 * byte code limit; nothing moves between contracts.
 *
 * SECURITY MODEL
 *  - OPERATOR and BACKRUN_OPERATOR are hot keys with the same single right: they
 *    can only call execute(). Neither can move funds to any address of its
 *    choosing, change PROFIT_SINK, or make an arbitrary external call. Two keys
 *    are two nonce sequences: MEV-Share backruns and builder bundles for the same
 *    block never wait on each other's nonce.
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
 *  - A module runs with this contract's full authority, so which code that is
 *    is fixed here: both module addresses are immutables, checked at
 *    construction to hold code that answers the expected MODULE_ID and our WETH.
 *    Delegatecall goes nowhere else, and only from inside execute(). The modules
 *    have no storage and refuse a direct call; their code cannot change
 *    (no SELFDESTRUCT after creation, EIP-6780).
 *  - Immutable, no proxy, no allowlist setter (D55 = A). A new liquidation ABI
 *    or a new module is a redeploy (D48, `10R-n`).
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
    /// Second hot key, same right as OPERATOR. Signs the MEV-Share backruns, so
    /// they have a nonce sequence of their own.
    address public immutable BACKRUN_OPERATOR;
    address public immutable PROFIT_SINK;
    /// Canonical WETH. Profit is denominated in ETH, so every plan converges here.
    address public immutable WETH;
    /// Uniswap V3 factory — used to verify swap callbacks by CREATE2 address.
    address public immutable UNIV3_FACTORY;
    bytes32 public immutable UNIV3_POOL_INIT_HASH;
    /// Protocol liquidations and governance actions (delegatecall).
    address public immutable LIQUIDATION_MODULE;
    /// Swap and unwrap legs (delegatecall). Holds the venue anchors: routers,
    /// V2/Sushi factories, the Curve MetaRegistry.
    address public immutable SWAP_MODULE;

    // Transient storage slots (EIP-1153) are `lib/ExecutorShared.sol`'s,
    // shared with the modules: under delegatecall they read and write this
    // contract's transient storage. ~100 gas vs 20k/5k for SSTORE.
    uint256 private constant NO_CALLBACK = type(uint256).max;

    // Provider ids — `liq_types::FlashProvider` discriminants (D09).
    uint8 private constant P_AAVE    = 0;
    uint8 private constant P_UNIV3   = 1;
    uint8 private constant P_UNIV4   = 2;
    uint8 private constant P_MORPHO  = 3;
    uint8 private constant P_SKY_DSS = 4;
    /// Reward-only group: nothing is borrowed and nothing repaid. The legs
    /// pay the liquidator without taking its money (Liquity V2 gas
    /// compensation, Sky keeper incentives); `flashAmount` and `flashSource`
    /// must be zero.
    uint8 private constant P_NONE    = 5;
    /// Flash swap: `flashSource` is a Uniswap V3 pool holding `debtAsset`.
    /// `execute` asks it for exactly `flashAmount` of the debt (an exact-
    /// output swap). The pool pays first and calls `uniswapV3SwapCallback`,
    /// where the group runs and the pool is then paid its other token: a
    /// flash loan and the repay swap in one pool call, at the swap fee the
    /// exit pays anyway. The pool is locked while it swaps, so the group's
    /// own swap legs cannot use it.
    uint8 private constant P_UNIV3_SWAP = 6;

    /// `TickMath.MIN_SQRT_RATIO` / `MAX_SQRT_RATIO`: a flash swap's price
    /// limit is the pool's whole range (SwapModule swaps with the same).
    uint160 private constant TICK_MIN_SQRT = 4295128739;
    uint160 private constant TICK_MAX_SQRT = 1461446703485210103287273052203988822378723970342;

    // Flags
    uint8 private constant F_SWEEP = 1 << 0; // sweep WETH after this plan

    // Every error `execute` can surface, the modules' included: callers,
    // simulators and the inclusion watch decode reverts against this ABI.
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
    /// A module address holds no code, or not the module it is wired as.
    error BadModule(address module);
    /// A module entry point was called directly, not by this contract's
    /// delegatecall inside `execute` (thrown by the modules).
    error NotDelegated();

    // The modules emit these as this contract (delegatecall); listed here so
    // its ABI names every log it writes.
    /// Why a liquidation leg did not fill. Every `catch` in the liquidation
    /// module emits one before returning false, so a skipped leg is
    /// diagnosable from the receipt instead of being indistinguishable from
    /// "no opportunity".
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

    /// `executePayload` reverted inside a governance plan: already executed
    /// (a keeper or an earlier transaction in the bundle), cancelled, or not
    /// yet due. The liquidation legs still run against the resulting state.
    event GovExecSkipped(uint40 indexed payloadId, bytes reason);
    /// A spell plan's `cast()` was not run: DSPause does not hold its plan
    /// (already cast, dropped, or not a scheduled spell; empty reason), or
    /// `cast()` reverted (office hours, not yet due).
    event GovSpellSkipped(address indexed spell, bytes reason);

    constructor(
        address operator_, address backrunOperator_, address profitSink_, address weth_,
        address univ3Factory_, bytes32 univ3InitHash_,
        address liquidationModule_, address swapModule_
    ) {
        if (operator_ == address(0) || backrunOperator_ == address(0)
            || profitSink_ == address(0) || weth_ == address(0)
            || univ3Factory_ == address(0)) {
            revert ZeroAddress();
        }
        _checkModule(liquidationModule_, ModuleIds.LIQUIDATION, weth_);
        _checkModule(swapModule_, ModuleIds.SWAP, weth_);
        OPERATOR             = operator_;
        BACKRUN_OPERATOR     = backrunOperator_;
        PROFIT_SINK          = profitSink_;
        WETH                 = weth_;
        UNIV3_FACTORY        = univ3Factory_;
        UNIV3_POOL_INIT_HASH = univ3InitHash_;
        LIQUIDATION_MODULE   = liquidationModule_;
        SWAP_MODULE          = swapModule_;
    }

    /// A module is code that names itself `id` and works on our WETH. An
    /// EOA, a swapped pair of addresses, or a module built for other anchors
    /// is refused here rather than discovered in a revert. Static calls: a
    /// contract that is not a module can do nothing during this check.
    function _checkModule(address module, bytes32 id, address weth_) private view {
        if (module.code.length == 0) revert BadModule(module);
        (bool ok, bytes memory r) = module.staticcall(abi.encodeCall(ILiquidationModule.MODULE_ID, ()));
        if (!ok || r.length != 32 || abi.decode(r, (bytes32)) != id) revert BadModule(module);
        (ok, r) = module.staticcall(abi.encodeCall(ILiquidationModule.WETH, ()));
        if (!ok || r.length != 32 || abi.decode(r, (address)) != weth_) revert BadModule(module);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Entry point
    // ──────────────────────────────────────────────────────────────────────
    function execute(bytes calldata plan) external payable {
        if (msg.sender != OPERATOR && msg.sender != BACKRUN_OPERATOR) revert NotOperator();
        uint256 entered;
        assembly { entered := tload(T_ENTERED) }
        // Compiler-derived selector, not a hand-written literal: the previous
        // draft carried a wrong constant here, which the unit suite caught.
        if (entered != 0) revert Reentrant();
        assembly { tstore(T_ENTERED, 1) }

        // Decodes the header and bounds-checks the whole plan before any
        // external call (unknown adapter, truncated blob → revert here),
        // keeping every group it decoded for the loop below.
        (Plan memory p, FlashGroup[] memory groups) = plan.header();

        // Governance plan: apply the change that makes the legs liquidatable.
        // Several transactions in one bundle each carry it; only the first to
        // run applies it, so only that one is charged its gas (below). It runs
        // before the WETH snapshot so nothing it moves can count as profit.
        uint256 govCost;
        if (p.flags & (PlanDecoder.FLAG_GOV_EXEC | PlanDecoder.FLAG_GOV_SPELL) != 0) {
            govCost = abi.decode(
                _delegate(
                    LIQUIDATION_MODULE,
                    abi.encodeCall(ILiquidationModule.govExec, (p.flags, p.payloadId, p.spell))
                ),
                (uint256)
            );
        }

        // Profit is denominated in ETH, always, whatever was borrowed or seized.
        // Snapshot WETH before any borrowing, and measure after every group has
        // repaid. Anything left is ours — including the case where some group's
        // debt asset was WETH itself and repay and profit shared a balance.
        uint256 wethBefore = IWETH(WETH).balanceOf(address(this));

        // Sequential, not nested (D32). Each group borrows, liquidates, swaps
        // enough to cover its own repay, and settles before the next one starts.
        // Multi-source cascade = sibling groups with the same debtAsset and
        // different providers (PLAN-ENCODING §1b′) — never nested callbacks.
        for (uint256 g; g < groups.length; ++g) {
            FlashGroup memory fg = groups[g];

            if (fg.provider == P_NONE) {
                if (fg.flashAmount != 0 || fg.flashSource != address(0)) revert FlashMismatch();
                // Reverts `AllLegsFailed` when nothing filled.
                _core(fg, plan, 0);
                continue;
            }

            assembly { tstore(T_FILLED, not(0)) } // NO_CALLBACK sentinel
            _storeGroup(fg);
            _arm(fg.flashSource);
            _initiate(fg, plan);     // returns only after the callback settled
            // The Uniswap paths are transfer-based: no allowance to clear.
            if (fg.provider != P_UNIV3 && fg.provider != P_UNIV4 && fg.provider != P_UNIV3_SWAP) {
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
        uint8 profitLegs = uint8(plan[p.profitSwapOffset]);
        if (profitLegs != 0) _runSwaps(p.profitSwapOffset + 1, profitLegs, plan);

        uint256 gross = IWETH(WETH).balanceOf(address(this)) - wethBefore;

        // Underflow here is the correct failure: gross ETH did not cover gas, so
        // the liquidation was never worth doing and the bundle should drop.
        // `govCost` is nonzero only in the transaction that applied a
        // governance change; `gasCostWei` never includes it.
        uint256 net = gross - p.gasCostWei - govCost;

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
        } else if (provider == P_UNIV3_SWAP) {
            // Exact output of the debt. The pool pays it, then calls
            // `uniswapV3SwapCallback` for its other token; the group's legs
            // run in between (`_flashSwap`).
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
    /// their ABIs has room to carry it. `execute` has just decoded it, so it
    /// passes it through transient storage: three words, cheaper than
    /// decoding the plan the provider hands back. Offsets are below the
    /// plan's length, which calldata keeps far under 2^64.
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

    /// The group `execute` stored for the callback it is waiting on. Only a
    /// callback that passed `_checkCallback` reads it: the armed provider,
    /// inside this `execute`, for this group.
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
    // Provider callbacks — each settles in its provider's own idiom
    // ──────────────────────────────────────────────────────────────────────

    /// Aave. Pull-based: approve exactly what is owed. The pull consumes the
    /// allowance to zero, so the clear `execute` makes after `_initiate`
    /// finds nothing to write; a provider that pulled less leaves an
    /// allowance, and that clear removes it.
    function executeOperation(
        address asset, uint256 amount, uint256 premium, address initiator, bytes calldata params
    ) external returns (bool) {
        _checkCallback();
        if (initiator != address(this)) revert BadCallback();
        FlashGroup memory fg = _currentGroup();
        if (asset != fg.debtAsset || amount != fg.flashAmount) revert FlashMismatch();
        _core(fg, params, premium);
        asset.safeApprove(msg.sender, amount + premium);
        return true;
    }

    /// Uniswap V3. Transfer-based: no approval anywhere in this path. Exactly
    /// one side was borrowed, so the other side's fee is zero and the sum is
    /// the fee owed without branching on direction.
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _checkCallback();
        FlashGroup memory fg = _currentGroup();
        _core(fg, data, fee0 + fee1);
        fg.debtAsset.safeTransfer(msg.sender, uint256(fg.flashAmount) + fee0 + fee1);
    }

    /// Uniswap V4. Zero fee. take() to borrow, sync/transfer/settle to return.
    /// unlock() reverts unless every currency delta nets to zero before it
    /// returns, so a missed settle fails closed rather than stealing.
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
        // Swap legs on the canonical PoolManager run inside this unlock.
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

    /// Sky DSS Flash (ERC-3156). Pull-based repay: approve Flash for amount+fee.
    /// Must return the ERC-3156 success magic or the mint reverts.
    function onFlashLoan(
        address initiator, address token, uint256 amount, uint256 fee, bytes calldata data
    ) external returns (bytes32) {
        _checkCallback();
        if (initiator != address(this)) revert BadCallback();
        FlashGroup memory fg = _currentGroup();
        if (token != fg.debtAsset || amount != fg.flashAmount) revert FlashMismatch();
        _core(fg, data, fee);
        token.safeApprove(msg.sender, amount + fee);
        return keccak256("ERC3156FlashBorrower.onFlashLoan");
    }

    // ──────────────────────────────────────────────────────────────────────
    // Core: liquidate legs → repay swaps. Repayment is the caller's job.
    // ──────────────────────────────────────────────────────────────────────
    /// `fee` is what the provider charges on top of `flashAmount`, as its
    /// callback reported it. The repay legs' exact outputs buy what the
    /// liquidations pulled; the first exact-output pool leg to run also buys
    /// `fee`, so the group owes it once whichever legs filled, and a fee
    /// that moved between simulation and inclusion is bought as it is.
    function _core(FlashGroup memory fg, bytes calldata plan, uint256 fee) internal {
        if (fg.liqCount == 0) revert NoLegs();

        uint256 filled = abi.decode(
            _delegate(
                LIQUIDATION_MODULE,
                abi.encodeCall(ILiquidationModule.runLegs, (fg.debtAsset, fg.liqOffset, fg.liqCount, plan))
            ),
            (uint256)
        );
        // Nothing seized in this group means nothing to swap and no premium
        // to pay from: the flash could not be settled anyway. Revert here,
        // by name, before a repay swap fails with a misleading transfer error.
        if (filled == 0) revert AllLegsFailed();
        // Which legs filled, for the swap legs tied to them; the fee, for the
        // first exact-output pool leg to buy.
        assembly {
            tstore(T_FILLED, filled)
            tstore(T_FEE, fee)
        }

        // Only this group's repay swaps run here — exact-output into its debt
        // asset, sized to what it owes. Profit swaps run once, after every
        // group has settled, so they see the whole batch's leftover collateral
        // at once and can be solved jointly (GUIDE 12 §4c). The repay blob has
        // NO count prefix — its count lives in the group head (PLAN-ENCODING §1c).
        if (fg.repaySwapCount != 0) {
            _runSwaps(fg.repaySwapOffset, fg.repaySwapCount, plan);
        }
        // Unbought only when no exact-output pool leg ran: an exact-input
        // repay's overshoot paid it, or the provider's pull now fails.
        assembly { tstore(T_FEE, 0) }
    }

    /// `legs` swap legs from `offset`, by the swap module.
    function _runSwaps(uint256 offset, uint8 legs, bytes calldata plan) internal {
        _delegate(SWAP_MODULE, abi.encodeCall(ISwapModule.runSwaps, (offset, legs, plan)));
    }

    /// Run module code as this contract. A revert comes back with the
    /// module's own error data, so it reads exactly as it did before the split.
    function _delegate(address module, bytes memory data) private returns (bytes memory ret) {
        bool ok;
        (ok, ret) = module.delegatecall(data);
        if (!ok) {
            assembly { revert(add(ret, 0x20), mload(ret)) }
        }
    }

    /// Uniswap V3 swap callback (also SushiSwap V3's, which is Uniswap's code).
    /// Distinct selector from the flash callback, and it needs its own
    /// authentication: verify the caller IS the canonical pool for (token0,
    /// token1, fee) by CREATE2, and that we are mid-swap. Storing "the pool
    /// we called" does not generalize to N legs; derivation does.
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        uint256 swapping;
        assembly { swapping := tload(T_SWAPPING) }
        // Outside a swap leg the only swap callback we take is the flash
        // swap's, from the pool `execute` armed.
        if (swapping == 0) {
            _flashSwap(amount0Delta, amount1Delta, data);
            return;
        }
        _v3SwapCallback(amount0Delta, amount1Delta, data);
    }

    /// PancakeSwap V3's swap callback: the same call under another name.
    /// Pancake pools are never a flash-swap source, so outside a swap leg it
    /// reverts.
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        uint256 swapping;
        assembly { swapping := tload(T_SWAPPING) }
        if (swapping == 0) revert BadSwapCallback();
        _v3SwapCallback(amount0Delta, amount1Delta, data);
    }

    /// A V3-family swap callback inside a swap leg, by what the leg passed
    /// the pool: 96 bytes (tokenIn, tokenOut, fee) is a Uniswap pool; 128
    /// bytes, with the factory id of a fork as a fourth word, is that fork's
    /// pool; anything longer is a chain hop (`S_CHAIN`), whose callback
    /// carries the chain, not a triple. The caller must be the pool the id's
    /// deployer derives for the triple. Either selector may carry either
    /// fork's id: only a genuine pool of that deployer passes, and it calls
    /// back under its own name.
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
            // The swap module continues a chain hop's callback and
            // authenticates the caller as the hop in flight.
            _delegate(SWAP_MODULE, abi.encodeCall(ISwapModule.v3ChainCallback, (amount0Delta, amount1Delta, data)));
            return;
        }
        if (msg.sender != expected) revert BadSwapCallback();

        // We called swap(zeroForOne = tokenIn < tokenOut), so the positive
        // delta is always tokenIn's side.
        uint256 owed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        tokenIn.safeTransfer(msg.sender, owed);
    }

    /// The flash swap's callback (`P_UNIV3_SWAP`): the pool has paid the
    /// group's debt and is owed its other token. Authenticated as every
    /// provider callback is — the armed source, inside `execute` — and only
    /// for a flash-swap group. A stray swap callback outside a swap leg
    /// still reverts `BadSwapCallback`.
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

        // Which side the pool paid, and which it is owed. The debt must have
        // been paid in full: a pool whose liquidity runs out before
        // `flashAmount` stops short, and the legs would repay less than the
        // plan says.
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

        // No fee: the pool's price carries it.
        _core(fg, plan, 0);
        tokenIn.safeTransfer(msg.sender, uint256(owed));
    }

    /// Receives ETH: WETH unwrapped for a coinbase bid or a native-ETH leg,
    /// and native ETH a protocol pays out (the leg wraps that back).
    receive() external payable {}
}
