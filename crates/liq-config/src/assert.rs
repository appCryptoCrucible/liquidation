//! Boot assertion (REGISTRY.md §4c). Re-reads `decimals`/`symbol` for every
//! token (and `asset()` for every unwrappable one), `token0`/`token1`/`fee`
//! for every pool (a Curve pool: its coins, and that the MetaRegistry
//! handler the registry names holds it), and `decimals`/`aggregator` for
//! every oracle proxy from chain. Any mismatch or RPC failure refuses to
//! start.

use crate::error::ConfigError;
use crate::registry::{PoolVenue, Registry, UnwrapKind};
use crate::rpc::ChainRpc;
use crate::validate::Validate;
use crate::Result;
use alloy_primitives::{address, Address, Bytes, B256};
use alloy_sol_types::{sol, SolCall};

/// Canonical Multicall3. Used only to batch `eth_call`s; each inner call is
/// still an on-chain view of the target, not a cache.
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

/// Pendle `PendleMarketFactoryV6` — the Executor's anchor for venue 8
/// (`MainnetVenues.PENDLE_MARKET_FACTORY_V6`).
const PENDLE_MARKET_FACTORY_V6: Address = address!("0x6d247b1c044fA1E22e6B04fA9F71Baf99EB29A9f");

/// Curve MetaRegistry — the Executor's anchor for the Curve venues
/// (`MainnetVenues.CURVE_META_REGISTRY`).
const CURVE_META_REGISTRY: Address = address!("0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC");

/// Inner calls per Multicall3 `eth_call`. Sized so a public RPC will accept
/// the payload; accuracy does not depend on the size.
const BATCH: usize = 64;

sol! {
    /// Uniswap V4 `StateView` (v4-periphery `lens/StateView.sol`).
    interface IV4StateView {
        function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
    }
    interface IERC20 {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
    interface IERC20Bytes32 {
        function symbol() external view returns (bytes32);
    }
    interface IERC4626 {
        function asset() external view returns (address);
    }
    interface IPendlePT {
        function YT() external view returns (address);
    }
    interface IPendleYT {
        function PT() external view returns (address);
        function SY() external view returns (address);
    }
    interface IPendleMarketFactory {
        function isValidMarket(address market) external view returns (bool);
    }
    interface IUniswapV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
    }
    interface IUniswapV2Pair {
        function factory() external view returns (address);
    }
    interface ICurvePool {
        function coins(uint256 i) external view returns (address);
    }
    interface ICurveMetaRegistry {
        function get_registry(uint256 i) external view returns (address);
    }
    interface ICurveRegistryHandler {
        function is_registered(address pool) external view returns (bool);
    }
    interface AggregatorV3Interface {
        function decimals() external view returns (uint8);
    }
    interface EACAggregatorProxy {
        function aggregator() external view returns (address);
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

/// Uniswap V4 `StateView` (v4 deployments, mainnet).
const V4_STATE_VIEW: Address =
    alloy_primitives::address!("7fFE42C4a5DEeA5b0feC41C94C136Cf115597227");

enum Expect<'a> {
    /// A Uniswap V4 pool's `getSlot0` must show it initialized.
    V4Initialized {
        pool: Address,
    },
    Decimals {
        token: Address,
        expected: u8,
    },
    Symbol {
        token: Address,
        expected: Option<&'a str>,
        bytes32: bool,
    },
    UnwrapAsset {
        token: Address,
        expected: Address,
    },
    /// Pendle's V6 factory must still know `market`.
    PendleMarketValid {
        token: Address,
        market: Address,
    },
    /// `what` read on `target` must equal `expected` (Pendle PT ↔ YT ↔ SY).
    PendleLink {
        token: Address,
        target: Address,
        what: &'static str,
        expected: Address,
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
    Factory {
        pool: Address,
        expected: Address,
    },
    CurveCoin {
        pool: Address,
        index: usize,
        expected: Address,
    },
    /// `handler` (the MetaRegistry's handler at `index`) must hold `pool`.
    CurveRegistered {
        pool: Address,
        index: u8,
        handler: Address,
    },
    OracleDecimals {
        proxy: Address,
        expected: u8,
    },
    Aggregator {
        proxy: Address,
        expected: Address,
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
    assert_registry_views(reg, rpc).await
}

/// Token / pool / oracle views. Does not call `eth_chainId` — `boot` fetches
/// that once and checks config + registry against it.
pub(crate) async fn assert_registry_views<R: ChainRpc + Sync>(
    reg: &Registry,
    rpc: &R,
) -> Result<()> {
    if reg.tokens.is_empty() {
        return Err(ConfigError::EmptyRegistry);
    }

    // The Executor's own check on a Curve leg, in its two steps: the
    // MetaRegistry's handler at the index the registry names, then that
    // handler's `is_registered(pool)`. The handler addresses are read first;
    // the per-pool views join the batch below.
    let curve_handlers = read_curve_handlers(reg, rpc).await?;

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
        match &tok.unwrap {
            Some(u) if u.kind == UnwrapKind::Erc4626 => {
                calls.push(call3(
                    *addr,
                    Bytes::from(IERC4626::assetCall {}.abi_encode()),
                ));
                expect.push(Expect::UnwrapAsset {
                    token: *addr,
                    expected: u.into,
                });
            }
            // The LP is its own pool, a registry pool whose `coins(i)` are
            // asserted with the pools below.
            Some(u) if u.kind == UnwrapKind::CurveLp => {}
            Some(u) => {
                let (Some(yt), Some(sy)) = (u.yt, u.sy) else {
                    return Err(ConfigError::PendleMismatch {
                        token: *addr,
                        what: "yt/sy",
                        expected: Address::ZERO,
                        found: Address::ZERO,
                    });
                };
                let links: [(Address, &'static str, Bytes, Address); 3] = [
                    (
                        *addr,
                        "YT()",
                        Bytes::from(IPendlePT::YTCall {}.abi_encode()),
                        yt,
                    ),
                    (
                        yt,
                        "PT()",
                        Bytes::from(IPendleYT::PTCall {}.abi_encode()),
                        *addr,
                    ),
                    (
                        yt,
                        "SY()",
                        Bytes::from(IPendleYT::SYCall {}.abi_encode()),
                        sy,
                    ),
                ];
                for (target, what, data, expected) in links {
                    calls.push(call3(target, data));
                    expect.push(Expect::PendleLink {
                        token: *addr,
                        target,
                        what,
                        expected,
                    });
                }
                if u.kind == UnwrapKind::PendleMarket {
                    let Some(market) = u.market else {
                        return Err(ConfigError::PendleMismatch {
                            token: *addr,
                            what: "market",
                            expected: Address::ZERO,
                            found: Address::ZERO,
                        });
                    };
                    calls.push(call3(
                        PENDLE_MARKET_FACTORY_V6,
                        Bytes::from(
                            IPendleMarketFactory::isValidMarketCall { market }.abi_encode(),
                        ),
                    ));
                    expect.push(Expect::PendleMarketValid {
                        token: *addr,
                        market,
                    });
                }
            }
            None => {}
        }
    }

    for (addr, pool) in &reg.pools {
        if pool.venue.is_curve() {
            for (index, coin) in pool.coins.iter().enumerate() {
                calls.push(call3(
                    *addr,
                    Bytes::from(
                        ICurvePool::coinsCall {
                            i: alloy_primitives::U256::from(index),
                        }
                        .abi_encode(),
                    ),
                ));
                expect.push(Expect::CurveCoin {
                    pool: *addr,
                    index,
                    expected: *coin,
                });
            }
            let index = pool
                .curve_handler
                .ok_or(ConfigError::CurveHandlerMissing { pool: *addr })?;
            let handler = curve_handlers.get(&index).copied().unwrap_or(Address::ZERO);
            if handler.is_zero() {
                return Err(ConfigError::CurveHandlerMismatch {
                    pool: *addr,
                    index,
                    handler,
                });
            }
            calls.push(call3(
                handler,
                Bytes::from(ICurveRegistryHandler::is_registeredCall { pool: *addr }.abi_encode()),
            ));
            expect.push(Expect::CurveRegistered {
                pool: *addr,
                index,
                handler,
            });
            continue;
        }
        // A V4 pool has no contract of its own: its key must hash to its id,
        // the entry's address must be that id's low 20 bytes, and StateView
        // must find the pool initialized.
        if pool.venue == PoolVenue::Univ4 {
            let id = pool.v4_id.ok_or(ConfigError::CallFailed {
                address: *addr,
                what: "univ4 entry has no v4_id",
            })?;
            if pool.v4_key_id() != Some(id) || *addr != Address::from_word(id) {
                return Err(ConfigError::CallFailed {
                    address: *addr,
                    what: "univ4 key does not hash to its id",
                });
            }
            calls.push(call3(
                V4_STATE_VIEW,
                Bytes::from(IV4StateView::getSlot0Call { poolId: id }.abi_encode()),
            ));
            expect.push(Expect::V4Initialized { pool: *addr });
            continue;
        }
        if pool.venue == PoolVenue::Univ2 {
            calls.push(call3(
                *addr,
                Bytes::from(IUniswapV2Pair::factoryCall {}.abi_encode()),
            ));
            expect.push(Expect::Factory {
                pool: *addr,
                expected: pool.factory,
            });
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
        if pool.venue == PoolVenue::Univ3 {
            calls.push(call3(
                *addr,
                Bytes::from(IUniswapV3Pool::feeCall {}.abi_encode()),
            ));
            expect.push(Expect::Fee {
                pool: *addr,
                expected: pool.fee,
            });
        }
    }

    for (addr, oracle) in &reg.oracles {
        calls.push(call3(
            *addr,
            Bytes::from(AggregatorV3Interface::decimalsCall {}.abi_encode()),
        ));
        expect.push(Expect::OracleDecimals {
            proxy: *addr,
            expected: oracle.decimals,
        });
        calls.push(call3(
            *addr,
            Bytes::from(EACAggregatorProxy::aggregatorCall {}.abi_encode()),
        ));
        expect.push(Expect::Aggregator {
            proxy: *addr,
            expected: oracle.aggregator,
        });
    }

    let mut offset = 0;
    while offset < calls.len() {
        let end = core::cmp::min(offset.saturating_add(BATCH), calls.len());
        let Some(slice) = calls.get(offset..end) else {
            return Err(ConfigError::CallFailed {
                address: MULTICALL3,
                what: "multicall batch slice",
            });
        };
        let Some(exp) = expect.get(offset..end) else {
            return Err(ConfigError::CallFailed {
                address: MULTICALL3,
                what: "multicall expect slice",
            });
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

/// The MetaRegistry's handler address at every index a Curve pool of `reg`
/// names (`get_registry(i)`; the zero address past its list). One batch.
async fn read_curve_handlers<R: ChainRpc + Sync>(
    reg: &Registry,
    rpc: &R,
) -> Result<std::collections::BTreeMap<u8, Address>> {
    let indices: std::collections::BTreeSet<u8> = reg
        .pools
        .values()
        .filter(|p| p.venue.is_curve())
        .filter_map(|p| p.curve_handler)
        .collect();
    let mut out = std::collections::BTreeMap::new();
    if indices.is_empty() {
        return Ok(out);
    }
    let calls: Vec<IMulticall3::Call3> = indices
        .iter()
        .map(|i| {
            call3(
                CURVE_META_REGISTRY,
                Bytes::from(
                    ICurveMetaRegistry::get_registryCall {
                        i: alloy_primitives::U256::from(*i),
                    }
                    .abi_encode(),
                ),
            )
        })
        .collect();
    let results = aggregate3(rpc, &calls).await?;
    if results.len() != calls.len() {
        return Err(ConfigError::CallFailed {
            address: MULTICALL3,
            what: "multicall result count",
        });
    }
    for (index, row) in indices.iter().zip(&results) {
        if !row.success {
            return Err(ConfigError::CallFailed {
                address: CURVE_META_REGISTRY,
                what: "get_registry",
            });
        }
        let handler =
            ICurveMetaRegistry::get_registryCall::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: CURVE_META_REGISTRY,
                    what: "get_registry decode",
                })?;
        out.insert(*index, handler);
    }
    Ok(out)
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
    IMulticall3::aggregate3Call::abi_decode_returns_validate(&raw).map_err(|_| {
        ConfigError::CallFailed {
            address: MULTICALL3,
            what: "aggregate3 decode",
        }
    })
}

fn check_one(exp: &Expect<'_>, row: &IMulticall3::Result) -> Result<()> {
    let (address, what) = match exp {
        Expect::Decimals { token, .. }
        | Expect::Symbol { token, .. }
        | Expect::UnwrapAsset { token, .. } => (*token, "token view"),
        Expect::PendleLink { target, .. } => (*target, "pendle view"),
        Expect::PendleMarketValid { .. } => (PENDLE_MARKET_FACTORY_V6, "pendle factory view"),
        Expect::Token0 { pool, .. }
        | Expect::Token1 { pool, .. }
        | Expect::Fee { pool, .. }
        | Expect::Factory { pool, .. }
        | Expect::CurveCoin { pool, .. } => (*pool, "pool view"),
        Expect::V4Initialized { .. } => (V4_STATE_VIEW, "v4 state view"),
        Expect::CurveRegistered { handler, .. } => (*handler, "curve registry handler view"),
        Expect::OracleDecimals { proxy, .. } | Expect::Aggregator { proxy, .. } => {
            (*proxy, "oracle view")
        }
    };
    // A registry `null` symbol records a token whose `symbol()` reverts:
    // the chain agreeing is a match, and a symbol appearing is refused
    // below (`SymbolMissing`) — the registry is then out of date.
    if !row.success && matches!(exp, Expect::Symbol { expected: None, .. }) {
        return Ok(());
    }
    if !row.success {
        return Err(ConfigError::CallFailed { address, what });
    }
    match exp {
        Expect::Decimals { token, expected } => {
            let found = IERC20::decimalsCall::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: *token,
                    what: "decimals decode",
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
                IERC20::symbolCall::abi_decode_returns_validate(&row.returnData).map_err(|_| {
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
        Expect::UnwrapAsset { token, expected } => {
            let found = IERC4626::assetCall::abi_decode_returns_validate(&row.returnData).map_err(
                |_| ConfigError::CallFailed {
                    address: *token,
                    what: "asset decode",
                },
            )?;
            if found != *expected {
                return Err(ConfigError::UnwrapAssetMismatch {
                    token: *token,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::PendleMarketValid { token, market } => {
            let valid = IPendleMarketFactory::isValidMarketCall::abi_decode_returns_validate(
                &row.returnData,
            )
            .map_err(|_| ConfigError::CallFailed {
                address: PENDLE_MARKET_FACTORY_V6,
                what: "isValidMarket decode",
            })?;
            if !valid {
                return Err(ConfigError::PendleMismatch {
                    token: *token,
                    what: "market (not a V6-factory market)",
                    expected: *market,
                    found: Address::ZERO,
                });
            }
        }
        Expect::PendleLink {
            token,
            target,
            what,
            expected,
        } => {
            // All three views return one address word.
            let found = IERC4626::assetCall::abi_decode_returns_validate(&row.returnData).map_err(
                |_| ConfigError::CallFailed {
                    address: *target,
                    what: "pendle link decode",
                },
            )?;
            if found != *expected {
                return Err(ConfigError::PendleMismatch {
                    token: *token,
                    what,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::Token0 { pool, expected } => {
            let found = IUniswapV3Pool::token0Call::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: *pool,
                    what: "token0 decode",
                })?;
            if found != *expected {
                return Err(ConfigError::Token0Mismatch {
                    pool: *pool,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::Token1 { pool, expected } => {
            let found = IUniswapV3Pool::token1Call::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: *pool,
                    what: "token1 decode",
                })?;
            if found != *expected {
                return Err(ConfigError::Token1Mismatch {
                    pool: *pool,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::Factory { pool, expected } => {
            let found = IUniswapV2Pair::factoryCall::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                address: *pool,
                what: "factory decode",
            })?;
            if found != *expected {
                return Err(ConfigError::PoolFactoryMismatch {
                    pool: *pool,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::CurveCoin {
            pool,
            index,
            expected,
        } => {
            let found = ICurvePool::coinsCall::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: *pool,
                    what: "coins decode",
                })?;
            if found != *expected {
                return Err(ConfigError::CurveCoinMismatch {
                    pool: *pool,
                    index: *index,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::CurveRegistered {
            pool,
            index,
            handler,
        } => {
            let held = ICurveRegistryHandler::is_registeredCall::abi_decode_returns_validate(
                &row.returnData,
            )
            .map_err(|_| ConfigError::CallFailed {
                address: *handler,
                what: "is_registered decode",
            })?;
            if !held {
                return Err(ConfigError::CurveHandlerMismatch {
                    pool: *pool,
                    index: *index,
                    handler: *handler,
                });
            }
        }
        Expect::V4Initialized { pool } => {
            let s0 = IV4StateView::getSlot0Call::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: *pool,
                    what: "v4 getSlot0 decode",
                })?;
            if s0.sqrtPriceX96.is_zero() {
                return Err(ConfigError::CallFailed {
                    address: *pool,
                    what: "v4 pool not initialized",
                });
            }
        }
        Expect::Fee { pool, expected } => {
            let found = IUniswapV3Pool::feeCall::abi_decode_returns_validate(&row.returnData)
                .map_err(|_| ConfigError::CallFailed {
                    address: *pool,
                    what: "fee decode",
                })?;
            let found_u32 = found.to::<u32>();
            if found_u32 != *expected {
                return Err(ConfigError::FeeMismatch {
                    pool: *pool,
                    expected: *expected,
                    found: found_u32,
                });
            }
        }
        Expect::OracleDecimals { proxy, expected } => {
            let found =
                AggregatorV3Interface::decimalsCall::abi_decode_returns_validate(&row.returnData)
                    .map_err(|_| ConfigError::CallFailed {
                    address: *proxy,
                    what: "oracle decimals decode",
                })?;
            if found != *expected {
                return Err(ConfigError::OracleDecimalsMismatch {
                    proxy: *proxy,
                    expected: *expected,
                    found,
                });
            }
        }
        Expect::Aggregator { proxy, expected } => {
            let found =
                EACAggregatorProxy::aggregatorCall::abi_decode_returns_validate(&row.returnData)
                    .map_err(|_| ConfigError::CallFailed {
                        address: *proxy,
                        what: "aggregator decode",
                    })?;
            if found != *expected {
                return Err(ConfigError::AggregatorMismatch {
                    proxy: *proxy,
                    expected: *expected,
                    found,
                });
            }
        }
    }
    Ok(())
}

fn symbol_from_bytes32(token: Address, data: &[u8]) -> Result<String> {
    let raw = IERC20Bytes32::symbolCall::abi_decode_returns_validate(data).map_err(|_| {
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
    use crate::registry::{OracleEntry, PoolEntry, PoolVenue, Registry, TokenEntry, TokenQuirk};
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
    /// Aave V3 WETH source (Chainlink EACAggregatorProxy). First oracle in the
    /// committed registry — decimals + aggregator are chain-derived, not guessed.
    const AAVE_WETH_PROXY: Address = address!("0x5424384B256154046E9667dDfAaa5e550145215e");
    const AAVE_WETH_AGG: Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");

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
                unwrap: None,
            },
        );
        t.insert(
            USDC,
            TokenEntry {
                symbol: Some("USDC".into()),
                decimals: 6,
                quirks: vec![TokenQuirk::LowDecimals],
                symbol_collision: None,
                unwrap: None,
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
                unwrap: None,
            },
        );
        t.insert(
            MKR,
            TokenEntry {
                symbol: Some("MKR".into()),
                decimals: 18,
                quirks: vec![TokenQuirk::NonstandardMetadata],
                symbol_collision: None,
                unwrap: None,
            },
        );
        t
    }

    fn aave_weth_oracle() -> (Address, OracleEntry) {
        (
            AAVE_WETH_PROXY,
            OracleEntry {
                aggregator: AAVE_WETH_AGG,
                pair: "aave-v3/0xc02aaa39".into(),
                decimals: 8,
                svr: false,
                source: "aave-v3:getSourceOfAsset".into(),
            },
        )
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
                coins: Vec::new(),
                asset_types: Vec::new(),
                crypto_kind: None,
                pool_id: None,
                balancer_kind: None,
                curve_d_once: false,
                curve_handler: None,
                tick_spacing: None,
                hooks: None,
                v4_id: None,
                native: false,
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
            asset_ledger: None,
        }
    }

    /// Oracle: chain (WETH/USDC/USDT/MKR decimals+symbol, TUSD/WETH pool
    /// token0/token1/fee, one Aave V3 Chainlink proxy decimals+aggregator).
    /// Negative: a wrong registry must not pass.
    #[tokio::test(flavor = "current_thread")]
    async fn live_subset_matches_chain() {
        let rpc = live_rpc();
        let mut reg = tiny_registry(tokens_weth_usdc_usdt_mkr());
        let (proxy, entry) = aave_weth_oracle();
        reg.oracles.insert(proxy, entry);
        tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &rpc))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap();
    }

    /// Oracle: `symbol: null` records a token whose `symbol()` reverts. The
    /// committed null row boots (the chain still reverts). Negative: a token
    /// that does have a symbol, recorded as null, refuses (`SymbolMissing`).
    #[tokio::test(flavor = "current_thread")]
    async fn null_symbol_means_symbol_reverts() {
        let committed =
            crate::registry::Registry::from_path(&workspace_root().join("registry/registry.json"))
                .unwrap();
        let tokens: BTreeMap<_, _> = committed
            .tokens
            .iter()
            .filter(|(_, t)| t.symbol.is_none())
            .map(|(a, t)| (*a, t.clone()))
            .collect();
        assert_eq!(tokens.len(), 1, "committed file has one null symbol");
        let mut reg = tiny_registry(tokens);
        reg.pools.clear();
        tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &live_rpc()))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap();

        let mut tokens = tokens_weth_usdc_usdt_mkr();
        tokens.get_mut(&USDC).unwrap().symbol = None;
        let mut reg = tiny_registry(tokens);
        reg.pools.clear();
        let err = tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &live_rpc()))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap_err();
        assert!(
            matches!(err, ConfigError::SymbolMissing { .. }),
            "a real symbol recorded as null must refuse to start; got {err:?}"
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

    const DAI: Address = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
    /// Curve 3pool: coins DAI, USDC, USDT, as the committed registry has it.
    const CURVE_3POOL: Address = address!("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    /// The 3pool and its coins, with `handler` as its MetaRegistry handler index.
    fn curve_registry(handler: Option<u8>) -> Registry {
        let mut tokens = tokens_weth_usdc_usdt_mkr();
        tokens.insert(
            DAI,
            TokenEntry {
                symbol: Some("DAI".into()),
                decimals: 18,
                quirks: vec![],
                symbol_collision: None,
                unwrap: None,
            },
        );
        let mut reg = tiny_registry(tokens);
        reg.pools.clear();
        reg.pools.insert(
            CURVE_3POOL,
            PoolEntry {
                venue: PoolVenue::Curve,
                token0: DAI,
                token1: USDC,
                fee: 1_500_000,
                factory: super::CURVE_META_REGISTRY,
                deployed_block: 0,
                derived_via: "metaregistry.pool_list+get_dy".into(),
                coins: vec![DAI, USDC, USDT],
                asset_types: Vec::new(),
                crypto_kind: None,
                pool_id: None,
                balancer_kind: None,
                curve_d_once: false,
                curve_handler: handler,
                tick_spacing: None,
                hooks: None,
                v4_id: None,
                native: false,
            },
        );
        reg
    }

    async fn boot(reg: &Registry) -> crate::Result<()> {
        tokio::time::timeout(Duration::from_secs(45), assert_registry(reg, &live_rpc()))
            .await
            .expect("rpc timed out — fail closed")
    }

    /// Oracle: Curve's MetaRegistry on chain, asked as the Executor asks it
    /// (`get_registry(i)`, then that handler's `is_registered(pool)`). The
    /// 3pool sits in handler 0, the base registry's, and boots with that
    /// index. Negative: the index of a handler that does not hold it (6, the
    /// StableSwap-NG factory's), an index past the MetaRegistry's list, and no
    /// index at all each refuse to start: the Executor would refuse every
    /// leg through the pool, or the leg could not be encoded.
    #[tokio::test(flavor = "current_thread")]
    async fn curve_handler_index_is_checked_against_the_metaregistry() {
        boot(&curve_registry(Some(0))).await.unwrap();

        match boot(&curve_registry(Some(6))).await.unwrap_err() {
            ConfigError::CurveHandlerMismatch {
                pool,
                index,
                handler,
            } => {
                assert_eq!(pool, CURVE_3POOL);
                assert_eq!(index, 6);
                assert!(
                    !handler.is_zero(),
                    "handler 6 exists; it does not hold the 3pool"
                );
            }
            other => panic!("expected CurveHandlerMismatch, got {other}"),
        }

        match boot(&curve_registry(Some(200))).await.unwrap_err() {
            ConfigError::CurveHandlerMismatch { index, handler, .. } => {
                assert_eq!(index, 200);
                assert!(handler.is_zero(), "no handler at an index past the list");
            }
            other => panic!("expected CurveHandlerMismatch, got {other}"),
        }

        match boot(&curve_registry(None)).await.unwrap_err() {
            ConfigError::CurveHandlerMissing { pool } => assert_eq!(pool, CURVE_3POOL),
            other => panic!("expected CurveHandlerMissing, got {other}"),
        }
    }

    /// Oracle: REGISTRY.md §4c — a proxy upgraded underneath us is an alert.
    /// Flip committed aggregator decimals 8 → 18; boot must refuse.
    #[tokio::test(flavor = "current_thread")]
    async fn corrupting_oracle_decimals_fails_startup() {
        let rpc = live_rpc();
        let mut reg = tiny_registry(tokens_weth_usdc_usdt_mkr());
        let (proxy, mut entry) = aave_weth_oracle();
        entry.decimals = 18;
        reg.oracles.insert(proxy, entry);
        let err = tokio::time::timeout(Duration::from_secs(45), assert_registry(&reg, &rpc))
            .await
            .expect("rpc timed out — fail closed")
            .unwrap_err();
        match err {
            ConfigError::OracleDecimalsMismatch {
                proxy: got,
                expected,
                found,
            } => {
                assert_eq!(got, AAVE_WETH_PROXY);
                assert_eq!(expected, 18);
                assert_eq!(found, 8);
            }
            other => panic!("expected OracleDecimalsMismatch, got {other}"),
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
            matches!(err, Err(ConfigError::RpcUnavailable { .. })),
            "got {err:?}"
        );
    }

    /// Oracle: empty rpc_url is invalid before any socket is opened.
    #[test]
    fn empty_rpc_url_is_unavailable() {
        assert!(matches!(
            HttpRpc::connect(""),
            Err(ConfigError::RpcUnavailable { .. })
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
