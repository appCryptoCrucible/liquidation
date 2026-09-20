//! Live RPC. Every read is an `eth_call` / `eth_chainId` against the node.
//! There is no cache and no fabricated return.

use crate::error::ConfigError;
use crate::Result;
use alloy_primitives::{Address, Bytes};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};

/// The only chain surface `Validate` / the boot assertion need.
#[allow(async_fn_in_trait)] // boot-only; awaited in place, never spawned
pub trait ChainRpc: Send {
    /// `eth_chainId`. Transport failure → [`ConfigError::RpcUnavailable`].
    async fn chain_id(&self) -> Result<u64>;

    /// `eth_call` to `to` with calldata `data`. Transport failure →
    /// [`ConfigError::RpcUnavailable`]. A revert is [`ConfigError::CallFailed`].
    async fn call(&self, to: Address, data: Bytes) -> Result<Bytes>;
}

/// HTTP JSON-RPC implementor. Constructed from the operator's `rpc_url`.
#[derive(Clone, Debug)]
pub struct HttpRpc {
    provider: RootProvider,
}

impl HttpRpc {
    /// Parse `rpc_url` and build a provider. Does not hit the network yet;
    /// the first [`ChainRpc`] method does. Empty URL fails closed.
    pub fn connect(rpc_url: &str) -> Result<Self> {
        if rpc_url.is_empty() {
            return Err(ConfigError::RpcUnavailable);
        }
        let url = rpc_url.parse().map_err(|_| ConfigError::RpcUnavailable)?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(url);
        Ok(Self { provider })
    }
}

impl ChainRpc for HttpRpc {
    async fn chain_id(&self) -> Result<u64> {
        self.provider
            .get_chain_id()
            .await
            .map_err(|_| ConfigError::RpcUnavailable)
    }

    async fn call(&self, to: Address, data: Bytes) -> Result<Bytes> {
        let tx = TransactionRequest {
            to: Some(to.into()),
            input: TransactionInput::new(data),
            ..Default::default()
        };
        match self.provider.call(tx).await {
            Ok(bytes) => {
                if bytes.is_empty() {
                    Err(ConfigError::CallFailed {
                        address: to,
                        what: "empty eth_call return",
                    })
                } else {
                    Ok(bytes)
                }
            }
            Err(_) => Err(ConfigError::RpcUnavailable),
        }
    }
}
