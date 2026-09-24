//! Live RPC bridge for the per-adapter boot-time assertions that
//! `Config::new` refuses without: Liquity V2 `assert_live_registry`,
//! Fluid `assert_live_factory`, Gearbox `assert_live_registry`, Compound V2
//! `assert_live_registry`. Aave V3, Aave V4, Morpho Blue, Euler V2 and Silo
//! V2 carry no such gate — their `Config::new` only shape-validates the
//! committed TOML, so they need nothing here.
//!
//! Each adapter's `RegistryRpc` / `FactoryRpc` trait is one synchronous
//! method, `eth_call(to, data, block) -> Result<Bytes, ConfigError>`. This
//! bridges it onto [`HttpRpc`] (already used for the boot-time token
//! registry assertion in `liq_config::boot`), one dedicated OS thread and a
//! throwaway current-thread runtime per call. That is only correct because
//! this runs a few dozen times at startup and never on the hot path — it
//! deliberately avoids `Handle::block_on`, which would panic if called from
//! a thread that already has a tokio context entered (exactly the case here:
//! `load_protocols` runs inline inside `startup.rs`'s async body).

use alloy_primitives::{Address, Bytes};
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_protocol::BlockNum;

/// Bridges [`HttpRpc`] to every gated adapter's synchronous RPC trait.
#[derive(Clone)]
pub struct LiveRpc {
    inner: HttpRpc,
}

impl LiveRpc {
    #[must_use]
    pub const fn new(inner: HttpRpc) -> Self {
        Self { inner }
    }

    /// Blocking `eth_call` at a pinned block, off the caller's runtime.
    /// `None` on any transport failure, decode failure, or thread panic —
    /// callers turn that into their own `ConfigError` variant so the
    /// address that failed is never lost.
    fn call_blocking(&self, to: Address, data: &[u8], block: BlockNum) -> Option<Bytes> {
        let inner = self.inner.clone();
        let data = Bytes::copy_from_slice(data);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(inner.call_at(to, data, block)).ok()
        })
        .join()
        .ok()
        .flatten()
    }
}

impl liq_adapters_liquity_v2::RegistryRpc for LiveRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, liq_adapters_liquity_v2::ConfigError> {
        self.call_blocking(to, data, block)
            .ok_or(liq_adapters_liquity_v2::ConfigError::RegistryCall(to))
    }
}

impl liq_adapters_fluid::FactoryRpc for LiveRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, liq_adapters_fluid::ConfigError> {
        self.call_blocking(to, data, block)
            .ok_or(liq_adapters_fluid::ConfigError::FactoryCall(to))
    }
}

impl liq_adapters_gearbox::RegistryRpc for LiveRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, liq_adapters_gearbox::ConfigError> {
        self.call_blocking(to, data, block)
            .ok_or(liq_adapters_gearbox::ConfigError::RegistryCall(to))
    }
}

impl liq_adapters_compound_v2::RegistryRpc for LiveRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, liq_adapters_compound_v2::ConfigError> {
        self.call_blocking(to, data, block)
            .ok_or(liq_adapters_compound_v2::ConfigError::RegistryCall(to))
    }
}
