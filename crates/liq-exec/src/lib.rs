//! Executor-side wire format and call encoding (WP 10A).
//!
//! * [`wire`] — the packed-plan **decoder**, a line-for-line mirror of
//!   `contracts/src/lib/PlanDecoder.sol`. `liq-plan` (WP 10B) encodes; this
//!   decodes; both are checked against the Solidity decoder on one committed
//!   fixture (`contracts/test/fixtures/plan_v1.hex`), and 10B's proptest
//!   randomises that.
//! * [`executor`] — `Executor.execute(bytes)` / `sweep(address[])` ABI, the
//!   custom errors, constructor arguments and the mainnet constants a deploy
//!   or fork harness needs.
//!
//! Submitters (WP 13A): MevShare + BuilderBundle, nonce, inclusion watch,
//! templates, fee fields. Live HTTP is `submit_enabled` (default false).
//! `wire` / `executor` stay 10A/10E-owned.

#![forbid(unsafe_code)]
#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::float_arithmetic
    )
)]

pub mod builders;
pub mod error;
pub mod executor;
pub mod fee;
pub mod inclusion;
pub mod nonce;
pub mod path;
pub mod submit;
pub mod template;
pub mod wire;
