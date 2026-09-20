// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// Packed case shared with `liq-replay` (`alloy::sol!` in `crates/liq-replay/src/diff/abi.rs`).
/// Two held reserves: collateral `coll` (spoke slot 1) and debt `debt` (slot 2).
struct DiffSlot {
    uint8 decimals;
    uint16 collateralFactor;
    uint8 flags;
    uint128 suppliedShares;
    uint128 drawnShares;
    uint128 premiumShares;
    int256 premiumOffsetRay;
    uint128 drawnIndex;
    uint128 drawnRate;
    uint40 lastUpdate;
    uint128 addedShares;
    uint128 liquidity;
    uint128 swept;
    uint128 realizedFees;
    uint128 poolDrawnShares;
    uint128 poolPremiumShares;
    int256 poolPremiumOffsetRay;
    uint256 deficitRay;
    uint16 liquidityFee;
    uint256 priceP8;
}

struct DiffCase {
    uint64 timestamp;
    uint8 spokeKind;
    uint8 emodeKind;
    bool isolation;
    DiffSlot coll;
    DiffSlot debt;
}

/// `ISpoke.UserAccountData` fields the adapter's `health()` is scored against.
struct AccountView {
    uint256 healthFactor;
    uint256 totalCollateralValue;
    uint256 totalDebtValueRay;
    uint256 avgCollateralFactor;
}

library SlotFlags {
    uint8 internal constant USING_AS_COLLATERAL = 1 << 0;
    uint8 internal constant PAUSED = 1 << 1;
    uint8 internal constant SPOKE_ACTIVE = 1 << 2;
    uint8 internal constant SPOKE_HALTED = 1 << 3;
}
