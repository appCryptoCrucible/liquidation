//! Curated builder set from `config/builders.toml` (GUIDE 13 §1).
//!
//! Endpoints stay `&'static str` after a one-time leak (00C). A `Url` is
//! parsed only at the POST boundary (`reqwest`). No public-RPC submit URL
//! and no public-mempool broadcast.

use crate::error::{ExecError, Result};
use alloy_primitives::{address, Address};
use liq_types::BuilderId;
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// Same documented pre-H3 insertion address as `liq_sim::PLANNED_EXECUTOR`.
/// H3 has not deployed; this is not a mainnet claim.
pub const PLANNED_EXECUTOR: Address = address!("e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0");

/// One curated builder. `endpoint` is leaked once at load.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BuilderEndpoint {
    pub id: BuilderId,
    pub name: &'static str,
    pub endpoint: &'static str,
}

/// Loaded builder fan-out + MEV-Share relay.
#[derive(Clone, Debug)]
pub struct BuilderSet {
    pub builders: Vec<BuilderEndpoint>,
    pub mevshare_relay: &'static str,
}

#[derive(Deserialize)]
struct File {
    builders: Vec<Row>,
    mevshare: MevShareRow,
}

#[derive(Deserialize)]
struct Row {
    id: u16,
    name: String,
    endpoint: String,
}

#[derive(Deserialize)]
struct MevShareRow {
    relay: String,
}

/// Leak a config string once (00C / 06B `leak_endpoint`).
#[must_use]
pub fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Reject public-RPC / sendRaw / sendPrivate URLs (D19, D54).
pub fn reject_public_rpc(url: &str) -> Result<()> {
    let l = url.to_ascii_lowercase();
    const FORBIDDEN: &[&str] = &[
        "infura.io",
        "alchemy.com",
        "llamarpc.com",
        "publicnode.com",
        "cloudflare-eth.com",
        "eth_sendraw",
        "sendprivatetransaction",
        "sendrawtransaction",
    ];
    for needle in FORBIDDEN {
        if l.contains(needle) {
            tracing::error!(url, needle, "refusing public-RPC / mempool submit URL");
            return Err(ExecError::PublicRpcForbidden);
        }
    }
    Ok(())
}

/// Load `config/builders.toml` and leak every endpoint.
pub fn load_builders(path: &Path) -> Result<BuilderSet> {
    let text = fs::read_to_string(path).map_err(|e| ExecError::Config(e.to_string()))?;
    let file: File = toml::from_str(&text).map_err(|e| ExecError::Config(e.to_string()))?;
    if file.builders.is_empty() {
        return Err(ExecError::EmptyBuilders);
    }
    reject_public_rpc(&file.mevshare.relay)?;
    let mut builders = Vec::with_capacity(file.builders.len());
    for row in file.builders {
        reject_public_rpc(&row.endpoint)?;
        builders.push(BuilderEndpoint {
            id: BuilderId(row.id),
            name: leak_str(row.name),
            endpoint: leak_str(row.endpoint),
        });
    }
    Ok(BuilderSet {
        builders,
        mevshare_relay: leak_str(file.mevshare.relay),
    })
}

impl BuilderSet {
    /// In-memory set for tests (mock HTTP). Still rejects forbidden URLs.
    pub fn from_parts(
        builders: Vec<BuilderEndpoint>,
        mevshare_relay: &'static str,
    ) -> Result<Self> {
        if builders.is_empty() {
            return Err(ExecError::EmptyBuilders);
        }
        reject_public_rpc(mevshare_relay)?;
        for b in &builders {
            reject_public_rpc(b.endpoint)?;
        }
        Ok(Self {
            builders,
            mevshare_relay,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn shipped_toml_loads_and_leaks() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml");
        let set = load_builders(&path).expect("shipped builders.toml");
        assert_eq!(set.builders.len(), 7);
        assert_eq!(set.mevshare_relay, "https://relay.flashbots.net");
        let endpoint = |name: &str| {
            set.builders
                .iter()
                .find(|b| b.name == name)
                .map(|b| b.endpoint)
        };
        assert_eq!(endpoint("beaverbuild"), Some("https://rpc.beaverbuild.org/"));
        assert_eq!(endpoint("rsync"), Some("https://rsync-builder.xyz"));
        assert_eq!(
            endpoint("titan-us"),
            Some("https://us.rpc.titanbuilder.xyz")
        );
        assert_eq!(
            endpoint("titan-eu"),
            Some("https://eu.rpc.titanbuilder.xyz")
        );
        assert_eq!(endpoint("flashbots"), Some("https://relay.flashbots.net"));
        assert_eq!(
            endpoint("buildernet-us"),
            Some("https://direct-us.buildernet.org")
        );
        assert_eq!(
            endpoint("buildernet-eu"),
            Some("https://direct-eu.buildernet.org")
        );
        let urls: Vec<_> = set.builders.iter().map(|b| b.endpoint).collect();
        assert!(
            !urls.contains(&"https://rpc.titanbuilder.xyz"),
            "Titan's geo URL is documented to misroute"
        );
        assert!(!urls.iter().any(|u| u.contains("ap.rpc") || u.contains("direct-ap")));
        assert_eq!(
            PLANNED_EXECUTOR.to_string().to_ascii_lowercase(),
            "0xe0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0"
        );
    }

    #[test]
    fn public_rpc_url_is_refused() {
        assert!(matches!(
            reject_public_rpc("https://mainnet.infura.io/v3/x"),
            Err(ExecError::PublicRpcForbidden)
        ));
        assert!(matches!(
            reject_public_rpc("http://127.0.0.1:9/sendrawtransaction"),
            Err(ExecError::PublicRpcForbidden)
        ));
        assert!(reject_public_rpc("http://127.0.0.1:9/").is_ok());
    }
}
