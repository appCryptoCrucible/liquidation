//! Boot assertion (REGISTRY.md §4c). Re-reads `decimals`/`symbol` for every
//! token and `token0`/`token1`/`fee` for every pool from chain. Any mismatch
//! or RPC failure refuses to start.

use crate::error::ConfigError;
use crate::registry::{PoolVenue, Registry};
use crate::rpc::ChainRpc;
use crate::validate::Validate;
use crate::Result;
use alloy_primitives::{address, Address, Bytes, B256};
use alloy_sol_types::{sol, SolCall};

/// Canonical Multicall3. Used only to batch `eth_call`s; each inner call is
/// still an on-chain view of the target, not a cache.
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

/// Inner calls per Multicall3 `eth_call`. Sized so a public RPC will accept
/// the payload; accuracy does not depend on the size.
const BATCH: usize = 64;

sol! {
    interface IERC20 {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
    interface IERC20Bytes32 {
        function symbol() external view returns (bytes32);
    }
    interface IUniswapV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
    }
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

enum Expect<'a> {
    Decimals {
        token: Address,
        expected: u8,
    },
    Symbol {
        token: Address,
        expected: Option<&'a str>,
        bytes32: bool,
    },
    Token0 {
        pool: Address,
        expected: Address,
    },
    Token1 {
        pool: Address,
        expected: Address,
    },
    Fee {
        pool: Address,
        expected: u32,
    },
}

/// Re-read every boot-checkable field. Fail closed on mismatch or RPC fault.
pub async fn assert_registry<R: ChainRpc + Sync>(reg: &Registry, rpc: &R) -> Result<()> {
    if reg.tokens.is_empty() {
        return Err(ConfigError::EmptyRegistry);
    }
    let found = rpc.chain_id().await?;
    if found != reg.chain_id {
        return Err(ConfigError::ChainIdMismatch {
            expected: reg.chain_id,
            found,
        });
    }

    let mut calls: Vec<IMulticall3::Call3> = Vec::new();
    let mut expect: Vec<Expect<'_>> = Vec::new();

    for (addr, tok) in &reg.tokens {
        calls.push(call3(
            *addr,
            Bytes::from(IERC20::decimalsCall {}.abi_encode()),
        ));
        expect.push(Expect::Decimals {
            token: *addr,
            expected: tok.decimals,
        });
        let bytes32 = tok.symbol_is_bytes32();
        let data = if bytes32 {
            Bytes::from(IERC20Bytes32::symbolCall {}.abi_encode())
        } else {
            Bytes::from(IERC20::symbolCall {}.abi_encode())
        };
        calls.push(call3(*addr, data));
        expect.push(Expect::Symbol {
            token: *addr,
            expected: tok.symbol.as_deref(),
            bytes32,
        });
    }

    for (addr, pool) in &reg.pools {
        match pool.venue {
            PoolVenue::Univ3 => {}
        }
        calls.push(call3(
            *addr,
            Bytes::from(IUniswapV3Pool::token0Call {}.abi_encode()),
        ));
        expect.push(Expect::Token0 {
            pool: *addr,
            expected: pool.token0,
        });
        calls.push(call3(
            *addr,
            Bytes::from(IUniswapV3Pool::token1Call {}.abi_encode()),
        ));
        expect.push(Expect::Token1 {
            pool: *addr,
            expected: pool.token1,
        });
        calls.push(call3(
            *addr,
            Bytes::from(IUniswapV3Pool::feeCall {}.abi_encode()),
        ));
        expect.push(Expect::Fee {
            pool: *addr,
            expected: pool.fee,
        });
    }

    let mut offset = 0;
    while offset < calls.len() {
        let end = core::cmp::min(offset.saturating_add(BATCH), calls.len());
        let Some(slice) = calls.get(offset..end) else {
            return Err(ConfigError::RpcUnavailable);
        };
        let Some(exp) = expect.get(offset..end) else {
            return Err(ConfigError::RpcUnavailable);
        };
        let results = aggregate3(rpc, slice).await?;
        if results.len() != slice.len() {
            return Err(ConfigError::CallFailed {
                address: MULTICALL3,
                what: "multicall result count",
            });
        }
        for (i, exp_one) in exp.iter().enumerate() {
            let Some(row) = results.get(i) else {
                return Err(ConfigError::CallFailed {
                    address: MULTICALL3,
                    what: "multicall index",
                });
            };
            check_one(exp_one, row)?;
        }
        offset = end;
    }
    Ok(())
}

fn call3(target: Address, call_data: Bytes) -> IMulticall3::Call3 {
    IMulticall3::Call3 {
        target,
        allowFailure: true,
        callData: call_data,
    }
}

async fn aggregate3<R: ChainRpc + Sync>(
    rpc: &R,
    calls: &[IMulticall3::Call3],
) -> Result<Vec<IMulticall3::Result>> {
    let data = Bytes::from(
        IMulticall3::aggregate3Call {
            calls: calls.to_vec(),
        }
        .abi_encode(),
    );
    let raw = rpc.call(MULTICALL3, data).await?;
    IMulticall3::aggregate3Call::abi_decode_returns(&raw).map_err(|_| ConfigError::CallFailed {
        address: MULTICALL3,
        what: "aggregate3 decode",
    })
}

fn check_one(exp: &Expect<'_>, row: &IMulticall3::Result) -> Result<()> {
    let (address, what) = match exp {
        Expect::Decimals { token, .. } | Expect::Symbol { token, .. } => (*token, "token view"),
        Expect::Token0 { pool, .. } | Expect::Token1 { pool, .. } | Expect::Fee { pool, .. } => {
            (*pool, "pool view")
        }
    };
    if !row.success {
        return Err(ConfigError::CallFailed { address, what });
    }
    match exp {
        Expect::Decimals { token, expected } => {
            let found =
                IERC20::decimalsCall::abi_decode_returns(&row.returnData).map_err(|_| {
                    ConfigError::CallFailed {
                        address: *token,
                        what: "decimals decode",
                    }
                })?;
            if found != *expected {
                return Err(ConfigError::DecimalsMismatch {
                    token: *token,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::Symbol {
            token,
            expected,
            bytes32,
        } => {
            let found = if *bytes32 {
                symbol_from_bytes32(*token, &row.returnData)?
            } else {
                IERC20::symbolCall::abi_decode_returns(&row.returnData).map_err(|_| {
                    ConfigError::CallFailed {
                        address: *token,
                        what: "symbol decode",
                    }
                })?
            };
            if let Some(expected) = *expected {
                if found != expected {
                    return Err(ConfigError::SymbolMismatch {
                        token: *token,
                        expected: expected.to_string(),
                        found,
                    });
                }
            } else {
                return Err(ConfigError::SymbolMissing {
                    token: *token,
                    found,
                });
            }
        }
        Expect::Token0 { pool, expected } => {
            let found =
                IUniswapV3Pool::token0Call::abi_decode_returns(&row.returnData).map_err(|_| {
                    ConfigError::CallFailed {
                        address: *pool,
                        what: "token0 decode",
                    }
                })?;
            if found != *expected {
                return Err(ConfigError::TokenOrderMismatch {
                    pool: *pool,
                    expected0: *expected,
                    expected1: Address::ZERO,
                    found0: found,
                    found1: Address::ZERO,
                });
            }
        }
        Expect::Token1 { pool, expected } => {
            let found =
                IUniswapV3Pool::token1Call::abi_decode_returns(&row.returnData).map_err(|_| {
                    ConfigError::CallFailed {
                        address: *pool,
                        what: "token1 decode",
                    }
                })?;
            // token0 already checked; a token1-only mismatch still names both
            // slots so the operator sees the on-chain pair.
            if found != *expected {
                return Err(ConfigError::TokenOrderMismatch {
                    pool: *pool,
                    expected0: Address::ZERO,
                    expected1: *expected,
                    found0: Address::ZERO,
                    found1: found,
                });
            }
        }
        Expect::Fee { pool, expected } => {
            let found =
                IUniswapV3Pool::feeCall::abi_decode_returns(&row.returnData).map_err(|_| {
                    ConfigError::CallFailed {
                        address: *pool,
                        what: "fee decode",
                    }
                })?;
            let Some(&limb) = found.into_limbs().first() else {
                return Err(ConfigError::CallFailed {
                    address: *pool,
                    what: "fee limbs",
                });
            };
            let found_u32 = u32::try_from(limb).map_err(|_| ConfigError::CallFailed {
                address: *pool,
                what: "fee width",
            })?;
            if found_u32 != *expected {
                return Err(ConfigError::FeeMismatch {
                    pool: *pool,
                    expected: *expected,
                    found: found_u32,
                });
            }
        }
    }
    Ok(())
}

fn symbol_from_bytes32(token: Address, data: &[u8]) -> Result<String> {
    let raw = IERC20Bytes32::symbolCall::abi_decode_returns(data).map_err(|_| {
        ConfigError::CallFailed {
            address: token,
            what: "symbol bytes32 decode",
        }
    })?;
    let b256 = B256::from(raw);
    let bytes = b256.as_slice();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let slice = bytes.get(..end).ok_or(ConfigError::CallFailed {
        address: token,
        what: "symbol bytes32 slice",
    })?;
    core::str::from_utf8(slice)
        .map(str::to_string)
        .map_err(|_| ConfigError::CallFailed {
            address: token,
            what: "symbol bytes32 utf8",
        })
}

impl Validate for Registry {
    async fn validate<R: ChainRpc + Sync>(&self, rpc: &R) -> Result<()> {
        assert_registry(self, rpc).await
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::assert_registry;
    use crate::error::ConfigError;
    use crate::registry::{PoolEntry, PoolVenue, Registry, TokenEntry, TokenQuirk};
    use crate::rpc::HttpRpc;
    use alloy_primitives::{address, Address};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const USDT: Address = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    const MKR: Address = address!("0x9f8F72aA9304c8B593d555F12eF6589cC3A579A2");
    /// TUSD/WETH 0.3% — first pool in the committed registry (derived via factory).
    const TUSD_WETH: Address = address!("0x714b8443D0AdA18Ece1fCE5702567e313Bfa8f29");
    const TUSD: Address = address!("0x0000000000085d4780B73119b644AE5Ecd22b376");
    const UNI_FACTORY: Address = address!("0x1F98431c8aD98523631AE4a59f267346ea31F984");

    fn rpc_url() -> String {
        std::env::var("LIQ_RPC_URL")
            .unwrap_or_else(|_| "https://ethereum.publicnode.com".to_string())
    }

    fn live_rpc() -> HttpRpc {
        HttpRpc::connect(&rpc_url()).expect("rpc url parses")
    }

    fn tokens_weth_usdc_usdt_mkr() -> BTreeMap<Address, TokenEntry> {
        let mut t = BTreeMap::new();
        t.insert(
            WETH,
            TokenEntry {
                symbol: Some("WETH".into()),
                decimals: 18,
                quirks: vec![],
                symbol_collision: None,
            },
        );
        t.insert(
            USDC,
            TokenEntry {
                symbol: Some("USDC".into()),
                decimals: 6,
                quirks: vec![TokenQuirk::LowDecimals],
                symbol_collision: None,
            },
        );
        t.insert(
            USDT,
            TokenEntry {
                symbol: Some("USDT".into()),
                decimals: 6,
                quirks: vec![
                    TokenQuirk::ApproveNonzeroReverts,
                    TokenQuirk::LowDecimals,
                    TokenQuirk::NoReturnData,
                ],
                symbol_collision: None,
            },
        );
        t.insert(
            MKR,
            TokenEntry {
                symbol: Some("MKR".into()),
                decimals: 18,
                quirks: vec![TokenQuirk::NonstandardMetadata],
                symbol_collision: None,
            },
        );
        t
    }

    fn tiny_registry(tokens: BTreeMap<Address, TokenEntry>) -> Registry {
        let mut pools = BTreeMap::new();
        pools.insert(
            TUSD_WETH,
            PoolEntry {
                venue: PoolVenue::Univ3,
                token0: TUSD,
                token1: WETH,
                fee: 3000,
                factory: UNI_FACTORY,
                deployed_block: 0,
                derived_via: "factory.getPool".into(),
            },
        );
        Registry {
            chain_id: 1,
            generated_at_block: 0,
            tokens,
            protocols: BTreeMap::new(),
            oracles: BTreeMap::new(),
            pools,
            flash_sources: BTreeMap::new(),
            routers: BTreeMap::new(),
        }
    }

    /// Oracle: chain (WETH/USDC/USDT/MKR decimals+symbol, TUSD/WETH pool
    /// token0/token1/fee). Negative: a wrong registry must not pass.
    #[tokio::test(flavor = "current_thread")]
    async fn live_subset_matches_chain() {
        let rpc = live_rpc();
        let reg = tiny_registry(tokens_weth_usdc_usdt_mkr());
        tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &rpc))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap();
    }

    /// Oracle: committed registry records `symbol: null` on two tokens (discovery
    /// gap, not a guess). Boot must refuse rather than skip the field. One of
    /// those tokens also reverts a metadata view (`CallFailed`) — same outcome.
    #[tokio::test(flavor = "current_thread")]
    async fn committed_null_symbol_fails_startup() {
        let committed =
            crate::registry::Registry::from_path(&workspace_root().join("registry/registry.json"))
                .unwrap();
        let tokens: BTreeMap<_, _> = committed
            .tokens
            .iter()
            .filter(|(_, t)| t.symbol.is_none())
            .map(|(a, t)| (*a, t.clone()))
            .collect();
        assert_eq!(
            tokens.len(),
            2,
            "committed file currently has two null symbols"
        );
        let mut reg = tiny_registry(tokens);
        reg.pools.clear();
        let err = tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &live_rpc()))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::SymbolMissing { .. } | ConfigError::CallFailed { .. }
            ),
            "null-symbol rows must refuse to start; got {err:?}"
        );
    }

    /// Oracle: REGISTRY.md §4c + GUIDE 00 acceptance. Flip USDC decimals 6 → 8;
    /// boot must refuse. The RPC still returns the real 6 — we do not mock it.
    #[tokio::test(flavor = "current_thread")]
    async fn corrupting_token_decimals_fails_startup() {
        let rpc = live_rpc();
        let mut tokens = tokens_weth_usdc_usdt_mkr();
        tokens.get_mut(&USDC).unwrap().decimals = 8;
        let reg = tiny_registry(tokens);
        let err = tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &rpc))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap_err();
        match err {
            ConfigError::DecimalsMismatch {
                token,
                expected,
                found,
            } => {
                assert_eq!(token, USDC);
                assert_eq!(expected, 8);
                assert_eq!(found, 6);
            }
            other => panic!("expected DecimalsMismatch, got {other}"),
        }
    }

    /// Oracle: REGISTRY.md §4c — no degraded mode. A closed port is not a
    /// reason to start on cached decimals.
    #[tokio::test(flavor = "current_thread")]
    async fn rpc_unavailable_fails_closed() {
        let rpc = HttpRpc::connect("http://127.0.0.1:1").unwrap();
        let reg = tiny_registry(tokens_weth_usdc_usdt_mkr());
        let err = tokio::time::timeout(Duration::from_secs(15), assert_registry(&reg, &rpc))
            .await
            .expect("hung rpc");
        assert!(
            matches!(err, Err(ConfigError::RpcUnavailable)),
            "got {err:?}"
        );
    }

    /// Oracle: empty rpc_url is invalid before any socket is opened.
    #[test]
    fn empty_rpc_url_is_unavailable() {
        assert!(matches!(
            HttpRpc::connect(""),
            Err(ConfigError::RpcUnavailable)
        ));
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    /// Full committed registry vs chain. Ignored in CI: ~5k views, needs a
    /// local node or a generous RPC. Run with `LIQ_RPC_URL` set.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "full-registry boot assertion; run with a local node"]
    async fn committed_registry_matches_chain() {
        let rpc = live_rpc();
        let reg =
            crate::registry::Registry::from_path(&workspace_root().join("registry/registry.json"))
                .unwrap();
        assert_registry(&reg, &rpc).await.unwrap();
    }
}
