//! Deployment pin: the Gearbox v3.1 `AddressProviderV3_1`. Managers are
//! discovered live — `MARKET_CONFIGURATOR_FACTORY` → every market
//! configurator → its `contractsRegister()` → `getCreditManagers()`, keeping
//! `version()` 310..=399 — never a hand list. (The v3.0 register
//! `0xA50d…4D99` still answers, but every v3.0 manager has zero debt: Gearbox
//! lends through v3.1 markets now.) A bare `register` (no address provider)
//! is the single-register path the conformance fixtures use. `fees()` is
//! live-asserted; [`crate::GearboxV3::new`] refuses a config that has not
//! passed [`Config::assert_live_registry`].
//!
//! # MarketId allocator (intern-global)
//!
//! `StateStore.market_index` is `MarketId → row` with no `ProtocolId`.
//! Gearbox is not in `registry.json`. Locked range:
//! - ProtocolId **11**
//! - Catalog (register index, not a manager): [`CATALOG_MARKET`] = 4200
//! - Credit managers sequential from [`FIRST_MANAGER_MARKET`] = 4201
//! - Inclusive last: [`LAST_MANAGER_MARKET`] = 4299
//!
//! Never intern 0..=3480, 3481..=4199, or 4300+.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use liq_config::Intern;
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::events::views::{
    IAddressProviderV31, IContractsRegister, ICreditFacadeV3, ICreditManagerV3,
    IMarketConfigurator, IMarketConfiguratorFactory, IPoolV3, IVersion, IERC20,
};
use crate::layout::UNMAPPED_ASSET;
use crate::math::PERCENTAGE_FACTOR;

/// Locked protocol id (allocator).
pub const PROTOCOL: ProtocolId = ProtocolId(11);
/// ContractsRegister catalog. Not a credit manager.
pub const CATALOG_MARKET: MarketId = MarketId(4200);
/// First credit-manager MarketId.
pub const FIRST_MANAGER_MARKET: MarketId = MarketId(4201);
/// Inclusive end of the Gearbox band.
pub const LAST_MANAGER_MARKET: MarketId = MarketId(4299);
/// D15 cardinality on the v3.0 register (historical; v3.0 holds no debt).
pub const D15_MANAGER_COUNT: u32 = 34;
/// Manager versions discovered through the v3.1 address provider.
pub const MIN_MANAGER_VERSION: u64 = 310;
pub const MAX_MANAGER_VERSION: u64 = 399;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fees {
    pub fee_interest: u16,
    pub fee_liquidation: u16,
    pub liquidation_discount: u16,
    pub fee_liquidation_expired: u16,
    pub liquidation_discount_expired: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenConfig {
    pub token: Address,
    pub mask: u64,
    pub slot: u16,
    pub lt_initial: u16,
    pub lt_final: u16,
    pub ramp_start: u64,
    pub ramp_duration: u32,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagerConfig {
    pub market: MarketId,
    pub manager: Address,
    pub facade: Address,
    pub pool: Address,
    pub configurator: Address,
    pub factory: Address,
    pub quota_keeper: Address,
    pub underlying: Address,
    pub fees: Fees,
    pub lt_underlying: u16,
    pub expirable: bool,
    pub expiration_date: u64,
    pub quoted_tokens_mask: u64,
    /// Facade `debtLimits().minDebt` (underlying units).
    pub min_debt: u128,
    pub tokens: Vec<TokenConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    /// Gearbox v3.1 address provider. Non-zero → discover every market
    /// configurator's register; zero → the single `register` below.
    pub address_provider: Address,
    /// Single-register path (fixtures), or zero when `address_provider` is set.
    pub register: Address,
    /// Every register discovered (v3.1) or `[register]`. Log halts subscribe
    /// to each.
    pub registers: Vec<Address>,
    pub catalog: MarketId,
    pub first_market: MarketId,
    pub expected_managers: u32,
    pub managers: Vec<ManagerConfig>,
    pub assets: Vec<AssetConfig>,
    pub pinned_through: BlockNum,
    /// False until [`Self::assert_live_registry`] succeeds.
    pub live_fees_asserted: bool,
    /// v3.1 managers discovery skipped, with why (the binder logs them).
    pub skipped: Vec<(Address, ConfigError)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Register,
    Facade(usize),
    Manager(usize),
    Configurator(usize),
    Pool(usize),
    Factory(usize),
    Quota(usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("contracts register is the zero address")]
    ZeroRegister,
    #[error("protocol id is not 11")]
    ProtocolMismatch,
    #[error("catalog / first_market outside 4200..=4299")]
    MarketOutOfRange,
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("live getCreditManagers len {found} != expected {expected}")]
    ManagerCount { expected: u32, found: u32 },
    #[error("too many credit managers for 4201..=4299")]
    TooManyManagers,
    #[error("fees() discount is zero or above PERCENTAGE_FACTOR")]
    FeesBounds,
    #[error("eth_call failed at {0}")]
    RegistryCall(Address),
    #[error("live fees() was not asserted")]
    LiveFeesUnasserted,
    #[error("expirationDate or LT ramp does not fit store width")]
    TruncatingParam,
    #[error("underlying {0} is not interned")]
    UnknownUnderlying(Address),
    #[error("protocol toml is malformed")]
    MalformedToml,
}

/// Synchronous `eth_call` at a block. Boot-only; not on the hot path.
pub trait RegistryRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, ConfigError>;
}

impl Config {
    fn validate_shape(&self) -> core::result::Result<(), ConfigError> {
        if self.protocol != PROTOCOL {
            return Err(ConfigError::ProtocolMismatch);
        }
        if self.register == Address::ZERO && self.address_provider == Address::ZERO {
            return Err(ConfigError::ZeroRegister);
        }
        if !in_band(self.catalog) || !in_band(self.first_market) {
            return Err(ConfigError::MarketOutOfRange);
        }
        if self.catalog != CATALOG_MARKET || self.first_market != FIRST_MANAGER_MARKET {
            return Err(ConfigError::MarketOutOfRange);
        }
        let mut unique: Vec<Address> = vec![self.register, self.address_provider];
        let mut markets: Vec<MarketId> = Vec::new();
        for m in &self.managers {
            if !in_band(m.market) || m.market == CATALOG_MARKET {
                return Err(ConfigError::MarketOutOfRange);
            }
            if markets.contains(&m.market) {
                return Err(ConfigError::DuplicateMarket(m.market));
            }
            markets.push(m.market);
            // Pools / factories / quota keepers are shared across managers (D15:
            // 34 managers, 16 pools). Uniqueness is per manager + facade +
            // configurator.
            for a in [m.manager, m.facade, m.configurator] {
                if a == Address::ZERO {
                    return Err(ConfigError::ZeroRegister);
                }
                if unique.contains(&a) {
                    return Err(ConfigError::DuplicateAddress(a));
                }
                unique.push(a);
            }
            if m.pool == Address::ZERO || m.factory == Address::ZERO {
                return Err(ConfigError::ZeroRegister);
            }
            check_fees(&m.fees)?;
        }
        for (i, a) in self.assets.iter().enumerate() {
            if a.underlying == Address::ZERO {
                return Err(ConfigError::ZeroRegister);
            }
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

    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        self.validate_shape()
    }

    /// `eth_call` `getCreditManagers()` then per-manager `fees()` / `underlying()`
    /// / facade / tokens. Mismatch or RPC failure → `Err` and
    /// [`Self::live_fees_asserted`] stays false. Success is the only path
    /// that sets the flag true. D15 `expected_managers` is a cardinality
    /// check, not a hand list.
    pub fn assert_live_registry<R: RegistryRpc>(
        &mut self,
        provider: &R,
        block: BlockNum,
    ) -> core::result::Result<(), ConfigError> {
        self.live_fees_asserted = false;
        self.managers.clear();
        self.registers.clear();
        self.skipped.clear();
        self.validate_shape()?;
        let managers: Vec<(Address, Address)> = if self.address_provider == Address::ZERO {
            let list = register_managers(provider, self.register, block)?;
            let found = u32::try_from(list.len()).map_err(|_| ConfigError::TooManyManagers)?;
            if found != self.expected_managers {
                return Err(ConfigError::ManagerCount {
                    expected: self.expected_managers,
                    found,
                });
            }
            self.registers.push(self.register);
            list.into_iter().map(|m| (self.register, m)).collect()
        } else {
            self.discover_v31(provider, block)?
        };
        let found = u32::try_from(managers.len()).map_err(|_| ConfigError::TooManyManagers)?;
        if found == 0 {
            return Err(ConfigError::ManagerCount {
                expected: self.expected_managers,
                found: 0,
            });
        }
        let last = u64::from(self.first_market.0)
            .checked_add(u64::from(found))
            .and_then(|v| v.checked_sub(1))
            .ok_or(ConfigError::TooManyManagers)?;
        if last > u64::from(LAST_MANAGER_MARKET.0) {
            return Err(ConfigError::TooManyManagers);
        }
        let lenient = self.address_provider != Address::ZERO;
        for (register, manager) in managers {
            if manager == Address::ZERO {
                return Err(ConfigError::RegistryCall(register));
            }
            let is_cm = decode_bool(
                provider,
                register,
                block,
                &IContractsRegister::isCreditManagerCall(manager).abi_encode(),
            )?;
            if !is_cm {
                return Err(ConfigError::RegistryCall(manager));
            }
            let loaded =
                u32::try_from(self.managers.len()).map_err(|_| ConfigError::TooManyManagers)?;
            let market = MarketId(
                self.first_market
                    .0
                    .checked_add(loaded)
                    .ok_or(ConfigError::TooManyManagers)?,
            );
            if market > LAST_MANAGER_MARKET {
                return Err(ConfigError::TooManyManagers);
            }
            match load_manager(provider, block, manager, market, &self.assets) {
                Ok(m) => self.managers.push(m),
                // v3.1 discovery: one manager this store cannot hold (e.g.
                // > 64 collateral tokens) is skipped, not the whole family.
                Err(e) if lenient => self.skipped.push((manager, e)),
                Err(e) => return Err(e),
            }
        }
        self.validate_shape()?;
        self.live_fees_asserted = true;
        Ok(())
    }

    /// v3.1: every `(register, manager)` with `version()` in
    /// [`MIN_MANAGER_VERSION`]..=[`MAX_MANAGER_VERSION`], in configurator
    /// then register order. Records each register in [`Self::registers`].
    fn discover_v31<R: RegistryRpc>(
        &mut self,
        provider: &R,
        block: BlockNum,
    ) -> core::result::Result<Vec<(Address, Address)>, ConfigError> {
        let mut key = [0u8; 32];
        let name = b"MARKET_CONFIGURATOR_FACTORY";
        key.get_mut(..name.len())
            .ok_or(ConfigError::MalformedToml)?
            .copy_from_slice(name);
        let factory = decode_addr(
            provider,
            self.address_provider,
            block,
            &IAddressProviderV31::getAddressOrRevertCall {
                key: key.into(),
                ver: U256::ZERO,
            }
            .abi_encode(),
        )?;
        let raw = provider.eth_call(
            factory,
            &IMarketConfiguratorFactory::getMarketConfiguratorsCall {}.abi_encode(),
            block,
        )?;
        let configurators =
            IMarketConfiguratorFactory::getMarketConfiguratorsCall::abi_decode_returns(&raw)
                .map_err(|_| ConfigError::RegistryCall(factory))?;
        let mut out = Vec::new();
        for mc in configurators {
            let register = decode_addr(
                provider,
                mc,
                block,
                &IMarketConfigurator::contractsRegisterCall {}.abi_encode(),
            )?;
            if register == Address::ZERO || self.registers.contains(&register) {
                continue;
            }
            self.registers.push(register);
            for manager in register_managers(provider, register, block)? {
                let ver = decode_u64(
                    provider,
                    manager,
                    block,
                    &IVersion::versionCall {}.abi_encode(),
                )?;
                if (MIN_MANAGER_VERSION..=MAX_MANAGER_VERSION).contains(&ver) {
                    out.push((register, manager));
                }
            }
        }
        if out.is_empty() {
            return Err(ConfigError::ManagerCount {
                expected: 1,
                found: 0,
            });
        }
        Ok(out)
    }

    /// Fill [`Self::assets`] from intern by on-chain token address. Missing
    /// tokens stay unmapped; health fails closed (`OracleSourceMismatch`).
    pub fn bind_assets_from_intern(
        &mut self,
        intern: &Intern,
    ) -> core::result::Result<(), ConfigError> {
        let mut assets = Vec::new();
        for m in &self.managers {
            for t in &m.tokens {
                if assets.iter().any(|a: &AssetConfig| a.underlying == t.token) {
                    continue;
                }
                if let Some(id) = intern.asset(t.token) {
                    let rec = intern
                        .asset_rec(id)
                        .ok_or(ConfigError::UnknownUnderlying(t.token))?;
                    assets.push(AssetConfig {
                        underlying: t.token,
                        asset: id,
                        // FeedId(0) is the first interned Aave oracle, not unset.
                        // Gearbox oracles are not joined via row.price_feed.
                        feed: FeedId(0),
                        decimals: rec.decimals,
                    });
                }
            }
        }
        // Tokens were loaded before the intern join: map them now, so health
        // prices them instead of failing closed as unmapped.
        for m in &mut self.managers {
            for t in &mut m.tokens {
                if let Some(a) = assets.iter().find(|a| a.underlying == t.token) {
                    t.asset = a.asset;
                    t.feed = a.feed;
                    t.decimals = a.decimals;
                }
            }
        }
        self.assets = assets;
        self.validate_shape()
    }

    #[inline]
    pub(crate) fn manager(&self, i: usize) -> Option<&ManagerConfig> {
        self.managers.get(i)
    }

    #[inline]
    pub(crate) fn manager_by_market(&self, market: MarketId) -> Option<(usize, &ManagerConfig)> {
        self.managers
            .iter()
            .enumerate()
            .find(|(_, m)| m.market == market)
    }

    #[inline]
    pub(crate) fn manager_by_addr(&self, manager: Address) -> Option<(usize, &ManagerConfig)> {
        self.managers
            .iter()
            .enumerate()
            .find(|(_, m)| m.manager == manager)
    }

    #[inline]
    pub(crate) fn underlying_of(&self, asset: AssetId) -> Option<Address> {
        self.assets
            .iter()
            .find(|a| a.asset == asset)
            .map(|a| a.underlying)
    }

    pub(crate) fn emitter(&self, address: Address) -> Option<Emitter> {
        if address == self.register || self.registers.contains(&address) {
            return Some(Emitter::Register);
        }
        for (i, m) in self.managers.iter().enumerate() {
            if address == m.facade {
                return Some(Emitter::Facade(i));
            }
            if address == m.manager {
                return Some(Emitter::Manager(i));
            }
            if address == m.configurator {
                return Some(Emitter::Configurator(i));
            }
            if address == m.pool {
                return Some(Emitter::Pool(i));
            }
            if address == m.factory {
                return Some(Emitter::Factory(i));
            }
            if address == m.quota_keeper {
                return Some(Emitter::Quota(i));
            }
        }
        None
    }

    /// Parse `config/protocols/gearbox.toml`. Managers are empty; bind via
    /// [`Self::assert_live_registry`] before [`crate::GearboxV3::new`].
    pub fn from_toml(raw: &str) -> core::result::Result<Self, ConfigError> {
        let f: TomlFile = toml::from_str(raw).map_err(|_| ConfigError::MalformedToml)?;
        let opt = |s: &Option<String>| s.as_deref().map_or(Ok(Address::ZERO), parse_addr);
        let cfg = Self {
            protocol: ProtocolId(f.protocol),
            address_provider: opt(&f.address_provider)?,
            register: opt(&f.register)?,
            registers: Vec::new(),
            catalog: MarketId(f.catalog),
            first_market: MarketId(f.first_market),
            expected_managers: f.expected_managers,
            managers: Vec::new(),
            assets: Vec::new(),
            pinned_through: f.pinned_through,
            live_fees_asserted: false,
            skipped: Vec::new(),
        };
        cfg.validate_shape()?;
        Ok(cfg)
    }
}

impl ManagerConfig {
    #[inline]
    pub(crate) fn token(&self, addr: Address) -> Option<&TokenConfig> {
        self.tokens.iter().find(|t| t.token == addr)
    }
}

#[inline]
fn in_band(id: MarketId) -> bool {
    id.0 >= CATALOG_MARKET.0 && id.0 <= LAST_MANAGER_MARKET.0
}

fn check_fees(f: &Fees) -> core::result::Result<(), ConfigError> {
    if f.liquidation_discount == 0
        || U256::from(f.liquidation_discount) > PERCENTAGE_FACTOR
        || f.liquidation_discount_expired == 0
        || U256::from(f.liquidation_discount_expired) > PERCENTAGE_FACTOR
        || f.fee_liquidation >= f.liquidation_discount
        || f.fee_liquidation_expired >= f.liquidation_discount_expired
    {
        return Err(ConfigError::FeesBounds);
    }
    Ok(())
}

fn parse_addr(s: &str) -> core::result::Result<Address, ConfigError> {
    s.parse().map_err(|_| ConfigError::MalformedToml)
}

fn load_manager<R: RegistryRpc>(
    provider: &R,
    block: BlockNum,
    manager: Address,
    market: MarketId,
    assets: &[AssetConfig],
) -> core::result::Result<ManagerConfig, ConfigError> {
    let underlying = decode_addr(
        provider,
        manager,
        block,
        &ICreditManagerV3::underlyingCall {}.abi_encode(),
    )?;
    let facade = decode_addr(
        provider,
        manager,
        block,
        &ICreditManagerV3::creditFacadeCall {}.abi_encode(),
    )?;
    let configurator = decode_addr(
        provider,
        manager,
        block,
        &ICreditManagerV3::creditConfiguratorCall {}.abi_encode(),
    )?;
    let factory = decode_addr(
        provider,
        manager,
        block,
        &ICreditManagerV3::accountFactoryCall {}.abi_encode(),
    )?;
    let pool = decode_addr(
        provider,
        manager,
        block,
        &ICreditManagerV3::poolCall {}.abi_encode(),
    )?;
    let quota_keeper = decode_addr(
        provider,
        pool,
        block,
        &IPoolV3::poolQuotaKeeperCall {}.abi_encode(),
    )?;
    let fees_raw =
        provider.eth_call(manager, &ICreditManagerV3::feesCall {}.abi_encode(), block)?;
    let fees_t = ICreditManagerV3::feesCall::abi_decode_returns(&fees_raw)
        .map_err(|_| ConfigError::RegistryCall(manager))?;
    let fees = Fees {
        fee_interest: fees_t.feeInterest,
        fee_liquidation: fees_t.feeLiquidation,
        liquidation_discount: fees_t.liquidationDiscount,
        fee_liquidation_expired: fees_t.feeLiquidationExpired,
        liquidation_discount_expired: fees_t.liquidationDiscountExpired,
    };
    check_fees(&fees)?;
    let count = decode_u8(
        provider,
        manager,
        block,
        &ICreditManagerV3::collateralTokensCountCall {}.abi_encode(),
    )?;
    if count == 0 {
        return Err(ConfigError::RegistryCall(manager));
    }
    // Token masks are stored as u64.
    if count > 64 {
        return Err(ConfigError::TruncatingParam);
    }
    let mut tokens = Vec::with_capacity(usize::from(count));
    for i in 0u8..count {
        let mask = 1u64
            .checked_shl(u32::from(i))
            .ok_or(ConfigError::RegistryCall(manager))?;
        let token = decode_addr(
            provider,
            manager,
            block,
            &ICreditManagerV3::getTokenByMaskCall {
                tokenMask: U256::from(mask),
            }
            .abi_encode(),
        )?;
        let lt_raw = provider.eth_call(
            manager,
            &ICreditManagerV3::ltParamsCall { token }.abi_encode(),
            block,
        )?;
        let lt_t = ICreditManagerV3::ltParamsCall::abi_decode_returns(&lt_raw)
            .map_err(|_| ConfigError::RegistryCall(manager))?;
        // Pin static LT is `type(uint40).max` (`2^40-1` > `u32::MAX`). Store as
        // u64; `get_liquidation_threshold` uses CreditLogic `now <= start`.
        let ramp_start = u64::try_from(U256::from(lt_t.timestampRampStart))
            .map_err(|_| ConfigError::TruncatingParam)?;
        let ramp_duration = u32::try_from(U256::from(lt_t.rampDuration))
            .map_err(|_| ConfigError::TruncatingParam)?;
        let (asset, feed, decimals) = match assets.iter().find(|a| a.underlying == token) {
            Some(a) => (a.asset, a.feed, a.decimals),
            None => {
                let decimals = decode_u8(
                    provider,
                    token,
                    block,
                    &IERC20::decimalsCall {}.abi_encode(),
                )?;
                (UNMAPPED_ASSET, FeedId(0), decimals)
            }
        };
        tokens.push(TokenConfig {
            token,
            mask,
            slot: u16::from(i),
            lt_initial: lt_t.ltInitial,
            lt_final: lt_t.ltFinal,
            ramp_start,
            ramp_duration,
            asset,
            feed,
            decimals,
        });
    }
    // G2. `ltParams(underlying)` reads `collateralTokensData[1]`, a slot
    // `CreditConfiguratorV3.setCollateralTokenData` (pin `510fc654`) reverts
    // on ever writing for the underlying — so that slot is permanently zero,
    // and `tokens.first().lt_initial` (the underlying's own listing entry)
    // always read as 0 regardless of the deployment's real threshold.
    //
    // The live value is the immutable identity `CreditManagerV3.sol:184`:
    //   ltUnderlying = PERCENTAGE_FACTOR - liquidationPremium - feeLiquidation
    //                = liquidationDiscount - feeLiquidation
    // (`liquidationDiscount = PERCENTAGE_FACTOR - liquidationPremium`, the
    // field this adapter already reads as `fees.liquidation_discount`). The
    // identity cannot drift: `CreditConfiguratorV3.sol:424` reverts
    // `InconsistentLiquidationFeesException` on any `setFees` that would
    // change it, so re-deriving it from a fresher `fees` reading is always
    // exact, never a stale snapshot.
    let lt_underlying = u16::try_from(
        U256::from(fees.liquidation_discount)
            .checked_sub(U256::from(fees.fee_liquidation))
            .ok_or(ConfigError::RegistryCall(manager))?,
    )
    .map_err(|_| ConfigError::RegistryCall(manager))?;
    let quoted_raw = provider.eth_call(
        manager,
        &ICreditManagerV3::quotedTokensMaskCall {}.abi_encode(),
        block,
    )?;
    let quoted_u = ICreditManagerV3::quotedTokensMaskCall::abi_decode_returns(&quoted_raw)
        .map_err(|_| ConfigError::RegistryCall(manager))?;
    // v3.1 marks every non-underlying token quoted (`quotedTokensMask` =
    // 2^256 − 2); only this manager's `count` bits are meaningful.
    let width = if count == 64 {
        U256::from(u64::MAX)
    } else {
        U256::from((1u64 << count).wrapping_sub(1))
    };
    let quoted_tokens_mask =
        u64::try_from(quoted_u & width).map_err(|_| ConfigError::TruncatingParam)?;
    let expirable = decode_bool(
        provider,
        facade,
        block,
        &ICreditFacadeV3::expirableCall {}.abi_encode(),
    )?;
    let expiration_date = decode_u64(
        provider,
        facade,
        block,
        &ICreditFacadeV3::expirationDateCall {}.abi_encode(),
    )?;
    if expiration_date > u64::from(u32::MAX) {
        return Err(ConfigError::TruncatingParam);
    }
    let limits_raw = provider.eth_call(
        facade,
        &ICreditFacadeV3::debtLimitsCall {}.abi_encode(),
        block,
    )?;
    let min_debt = ICreditFacadeV3::debtLimitsCall::abi_decode_returns(&limits_raw)
        .map_err(|_| ConfigError::RegistryCall(facade))?
        .minDebt;
    Ok(ManagerConfig {
        market,
        manager,
        facade,
        pool,
        configurator,
        factory,
        quota_keeper,
        underlying,
        fees,
        lt_underlying,
        expirable,
        expiration_date,
        quoted_tokens_mask,
        min_debt,
        tokens,
    })
}

fn decode_addr<R: RegistryRpc>(
    rpc: &R,
    to: Address,
    block: BlockNum,
    data: &[u8],
) -> core::result::Result<Address, ConfigError> {
    let raw = rpc.eth_call(to, data, block)?;
    if raw.len() < 32 {
        return Err(ConfigError::RegistryCall(to));
    }
    let slice = raw.get(12..32).ok_or(ConfigError::RegistryCall(to))?;
    Address::try_from(slice).map_err(|_| ConfigError::RegistryCall(to))
}

fn decode_bool<R: RegistryRpc>(
    rpc: &R,
    to: Address,
    block: BlockNum,
    data: &[u8],
) -> core::result::Result<bool, ConfigError> {
    let raw = rpc.eth_call(to, data, block)?;
    if raw.len() < 32 {
        return Err(ConfigError::RegistryCall(to));
    }
    Ok(*raw.get(31).ok_or(ConfigError::RegistryCall(to))? != 0)
}

fn decode_u8<R: RegistryRpc>(
    rpc: &R,
    to: Address,
    block: BlockNum,
    data: &[u8],
) -> core::result::Result<u8, ConfigError> {
    let raw = rpc.eth_call(to, data, block)?;
    if raw.len() < 32 {
        return Err(ConfigError::RegistryCall(to));
    }
    Ok(*raw.get(31).ok_or(ConfigError::RegistryCall(to))?)
}

fn decode_u64<R: RegistryRpc>(
    rpc: &R,
    to: Address,
    block: BlockNum,
    data: &[u8],
) -> core::result::Result<u64, ConfigError> {
    let raw = rpc.eth_call(to, data, block)?;
    if raw.len() < 32 {
        return Err(ConfigError::RegistryCall(to));
    }
    let mut b = [0u8; 8];
    let slice = raw.get(24..32).ok_or(ConfigError::RegistryCall(to))?;
    b.copy_from_slice(slice);
    Ok(u64::from_be_bytes(b))
}

#[derive(serde::Deserialize)]
struct TomlFile {
    protocol: u16,
    #[serde(default)]
    address_provider: Option<String>,
    #[serde(default)]
    register: Option<String>,
    catalog: u32,
    first_market: u32,
    #[serde(default)]
    expected_managers: u32,
    pinned_through: u64,
}

fn register_managers<R: RegistryRpc>(
    provider: &R,
    register: Address,
    block: BlockNum,
) -> core::result::Result<Vec<Address>, ConfigError> {
    let raw = provider.eth_call(
        register,
        &IContractsRegister::getCreditManagersCall {}.abi_encode(),
        block,
    )?;
    IContractsRegister::getCreditManagersCall::abi_decode_returns(&raw)
        .map_err(|_| ConfigError::RegistryCall(register))
}
