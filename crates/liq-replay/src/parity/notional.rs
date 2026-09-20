//! Input sizes biased to notionals this bot actually trades (GUIDE 05 §6b).
//! Uniform U256 sampling is a generator bug: we never quote 2^200 wei.

use alloy_primitives::{uint, U256};

/// 0.1 ether (18-dec). Floor of a typical seized-collateral hop.
pub const TRADED_WAD_MIN: U256 = uint!(100_000_000_000_000_000_U256);
/// 50 ether. Above this, a single hop is split; still in-band for stress.
pub const TRADED_WAD_MAX: U256 = uint!(50_000_000_000_000_000_000_U256);

/// Rungs used in production size ladders (0.1, 0.5, 1, 2, 5, 10, 20, 50 WAD).
pub const TRADED_LADDER_WAD: [U256; 8] = [
    uint!(100_000_000_000_000_000_U256),
    uint!(500_000_000_000_000_000_U256),
    uint!(1_000_000_000_000_000_000_U256),
    uint!(2_000_000_000_000_000_000_U256),
    uint!(5_000_000_000_000_000_000_U256),
    uint!(10_000_000_000_000_000_000_U256),
    uint!(20_000_000_000_000_000_000_U256),
    uint!(50_000_000_000_000_000_000_U256),
];

#[must_use]
pub fn in_traded_band(x: U256) -> bool {
    x >= TRADED_WAD_MIN && x <= TRADED_WAD_MAX
}

/// Deterministic amount at `index`, concentrated on [`TRADED_LADDER_WAD`].
/// Jitter is ±(`index % 997`) wei so we hit rounding edges, not a second
/// distribution.
#[must_use]
pub fn biased_wad(index: u32) -> U256 {
    let want = index & 7;
    let mut k = 0u32;
    let mut rung = TRADED_WAD_MIN;
    for &r in &TRADED_LADDER_WAD {
        if k == want {
            rung = r;
            break;
        }
        k = k.saturating_add(1);
    }
    let j = U256::from(index.checked_rem(997).unwrap_or(0));
    if index.is_multiple_of(2) {
        rung.saturating_add(j)
    } else {
        rung.saturating_sub(j).max(TRADED_WAD_MIN)
    }
}
