//! Sky DSS Flash â€” DAI only (GUIDE 07 Â§3a, D09).
//!
//! `available = max` (governance ceiling, wad) if `asset` is DAI and Vat is
//! live; **0 for every non-DAI asset**. `flashFee` is 0 on the current
//! deployment (`src/flash.sol` returns 0 and has no `toll`; `toll()` reverts
//! on `MCD_FLASH` at block 26M). `File("toll")` is decoded only so a future
//! deployment that reintroduces it does not require a rebuild.
//!
//! **Cage is observed at `End`, not `Vat`.** `flashLoan` requires
//! `vat.live() == 1`, but the Vat emits no events at all (its bytecode has
//! no `Cage()` topic0). Emergency Shutdown runs `End.cage()` â†’ `Vat.cage()`,
//! and `End` emits `Cage()`. Subscribe there (`MCD_END` via Chainlog).
//!
//! D08/D09: the fifth arena is Sky DSS, not a vault flash-loan callback.

use alloy_primitives::{b256, Address, B256, U256};
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::fixed::{mul_div, Rounding, WAD};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber};

use crate::{FlashSource, GAS_OVERHEAD_STUB};

sol! {
    interface IDssFlash {
        event File(bytes32 indexed what, uint256 data);
    }
    interface IEnd {
        event Cage();
    }
}

/// Solidity `bytes32("max")` / `bytes32("toll")`.
const WHAT_MAX: B256 = b256!("0x6d61780000000000000000000000000000000000000000000000000000000000");
const WHAT_TOLL: B256 = b256!("0x746f6c6c00000000000000000000000000000000000000000000000000000000");

/// Sky DSS Flash module.
pub struct SkyDssFlash {
    flash: Address,
    /// `MCD_END`. Emits `Cage()` when Emergency Shutdown sets `Vat.live = 0`.
    end: Address,
    dai: AssetId,
    max: U256,
    /// ERC-3156 `toll` [wad]. Current deployment hardcodes `flashFee = 0`.
    toll: U256,
    live: bool,
    overhead: u64,
}

impl SkyDssFlash {
    #[must_use]
    pub fn new(
        flash: Address,
        end: Address,
        dai: AssetId,
        max: U256,
        toll: U256,
        live: bool,
    ) -> Self {
        Self {
            flash,
            end,
            dai,
            max,
            toll,
            live,
            overhead: GAS_OVERHEAD_STUB,
        }
    }

    #[must_use]
    pub fn with_overhead(mut self, gas: u64) -> Self {
        self.overhead = gas;
        self
    }

    fn bps_from_toll(&self) -> u16 {
        if self.toll.is_zero() {
            return 0;
        }
        const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);
        const U16M: U256 = U256::from_limbs([u16::MAX as u64, 0, 0, 0]);
        match mul_div(self.toll, BPS, WAD, Rounding::Up) {
            Ok(v) if v <= U16M => {
                u16::try_from(v.as_limbs().first().copied().unwrap_or(0)).unwrap_or(u16::MAX)
            }
            Ok(_) | Err(_) => u16::MAX,
        }
    }
}

impl LogSubscriber for SkyDssFlash {
    fn subscriptions(&self) -> Vec<LogFilter> {
        vec![
            LogFilter {
                address: self.flash,
                topic0: IDssFlash::File::SIGNATURE_HASH,
            },
            LogFilter {
                address: self.end,
                topic0: IEnd::Cage::SIGNATURE_HASH,
            },
        ]
    }
}

impl FlashSource for SkyDssFlash {
    #[inline]
    fn provider(&self) -> FlashProvider {
        FlashProvider::SkyDss
    }

    #[inline]
    fn source(&self) -> Address {
        self.flash
    }

    #[inline]
    fn available(&self, asset: AssetId) -> U256 {
        if asset != self.dai || !self.live {
            U256::ZERO
        } else {
            self.max
        }
    }

    #[inline]
    fn fee_bps(&self, asset: AssetId, _amount: U256) -> u16 {
        if asset != self.dai {
            return 0;
        }
        self.bps_from_toll()
    }

    #[inline]
    fn callback(&self) -> CallbackShape {
        CallbackShape::SkyDssOnFlashLoan
    }

    #[inline]
    fn gas_overhead(&self) -> u64 {
        self.overhead
    }

    fn apply_log(&mut self, log: &DecodedLog<'_>) {
        let Some(&t0) = log.topics.first() else {
            return;
        };
        if log.address == self.end && t0 == IEnd::Cage::SIGNATURE_HASH {
            self.live = false;
            return;
        }
        if log.address != self.flash || t0 != IDssFlash::File::SIGNATURE_HASH {
            return;
        }
        let Ok(ev) = IDssFlash::File::decode_raw_log(log.topics.iter().copied(), log.data) else {
            return;
        };
        if ev.what == WHAT_MAX {
            self.max = ev.data;
        } else if ev.what == WHAT_TOLL {
            self.toll = ev.data;
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sources::fixtures::*;
    use alloy_primitives::U256;
    use liq_types::LogSubscriber;

    fn dss() -> SkyDssFlash {
        SkyDssFlash::new(DSS_FLASH, END, ID_DAI, u256(DSS_MAX_26M), U256::ZERO, true)
    }

    #[test]
    fn identities() {
        let s = dss();
        assert_eq!(s.provider(), FlashProvider::SkyDss);
        assert_eq!(s.source(), DSS_FLASH, "oracle: D15 sky-maker dss_flash");
        assert_eq!(s.callback(), CallbackShape::SkyDssOnFlashLoan);
        assert_eq!(s.gas_overhead(), 0);
        assert_eq!(
            s.fee_bps(ID_DAI, u256("1000000000000000000")),
            0,
            "oracle: eth_call flashFee(DAI,1e18)=0 at 26M"
        );
    }

    /// Oracle: eth_call `max()` at block 26_000_000 = 500e24; non-DAI is 0
    /// by GUIDE-07 Â§3 (DAI only).
    #[test]
    fn dai_only_max_at_26m() {
        let s = dss();
        assert_eq!(s.available(ID_DAI), u256(DSS_MAX_26M));
        assert_eq!(s.available(ID_USDC), U256::ZERO);
        assert_eq!(s.available(ID_WETH), U256::ZERO);
        assert_eq!(s.fee_bps(ID_USDC, U256::MAX), 0);
    }

    #[test]
    fn file_from_wrong_address_is_noop() {
        let mut s = dss();
        let topics = [T0_FILE, WHAT_MAX];
        let data = u256_be(U256::from(1u64));
        s.apply_log(&log(DAI, &topics, &data));
        assert_eq!(s.available(ID_DAI), u256(DSS_MAX_26M));
    }

    #[test]
    fn file_max_updates_ceiling() {
        let mut s = dss();
        let new_max = u256("1000000000000000000");
        let topics = [T0_FILE, WHAT_MAX];
        let data = u256_be(new_max);
        s.apply_log(&log(DSS_FLASH, &topics, &data));
        assert_eq!(s.available(ID_DAI), new_max);
        assert_eq!(s.available(ID_USDC), U256::ZERO);
    }

    /// Oracle: `End.cage()` â†’ `Vat.cage()`; `flashLoan` requires
    /// `vat.live() == 1`. `Cage()` is emitted by `MCD_END`.
    #[test]
    fn end_cage_zeros_available() {
        let mut s = dss();
        let topics = [T0_CAGE];
        s.apply_log(&log(END, &topics, &[]));
        assert_eq!(s.available(ID_DAI), U256::ZERO);
        assert_eq!(s.available(ID_USDC), U256::ZERO);
    }

    /// Negative: a `Cage()` at any other address (the Vat, which emits no
    /// events; or a spoofing contract) must not cage the source.
    #[test]
    fn cage_from_wrong_address_is_noop() {
        let mut s = dss();
        let topics = [T0_CAGE];
        s.apply_log(&log(VAT, &topics, &[]));
        s.apply_log(&log(DSS_FLASH, &topics, &[]));
        assert_eq!(s.available(ID_DAI), u256(DSS_MAX_26M));
    }

    /// Oracle: `eth_getCode(MCD_VAT)` at 26M contains no `Cage()` topic0;
    /// `eth_getCode(MCD_END)` does. A Vat subscription would never fire.
    #[test]
    fn subscriptions_file_and_end_cage_not_vat() {
        let s = dss();
        let subs = s.subscriptions();
        assert_eq!(subs.len(), 2);
        assert!(subs
            .iter()
            .any(|f| f.address == DSS_FLASH && f.topic0 == T0_FILE));
        assert!(subs.iter().any(|f| f.address == END && f.topic0 == T0_CAGE));
        assert!(
            !subs.iter().any(|f| f.address == VAT),
            "Vat emits no events â€” subscribing there is a dead filter"
        );
        assert_eq!(IDssFlash::File::SIGNATURE_HASH, T0_FILE);
        assert_eq!(IEnd::Cage::SIGNATURE_HASH, T0_CAGE);
    }
}
