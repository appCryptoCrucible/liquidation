//! VaultFactory + the vaults read from chain at bind.
//!
//! # MarketId allocator (intern-global)
//!
//! Fluid is not in `registry.json`. ProtocolId **10**. Markets **4000..=4199**:
//! catalog = 4000; vault `vaultId` → `4000 + vaultId` (vault 1 → 4001).
//! Never intern 0..=3480, never 3481..=3999, never 4200+ (Gearbox).
//!
//! Boot: [`Config::from_toml`] → [`Config::bind_live`] (every vault the
//! factory has deployed: type, tokens, decimals, DEX sides; tokens mapped
//! through the registry intern) → [`crate::Fluid::new`], which refuses a
//! config that was not bound live.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::events::{erc20, factory, multicall, smart, t1, vault};
use crate::layout::{
    CATALOG_MARKET, FIRST_VAULT_MARKET, LAST_VAULT_MARKET, NATIVE_TOKEN, VAULT_T1, VAULT_T2,
    VAULT_T3, VAULT_T4,
};

/// Multicall3, the same deployment on every chain.
pub const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");
/// Calls per `aggregate3` at bind.
const BIND_BATCH: usize = 120;

/// Synchronous `eth_call` at a block. Boot-only; not on the hot path.
pub trait FactoryRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, ConfigError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

/// One vault as its own `constantsView` / `TYPE` report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultPin {
    pub vault: Address,
    pub vault_id: u32,
    pub vault_type: u32,
    /// Collateral side contract: the DEX on T2/T4, else Liquidity.
    pub supply: Address,
    /// Debt side contract: the DEX on T3/T4, else Liquidity.
    pub borrow: Address,
    pub supply0: Address,
    pub supply1: Address,
    pub borrow0: Address,
    pub borrow1: Address,
    pub supply_decimals0: u8,
    pub supply_decimals1: u8,
    pub borrow_decimals0: u8,
    pub borrow_decimals1: u8,
}

impl VaultPin {
    #[must_use]
    pub const fn market(&self) -> MarketId {
        MarketId(CATALOG_MARKET.0.saturating_add(self.vault_id))
    }

    /// Collateral tokens in slot order, with decimals.
    #[must_use]
    pub fn col_tokens(&self) -> Vec<(Address, u8)> {
        let mut v = vec![(self.supply0, self.supply_decimals0)];
        if self.vault_type == VAULT_T2 || self.vault_type == VAULT_T4 {
            v.push((self.supply1, self.supply_decimals1));
        }
        v
    }

    /// Debt tokens in slot order, with decimals.
    #[must_use]
    pub fn debt_tokens(&self) -> Vec<(Address, u8)> {
        let mut v = vec![(self.borrow0, self.borrow_decimals0)];
        if self.vault_type == VAULT_T3 || self.vault_type == VAULT_T4 {
            v.push((self.borrow1, self.borrow_decimals1));
        }
        v
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub factory: Address,
    pub catalog: MarketId,
    pub first_market: MarketId,
    /// What native ETH is paid and received as.
    pub weth: Address,
    pub vault_pins: Vec<VaultPin>,
    /// Every vault token the registry interns (native ETH as WETH).
    pub assets: Vec<AssetConfig>,
    /// False until [`Self::bind_live`] succeeds.
    pub live_bound: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("fluid factory is the zero address")]
    ZeroFactory,
    #[error("catalog/first_market outside Fluid 4000..=4199")]
    MarketRange,
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("vault type is not T1–T4")]
    BadVaultType,
    #[error("config was not bound to the live factory")]
    LiveUnbound,
    #[error("eth_call to {0} failed")]
    FactoryCall(Address),
    #[error("vault {0}: constantsView/TYPE did not decode")]
    VaultDecode(Address),
    #[error("factory reports no vaults")]
    NoVaults,
    #[error("protocol toml is malformed")]
    MalformedToml,
}

type R<T> = core::result::Result<T, ConfigError>;

impl Config {
    fn validate_shape(&self) -> R<()> {
        if self.factory == Address::ZERO || self.weth == Address::ZERO {
            return Err(ConfigError::ZeroFactory);
        }
        if self.catalog != CATALOG_MARKET || self.first_market != FIRST_VAULT_MARKET {
            return Err(ConfigError::MarketRange);
        }
        if self.protocol != ProtocolId(10) {
            return Err(ConfigError::MalformedToml);
        }
        let mut seen: Vec<Address> = Vec::with_capacity(self.vault_pins.len());
        let mut ids: Vec<u32> = Vec::with_capacity(self.vault_pins.len());
        for p in &self.vault_pins {
            if p.vault == Address::ZERO || seen.contains(&p.vault) {
                return Err(ConfigError::DuplicateAddress(p.vault));
            }
            seen.push(p.vault);
            if ids.contains(&p.vault_id) {
                return Err(ConfigError::DuplicateMarket(p.market()));
            }
            ids.push(p.vault_id);
            if ![VAULT_T1, VAULT_T2, VAULT_T3, VAULT_T4].contains(&p.vault_type) {
                return Err(ConfigError::BadVaultType);
            }
            if p.vault_id == 0 || p.market().0 > LAST_VAULT_MARKET.0 {
                return Err(ConfigError::MarketRange);
            }
        }
        for (i, a) in self.assets.iter().enumerate() {
            if self
                .assets
                .iter()
                .skip(i.saturating_add(1))
                .any(|b| b.asset == a.asset || b.underlying == a.underlying)
            {
                return Err(ConfigError::DuplicateAsset(a.asset));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> R<()> {
        self.validate_shape()
    }

    /// Read every vault the factory has deployed, at `block`: `TYPE()` (T1
    /// has none), `constantsView()` in the type's shape, token decimals on
    /// smart sides. `asset_of` is the registry intern; a token it does not
    /// know stays unmapped (that vault side is not quoted), native ETH is
    /// mapped as WETH. A vault whose reads fail is left out and logged.
    pub fn bind_live<P: FactoryRpc>(
        &mut self,
        rpc: &P,
        block: BlockNum,
        asset_of: &dyn Fn(Address) -> Option<AssetId>,
    ) -> R<()> {
        self.live_bound = false;
        self.validate_shape()?;
        let tv = rpc.eth_call(
            self.factory,
            &factory::totalVaultsCall {}.abi_encode(),
            block,
        )?;
        let total = factory::totalVaultsCall::abi_decode_returns(&tv)
            .map_err(|_| ConfigError::FactoryCall(self.factory))?;
        let total = u32::try_from(total).map_err(|_| ConfigError::FactoryCall(self.factory))?;
        if total == 0 {
            return Err(ConfigError::NoVaults);
        }
        let ids: Vec<u32> = (1..=total).collect();
        let addr_calls: Vec<(Address, Vec<u8>)> = ids
            .iter()
            .map(|&i| {
                (
                    self.factory,
                    factory::getVaultAddressCall {
                        vaultId: U256::from(i),
                    }
                    .abi_encode(),
                )
            })
            .collect();
        let addrs = aggregate(rpc, &addr_calls, block)?;
        let mut vaults = Vec::with_capacity(ids.len());
        for (&id, r) in ids.iter().zip(&addrs) {
            let Some(a) = r
                .as_ref()
                .and_then(|b| factory::getVaultAddressCall::abi_decode_returns(b).ok())
                .filter(|a| !a.is_zero())
            else {
                tracing::warn!(
                    vault_id = id,
                    "fluid getVaultAddress failed — vault left out"
                );
                continue;
            };
            vaults.push((id, a));
        }
        let mut calls = Vec::with_capacity(vaults.len().saturating_mul(2));
        for &(_, v) in &vaults {
            calls.push((v, vault::TYPECall {}.abi_encode()));
            calls.push((v, t1::constantsViewCall {}.abi_encode()));
        }
        let res = aggregate(rpc, &calls, block)?;
        let mut pins = Vec::with_capacity(vaults.len());
        let mut need_decimals: Vec<Address> = Vec::new();
        for (k, &(id, v)) in vaults.iter().enumerate() {
            let ty = res
                .get(k.saturating_mul(2))
                .and_then(Option::as_ref)
                .and_then(|b| vault::TYPECall::abi_decode_returns(b).ok());
            let Some(cv) = res
                .get(k.saturating_mul(2).saturating_add(1))
                .and_then(Option::as_ref)
            else {
                tracing::warn!(vault = %v, "fluid constantsView failed — vault left out");
                continue;
            };
            let pin = match ty {
                None => decode_t1(v, id, cv),
                Some(t) => decode_smart(v, id, t, cv),
            };
            match pin {
                Ok(p) if p.market().0 > LAST_VAULT_MARKET.0 => tracing::warn!(
                    target: "coverage",
                    vault = %v,
                    vault_id = id,
                    "fluid vault id past the 4001..=4199 MarketId band — left out"
                ),
                Ok(p) => {
                    for t in [p.supply0, p.supply1, p.borrow0, p.borrow1] {
                        if !t.is_zero() && t != NATIVE_TOKEN && !need_decimals.contains(&t) {
                            need_decimals.push(t);
                        }
                    }
                    pins.push(p);
                }
                Err(e) => tracing::warn!(vault = %v, error = %e, "fluid vault left out"),
            }
        }
        // Decimals from the token itself: smart-side tokens have none in
        // `constantsView`, and every mapped asset needs them.
        let dec_calls: Vec<(Address, Vec<u8>)> = need_decimals
            .iter()
            .map(|&t| (t, erc20::decimalsCall {}.abi_encode()))
            .collect();
        let dec_res = aggregate(rpc, &dec_calls, block)?;
        let mut decimals: Vec<(Address, u8)> = Vec::with_capacity(need_decimals.len());
        for (&t, r) in need_decimals.iter().zip(&dec_res) {
            if let Some(d) = r
                .as_ref()
                .and_then(|b| erc20::decimalsCall::abi_decode_returns(b).ok())
            {
                decimals.push((t, d));
            }
        }
        let dec_of = |t: Address| -> Option<u8> {
            if t == NATIVE_TOKEN {
                return Some(18);
            }
            decimals.iter().find(|(a, _)| *a == t).map(|(_, d)| *d)
        };
        for p in &mut pins {
            for (tok, d) in [
                (p.supply0, &mut p.supply_decimals0),
                (p.supply1, &mut p.supply_decimals1),
                (p.borrow0, &mut p.borrow_decimals0),
                (p.borrow1, &mut p.borrow_decimals1),
            ] {
                if !tok.is_zero() && *d == 0 {
                    *d = dec_of(tok).unwrap_or(0);
                }
            }
        }
        let mut assets: Vec<AssetConfig> = Vec::new();
        let weth_asset = asset_of(self.weth);
        for p in &pins {
            for (tok, d) in p.col_tokens().into_iter().chain(p.debt_tokens()) {
                let (underlying, asset) = if tok == NATIVE_TOKEN {
                    (self.weth, weth_asset)
                } else {
                    (tok, asset_of(tok))
                };
                let Some(asset) = asset else {
                    continue;
                };
                if d == 0 && tok != NATIVE_TOKEN {
                    continue;
                }
                if assets.iter().any(|a| a.underlying == underlying) {
                    continue;
                }
                assets.push(AssetConfig {
                    underlying,
                    asset,
                    feed: FeedId(0),
                    decimals: if tok == NATIVE_TOKEN { 18 } else { d },
                });
            }
        }
        let mapped = pins
            .iter()
            .filter(|p| {
                let m = |t: Address| {
                    let u = if t == NATIVE_TOKEN { self.weth } else { t };
                    assets.iter().any(|a| a.underlying == u)
                };
                p.col_tokens().iter().any(|(t, _)| m(*t))
                    && p.debt_tokens().iter().any(|(t, _)| m(*t))
            })
            .count();
        tracing::info!(
            block,
            vaults = total,
            bound = pins.len(),
            quotable = mapped,
            tokens = assets.len(),
            "fluid vaults read from chain"
        );
        self.vault_pins = pins;
        self.assets = assets;
        self.validate_shape()?;
        self.live_bound = true;
        Ok(())
    }

    #[inline]
    #[must_use]
    pub fn pin_of(&self, vault: Address) -> Option<&VaultPin> {
        self.vault_pins.iter().find(|p| p.vault == vault)
    }

    #[inline]
    #[must_use]
    pub fn pin_index(&self, vault: Address) -> Option<usize> {
        self.vault_pins.iter().position(|p| p.vault == vault)
    }

    #[inline]
    #[must_use]
    pub fn pin_by_market(&self, market: MarketId) -> Option<&VaultPin> {
        self.vault_pins.iter().find(|p| p.market() == market)
    }

    #[inline]
    #[must_use]
    pub fn asset_by_underlying(&self, underlying: Address) -> Option<&AssetConfig> {
        self.assets.iter().find(|a| a.underlying == underlying)
    }

    /// The asset a vault token is priced and routed as (native → WETH).
    #[inline]
    #[must_use]
    pub fn asset_of_token(&self, token: Address) -> Option<&AssetConfig> {
        if token == NATIVE_TOKEN {
            self.asset_by_underlying(self.weth)
        } else {
            self.asset_by_underlying(token)
        }
    }

    #[inline]
    #[must_use]
    pub fn underlying_of(&self, asset: AssetId) -> Option<Address> {
        self.assets
            .iter()
            .find(|a| a.asset == asset)
            .map(|a| a.underlying)
    }

    pub fn from_toml(raw: &str) -> R<Self> {
        let f: TomlFile = toml::from_str(raw).map_err(|_| ConfigError::MalformedToml)?;
        let cfg = Self {
            protocol: ProtocolId(f.protocol),
            factory: parse_addr(&f.factory)?,
            catalog: MarketId(f.catalog),
            first_market: MarketId(f.first_market),
            weth: parse_addr(&f.weth)?,
            vault_pins: Vec::new(),
            assets: Vec::new(),
            live_bound: false,
        };
        cfg.validate_shape()?;
        Ok(cfg)
    }
}

fn decode_t1(v: Address, id: u32, raw: &Bytes) -> R<VaultPin> {
    let c =
        t1::constantsViewCall::abi_decode_returns(raw).map_err(|_| ConfigError::VaultDecode(v))?;
    if u32::try_from(c.vaultId).ok() != Some(id) {
        return Err(ConfigError::VaultDecode(v));
    }
    Ok(VaultPin {
        vault: v,
        vault_id: id,
        vault_type: VAULT_T1,
        supply: c.liquidity,
        borrow: c.liquidity,
        supply0: c.supplyToken,
        supply1: Address::ZERO,
        borrow0: c.borrowToken,
        borrow1: Address::ZERO,
        supply_decimals0: c.supplyDecimals,
        supply_decimals1: 0,
        borrow_decimals0: c.borrowDecimals,
        borrow_decimals1: 0,
    })
}

fn decode_smart(v: Address, id: u32, ty: U256, raw: &Bytes) -> R<VaultPin> {
    let c = smart::constantsViewCall::abi_decode_returns(raw)
        .map_err(|_| ConfigError::VaultDecode(v))?;
    let t = u32::try_from(ty).map_err(|_| ConfigError::BadVaultType)?;
    if ![VAULT_T1, VAULT_T2, VAULT_T3, VAULT_T4].contains(&t)
        || u32::try_from(c.vaultId).ok() != Some(id)
        || c.vaultType != ty
    {
        return Err(ConfigError::VaultDecode(v));
    }
    Ok(VaultPin {
        vault: v,
        vault_id: id,
        vault_type: t,
        supply: c.supply,
        borrow: c.borrow,
        supply0: c.supplyToken.token0,
        supply1: c.supplyToken.token1,
        borrow0: c.borrowToken.token0,
        borrow1: c.borrowToken.token1,
        supply_decimals0: 0,
        supply_decimals1: 0,
        borrow_decimals0: 0,
        borrow_decimals1: 0,
    })
}

/// `aggregate3` with `allowFailure`, in batches. `None` = that call reverted.
fn aggregate<P: FactoryRpc>(
    rpc: &P,
    calls: &[(Address, Vec<u8>)],
    block: BlockNum,
) -> R<Vec<Option<Bytes>>> {
    let mut out = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(BIND_BATCH) {
        let c3: Vec<multicall::Call3> = chunk
            .iter()
            .map(|(to, data)| multicall::Call3 {
                target: *to,
                allowFailure: true,
                callData: Bytes::copy_from_slice(data),
            })
            .collect();
        let raw = rpc.eth_call(
            MULTICALL3,
            &multicall::aggregate3Call { calls: c3 }.abi_encode(),
            block,
        )?;
        let res = multicall::aggregate3Call::abi_decode_returns(&raw)
            .map_err(|_| ConfigError::FactoryCall(MULTICALL3))?;
        if res.len() != chunk.len() {
            return Err(ConfigError::FactoryCall(MULTICALL3));
        }
        out.extend(res.into_iter().map(|r| r.success.then_some(r.returnData)));
    }
    Ok(out)
}

fn parse_addr(s: &str) -> R<Address> {
    s.parse().map_err(|_| ConfigError::MalformedToml)
}

#[derive(serde::Deserialize)]
struct TomlFile {
    protocol: u16,
    factory: String,
    catalog: u32,
    first_market: u32,
    weth: String,
}
