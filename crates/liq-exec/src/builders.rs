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

/// One curated builder. `endpoint` is leaked once at load. `warm` marks a
/// builder whose pooled connection the exec path re-warms on a timer
/// (`ExecPath::spawn_rewarm`), so a bundle there never pays a TCP + TLS
/// handshake. The MEV-Share relay is always re-warmed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BuilderEndpoint {
    pub id: BuilderId,
    pub name: &'static str,
    pub endpoint: &'static str,
    pub warm: bool,
}

/// Loaded builder fan-out + MEV-Share relay.
#[derive(Clone, Debug)]
pub struct BuilderSet {
    pub builders: Vec<BuilderEndpoint>,
    pub mevshare_relay: &'static str,
    /// `privacy.builders` on every `mev_sendBundle` (`[mevshare].builders`).
    /// Registry names, lowercase. Empty: internal builders only.
    pub mevshare_builders: &'static [&'static str],
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
    /// `warm = true` in `builders.toml`; absent means false.
    #[serde(default)]
    warm: bool,
}

#[derive(Deserialize)]
struct MevShareRow {
    relay: String,
    /// Absent means none named (internal builders only).
    #[serde(default)]
    builders: Vec<String>,
}

/// Check and leak `[mevshare].builders`. mev-share-node lowercases names on
/// both sides, so a mixed-case or repeated name is a config typo: refuse it
/// rather than send something the node would read differently.
fn leak_mevshare_builders(names: Vec<String>) -> Result<&'static [&'static str]> {
    let mut out: Vec<&'static str> = Vec::with_capacity(names.len());
    for name in names {
        let ok = !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b".-_".contains(&c));
        if !ok {
            return Err(ExecError::Config(format!(
                "mevshare.builders: {name:?} is not a lowercase registry name"
            )));
        }
        if out.contains(&name.as_str()) {
            return Err(ExecError::Config(format!(
                "mevshare.builders: {name:?} listed twice"
            )));
        }
        out.push(leak_str(name));
    }
    Ok(Box::leak(out.into_boxed_slice()))
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
            warm: row.warm,
        });
    }
    let mevshare_builders = leak_mevshare_builders(file.mevshare.builders)?;
    Ok(BuilderSet {
        builders,
        mevshare_relay: leak_str(file.mevshare.relay),
        mevshare_builders,
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
            mevshare_builders: &[],
        })
    }

    /// Name the `privacy.builders` for MEV-Share bundles (tests; the loader
    /// reads them from `[mevshare].builders`).
    #[must_use]
    pub fn with_mevshare_builders(mut self, names: &'static [&'static str]) -> Self {
        self.mevshare_builders = names;
        self
    }

    /// Endpoints kept warm: every builder flagged `warm`, then the MEV-Share
    /// relay. Each URL once, in that order (a relay that is also a flagged
    /// builder is one socket, warmed once).
    #[must_use]
    pub fn warm_targets(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::with_capacity(self.builders.len().saturating_add(1));
        let flagged = self.builders.iter().filter(|b| b.warm).map(|b| b.endpoint);
        for url in flagged.chain(std::iter::once(self.mevshare_relay)) {
            if !out.contains(&url) {
                out.push(url);
            }
        }
        out
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
        assert_eq!(set.builders.len(), 3, "BuilderNet EU, Titan EU and Quasar");
        assert_eq!(set.mevshare_relay, "https://relay.flashbots.net");
        let endpoint = |name: &str| {
            set.builders
                .iter()
                .find(|b| b.name == name)
                .map(|b| b.endpoint)
        };
        assert_eq!(endpoint("beaverbuild"), None, "beaverbuild runs BuilderNet");
        assert_eq!(endpoint("rsync"), None, "rsync-builder.xyz has no address");
        assert_eq!(endpoint("quasar"), Some("https://rpc.quasar.win"));
        assert_eq!(set.mevshare_builders, ["flashbots", "titan", "quasar"]);
        assert_eq!(endpoint("titan-us"), None, "the box is in the EU");
        assert_eq!(
            endpoint("titan-eu"),
            Some("https://eu.rpc.titanbuilder.xyz")
        );
        assert_eq!(endpoint("flashbots"), None, "the relay feeds BuilderNet");
        assert_eq!(endpoint("buildernet-us"), None, "the box is in the EU");
        assert_eq!(
            endpoint("buildernet-eu"),
            Some("https://direct-eu.buildernet.org")
        );
        let urls: Vec<_> = set.builders.iter().map(|b| b.endpoint).collect();
        assert!(
            !urls.contains(&"https://rpc.titanbuilder.xyz"),
            "Titan's geo URL is documented to misroute"
        );
        assert!(!urls
            .iter()
            .any(|u| u.contains("ap.rpc") || u.contains("direct-ap")));
        assert_eq!(
            PLANNED_EXECUTOR.to_string().to_ascii_lowercase(),
            "0xe0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0"
        );
        // Warm set (2026-10-10): BuilderNet, Titan and the MEV-Share relay.
        assert!(set.builders.iter().all(|b| b.warm), "every builder is warm");
        assert_eq!(
            set.warm_targets(),
            [
                "https://eu.rpc.titanbuilder.xyz",
                "https://direct-eu.buildernet.org",
                "https://rpc.quasar.win",
                "https://relay.flashbots.net",
            ]
        );
    }

    #[test]
    fn mevshare_builders_must_be_lowercase_and_unique() {
        let base = |list: &str| {
            format!(
                "[[builders]]\nid = 1\nname = \"a\"\nendpoint = \"https://a.example\"\n\n\
                 [mevshare]\nrelay = \"https://relay.example\"\n{list}"
            )
        };
        let load = |text: String| {
            let dir =
                std::env::temp_dir().join(format!("liq-msb-{}-{}", std::process::id(), text.len()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("builders.toml");
            std::fs::write(&path, text).unwrap();
            let r = load_builders(&path);
            let _ = std::fs::remove_dir_all(&dir);
            r
        };
        assert!(load(base("")).unwrap().mevshare_builders.is_empty());
        assert_eq!(
            load(base("builders = [\"flashbots\", \"beaverbuild.org\"]\n"))
                .unwrap()
                .mevshare_builders,
            ["flashbots", "beaverbuild.org"]
        );
        assert!(
            load(base("builders = [\"Titan\"]\n")).is_err(),
            "mixed case"
        );
        assert!(
            load(base("builders = [\"titan\", \"titan\"]\n")).is_err(),
            "twice"
        );
        assert!(load(base("builders = [\"\"]\n")).is_err(), "empty name");
    }

    #[test]
    fn warm_defaults_false_and_targets_name_each_socket_once() {
        let text = r#"
[[builders]]
id = 1
name = "a"
endpoint = "https://a.example"

[[builders]]
id = 2
name = "b"
endpoint = "https://b.example"
warm = true

[[builders]]
id = 3
name = "relay-as-builder"
endpoint = "https://relay.example"
warm = true

[mevshare]
relay = "https://relay.example"
"#;
        let dir = std::env::temp_dir().join(format!("liq-builders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("builders.toml");
        std::fs::write(&path, text).unwrap();
        let set = load_builders(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let by_name = |n: &str| set.builders.iter().find(|b| b.name == n).unwrap().warm;
        assert!(!by_name("a"), "no `warm` key means not warmed");
        assert!(by_name("b"));
        assert_eq!(
            set.warm_targets(),
            ["https://b.example", "https://relay.example"],
            "the relay URL is one socket even when it is also a flagged builder"
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
