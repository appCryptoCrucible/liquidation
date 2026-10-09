//! Warm account cache and pre-H3 Executor insertion (GUIDE 11 Step 2).

use crate::{BlockRef, BlockState, SimError, StateProviderFactory};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolValue;
use revm::database::{CacheDB, EmptyDB};
use revm::database_interface::DatabaseRef;
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::state::Bytecode;
use revm::{context::TxEnv, Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Rules the compiled Executor is constructed under. Its constructor only
/// checks and stores its arguments, so the runtime is the same under any
/// fork it compiles for (Cancun opcodes).
const CONSTRUCT_SPEC: SpecId = SpecId::OSAKA;
/// Sender of the in-memory CREATE. The constructor does not read it.
const RUNTIME_DEPLOYER: Address =
    alloy_primitives::address!("00000000000000000000000000000000000de901");

/// Documented pre-H3 insertion address. Replaced by `venues.executor` after
/// the human deploy (H3). Not a mainnet claim.
pub const PLANNED_EXECUTOR: Address =
    alloy_primitives::address!("e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0");
/// Where the compiled `LiquidationModule` is placed beside the planned
/// Executor, which is built to delegatecall it there. Not a mainnet claim.
pub const PLANNED_LIQUIDATION_MODULE: Address =
    alloy_primitives::address!("e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1");
/// Where the compiled `SwapModule` is placed. Not a mainnet claim.
pub const PLANNED_SWAP_MODULE: Address =
    alloy_primitives::address!("e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2");
/// Where the compiled `DexModule` is placed: the swap module is built to
/// delegatecall it there for Balancer and Fluid legs. Not a mainnet claim.
pub const PLANNED_DEX_MODULE: Address =
    alloy_primitives::address!("e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3");

/// Constructor arguments of a CacheDB-inserted Executor and its modules: the
/// core takes the keys, the sink, WETH and the V3 anchors; the swap module
/// takes WETH, the routers, the V2/Sushi/Curve anchors and the dex module; the
/// liquidation and dex modules take WETH.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ExecutorSpec {
    pub operator: Address,
    /// Second operator key, same right as `operator`: signs the MEV-Share
    /// backruns on a nonce sequence of its own.
    pub backrun_operator: Address,
    pub profit_sink: Address,
    pub univ3_factory: Address,
    pub univ3_init_hash: B256,
    pub router_a: Address,
    pub router_b: Address,
    pub weth: Address,
    /// Pool-direct V2/Sushi/Curve anchors (`MainnetVenues.sol`): each pair
    /// is verified by CREATE2 against its factory, each Curve pool against
    /// the MetaRegistry. The constructor refuses a zero anchor.
    pub univ2_factory: Address,
    pub univ2_init_hash: B256,
    pub sushi_factory: Address,
    pub sushi_init_hash: B256,
    pub curve_registry: Address,
}

impl ExecutorSpec {
    /// Mainnet venue anchors, as `contracts/src/lib/MainnetVenues.sol`.
    pub const UNIV2_FACTORY: Address =
        alloy_primitives::address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
    pub const UNIV2_INIT_HASH: B256 =
        alloy_primitives::b256!("96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f");
    pub const SUSHI_FACTORY: Address =
        alloy_primitives::address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");
    pub const SUSHI_INIT_HASH: B256 =
        alloy_primitives::b256!("e18a34eb0e04b04f7a0ac29a6e80748dca96319b42c54d679cb821dca90c6303");
    pub const CURVE_META_REGISTRY: Address =
        alloy_primitives::address!("F98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC");
    /// Uniswap SwapRouter02, both router slots in `DeployExecutor.s.sol`.
    pub const SWAP_ROUTER02: Address =
        alloy_primitives::address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45");

    /// The mainnet constructor arguments `DeployExecutor.s.sol` uses, for
    /// the two operator keys and `profit_sink`.
    #[must_use]
    pub fn mainnet(operator: Address, backrun_operator: Address, profit_sink: Address) -> Self {
        Self {
            operator,
            backrun_operator,
            profit_sink,
            univ3_factory: UNIV3_FACTORY,
            univ3_init_hash: UNIV3_POOL_INIT_HASH,
            router_a: Self::SWAP_ROUTER02,
            router_b: Self::SWAP_ROUTER02,
            weth: WETH,
            univ2_factory: Self::UNIV2_FACTORY,
            univ2_init_hash: Self::UNIV2_INIT_HASH,
            sushi_factory: Self::SUSHI_FACTORY,
            sushi_init_hash: Self::SUSHI_INIT_HASH,
            curve_registry: Self::CURVE_META_REGISTRY,
        }
    }
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

/// Foundry artifact of `contract`: `contracts/out/<contract>.sol/<contract>.json`.
#[must_use]
pub fn artifact_path(contract: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/out")
        .join(format!("{contract}.sol"))
        .join(format!("{contract}.json"))
}

/// Foundry artifact: `contracts/out/Executor.sol/Executor.json`.
#[must_use]
pub fn executor_artifact_path() -> PathBuf {
    artifact_path("Executor")
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

/// `contract`'s creation code followed by ABI-encoded `args`: the input of
/// its deploy transaction.
fn initcode(contract: &str, args: &[u8]) -> Result<Bytes, SimError> {
    let mut data = load_creation_from(&artifact_path(contract))?.to_vec();
    data.extend_from_slice(args);
    Ok(data.into())
}

/// The Executor's deploy input for `spec`, wired to the modules at
/// `liquidation_module` and `swap_module`.
pub fn executor_initcode(
    spec: &ExecutorSpec,
    liquidation_module: Address,
    swap_module: Address,
) -> Result<Bytes, SimError> {
    let args = (
        spec.operator,
        spec.backrun_operator,
        spec.profit_sink,
        spec.weth,
        spec.univ3_factory,
        spec.univ3_init_hash,
        liquidation_module,
        swap_module,
    )
        .abi_encode();
    initcode("Executor", &args)
}

/// `LiquidationModule`'s deploy input for `spec`.
pub fn liquidation_module_initcode(spec: &ExecutorSpec) -> Result<Bytes, SimError> {
    initcode("LiquidationModule", &spec.weth.abi_encode())
}

/// `DexModule`'s deploy input for `spec`.
pub fn dex_module_initcode(spec: &ExecutorSpec) -> Result<Bytes, SimError> {
    initcode("DexModule", &spec.weth.abi_encode())
}

/// `SwapModule`'s deploy input for `spec`, wired to the dex module at
/// `dex_module` (whose constructor-time check reads it).
pub fn swap_module_initcode(spec: &ExecutorSpec, dex_module: Address) -> Result<Bytes, SimError> {
    let args = (
        spec.weth,
        spec.router_a,
        spec.router_b,
        spec.univ2_factory,
        spec.univ2_init_hash,
        spec.sushi_factory,
        spec.sushi_init_hash,
        spec.curve_registry,
        dex_module,
    )
        .abi_encode();
    initcode("SwapModule", &args)
}

/// Runtime code of the compiled Executor and its modules, built for `spec`.
/// The core is wired to [`PLANNED_LIQUIDATION_MODULE`] and
/// [`PLANNED_SWAP_MODULE`], the swap module to [`PLANNED_DEX_MODULE`]: placed
/// there with [`ExecutorCode::placements`], the four run as the deployed
/// system would.
#[derive(Clone, Debug)]
pub struct ExecutorCode {
    pub core: Bytecode,
    pub liquidation: Bytecode,
    pub swap: Bytecode,
    pub dex: Bytecode,
}

impl ExecutorCode {
    /// Each contract and the address it runs at: the core at `core_at`,
    /// the modules where the core delegatecalls them.
    #[must_use]
    pub fn placements(&self, core_at: Address) -> [(Address, Bytecode); 4] {
        [
            (core_at, self.core.clone()),
            (PLANNED_LIQUIDATION_MODULE, self.liquidation.clone()),
            (PLANNED_SWAP_MODULE, self.swap.clone()),
            (PLANNED_DEX_MODULE, self.dex.clone()),
        ]
    }

    /// [`Self::placements`] as raw runtime bytes, for a node's state
    /// override.
    #[must_use]
    pub fn runtimes(&self, core_at: Address) -> [(Address, Bytes); 4] {
        self.placements(core_at)
            .map(|(at, code)| (at, code.original_bytes()))
    }
}

/// Build the compiled system for `spec`: each module run once as a CREATE
/// in an empty state, then the core, whose constructor checks the modules
/// where they will be placed. No chain state is read: the constructors
/// only check and store their arguments.
pub fn executor_stack(spec: &ExecutorSpec) -> Result<ExecutorCode, SimError> {
    let mut world: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    let liquidation = deploy_runtime(
        &world.cache,
        liquidation_module_initcode(spec)?,
        RUNTIME_DEPLOYER,
    )?;
    let dex = deploy_runtime(&world.cache, dex_module_initcode(spec)?, RUNTIME_DEPLOYER)?;
    place_code(&mut world, PLANNED_DEX_MODULE, dex.clone())?;
    let swap = deploy_runtime(
        &world.cache,
        swap_module_initcode(spec, PLANNED_DEX_MODULE)?,
        RUNTIME_DEPLOYER,
    )?;
    place_code(&mut world, PLANNED_LIQUIDATION_MODULE, liquidation.clone())?;
    place_code(&mut world, PLANNED_SWAP_MODULE, swap.clone())?;
    let core = deploy_runtime(
        &world.cache,
        executor_initcode(spec, PLANNED_LIQUIDATION_MODULE, PLANNED_SWAP_MODULE)?,
        RUNTIME_DEPLOYER,
    )?;
    Ok(ExecutorCode {
        core,
        liquidation,
        swap,
        dex,
    })
}

/// Deploy the compiled system for `spec` and place it: the core at `at`,
/// the modules at their planned addresses.
pub fn insert_executor<Ext: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Ext>,
    at: Address,
    spec: &ExecutorSpec,
    _deployer: Address,
) -> Result<(), SimError> {
    for (addr, code) in executor_stack(spec)?.placements(at) {
        place_code(db, addr, code)?;
    }
    Ok(())
}

/// `code` as the code of `at`, keeping its balance; nothing else of the
/// account changes. A contract account's nonce is at least 1 (EIP-161).
pub fn place_code<Ext: DatabaseRef>(
    db: &mut CacheDB<Ext>,
    at: Address,
    code: Bytecode,
) -> Result<(), SimError> {
    let mut info = db
        .basic_ref(at)
        .map_err(|_| SimError::StateUnavailable)?
        .unwrap_or_default();
    info.code_hash = code.hash_slow();
    info.code = Some(code);
    info.nonce = info.nonce.max(1);
    db.insert_account_info(at, info);
    Ok(())
}

/// Run `initcode` as a CREATE from `deployer` over a copy of `cache` (the
/// constructor sees those accounts and nothing else) and return the
/// created account's code. Nothing is written back. Mainnet's size limits
/// hold (EIP-170 runtime, EIP-3860 initcode): code the chain would refuse
/// to deploy is refused here, not simulated as if it were deployable.
pub(crate) fn deploy_runtime(
    cache: &revm::database::Cache,
    initcode: Bytes,
    deployer: Address,
) -> Result<Bytecode, SimError> {
    let data = initcode.to_vec();
    let mut owned: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    owned.cache = cache.clone();
    let mut info = owned
        .basic_ref(deployer)
        .map_err(|_| SimError::StateUnavailable)?
        .unwrap_or_default();
    const TEN_ETH: u128 = 10_000_000_000_000_000_000;
    if info.balance < U256::from(TEN_ETH) {
        info.balance = U256::from(TEN_ETH);
        owned.insert_account_info(deployer, info);
    }

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
            cfg.spec = CONSTRUCT_SPEC;
            cfg.disable_nonce_check = true;
            cfg.tx_gas_limit_cap = Some(100_000_000);
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
    let runtime = owned
        .basic_ref(created)
        .map_err(|_| SimError::StateUnavailable)?
        .ok_or(SimError::Bytecode("created account missing"))?;
    if runtime.code_hash.is_zero() || runtime.code_hash == revm::primitives::KECCAK_EMPTY {
        return Err(SimError::Bytecode("created account has no code"));
    }
    match runtime.code {
        Some(code) if !code.is_empty() => Ok(code),
        _ => owned
            .code_by_hash_ref(runtime.code_hash)
            .map_err(|_| SimError::StateUnavailable),
    }
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

impl<P: DatabaseRef<Error = SimError>> Simulator<P> {
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
        warm.insert(PLANNED_LIQUIDATION_MODULE);
        warm.insert(PLANNED_SWAP_MODULE);
        warm.insert(PLANNED_DEX_MODULE);
        warm.insert(spec.weth);
        warm.insert(spec.profit_sink);
        warm.insert(spec.operator);
        warm.insert(spec.backrun_operator);
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

    /// Overlay on `provider` with the Executor the jobs are sent to: the
    /// compiled system placed (`code`, the core and its modules) when it is
    /// not deployed, the chain's accounts when it is (empty). Kept across
    /// resets.
    pub fn with_executor(
        provider: Arc<P>,
        executor: Address,
        code: &[(Address, Bytecode)],
        weth: Address,
        profit_account: Address,
    ) -> Result<Self, SimError> {
        let mut db = CacheDB::new(provider);
        let mut warm = WarmSet::new();
        for (at, code) in code {
            place_code(&mut db, *at, code.clone())?;
            warm.insert(*at);
        }
        db.load_account(executor)?;
        warm.insert(executor);
        let mut snapshot = CacheDB::new(EmptyDB::default());
        snapshot.cache = db.cache.clone();
        Ok(Self {
            db,
            warm,
            snapshot,
            executor,
            weth,
            profit_account,
        })
    }

    /// Overlay on an existing provider. Does not insert Executor bytecode
    /// (H3 undeployed / artifact absent). Does not invent account state.
    pub fn from_provider(
        provider: Arc<P>,
        executor: Address,
        weth: Address,
        profit_account: Address,
    ) -> Self {
        Self {
            db: CacheDB::new(provider),
            warm: WarmSet::new(),
            snapshot: CacheDB::new(EmptyDB::default()),
            executor,
            weth,
            profit_account,
        }
    }

    #[inline]
    pub fn reset(&mut self) {
        clear_except(&mut self.db, &self.warm, &self.snapshot);
    }
}

impl Simulator<BlockState> {
    /// [`Simulator::boot`] on the state after block `at`.
    pub fn from_factory<F: StateProviderFactory + ?Sized>(
        factory: &F,
        at: BlockRef,
        warm: WarmSet,
        executor: Address,
        spec: &ExecutorSpec,
        deployer: Address,
    ) -> Result<Self, SimError> {
        Self::boot(
            Arc::new(factory.state_at(at)?),
            warm,
            executor,
            spec,
            deployer,
        )
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
