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
pub mod assemble_view;
pub mod bands;
pub mod bind;
pub mod crossing;
pub mod drain;
pub mod exec_bind;
pub mod exec_worker;
pub mod exex_install;
pub mod gas_model;
pub mod inclusion_feed;
pub mod index;
pub mod lease;
pub mod live_rpc;
pub mod pool_seed;
pub mod rebuild;
pub mod reload;
pub mod routes;
pub mod shared;
pub mod stall;
pub mod startup;
pub mod threads;
