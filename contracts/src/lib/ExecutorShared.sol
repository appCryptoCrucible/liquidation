// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/*
 * What the Executor and its two modules share (GUIDE 10).
 *
 * The modules run by DELEGATECALL: their code executes as the Executor —
 * same address, balances, allowances, transient storage and event emitter.
 * The transient slots are therefore one namespace across all three
 * contracts, and are defined once, here. File-level constants, because
 * inline assembly reads only literal-valued constants.
 */

// EIP-1153 slots, all in the Executor's transient storage.
// The provider an armed flash callback must come from.
uint256 constant T_EXPECTED_CALLER = 0x00;
// Non-zero while an `execute` is running.
uint256 constant T_ENTERED         = 0x01;
// Non-zero while swap legs run: a Uniswap V3 swap callback is expected.
uint256 constant T_SWAPPING        = 0x02;
// The flash group that is borrowing, as `execute` decoded it, in three
// words: its callback reads it here instead of decoding the plan again.
// provider << 160 | flashSource
uint256 constant T_GROUP_SOURCE    = 0x03;
// Which liquidation legs of the group currently executing filled, one bit
// per leg (bit i = leg i), or `NO_CALLBACK` if the provider returned without
// ever calling back. A swap leg tied to a leg whose bit is clear is skipped
// (`SwapModule.runSwaps`).
uint256 constant T_FILLED          = 0x04;
// repaySwapCount << 168 | liqCount << 160 | debtAsset
uint256 constant T_GROUP_DEBT      = 0x05;
// repaySwapOffset << 192 | liqOffset << 128 | flashAmount
uint256 constant T_GROUP_SPAN      = 0x06;
// The fee the group's flash provider charges, in its debt asset, until the
// first exact-output pool leg of the group's repay blob buys it.
uint256 constant T_FEE             = 0x07;
// Non-zero while this contract is the Uniswap V4 PoolManager's unlocker (a
// V4 flash group's callback is running): a V4 swap leg then swaps and
// settles inside that unlock instead of unlocking again, which V4 refuses.
uint256 constant T_V4_UNLOCKED     = 0x08;
// The Uniswap V3 pool of the chain hop in flight (`SwapModule`'s
// `S_CHAIN`): its swap callback is the only one that continues the chain.
// Set before each hop's swap and restored after, so a callback from any
// other address, or out of order, is refused.
uint256 constant T_CHAIN_POOL      = 0x09;

/// What each module answers to `MODULE_ID()`. The Executor's constructor
/// refuses an address that is not the module it is wired as.
library ModuleIds {
    bytes32 internal constant LIQUIDATION = keccak256("liq-executor/liquidation-module/v1");
    bytes32 internal constant SWAP        = keccak256("liq-executor/swap-module/v1");
    bytes32 internal constant DEX         = keccak256("liq-executor/dex-module/v1");
}

/// Protocol liquidations and governance actions, run as the Executor.
/// Entry points are `payable`: delegatecall keeps the Executor's
/// `msg.value`, and no value is ever sent to them.
interface ILiquidationModule {
    function MODULE_ID() external view returns (bytes32);
    function WETH() external view returns (address);
    /// One flash group's legs. Returns which filled, one bit per leg.
    function runLegs(address debtAsset, uint256 liqOffset, uint8 liqCount, bytes calldata plan)
        external payable returns (uint256 filled);
    /// The plan's governance action. Returns its gas cost in wei when it ran.
    function govExec(uint8 flags, uint40 payloadId, address spell)
        external payable returns (uint256 cost);
}

/// Swap and unwrap legs, run as the Executor.
interface ISwapModule {
    function MODULE_ID() external view returns (bytes32);
    function WETH() external view returns (address);
    /// `legs` swap legs, the first at `offset`.
    function runSwaps(uint256 offset, uint8 legs, bytes calldata plan) external payable;
    /// A V4 swap leg's own unlock: the PoolManager called the Executor's
    /// `unlockCallback` with what `_swapV4` passed to `unlock`.
    function v4SwapCallback(bytes calldata data) external payable;
    /// A chain hop's Uniswap V3 swap callback (`S_CHAIN`), as the Executor
    /// relays it.
    function v3ChainCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data)
        external payable;
}

/// Balancer and Fluid DEX legs, run as the Executor by the swap module,
/// which delegatecalls it for venues 11 and 12 and for their chain hops.
interface IDexModule {
    function MODULE_ID() external view returns (bytes32);
    function WETH() external view returns (address);
    /// One swap of `amount` of `tokenIn` for `tokenOut` on `venue` (exact
    /// output when `exactOut`). Returns how much `tokenOut` this contract
    /// received.
    function swapLeg(uint8 venue, address tokenIn, address tokenOut, bool exactOut, uint256 amount, bytes calldata data)
        external payable returns (uint256 received);
}

/// The Executor's V3 anchors, read by the swap module through the
/// Executor itself (it runs as the Executor, whose immutables it cannot
/// read directly).
interface IV3Anchors {
    function UNIV3_FACTORY() external view returns (address);
    function UNIV3_POOL_INIT_HASH() external view returns (bytes32);
}
