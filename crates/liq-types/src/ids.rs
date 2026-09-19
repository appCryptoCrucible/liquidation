//! Identity newtypes (GUIDE 00 §3).
//!
//! `AssetId` is **global**, not protocol-scoped: WETH is one id across every
//! adapter. `MarketId` is protocol-scoped and interned to a dense `u32` with a
//! side table (owned by later WPs) mapping to the protocol's own identifier.

use alloy_primitives::Address;
use bytemuck::{Pod, Zeroable};

/// EIP-155 chain id. This project is Ethereum mainnet only (D01), i.e. `1`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(pub u64);

/// Interned protocol (`aave-v4`, `aave-v3`, `morpho-blue`, …).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolId(pub u16);

/// Spoke / pool / market, **protocol-scoped**. Dense `u32` so it can hold
/// Aave V4 `(Hub, Spoke)`, a Morpho Blue market id, or Aave V3's single pool.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarketId(pub u32);

/// Global asset id. WETH is one [`AssetId`] across every adapter (GUIDE 00 §3).
/// `repr(transparent)` + `Pod`: `MarketRow` zero-copies this field via
/// `bytemuck::from_bytes` after `fs::read` (WP 02B, D59).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub struct AssetId(pub u16);

/// Dense interned position index. The hot path uses this, never [`PositionKey`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositionId(pub u32);

/// Flash-loan provider identity (GUIDE 07 §1, D09). Discriminant `4` is Sky DSS.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FlashProvider {
    Aave = 0,
    UniV3 = 1,
    UniV4 = 2,
    Morpho = 3,
    /// D09: provider id `4` = Sky DSS Flash.
    SkyDss = 4,
}

/// The full natural key. Interned to [`PositionId`] once, then never used on
/// the hot path.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PositionKey {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub user: Address,
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::{AssetId, FlashProvider, MarketId, PositionId, ProtocolId};
    use alloy_primitives::{address, Address};
    use std::collections::HashMap;

    /// Mainnet WETH9. Oracle: the chain.
    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    /// Mainnet USDC. Oracle: the chain.
    const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    /// Two protocol configs naming the same on-chain token. Intern is
    /// address-keyed (global), not `(protocol, address)`-keyed.
    struct ProtocolConfig {
        tokens: &'static [Address],
    }

    fn intern_global(table: &mut HashMap<Address, AssetId>, addr: Address) -> AssetId {
        let next = u16::try_from(table.len()).expect("test intern table fits u16");
        *table.entry(addr).or_insert(AssetId(next))
    }

    /// Load-bearing: the same WETH address in two protocol configs is one
    /// [`AssetId`]. Oracle: the chain (one WETH9 contract) + GUIDE 00 §3
    /// (AssetId intern is a function of token address, not protocol).
    #[test]
    fn weth_resolves_to_one_asset_id_from_two_protocol_configs() {
        let aave_v4 = ProtocolConfig {
            tokens: &[WETH, USDC],
        };
        let morpho_blue = ProtocolConfig { tokens: &[WETH] };

        let mut intern = HashMap::new();
        let weth_aave = intern_global(&mut intern, aave_v4.tokens[0]);
        let usdc_aave = intern_global(&mut intern, aave_v4.tokens[1]);
        let weth_morpho = intern_global(&mut intern, morpho_blue.tokens[0]);

        assert_eq!(
            weth_aave, weth_morpho,
            "oracle: chain WETH9 is one contract; GUIDE-00 §3 AssetId is global"
        );
        assert_ne!(
            weth_aave, usdc_aave,
            "oracle: chain — WETH9 and USDC are distinct contracts"
        );
    }

    /// Oracle: GUIDE 00 §3 published widths. A protocol-scoped asset id that
    /// carried a [`ProtocolId`] would not fit in two bytes. `Pod` bytes are
    /// the native `u16` (WP 02B `MarketRow` zero-copy via `bytemuck`).
    #[test]
    fn identity_widths_match_guide_00() {
        use std::mem::size_of;
        assert_eq!(size_of::<ProtocolId>(), size_of::<u16>());
        assert_eq!(size_of::<MarketId>(), size_of::<u32>());
        assert_eq!(size_of::<AssetId>(), size_of::<u16>());
        assert_eq!(size_of::<PositionId>(), size_of::<u32>());
        // Negative: the smallest (protocol, asset) pair is wider than AssetId, so
        // AssetId cannot carry a ProtocolId by construction.
        assert!(size_of::<AssetId>() < size_of::<(ProtocolId, AssetId)>());
        let id = AssetId(0xABCD);
        assert_eq!(bytemuck::bytes_of(&id), 0xABCDu16.to_ne_bytes().as_slice());
    }

    /// Oracle: an independent implementation — `Executor.sol` constants
    /// `P_AAVE = 0`, `P_UNIV3 = 1`, `P_UNIV4 = 2`, `P_MORPHO = 3`,
    /// `P_SKY_DSS = 4` (D09). The plan encoder (10B) writes `as u8`; a drift
    /// here routes a flash loan to the wrong provider on-chain.
    #[test]
    fn flash_provider_discriminants_match_executor_sol() {
        assert_eq!(FlashProvider::Aave as u8, 0);
        assert_eq!(FlashProvider::UniV3 as u8, 1);
        assert_eq!(FlashProvider::UniV4 as u8, 2);
        assert_eq!(FlashProvider::Morpho as u8, 3);
        assert_eq!(FlashProvider::SkyDss as u8, 4);
    }
}
