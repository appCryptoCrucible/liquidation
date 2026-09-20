//! Top-level binary: startup wiring, thread pinning, lease, hot-reload, ExEx registration.

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
#![cfg_attr(not(feature = "alloc-assert"), forbid(unsafe_code))]

pub mod alloc;
pub mod threads;
