//! Warm account cache and pre-H3 Executor insertion (GUIDE 11 Step 2).

use crate::{SimError, StateProviderFactory};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolValue;
use revm::database::{CacheDB, EmptyDB};
use revm::database_interface::DatabaseRef;
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::{context::TxEnv, Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Documented pre-H3 insertion address. Replaced by `venues.executor` after
/// the human deploy (H3). Not a mainnet claim.
pub const PLANNED_EXECUTOR: Address =
    alloy_primitives::address!("e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0");

/// Constructor immutables for a CacheDB-inserted Executor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ExecutorSpec {
    pub operator: Address,
    pub profit_sink: Address,
    pub univ3_factory: Address,
    pub univ3_init_hash: B256,
    pub router_a: Address,
    pub router_b: Address,
    pub weth: Address,
}

/// Addresses kept resident across `verify` resets.
#[derive(Clone, Debug, Default)]
pub struct WarmSet {
    addrs: HashSet<Address>,
}

impl WarmSet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            addrs: HashSet::new(),
        }
    }

    pub fn insert(&mut self, addr: Address) {
        self.addrs.insert(addr);
    }

    #[must_use]
    pub fn contains(&self, addr: &Address) -> bool {
        self.addrs.contains(addr)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Address> {
        self.addrs.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.addrs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }
}

/// Drop overlay accounts that are not warm. Keeps map capacity. Restores
/// warm accounts from `snapshot` so the previous sim's writes do not leak.
pub fn clear_except<Ext: DatabaseRef>(
    db: &mut CacheDB<Ext>,
    warm: &WarmSet,
    snapshot: &CacheDB<EmptyDB>,
) {
    db.cache.accounts.retain(|addr, acc| {
        if !warm.contains(addr) {
            return false;
        }
        if let Some(snap) = snapshot.cache.accounts.get(addr) {
            *acc = snap.clone();
        }
        true
    });
    db.cache.logs.clear();
    for (hash, code) in &snapshot.cache.contracts {
        db.cache.contracts.insert(*hash, code.clone());
    }
}

/// Foundry artifact: `contracts/out/Executor.sol/Executor.json`.
#[must_use]
pub fn executor_artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out/Executor.sol/Executor.json")
}

/// Creation bytecode from the compiled Executor artifact. Fails closed if
/// forge has not been run.
pub fn load_executor_creation_bytecode() -> Result<Bytes, SimError> {
    load_creation_from(&executor_artifact_path())
}

pub fn load_creation_from(path: &Path) -> Result<Bytes, SimError> {
    let raw =
        std::fs::read_to_string(path).map_err(|_| SimError::Bytecode("artifact unreadable"))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| SimError::Bytecode("artifact json"))?;
    let hex = v
        .get("bytecode")
        .and_then(|b| b.get("object"))
        .and_then(|o| o.as_str())
        .ok_or(SimError::Bytecode("bytecode.object missing"))?;
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    if hex.is_empty() {
        return Err(SimError::Bytecode("empty bytecode"));
    }
    Bytes::from_str_radix_hex(hex).or_else(|_| parse_hex(hex))
}

fn parse_hex(hex: &str) -> Result<Bytes, SimError> {
    if !hex.len().is_multiple_of(2) {
        return Err(SimError::Bytecode("odd hex length"));
    }
    let nbytes = hex.len().checked_div(2).ok_or(SimError::Bytecode("hex"))?;
    let mut out = Vec::with_capacity(nbytes);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi_b = bytes.get(i).ok_or(SimError::Bytecode("hex"))?;
        let hi = from_hex(*hi_b)?;
        let lo_b = bytes
            .get(i.checked_add(1).ok_or(SimError::Bytecode("hex"))?)
            .ok_or(SimError::Bytecode("hex"))?;
        let lo = from_hex(*lo_b)?;
        out.push(hi.checked_shl(4).ok_or(SimError::Bytecode("hex"))? | lo);
        i = i.checked_add(2).ok_or(SimError::Bytecode("hex"))?;
    }
    Ok(Bytes::from(out))
}

fn from_hex(b: u8) -> Result<u8, SimError> {
    match b {
        b'0'..=b'9' => Ok(b.saturating_sub(b'0')),
        b'a'..=b'f' => Ok(b.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Ok(b.saturating_sub(b'A').saturating_add(10)),
        _ => Err(SimError::Bytecode("non-hex nibble")),
    }
}

trait FromStrRadixHex {
    fn from_str_radix_hex(s: &str) -> Result<Bytes, SimError>;
}

impl FromStrRadixHex for Bytes {
    fn from_str_radix_hex(s: &str) -> Result<Bytes, SimError> {
        parse_hex(s)
    }
}

/// Deploy Executor via CREATE using real creation bytecode + constructor
/// args, then copy the resulting runtime into `at`.
pub fn insert_executor<Ext: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Ext>,
    at: Address,
    spec: &ExecutorSpec,
    deployer: Address,
) -> Result<(), SimError> {
    let creation = load_executor_creation_bytecode()?;
    let args = (
        spec.operator,
        spec.profit_sink,
        spec.univ3_factory,
        spec.univ3_init_hash,
        spec.router_a,
        spec.router_b,
        spec.weth,
    )
        .abi_encode();
    let mut data = creation.to_vec();
    data.extend_from_slice(&args);

    let mut info = db
        .basic_ref(deployer)
        .map_err(|_| SimError::StateUnavailable)?
        .unwrap_or_default();
    const TEN_ETH: u128 = 10_000_000_000_000_000_000;
    if info.balance < U256::from(TEN_ETH) {
        info.balance = U256::from(TEN_ETH);
        db.insert_account_info(deployer, info);
    }

    let mut owned: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    // Copy current cache so CREATE sees inserted accounts.
    owned.cache = db.cache.clone();

    let tx = TxEnv::builder()
        .caller(deployer)
        .kind(TxKind::Create)
        .data(data.into())
        .gas_limit(50_000_000)
        .gas_price(0)
        .build_fill();

    let mut evm = Context::mainnet()
        .with_db(&mut owned)
        .modify_cfg_chained(|cfg| {
            cfg.spec = SpecId::CANCUN;
            cfg.disable_nonce_check = true;
            cfg.tx_gas_limit_cap = Some(100_000_000);
            cfg.limit_contract_code_size = Some(0x60_000);
            cfg.limit_contract_initcode_size = Some(0xC0_000);
        })
        .build_mainnet();
    let exec = evm.transact(tx).map_err(|e| {
        tracing::error!(error = %e, "executor CREATE transact");
        SimError::Bytecode("CREATE transact failed")
    })?;
    drop(evm);
    owned.commit(exec.state);
    if !exec.result.is_success() {
        let reason = match exec.result {
            revm::context::result::ExecutionResult::Revert { output, .. } => output,
            _ => alloy_primitives::Bytes::new(),
        };
        tracing::error!(?reason, "executor CREATE reverted");
        return Err(SimError::Revert { reason });
    }
    let created = exec
        .result
        .created_address()
        .ok_or(SimError::Bytecode("CREATE returned no address"))?;

    db.cache = owned.cache;
    let runtime = db
        .basic_ref(created)
        .map_err(|_| SimError::StateUnavailable)?
        .ok_or(SimError::Bytecode("created account missing"))?;
    if runtime.code_hash.is_zero() {
        return Err(SimError::Bytecode("created code hash zero"));
    }
    let mut dest = runtime.clone();
    if dest.code.is_none() {
        dest.code = Some(
            db.code_by_hash_ref(runtime.code_hash)
                .map_err(|_| SimError::StateUnavailable)?,
        );
    }
    db.insert_account_info(at, dest);
    Ok(())
}

/// Per-worker simulator: overlay `CacheDB` + frozen warm snapshot.
pub struct Simulator<P: DatabaseRef> {
    pub db: CacheDB<Arc<P>>,
    pub warm: WarmSet,
    snapshot: CacheDB<EmptyDB>,
    pub executor: Address,
    pub weth: Address,
    pub profit_account: Address,
}

impl<P: DatabaseRef<Error = SimError> + Send + Sync> Simulator<P> {
    pub fn boot(
        provider: Arc<P>,
        mut warm: WarmSet,
        executor: Address,
        spec: &ExecutorSpec,
        deployer: Address,
    ) -> Result<Self, SimError> {
        let mut db = CacheDB::new(Arc::clone(&provider));
        insert_executor(&mut db, executor, spec, deployer)?;
        warm.insert(executor);
        warm.insert(spec.weth);
        warm.insert(spec.profit_sink);
        warm.insert(spec.operator);
        let addrs: Vec<Address> = warm.addrs.iter().copied().collect();
        for addr in addrs {
            let _ = db.load_account(addr);
        }
        let mut snapshot = CacheDB::new(EmptyDB::default());
        snapshot.cache = db.cache.clone();
        Ok(Self {
            db,
            warm,
            snapshot,
            executor,
            weth: spec.weth,
            profit_account: spec.profit_sink,
        })
    }

    pub fn from_factory<F: StateProviderFactory<Provider = P>>(
        factory: &F,
        warm: WarmSet,
        executor: Address,
        spec: &ExecutorSpec,
        deployer: Address,
    ) -> Result<Self, SimError> {
        Self::boot(factory.latest()?, warm, executor, spec, deployer)
    }

    #[inline]
    pub fn reset(&mut self) {
        clear_except(&mut self.db, &self.warm, &self.snapshot);
    }
}

/// Canonical WETH9 (mainnet). Used only as a default for tests that insert
/// the Executor; not a price.
pub const WETH: Address = alloy_primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Uniswap V3 factory (mainnet).
pub const UNIV3_FACTORY: Address =
    alloy_primitives::address!("1F98431c8aD98523631AE4a59f267346ea31F984");
/// `keccak256(type(UniswapV3Pool).creationCode)`.
pub const UNIV3_POOL_INIT_HASH: B256 =
    alloy_primitives::b256!("e34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54");
