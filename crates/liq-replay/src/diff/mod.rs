//! WP 05A — differential fuzz: Foundry-generated state vs Aave V4 `health()`.
//!
//! The generator lives in `contracts/src/differential/`. This module rebuilds
//! a [`PositionRef`] from the same ABI blob, runs the 04A adapter, and returns
//! the fields the on-chain `HealthOracle` (transcribed from
//! `Spoke._processUserAccountData` @ `40232a0a`) produced. A mismatch is a
//! bug in 04A, not in the generator.
//!
//! Carry-forwards: conformance check 4 is vacuous on published-parameter
//! fixtures — this harness is the fork/EVM path. 04A `alloc_meter()` is
//! `None` until 16A; this WP does not invent a substitute counter.

mod abi;
mod position;

pub use abi::{decode_case, encode_view, DiffCase};
pub use position::{adapter_view, AdapterView, CaseError};

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::decode_case;

    /// Oracle: ABI decoder. Empty input is not a DiffCase.
    #[test]
    fn decode_empty_is_error() {
        assert!(decode_case(b"").is_err());
        assert!(decode_case(b"0x").is_err());
        assert!(decode_case(b"zz").is_err());
    }

    /// 04A AllocMeter is None until 16A — documented, not faked.
    #[test]
    fn alloc_meter_unmetered_until_16a() {
        assert!(
            liq_adapters_aave_v4::alloc_meter().is_none(),
            "oracle: 04A seam returns None until 16A PanicOnAlloc"
        );
    }
}
