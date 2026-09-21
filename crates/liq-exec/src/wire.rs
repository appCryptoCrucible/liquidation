//! Packed-plan decoder — a line-for-line mirror of
//! `contracts/src/lib/PlanDecoder.sol` (PLAN-ENCODING.md §1).
//!
//! Zero-copy and allocation-free: every accessor borrows the plan bytes and
//! every offset is derived by walking, exactly as the contract does. This is
//! the Rust half of the round-trip test the wire format demands (§4): the
//! encoder (`liq-plan`, WP 10B) must produce bytes both this and the Solidity
//! decoder read back field-for-field.
//!
//! Every read is bounds-checked and returns [`WireError::Truncated`] rather
//! than panicking; the contract reverts on the same byte.

use alloy_primitives::{Address, B256, U256};
use liq_protocol::ExecutorAdapter;
use liq_types::FlashProvider;

pub const HEADER_LEN: usize = 35;
pub const GROUP_HEAD_LEN: usize = 59;
/// Fixed part of a liquidation leg; `+ ExecutorAdapter::tail_len`.
pub const LIQ_LEG_LEN: usize = 77;
pub const SWAP_LEG_HEAD_LEN: usize = 60;

/// Header flag bit 0 — sweep WETH to `PROFIT_SINK` after this plan.
pub const FLAG_SWEEP: u8 = 1 << 0;
/// Swap-leg flag bit 0 — spend the whole `tokenIn` balance.
pub const LEG_TAKE_BALANCE: u8 = 1 << 0;
/// Swap-leg flag bit 1 — `amount` is an exact output.
pub const LEG_EXACT_OUT: u8 = 1 << 1;
/// Swap venue 0 — Uniswap V3 pool-direct (`data` = 20-byte pool).
pub const VENUE_UNIV3_POOL: u8 = 0;
/// Swap venue 1 — allowlisted router (`data` = 20-byte target + calldata).
pub const VENUE_ROUTER: u8 = 1;

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("plan truncated: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },
    /// `PlanDecoder.UnknownAdapter`.
    #[error("unknown adapter id {0}")]
    UnknownAdapter(u8),
    /// `Executor.UnknownProvider` — surfaced at decode here, at `_initiate`
    /// on-chain (the contract's decoder does not inspect the byte).
    #[error("unknown provider id {0}")]
    UnknownProvider(u8),
    /// `PlanDecoder.NoGroups`.
    #[error("plan has zero flash groups")]
    NoGroups,
    /// `PlanDecoder.BadPlanLength`: walked length differs from the blob.
    #[error("walked {walked} bytes, plan is {actual}")]
    BadPlanLength { walked: usize, actual: usize },
    #[error("offset arithmetic overflow")]
    Overflow,
}

pub type Result<T> = core::result::Result<T, WireError>;

/// PLAN-ENCODING §1a plus the walked profit-swap offset.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub bid_bps: u16,
    pub gas_cost_wei: u128,
    pub min_profit: u128,
    pub group_count: u8,
    /// Offset of the profit-swap count byte.
    pub profit_swap_offset: usize,
}

/// PLAN-ENCODING §1b group head plus walked offsets.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GroupHead {
    pub provider: FlashProvider,
    pub flash_source: Address,
    pub debt_asset: Address,
    pub flash_amount: u128,
    pub liq_count: u8,
    pub repay_swap_count: u8,
    pub liq_offset: usize,
    pub repay_swap_offset: usize,
    /// Offset of the next group (or of the profit-swap count byte).
    pub next: usize,
}

/// Adapter-specific bytes after the 77 fixed leg bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LegTail {
    /// Aave V3 / Silo V2: nothing beyond the 77 fixed bytes.
    /// Silo `receiveSToken` is hardcoded `false` on-chain.
    None,
    /// Aave V4 `liquidationCall(collateralReserveId, debtReserveId, …)`.
    AaveV4 {
        collateral_reserve_id: u16,
        debt_reserve_id: u16,
    },
    /// Morpho Blue market `Id`; `idToMarketParams` on-chain.
    Morpho { market_id: B256 },
    /// Euler V2 `liquidate(…, minYieldBalance)` — quoted yield shares.
    Euler { min_yield: U256 },
    /// Liquity V2 `batchLiquidateTroves` — full uint256 trove id.
    Liquity { trove_id: U256 },
    /// Fluid T1 `liquidate(…, colPerUnitDebt_, …)` — quoted 1e27 ratio.
    /// `absorb_` is hardcoded `true` on-chain (matches the absorb-inclusive quote).
    Fluid { col_per_unit_debt: U256 },
    /// Gearbox V3 `partiallyLiquidateCreditAccount` — quoted min seized.
    Gearbox { min_seized: U256 },
    /// Compound V2: debt cToken is `market`; tail is the seize cToken + CEther flag.
    CompoundV2 {
        ctoken_collateral: Address,
        /// 1 = debt cToken is CEther (`liquidateBorrow` payable). From config,
        /// never guessed via `underlying()`.
        is_cether: u8,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiqLeg {
    pub adapter: ExecutorAdapter,
    pub market: Address,
    pub borrower: Address,
    pub collateral_asset: Address,
    pub repay_amount: u128,
    pub tail: LegTail,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SwapLeg<'a> {
    pub venue: u8,
    pub token_in: Address,
    pub token_out: Address,
    pub flags: u8,
    pub amount: u128,
    pub data: &'a [u8],
}

// ───────────────────────────── primitive reads ─────────────────────────────

#[inline]
fn take(b: &[u8], o: usize, n: usize) -> Result<&[u8]> {
    let end = o.checked_add(n).ok_or(WireError::Overflow)?;
    b.get(o..end).ok_or(WireError::Truncated {
        need: end,
        have: b.len(),
    })
}

#[inline]
fn u8_at(b: &[u8], o: usize) -> Result<u8> {
    b.get(o).copied().ok_or(WireError::Truncated {
        need: o.saturating_add(1),
        have: b.len(),
    })
}

#[inline]
fn u16_at(b: &[u8], o: usize) -> Result<u16> {
    let s = take(b, o, 2)?;
    let arr: [u8; 2] = s.try_into().map_err(|_| WireError::Overflow)?;
    Ok(u16::from_be_bytes(arr))
}

#[inline]
fn u128_at(b: &[u8], o: usize) -> Result<u128> {
    let s = take(b, o, 16)?;
    let arr: [u8; 16] = s.try_into().map_err(|_| WireError::Overflow)?;
    Ok(u128::from_be_bytes(arr))
}

#[inline]
fn addr_at(b: &[u8], o: usize) -> Result<Address> {
    Ok(Address::from_slice(take(b, o, 20)?))
}

#[inline]
fn b256_at(b: &[u8], o: usize) -> Result<B256> {
    Ok(B256::from_slice(take(b, o, 32)?))
}

#[inline]
fn u256_at(b: &[u8], o: usize) -> Result<U256> {
    Ok(U256::from_be_slice(take(b, o, 32)?))
}

#[inline]
fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b).ok_or(WireError::Overflow)
}

/// `liq_types::FlashProvider` discriminants are the wire byte (D09).
#[inline]
#[must_use]
pub const fn provider_from_wire(b: u8) -> Option<FlashProvider> {
    match b {
        0 => Some(FlashProvider::Aave),
        1 => Some(FlashProvider::UniV3),
        2 => Some(FlashProvider::UniV4),
        3 => Some(FlashProvider::Morpho),
        4 => Some(FlashProvider::SkyDss),
        _ => None,
    }
}

// ───────────────────────── PlanDecoder.sol mirrors ─────────────────────────

/// `PlanDecoder.tailLen`.
#[inline]
pub fn tail_len(adapter: u8) -> Result<usize> {
    ExecutorAdapter::from_wire(adapter)
        .map(ExecutorAdapter::tail_len)
        .ok_or(WireError::UnknownAdapter(adapter))
}

/// `PlanDecoder.group`.
pub fn decode_group(b: &[u8], o: usize) -> Result<GroupHead> {
    let provider_byte = u8_at(b, o)?;
    let provider =
        provider_from_wire(provider_byte).ok_or(WireError::UnknownProvider(provider_byte))?;
    let liq_count = u8_at(b, add(o, 57)?)?;
    let repay_swap_count = u8_at(b, add(o, 58)?)?;
    let liq_offset = add(o, GROUP_HEAD_LEN)?;
    let repay_swap_offset = skip_liq_legs(b, liq_offset, liq_count)?;
    let next = skip_swap_legs(b, repay_swap_offset, repay_swap_count)?;
    Ok(GroupHead {
        provider,
        flash_source: addr_at(b, add(o, 1)?)?,
        debt_asset: addr_at(b, add(o, 21)?)?,
        flash_amount: u128_at(b, add(o, 41)?)?,
        liq_count,
        repay_swap_count,
        liq_offset,
        repay_swap_offset,
        next,
    })
}

/// `PlanDecoder.liqLeg` — returns the leg and the offset past its tail.
pub fn decode_liq_leg(b: &[u8], o: usize) -> Result<(LiqLeg, usize)> {
    let adapter_byte = u8_at(b, o)?;
    let adapter =
        ExecutorAdapter::from_wire(adapter_byte).ok_or(WireError::UnknownAdapter(adapter_byte))?;
    let tail_offset = add(o, LIQ_LEG_LEN)?;
    let tail = match adapter {
        ExecutorAdapter::AaveV3 | ExecutorAdapter::SiloV2 => LegTail::None,
        ExecutorAdapter::AaveV4 => LegTail::AaveV4 {
            collateral_reserve_id: u16_at(b, tail_offset)?,
            debt_reserve_id: u16_at(b, add(tail_offset, 2)?)?,
        },
        ExecutorAdapter::MorphoBlue => LegTail::Morpho {
            market_id: b256_at(b, tail_offset)?,
        },
        ExecutorAdapter::EulerV2 => LegTail::Euler {
            min_yield: u256_at(b, tail_offset)?,
        },
        ExecutorAdapter::LiquityV2 => LegTail::Liquity {
            trove_id: u256_at(b, tail_offset)?,
        },
        ExecutorAdapter::Fluid => LegTail::Fluid {
            col_per_unit_debt: u256_at(b, tail_offset)?,
        },
        ExecutorAdapter::Gearbox => LegTail::Gearbox {
            min_seized: u256_at(b, tail_offset)?,
        },
        ExecutorAdapter::CompoundV2 => LegTail::CompoundV2 {
            ctoken_collateral: addr_at(b, tail_offset)?,
            is_cether: u8_at(b, add(tail_offset, 20)?)?,
        },
    };
    let next = add(tail_offset, adapter.tail_len())?;
    Ok((
        LiqLeg {
            adapter,
            market: addr_at(b, add(o, 1)?)?,
            borrower: addr_at(b, add(o, 21)?)?,
            collateral_asset: addr_at(b, add(o, 41)?)?,
            repay_amount: u128_at(b, add(o, 61)?)?,
            tail,
        },
        next,
    ))
}

/// `PlanDecoder.swapLeg` — returns the leg (data borrowed) and the next offset.
pub fn decode_swap_leg(b: &[u8], o: usize) -> Result<(SwapLeg<'_>, usize)> {
    let data_len = usize::from(u16_at(b, add(o, 58)?)?);
    let data_offset = add(o, SWAP_LEG_HEAD_LEN)?;
    let data = take(b, data_offset, data_len)?;
    Ok((
        SwapLeg {
            venue: u8_at(b, o)?,
            token_in: addr_at(b, add(o, 1)?)?,
            token_out: addr_at(b, add(o, 21)?)?,
            flags: u8_at(b, add(o, 41)?)?,
            amount: u128_at(b, add(o, 42)?)?,
            data,
        },
        add(data_offset, data_len)?,
    ))
}

/// `PlanDecoder.skipLiqLegs`.
pub fn skip_liq_legs(b: &[u8], mut o: usize, n: u8) -> Result<usize> {
    for _ in 0..n {
        let t = tail_len(u8_at(b, o)?)?;
        o = add(add(o, LIQ_LEG_LEN)?, t)?;
    }
    if o > b.len() {
        return Err(WireError::BadPlanLength {
            walked: o,
            actual: b.len(),
        });
    }
    Ok(o)
}

/// `PlanDecoder.skipSwapLegs`.
pub fn skip_swap_legs(b: &[u8], mut o: usize, n: u8) -> Result<usize> {
    for _ in 0..n {
        let data_len = usize::from(u16_at(b, add(o, 58)?)?);
        o = add(add(o, SWAP_LEG_HEAD_LEN)?, data_len)?;
    }
    if o > b.len() {
        return Err(WireError::BadPlanLength {
            walked: o,
            actual: b.len(),
        });
    }
    Ok(o)
}

// ─────────────────────────────── views ───────────────────────────────

/// A parsed plan. `parse` walks every group, leg and swap blob once
/// (`PlanDecoder.header`), so every later accessor reads validated bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Plan<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl<'a> Plan<'a> {
    /// `PlanDecoder.header`.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let group_count = u8_at(bytes, HEADER_LEN)?;
        if group_count == 0 {
            return Err(WireError::NoGroups);
        }
        let mut cur = add(HEADER_LEN, 1)?;
        for _ in 0..group_count {
            cur = decode_group(bytes, cur)?.next;
        }
        let profit_swap_offset = cur;
        let profit_count = u8_at(bytes, cur)?;
        cur = skip_swap_legs(bytes, add(cur, 1)?, profit_count)?;
        if cur != bytes.len() {
            return Err(WireError::BadPlanLength {
                walked: cur,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            bytes,
            header: Header {
                flags: u8_at(bytes, 0)?,
                bid_bps: u16_at(bytes, 1)?,
                gas_cost_wei: u128_at(bytes, 3)?,
                min_profit: u128_at(bytes, 19)?,
                group_count,
                profit_swap_offset,
            },
        })
    }

    #[inline]
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// `PlanDecoder.groupAt` — re-walks to group `g`.
    pub fn group(&self, g: u8) -> Result<Group<'a>> {
        if g >= self.header.group_count {
            return Err(WireError::NoGroups);
        }
        let mut o = add(HEADER_LEN, 1)?;
        for _ in 0..g {
            o = decode_group(self.bytes, o)?.next;
        }
        Ok(Group {
            bytes: self.bytes,
            head: decode_group(self.bytes, o)?,
        })
    }

    #[must_use]
    pub const fn groups(&self) -> Groups<'a> {
        Groups {
            bytes: self.bytes,
            o: HEADER_LEN + 1,
            remaining: self.header.group_count,
        }
    }

    /// Profit swap legs (count byte at `profit_swap_offset`).
    pub fn profit_swaps(&self) -> Result<SwapLegs<'a>> {
        let o = self.header.profit_swap_offset;
        Ok(SwapLegs {
            bytes: self.bytes,
            o: add(o, 1)?,
            remaining: u8_at(self.bytes, o)?,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Group<'a> {
    bytes: &'a [u8],
    pub head: GroupHead,
}

impl<'a> Group<'a> {
    #[must_use]
    pub const fn liq_legs(&self) -> LiqLegs<'a> {
        LiqLegs {
            bytes: self.bytes,
            o: self.head.liq_offset,
            remaining: self.head.liq_count,
        }
    }

    /// Repay swap legs — no count prefix (PLAN-ENCODING §1c).
    #[must_use]
    pub const fn repay_swaps(&self) -> SwapLegs<'a> {
        SwapLegs {
            bytes: self.bytes,
            o: self.head.repay_swap_offset,
            remaining: self.head.repay_swap_count,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Groups<'a> {
    bytes: &'a [u8],
    o: usize,
    remaining: u8,
}

impl<'a> Iterator for Groups<'a> {
    type Item = Result<Group<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining = self.remaining.saturating_sub(1);
        match decode_group(self.bytes, self.o) {
            Ok(head) => {
                self.o = head.next;
                Some(Ok(Group {
                    bytes: self.bytes,
                    head,
                }))
            }
            Err(e) => {
                self.remaining = 0;
                Some(Err(e))
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiqLegs<'a> {
    bytes: &'a [u8],
    o: usize,
    remaining: u8,
}

impl Iterator for LiqLegs<'_> {
    type Item = Result<LiqLeg>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining = self.remaining.saturating_sub(1);
        match decode_liq_leg(self.bytes, self.o) {
            Ok((leg, next)) => {
                self.o = next;
                Some(Ok(leg))
            }
            Err(e) => {
                self.remaining = 0;
                Some(Err(e))
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SwapLegs<'a> {
    bytes: &'a [u8],
    o: usize,
    remaining: u8,
}

impl<'a> Iterator for SwapLegs<'a> {
    type Item = Result<SwapLeg<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining = self.remaining.saturating_sub(1);
        match decode_swap_leg(self.bytes, self.o) {
            Ok((leg, next)) => {
                self.o = next;
                Some(Ok(leg))
            }
            Err(e) => {
                self.remaining = 0;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Oracle: `liq_types::FlashProvider` discriminants (D09) and
    /// `Executor.sol` `P_*` constants.
    #[test]
    fn provider_wire_bytes_are_the_discriminants() {
        for p in [
            FlashProvider::Aave,
            FlashProvider::UniV3,
            FlashProvider::UniV4,
            FlashProvider::Morpho,
            FlashProvider::SkyDss,
        ] {
            assert_eq!(provider_from_wire(p as u8), Some(p));
        }
        assert_eq!(provider_from_wire(5), None);
    }

    /// Oracle: `PlanDecoder.sol` constants.
    #[test]
    fn constants_match_plan_decoder_sol() {
        assert_eq!(HEADER_LEN, 35);
        assert_eq!(GROUP_HEAD_LEN, 59);
        assert_eq!(LIQ_LEG_LEN, 77);
        assert_eq!(SWAP_LEG_HEAD_LEN, 60);
        assert_eq!(tail_len(0).unwrap(), 0);
        assert_eq!(tail_len(1).unwrap(), 4);
        assert_eq!(tail_len(2).unwrap(), 32);
        assert_eq!(tail_len(3).unwrap(), 32);
        assert_eq!(tail_len(4).unwrap(), 0);
        assert_eq!(tail_len(5).unwrap(), 32);
        assert_eq!(tail_len(6).unwrap(), 32);
        assert_eq!(tail_len(7).unwrap(), 32);
        assert_eq!(tail_len(8).unwrap(), 21);
        assert_eq!(tail_len(9), Err(WireError::UnknownAdapter(9)));
    }

    #[test]
    fn decode_10e_tails() {
        let mut b = vec![0u8; LIQ_LEG_LEN + 32];
        b[0] = 3; // Euler
        b[LIQ_LEG_LEN + 31] = 7;
        let (leg, next) = decode_liq_leg(&b, 0).unwrap();
        assert_eq!(leg.adapter, ExecutorAdapter::EulerV2);
        assert_eq!(
            leg.tail,
            LegTail::Euler {
                min_yield: U256::from(7u64)
            }
        );
        assert_eq!(next, LIQ_LEG_LEN + 32);

        let mut c = vec![0u8; LIQ_LEG_LEN + 21];
        c[0] = 8; // Compound
        c[LIQ_LEG_LEN + 19] = 0xAB;
        c[LIQ_LEG_LEN + 20] = 1;
        let (leg, next) = decode_liq_leg(&c, 0).unwrap();
        assert_eq!(leg.adapter, ExecutorAdapter::CompoundV2);
        match leg.tail {
            LegTail::CompoundV2 {
                ctoken_collateral,
                is_cether,
            } => {
                assert_eq!(is_cether, 1);
                assert_eq!(ctoken_collateral.as_slice()[19], 0xAB);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(next, LIQ_LEG_LEN + 21);
        assert_eq!(decode_liq_leg(&[9u8], 0), Err(WireError::UnknownAdapter(9)));
    }

    #[test]
    fn empty_and_short_plans_fail_closed() {
        assert!(matches!(
            Plan::parse(&[]),
            Err(WireError::Truncated { need: 36, have: 0 })
        ));
        let mut b = vec![0u8; HEADER_LEN + 1];
        assert_eq!(Plan::parse(&b), Err(WireError::NoGroups));
        // One group claimed, no group bytes.
        *b.last_mut().unwrap() = 1;
        assert!(matches!(Plan::parse(&b), Err(WireError::Truncated { .. })));
    }
}
