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
//! Submitters, nonce allocation and inclusion watch arrive with WP 13A.

#![forbid(unsafe_code)]

pub mod executor;
pub mod wire;
