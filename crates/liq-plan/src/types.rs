//! `BatchPlan` — the off-chain value the encoder packs (PLAN-ENCODING §2).

use alloy_primitives::{Address, B256, U256};
use liq_protocol::ExecutorAdapter;
use liq_types::FlashProvider;
use liq_wire::wire::LegTail;

/// Header flag bit 0 — sweep WETH to `PROFIT_SINK` after this plan.
pub const FLAG_SWEEP: u8 = liq_wire::wire::FLAG_SWEEP;
/// Swap-leg flag bit 0 — spend the whole `tokenIn` balance.
pub const LEG_TAKE_BALANCE: u8 = liq_wire::wire::LEG_TAKE_BALANCE;
/// Swap-leg flag bit 1 — `amount` is an exact output.
pub const LEG_EXACT_OUT: u8 = liq_wire::wire::LEG_EXACT_OUT;
pub const VENUE_UNIV3_POOL: u8 = liq_wire::wire::VENUE_UNIV3_POOL;
pub const VENUE_ROUTER: u8 = liq_wire::wire::VENUE_ROUTER;
pub const VENUE_UNIV2_POOL: u8 = liq_wire::wire::VENUE_UNIV2_POOL;
pub const VENUE_CURVE_POOL: u8 = liq_wire::wire::VENUE_CURVE_POOL;
/// V2 pair factory ids carried in a `VENUE_UNIV2_POOL` leg's data.
pub const V2_FACTORY_UNISWAP: u8 = liq_wire::wire::V2_FACTORY_UNISWAP;
pub const V2_FACTORY_SUSHI: u8 = liq_wire::wire::V2_FACTORY_SUSHI;

pub const HEADER_LEN: usize = liq_wire::wire::HEADER_LEN;
pub const GROUP_HEAD_LEN: usize = liq_wire::wire::GROUP_HEAD_LEN;
pub const LIQ_LEG_LEN: usize = liq_wire::wire::LIQ_LEG_LEN;
pub const SWAP_LEG_HEAD_LEN: usize = liq_wire::wire::SWAP_LEG_HEAD_LEN;

/// One packed plan ready for `Executor.execute(bytes)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPlan(pub Vec<u8>);

impl EncodedPlan {
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[inline]
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchPlan {
    pub flags: u8,
    pub bid_bps: u16,
    pub gas_cost_wei: u128,
    pub min_profit_wei: u128,
    pub groups: Vec<FlashGroup>,
    pub profit_swaps: Vec<SwapLeg>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlashGroup {
    pub provider: FlashProvider,
    pub flash_source: Address,
    pub debt_asset: Address,
    pub flash_amount: u128,
    /// Off-wire, same as `protocol_pull`. Premium in bps of this group's
    /// source. UniV4 and Morpho are 0; a nonzero bps there is unpriceable.
    pub fee_bps: u16,
    pub liqs: Vec<LiqLeg>,
    pub repay_swaps: Vec<SwapLeg>,
}

/// Liquidation leg: 77 fixed bytes plus [`ExecutorAdapter::tail_len`].
///
/// `protocol_pull` is not on the wire. It is the amount the protocol will
/// actually take (V3 close-factor / V4 target-HF clamp / Morpho share
/// rounding). The repay swap must buy it back plus the flash premium.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiqLeg {
    pub adapter: ExecutorAdapter,
    pub market: Address,
    pub borrower: Address,
    pub collateral_asset: Address,
    pub repay_amount: u128,
    pub tail: LegTail,
    pub protocol_pull: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapLeg {
    pub venue: u8,
    pub token_in: Address,
    pub token_out: Address,
    pub flags: u8,
    pub amount: u128,
    pub data: Vec<u8>,
}

/// Registry pins `validate` checks against. Built from adapter config /
/// Morpho `MarketParams` (keccak id), never guessed.
#[derive(Clone, Debug, Default)]
pub struct ValidateCtx {
    pub weth: Address,
    /// `(spoke, reserve_id) → underlying`.
    pub v4_underlying: Vec<V4ReservePin>,
    /// Morpho markets whose `Id` is `keccak256(abi.encode(MarketParams))`.
    pub morpho: Vec<MorphoMarketPin>,
    /// Compound V2: debt cToken + seize cToken + CEther flag from config
    /// (`underlying == 0` ⇒ CEther). A random cToken or flipped flag
    /// must not encode.
    pub compound: Vec<CompoundMarketPin>,
    /// Liquity V2: TroveManager + full trove id + borrower from
    /// `TroveExtra` / branch. A trove id that is not the quoted position
    /// must not encode.
    pub liquity: Vec<LiquityTrovePin>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct V4ReservePin {
    pub spoke: Address,
    pub reserve_id: u16,
    pub underlying: Address,
}

/// Morpho Blue market. `id` must equal `keccak256(abi.encode(params))`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MorphoMarketPin {
    pub id: B256,
    pub morpho: Address,
    pub loan_token: Address,
    pub collateral_token: Address,
    pub oracle: Address,
    pub irm: Address,
    pub lltv: U256,
}

/// Compound V2 liquidation pair. `is_cether == 1` iff the **debt** cToken
/// has `underlying == 0` in config (CEther). Never guessed via `underlying()`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CompoundMarketPin {
    pub debt_ctoken: Address,
    pub ctoken_collateral: Address,
    pub is_cether: u8,
}

/// Liquity V2 quoted trove. `trove_id` is the full uint256 NFT id from
/// `TroveExtra` (intern `PositionKey.user` is only the low 160 bits).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiquityTrovePin {
    pub trove_manager: Address,
    pub trove_id: U256,
    pub borrower: Address,
}

impl ValidateCtx {
    #[must_use]
    pub fn v4_pin(&self, spoke: Address, id: u16) -> Option<Address> {
        self.v4_underlying
            .iter()
            .find(|p| p.spoke == spoke && p.reserve_id == id)
            .map(|p| p.underlying)
    }

    #[must_use]
    pub fn morpho_pin(&self, id: B256) -> Option<&MorphoMarketPin> {
        self.morpho.iter().find(|p| p.id == id)
    }

    #[must_use]
    pub fn compound_pin(
        &self,
        debt_ctoken: Address,
        ctoken_collateral: Address,
    ) -> Option<&CompoundMarketPin> {
        self.compound
            .iter()
            .find(|p| p.debt_ctoken == debt_ctoken && p.ctoken_collateral == ctoken_collateral)
    }

    #[must_use]
    pub fn liquity_pin(&self, trove_id: U256) -> Option<&LiquityTrovePin> {
        self.liquity.iter().find(|p| p.trove_id == trove_id)
    }

    /// Add a pin unless an identical one is present. Returns whether it
    /// was added. A *conflicting* pin for the same key is not replaced: the
    /// first one stands and `validate` refuses the other shape.
    pub fn add_v4(&mut self, pin: V4ReservePin) -> bool {
        if self.v4_pin(pin.spoke, pin.reserve_id).is_some() {
            return false;
        }
        self.v4_underlying.push(pin);
        true
    }

    pub fn add_morpho(&mut self, pin: MorphoMarketPin) -> bool {
        if self.morpho_pin(pin.id).is_some() {
            return false;
        }
        self.morpho.push(pin);
        true
    }

    pub fn add_compound(&mut self, pin: CompoundMarketPin) -> bool {
        if self
            .compound_pin(pin.debt_ctoken, pin.ctoken_collateral)
            .is_some()
        {
            return false;
        }
        self.compound.push(pin);
        true
    }

    pub fn add_liquity(&mut self, pin: LiquityTrovePin) -> bool {
        if self.liquity_pin(pin.trove_id).is_some() {
            return false;
        }
        self.liquity.push(pin);
        true
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn add_pin_dedupes_and_first_pin_stands() {
        let mut ctx = ValidateCtx::default();
        let spoke = Address::repeat_byte(0x51);
        let pin = V4ReservePin {
            spoke,
            reserve_id: 3,
            underlying: Address::repeat_byte(0xA1),
        };
        assert!(ctx.add_v4(pin));
        assert!(!ctx.add_v4(pin), "identical pin not duplicated");
        let conflicting = V4ReservePin {
            underlying: Address::repeat_byte(0xB2),
            ..pin
        };
        assert!(!ctx.add_v4(conflicting), "conflict does not replace");
        assert_eq!(ctx.v4_pin(spoke, 3), Some(Address::repeat_byte(0xA1)));
        assert_eq!(ctx.v4_underlying.len(), 1);

        let c = CompoundMarketPin {
            debt_ctoken: Address::repeat_byte(0xC1),
            ctoken_collateral: Address::repeat_byte(0xC2),
            is_cether: 0,
        };
        assert!(ctx.add_compound(c));
        assert!(!ctx.add_compound(c));
        let t = LiquityTrovePin {
            trove_manager: Address::repeat_byte(0x7A),
            trove_id: U256::from(42u64),
            borrower: Address::repeat_byte(0xB0),
        };
        assert!(ctx.add_liquity(t));
        assert!(!ctx.add_liquity(t));
    }
}
