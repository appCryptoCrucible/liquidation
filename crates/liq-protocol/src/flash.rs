//! Funding is external to the protocol (GUIDE 01 §5). The only place it
//! appears in the trait is as the `funding` parameter of `Protocol::encode`:
//! the adapter is told which provider wraps the calls and must emit a plan
//! valid under that provider's callback.

use alloy_primitives::{Address, U256};
use liq_types::{AssetId, FlashProvider};

/// Callback entry point each flash provider re-enters the Executor through
/// (GUIDE 07 §1). Five arenas; Balancer is out of scope (D08/D09) — do not
/// add it. The count is not a constant anywhere: code matches exhaustively
/// ([`CallbackShape::provider`]) so a sixth variant fails to compile until
/// every consumer handles it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CallbackShape {
    /// Aave V3/V4: `executeOperation(assets, amounts, premiums, initiator, params)`.
    AaveExecuteOperation,
    /// Uniswap V3: `uniswapV3FlashCallback(fee0, fee1, data)`.
    UniV3FlashCallback,
    /// Uniswap V4: `unlockCallback(data)` — `take()` then `settle()` inside.
    UniV4UnlockCallback,
    /// Morpho Blue: `onMorphoFlashLoan(assets, data)`.
    MorphoFlashCallback,
    /// Sky DSS Flash (ERC-3156): `onFlashLoan(initiator, token, amount, fee, data)`.
    SkyDssOnFlashLoan,
}

impl CallbackShape {
    /// Every shape, for harnesses that must exercise each one. Kept in step
    /// with the enum by [`CallbackShape::provider`]'s exhaustive `match` — a
    /// new variant is a compile error there before it can be missing here.
    pub const ALL: [Self; 5] = [
        Self::AaveExecuteOperation,
        Self::UniV3FlashCallback,
        Self::UniV4UnlockCallback,
        Self::MorphoFlashCallback,
        Self::SkyDssOnFlashLoan,
    ];

    /// The provider that re-enters through this shape. Exhaustive.
    #[inline]
    #[must_use]
    pub const fn provider(self) -> FlashProvider {
        match self {
            Self::AaveExecuteOperation => FlashProvider::Aave,
            Self::UniV3FlashCallback => FlashProvider::UniV3,
            Self::UniV4UnlockCallback => FlashProvider::UniV4,
            Self::MorphoFlashCallback => FlashProvider::Morpho,
            Self::SkyDssOnFlashLoan => FlashProvider::SkyDss,
        }
    }
}

/// Chosen funding for one liquidation. Produced by `liq-flash` (GUIDE 07),
/// consumed by `Protocol::encode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FlashRoute {
    pub provider: FlashProvider,
    /// The contract the flash is drawn from: the Aave `Pool`, the Uniswap V3
    /// pool, the V4 `PoolManager`, the Morpho singleton, the DSS Flash
    /// module. Becomes the plan's `flashSource` (PLAN-ENCODING §1b).
    pub source: Address,
    /// Borrowed asset. Must equal the repay leg's asset.
    pub asset: AssetId,
    /// Borrowed amount in `asset`'s raw units. May exceed the repay
    /// deliberately (over-borrow, PLAN-ENCODING §1b).
    pub amount: U256,
    /// Fee in bps; `0` for Uniswap V4 / Morpho / Sky DSS today. Runtime
    /// lookup upstream, never a constant.
    pub fee_bps: u16,
    /// How the provider re-enters. Must be `provider`'s shape.
    pub callback: CallbackShape,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::CallbackShape;
    use liq_types::FlashProvider;

    /// Oracle: GUIDE 07 §1 / D09 — each arena has exactly one callback and
    /// `ALL` names each shape once. Negative: no two shapes share a provider.
    #[test]
    fn shapes_and_providers_are_a_bijection() {
        let mut seen = Vec::new();
        for s in CallbackShape::ALL {
            let p = s.provider();
            assert!(!seen.contains(&p), "{p:?} reached from two shapes");
            seen.push(p);
        }
        // Every FlashProvider variant is reached (exhaustive match on the
        // provider side; adding a provider fails to compile here).
        for p in seen {
            match p {
                FlashProvider::Aave
                | FlashProvider::UniV3
                | FlashProvider::UniV4
                | FlashProvider::Morpho
                | FlashProvider::SkyDss => {}
            }
        }
        assert_eq!(
            CallbackShape::ALL.len(),
            5,
            "oracle: GUIDE 07 §3 — five arenas"
        );
    }
}
