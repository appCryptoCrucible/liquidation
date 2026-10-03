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
// Which flash group is executing, so a callback can find its own legs.
uint256 constant T_GROUP           = 0x03;
// Successful liquidation legs in the group currently executing, or
// `NO_CALLBACK` if the provider returned without ever calling back.
uint256 constant T_FILLED          = 0x04;

/// What each module answers to `MODULE_ID()`. The Executor's constructor
/// refuses an address that is not the module it is wired as.
library ModuleIds {
    bytes32 internal constant LIQUIDATION = keccak256("liq-executor/liquidation-module/v1");
    bytes32 internal constant SWAP        = keccak256("liq-executor/swap-module/v1");
}

/// Protocol liquidations and governance actions, run as the Executor.
/// Entry points are `payable`: delegatecall keeps the Executor's
/// `msg.value`, and no value is ever sent to them.
interface ILiquidationModule {
    function MODULE_ID() external view returns (bytes32);
    function WETH() external view returns (address);
    /// One flash group's legs. Returns how many filled.
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
}
