//! `alloy::sol!` ABI shared with `contracts/src/differential/Types.sol`.

use alloy_primitives::{hex, Bytes};
use alloy_sol_types::{sol, SolValue};
use thiserror::Error;

sol! {
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
}

#[derive(Debug, Error)]
pub enum AbiError {
    #[error("hex: {0}")]
    Hex(String),
    #[error("abi: {0}")]
    Abi(#[from] alloy_sol_types::Error),
}

/// Decode Foundry `abi.encode(DiffCase)` (`0x…` hex or raw ABI bytes).
pub fn decode_case(raw: &[u8]) -> Result<DiffCase, AbiError> {
    let bytes = decode_bytes(raw)?;
    Ok(DiffCase::abi_decode(&bytes)?)
}

pub fn encode_view(
    hf: alloy_primitives::U256,
    coll: alloy_primitives::U256,
    debt: alloy_primitives::U256,
) -> Vec<u8> {
    (hf, coll, debt).abi_encode()
}

fn decode_bytes(raw: &[u8]) -> Result<Bytes, AbiError> {
    let trimmed = raw
        .strip_prefix(b"0x")
        .or_else(|| raw.strip_prefix(b"0X"))
        .unwrap_or(raw);
    if trimmed.iter().all(u8::is_ascii_hexdigit)
        && trimmed.len() >= 2
        && trimmed.len().is_multiple_of(2)
    {
        hex::decode(trimmed)
            .map(Bytes::from)
            .map_err(|e| AbiError::Hex(e.to_string()))
    } else {
        Ok(Bytes::copy_from_slice(raw))
    }
}
