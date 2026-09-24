//! Packed liquidation-plan wire format: the pure encode/decode layer shared
//! by `liq-plan` (encoder), `liq-exec` (on-chain submission, decoder), and
//! `liq-router` (assembles [`wire::LegTail`] before handing a leg to
//! `liq-plan`).
//!
//! Split out of `liq-exec` so that a crate which only needs to speak the
//! wire format — never to submit a transaction — does not have to pull in
//! `liq-exec`'s RPC/tokio machinery. `liq-exec` re-exports this module as
//! `liq_exec::wire`, so existing `liq_exec::wire::X` call sites are
//! unaffected.

#![forbid(unsafe_code)]
// Matches the relaxation `liq-exec` gave this module's own tests before the
// move; the wire fixtures build raw byte buffers by index on purpose.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

pub mod wire;
