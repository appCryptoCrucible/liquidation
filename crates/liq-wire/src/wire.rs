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
/// Header flag bit 1 — `PlanDecoder.FLAG_GOV_EXEC`: a [`PAYLOAD_ID_LEN`]-byte
/// Aave governance payload id follows the profit swaps, and `execute`
/// tries `executePayload(id)` before borrowing.
pub const FLAG_GOV_EXEC: u8 = 1 << 1;
pub const PAYLOAD_ID_LEN: usize = 5;
/// Largest id a `uint40` payload id holds.
pub const PAYLOAD_ID_MAX: u64 = (1 << 40) - 1;
/// Header flag bit 2 — `PlanDecoder.FLAG_GOV_SPELL`: a 20-byte Sky spell
/// address follows the profit swaps, and `execute` casts it first when
/// DSPause holds its plan.
pub const FLAG_GOV_SPELL: u8 = 1 << 2;
pub const SPELL_LEN: usize = 20;
/// Swap-leg flag bit 0 — spend the whole `tokenIn` balance.
pub const LEG_TAKE_BALANCE: u8 = 1 << 0;
/// Swap-leg flag bit 1 — `amount` is an exact output.
pub const LEG_EXACT_OUT: u8 = 1 << 1;
/// Swap venue 0 — Uniswap V3 pool-direct (`data` = 20-byte pool).
pub const VENUE_UNIV3_POOL: u8 = 0;
/// Swap venue 1 — allowlisted router (`data` = 20-byte target + calldata).
pub const VENUE_ROUTER: u8 = 1;
/// Swap venue 2 — pair-direct Uniswap V2 / SushiSwap (`data` = 20-byte pair
/// ‖ 1-byte factory id: 0 = Uniswap V2, 1 = SushiSwap). The Executor
/// verifies the pair by CREATE2 against that factory.
pub const VENUE_UNIV2_POOL: u8 = 2;
/// Swap venue 3 — pool-direct Curve StableSwap plain pool (`data` = 20-byte
/// pool ‖ 1-byte i ‖ 1-byte j). Exact input only; the Executor verifies the
/// pool in Curve's MetaRegistry and the coin indices.
pub const VENUE_CURVE_POOL: u8 = 3;
/// Swap venue 4 — pool-direct Curve crypto pool (twocrypto-ng, tricrypto-ng,
/// the original `CurveCryptoSwap2`) (`data` = 20-byte pool ‖ 1-byte i ‖
/// 1-byte j), `exchange(uint256,uint256,uint256,uint256)`. Exact input only;
/// verified like venue 3.
pub const VENUE_CURVE_CRYPTO_POOL: u8 = 4;
/// Swap venue 5 — unwrap: redeem ERC-4626 shares (`tokenIn` is the vault)
/// for its `asset()` (`tokenOut`) (`data` = 20-byte vault). Exact input.
/// In a repay blob an unwrap leg converts the seized collateral before the
/// legs that sell it.
pub const VENUE_UNWRAP_4626: u8 = 5;
/// Swap venue 6 — unwrap: redeem an expired Pendle PT (`tokenIn`) through
/// its YT (`data` = 20-byte YT) for SY, then the SY for `tokenOut`. Exact
/// input. Placed like venue 5.
pub const VENUE_PENDLE_PT_REDEEM: u8 = 6;
/// UniV2 factory ids in venue-2 data.
pub const V2_FACTORY_UNISWAP: u8 = 0;
pub const V2_FACTORY_SUSHI: u8 = 1;

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
    /// Gearbox tail mode byte other than 0 (partial) / 1 (full); the
    /// Executor skips such a leg (`ST_TAIL`).
    #[error("gearbox tail mode {0} is not 0 or 1")]
    BadGearboxMode(u8),
    /// `PlanDecoder.TwoGovActions`: both governance flags on one plan.
    #[error("plan carries both a payload id and a spell")]
    TwoGovActions,
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
    /// Present iff `flags` has [`FLAG_GOV_EXEC`].
    pub payload_id: Option<u64>,
    /// Present iff `flags` has [`FLAG_GOV_SPELL`].
    pub spell: Option<Address>,
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

/// Fluid tail `kind`: normal collateral and debt.
pub const FLUID_T1: u8 = 1;
/// Fluid tail `kind`: smart collateral (DEX col shares), normal debt.
pub const FLUID_T2: u8 = 2;
/// Fluid tail `kind`: normal collateral, smart debt (DEX debt shares).
pub const FLUID_T3: u8 = 3;
/// Fluid tail `kind`: smart collateral and smart debt.
pub const FLUID_T4: u8 = 4;
/// Fluid tail flag: smart debt is repaid in token1 (else token0).
pub const FLUID_DEBT_TOKEN1: u8 = 1 << 0;
/// Fluid tail flag: smart collateral is withdrawn in token1 (else token0).
pub const FLUID_COL_TOKEN1: u8 = 1 << 1;
/// Fluid tail flag: `absorb_` — liquidate absorbed positions first.
pub const FLUID_ABSORB: u8 = 1 << 2;
/// Fluid tail flag: the repaid token is native ETH, paid from WETH.
pub const FLUID_NATIVE_DEBT: u8 = 1 << 3;
/// Fluid tail flag: the collateral token is native ETH, wrapped to WETH.
pub const FLUID_NATIVE_COL: u8 = 1 << 4;
/// Every defined Fluid flag bit.
pub const FLUID_FLAGS: u8 =
    FLUID_DEBT_TOKEN1 | FLUID_COL_TOKEN1 | FLUID_ABSORB | FLUID_NATIVE_DEBT | FLUID_NATIVE_COL;

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
    /// Euler V2 `liquidate(…, minYieldBalance)` — quoted yield shares, then
    /// the collateral vault. `LiqLeg.collateral_asset` stays the underlying.
    Euler { min_yield: U256, vault: Address },
    /// Liquity V2 `batchLiquidateTroves` — full uint256 trove id.
    Liquity { trove_id: U256 },
    /// Fluid vault liquidation (pin `9496626f`), every type: one debt token
    /// in, one collateral token out. `kind` is [`FLUID_T1`]..[`FLUID_T4`];
    /// `flags` are the `FLUID_*` bits. `col_per_unit_debt` is the vault's own
    /// `colPerUnitDebt_` (**1e18**, raw collateral units — tokens, or col
    /// shares on T2/T4 — per raw debt unit — tokens, or debt shares on
    /// T3/T4). `debt_shares_min_per_token`: T3/T4 min debt shares the exact
    /// token repay must burn, per debt token (1e18), zero otherwise.
    /// `col_per_share_min`: T2/T4 min collateral token per col share (1e18),
    /// zero otherwise.
    Fluid {
        kind: u8,
        flags: u8,
        col_per_unit_debt: U256,
        debt_shares_min_per_token: U256,
        col_per_share_min: U256,
    },
    /// Gearbox V3: minimum collateral received, then the path — `false`
    /// `partiallyLiquidateCreditAccount` (v3.1), `true` full
    /// `liquidateCreditAccount` with add/withdraw multicall. Wire byte 0 / 1.
    Gearbox { min_seized: U256, full: bool },
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

/// Big-endian `uint40`, as `uint40(bytes5(plan[o:o+5]))`.
#[inline]
fn payload_id_at(b: &[u8], o: usize) -> Result<u64> {
    let s = take(b, o, PAYLOAD_ID_LEN)?;
    let mut w = [0u8; 8];
    w.get_mut(8 - PAYLOAD_ID_LEN..)
        .ok_or(WireError::Overflow)?
        .copy_from_slice(s);
    Ok(u64::from_be_bytes(w))
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
        5 => Some(FlashProvider::None),
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
            vault: addr_at(b, add(tail_offset, 32)?)?,
        },
        ExecutorAdapter::LiquityV2 => LegTail::Liquity {
            trove_id: u256_at(b, tail_offset)?,
        },
        ExecutorAdapter::Fluid => LegTail::Fluid {
            kind: u8_at(b, tail_offset)?,
            flags: u8_at(b, add(tail_offset, 1)?)?,
            col_per_unit_debt: u256_at(b, add(tail_offset, 2)?)?,
            debt_shares_min_per_token: u256_at(b, add(tail_offset, 34)?)?,
            col_per_share_min: u256_at(b, add(tail_offset, 66)?)?,
        },
        ExecutorAdapter::Gearbox => LegTail::Gearbox {
            min_seized: u256_at(b, tail_offset)?,
            full: match u8_at(b, add(tail_offset, 32)?)? {
                0 => false,
                1 => true,
                m => return Err(WireError::BadGearboxMode(m)),
            },
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
        let flags = u8_at(bytes, 0)?;
        let (mut payload_id, mut spell) = (None, None);
        if flags & FLAG_GOV_EXEC != 0 {
            if flags & FLAG_GOV_SPELL != 0 {
                return Err(WireError::TwoGovActions);
            }
            payload_id = Some(payload_id_at(bytes, cur)?);
            cur = add(cur, PAYLOAD_ID_LEN)?;
        } else if flags & FLAG_GOV_SPELL != 0 {
            spell = Some(addr_at(bytes, cur)?);
            cur = add(cur, SPELL_LEN)?;
        }
        if cur != bytes.len() {
            return Err(WireError::BadPlanLength {
                walked: cur,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            bytes,
            header: Header {
                flags,
                bid_bps: u16_at(bytes, 1)?,
                gas_cost_wei: u128_at(bytes, 3)?,
                min_profit: u128_at(bytes, 19)?,
                group_count,
                profit_swap_offset,
                payload_id,
                spell,
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
            FlashProvider::None,
        ] {
            assert_eq!(provider_from_wire(p as u8), Some(p));
        }
        assert_eq!(provider_from_wire(6), None);
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
        assert_eq!(tail_len(3).unwrap(), 52);
        assert_eq!(tail_len(4).unwrap(), 0);
        assert_eq!(tail_len(5).unwrap(), 32);
        assert_eq!(tail_len(6).unwrap(), 98);
        assert_eq!(tail_len(7).unwrap(), 33);
        assert_eq!(tail_len(8).unwrap(), 21);
        assert_eq!(tail_len(9), Err(WireError::UnknownAdapter(9)));
    }

    /// Oracle: `PlanDecoder.sol` `TAIL_FLUID` layout and `FLUID_*` bits.
    #[test]
    fn fluid_tail_matches_plan_decoder_sol() {
        assert_eq!(
            (
                FLUID_DEBT_TOKEN1,
                FLUID_COL_TOKEN1,
                FLUID_ABSORB,
                FLUID_NATIVE_DEBT,
                FLUID_NATIVE_COL
            ),
            (1, 2, 4, 8, 16)
        );
        assert_eq!((FLUID_T1, FLUID_T2, FLUID_T3, FLUID_T4), (1, 2, 3, 4));
        let mut b = vec![0u8; LIQ_LEG_LEN + 98];
        b[0] = 6; // Fluid
        b[LIQ_LEG_LEN] = FLUID_T4;
        b[LIQ_LEG_LEN + 1] = FLUID_DEBT_TOKEN1 | FLUID_ABSORB;
        b[LIQ_LEG_LEN + 33] = 3;
        b[LIQ_LEG_LEN + 65] = 5;
        b[LIQ_LEG_LEN + 97] = 7;
        let (leg, next) = decode_liq_leg(&b, 0).unwrap();
        assert_eq!(
            leg.tail,
            LegTail::Fluid {
                kind: FLUID_T4,
                flags: FLUID_DEBT_TOKEN1 | FLUID_ABSORB,
                col_per_unit_debt: U256::from(3u8),
                debt_shares_min_per_token: U256::from(5u8),
                col_per_share_min: U256::from(7u8),
            }
        );
        assert_eq!(next, LIQ_LEG_LEN + 98);
    }

    #[test]
    fn decode_10e_tails() {
        let mut b = vec![0u8; LIQ_LEG_LEN + 52];
        b[0] = 3; // Euler
        b[LIQ_LEG_LEN + 31] = 7;
        b[LIQ_LEG_LEN + 51] = 0xAB;
        let (leg, next) = decode_liq_leg(&b, 0).unwrap();
        assert_eq!(leg.adapter, ExecutorAdapter::EulerV2);
        let mut vault = [0u8; 20];
        vault[19] = 0xAB;
        assert_eq!(
            leg.tail,
            LegTail::Euler {
                min_yield: U256::from(7u64),
                vault: Address::from(vault),
            }
        );
        assert_eq!(next, LIQ_LEG_LEN + 52);

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
