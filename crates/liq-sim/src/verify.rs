//! Bundle verification: trigger tx first, then our calls (GUIDE 11 Step 3).
//!
//! Profit is the WETH the Executor and its profit sink gain: the Executor
//! converges every group on WETH and sweeps it to `PROFIT_SINK` as a token,
//! never as ether.

use crate::env::SimEnv;
use crate::warm::Simulator;
use crate::{SimError, SimOutcome};
use alloy_primitives::{Address, Bytes, B256, I256, U256};
use alloy_sol_types::{sol, SolCall};
use liq_types::{Confidence, MevShareHint, Ray};
use revm::context::result::{ExecutionResult, Output};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::transaction::{AccessList, AccessListItem};
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use revm::primitives::TxKind;
use revm::{Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext};
use std::sync::Arc;
use tracing::error;

sol! {
    interface IExecutor {
        function execute(bytes calldata plan) external payable;
        function WETH() external view returns (address);
        function PROFIT_SINK() external view returns (address);
    }

    interface IErc20Balance {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Gas for a read-only call (a balance, an Executor immutable).
const VIEW_GAS: u64 = 1_000_000;

/// One EVM call. Trigger txs and our `execute` share this shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimTx {
    pub caller: Address,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub gas_limit: u64,
    /// EIP-2930 access list: `(address, storage keys)` warmed before the
    /// call and charged in its intrinsic gas. Empty for our own
    /// transactions, which are signed without one.
    pub access_list: Vec<(Address, Vec<B256>)>,
}

/// Trigger applied *before* our calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// MEV-Share SVR. `reconstructed` is set only when the hint included
    /// `from`, so `forward` can be replayed as that sender. Shared calldata
    /// without a sender is the normal hint: the bundle references the event
    /// by hash and this sim does not replay `forward`. Absent calldata is a
    /// predicted tx, flagged low-confidence.
    Svr {
        hint: Box<MevShareHint>,
        reconstructed: Option<Box<SimTx>>,
        predicted: Option<Box<SimTx>>,
    },
    /// Public mempool `transmit()`.
    Public { tx: SimTx },
    /// Interest/time only: `BlockEnv.timestamp` is the trigger.
    InterestDrift,
}

/// Optional post-trigger health view. The returned word is RAY HF as the
/// protocol reports it — never guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthProbe {
    pub to: Address,
    pub data: Bytes,
    pub caller: Address,
    pub liquidatable_lt: Ray,
}

/// Bundle GUIDE 12 hands this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub trigger: Trigger,
    pub calls: Vec<SimTx>,
    pub min_profit: U256,
    pub health: Option<HealthProbe>,
}

/// ABI-encode `Executor.execute(plan)`.
#[must_use]
pub fn execute_calldata(plan: &[u8]) -> Bytes {
    Bytes::from(
        IExecutor::executeCall {
            plan: plan.to_vec().into(),
        }
        .abi_encode(),
    )
}

fn to_tx(env: &SimTx) -> TxEnv {
    let access_list = AccessList(
        env.access_list
            .iter()
            .map(|(address, keys)| AccessListItem {
                address: *address,
                storage_keys: keys.clone(),
            })
            .collect(),
    );
    TxEnv::builder()
        .caller(env.caller)
        .kind(TxKind::Call(env.to))
        .value(env.value)
        .data(env.data.clone())
        .gas_limit(env.gas_limit)
        .gas_price(0)
        .access_list(access_list)
        .build_fill()
}

/// The block's fork rules. Nonces are not checked (the operator's is the
/// exec worker's to resync) and the transaction gas cap is lifted so a
/// replayed trigger keeps its own limit; our calls carry at most
/// [`crate::MAX_TX_GAS`] (the caller sets it).
fn configure(cfg: &mut CfgEnv, env: &SimEnv) {
    cfg.spec = env.spec;
    cfg.disable_nonce_check = true;
    cfg.tx_gas_limit_cap = Some(100_000_000);
}

/// One call's result, committed to the overlay.
struct Applied {
    /// Receipt gas: after refunds.
    gas_used: u64,
    /// Gas the call needed available: before refunds, or the EIP-7623
    /// floor when that is higher.
    gas_spent: u64,
    output: Bytes,
    success: bool,
}

fn apply_one<P: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Arc<P>>,
    env: &SimEnv,
    tx: &SimTx,
) -> Result<Applied, SimError> {
    let txenv = to_tx(tx);
    let exec = {
        let mut evm = Context::mainnet()
            .with_db(&mut *db)
            .with_block(env.block.clone())
            .modify_cfg_chained(|cfg| configure(cfg, env))
            .build_mainnet();
        evm.transact(txenv).map_err(|e| {
            tracing::error!(error = %e, "sim transact");
            SimError::StateUnavailable
        })?
    };
    db.commit(exec.state);
    let gas = exec.result.gas();
    let gas_used = gas.tx_gas_used();
    let gas_spent = gas.total_gas_spent().max(gas.floor_gas());
    match exec.result {
        ExecutionResult::Success { output, .. } => {
            let output = match output {
                Output::Call(b) | Output::Create(b, _) => b,
            };
            Ok(Applied {
                gas_used,
                gas_spent,
                output,
                success: true,
            })
        }
        ExecutionResult::Revert { output, .. } => Ok(Applied {
            gas_used,
            gas_spent,
            output,
            success: false,
        }),
        ExecutionResult::Halt { reason, .. } => {
            error!(?reason, gas_used, "sim halt");
            Err(SimError::Revert {
                reason: Bytes::new(),
            })
        }
    }
}

/// A read-only call; its writes are dropped. `None` when it reverts or
/// halts.
fn view<P: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Arc<P>>,
    env: &SimEnv,
    to: Address,
    data: Vec<u8>,
) -> Result<Option<Bytes>, SimError> {
    let tx = TxEnv::builder()
        .caller(Address::ZERO)
        .kind(TxKind::Call(to))
        .data(data.into())
        .gas_limit(VIEW_GAS)
        .gas_price(0)
        .build_fill();
    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(env.block.clone())
        .modify_cfg_chained(|cfg| configure(cfg, env))
        .build_mainnet();
    let exec = evm.transact(tx).map_err(|e| {
        tracing::error!(error = %e, "sim view call");
        SimError::StateUnavailable
    })?;
    Ok(match exec.result {
        ExecutionResult::Success { output, .. } => Some(match output {
            Output::Call(b) | Output::Create(b, _) => b,
        }),
        _ => None,
    })
}

/// `token.balanceOf(holder)`. A token with no code answers nothing; that is
/// a zero balance, so a profit floor still refuses.
fn erc20_balance<P: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Arc<P>>,
    env: &SimEnv,
    token: Address,
    holder: Address,
) -> Result<U256, SimError> {
    let call = IErc20Balance::balanceOfCall { account: holder }.abi_encode();
    let out = view(db, env, token, call)?.ok_or(SimError::Malformed("balanceOf reverted"))?;
    if out.is_empty() {
        return Ok(U256::ZERO);
    }
    IErc20Balance::balanceOfCall::abi_decode_returns(&out)
        .map_err(|_| SimError::Malformed("balanceOf return"))
}

/// WETH the Executor and its profit sink hold. Zero when the simulator has
/// no WETH address.
fn profit_held<P: DatabaseRef<Error = SimError>>(
    sim: &mut Simulator<P>,
    env: &SimEnv,
) -> Result<U256, SimError> {
    if sim.weth.is_zero() {
        return Ok(U256::ZERO);
    }
    let sink = erc20_balance(&mut sim.db, env, sim.weth, sim.profit_account)?;
    if sim.executor == sim.profit_account {
        return Ok(sink);
    }
    let own = erc20_balance(&mut sim.db, env, sim.weth, sim.executor)?;
    sink.checked_add(own)
        .ok_or(SimError::Malformed("profit overflow"))
}

/// The Executor's `WETH` and `PROFIT_SINK` immutables, read from the code
/// at `executor`. [`SimError::Bytecode`] when no Executor answers there.
pub fn executor_anchors<P: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Arc<P>>,
    env: &SimEnv,
    executor: Address,
) -> Result<(Address, Address), SimError> {
    let weth = read_address(db, env, executor, IExecutor::WETHCall {}.abi_encode())?;
    let sink = read_address(
        db,
        env,
        executor,
        IExecutor::PROFIT_SINKCall {}.abi_encode(),
    )?;
    if weth.is_zero() || sink.is_zero() {
        return Err(SimError::Bytecode("Executor immutable is zero"));
    }
    Ok((weth, sink))
}

/// An `address`-returning getter of the contract at `at`.
pub(crate) fn read_address<P: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<Arc<P>>,
    env: &SimEnv,
    at: Address,
    call: Vec<u8>,
) -> Result<Address, SimError> {
    let out = view(db, env, at, call)?.ok_or(SimError::Bytecode("Executor getter reverted"))?;
    if out.len() != 32 {
        return Err(SimError::Bytecode(
            "no Executor answers at the simulated address",
        ));
    }
    <alloy_sol_types::sol_data::Address as alloy_sol_types::SolType>::abi_decode(&out)
        .map_err(|_| SimError::Bytecode("Executor getter is not an address"))
}

fn trigger_tx(trigger: &Trigger) -> Result<(Option<&SimTx>, Confidence), SimError> {
    match trigger {
        Trigger::Svr {
            hint,
            reconstructed,
            predicted,
        } => {
            if let Some(tx) = reconstructed.as_deref() {
                Ok((Some(tx), Confidence::CERTAIN))
            } else if hint.call_data.is_some() {
                Ok((None, Confidence::CERTAIN))
            } else {
                let tx = predicted.as_deref().ok_or(SimError::Malformed(
                    "SVR partial hint requires predicted tx; refusing to invent one",
                ))?;
                Ok((Some(tx), Confidence(0)))
            }
        }
        Trigger::Public { tx } => Ok((Some(tx), Confidence::CERTAIN)),
        Trigger::InterestDrift => Ok((None, Confidence::CERTAIN)),
    }
}

fn word_to_ray(word: &[u8]) -> Result<Ray, SimError> {
    if word.len() < 32 {
        return Err(SimError::Malformed("health probe short return"));
    }
    let mut buf = [0u8; 32];
    let src = word.get(..32).ok_or(SimError::Malformed("health probe"))?;
    buf.copy_from_slice(src);
    Ok(Ray::from_raw(U256::from_be_bytes(buf)))
}

/// Verify a bundle against pending state. Trigger first, then our calls.
pub fn verify<P: DatabaseRef<Error = SimError>>(
    sim: &mut Simulator<P>,
    bundle: &Bundle,
    env: &SimEnv,
) -> Result<SimOutcome, SimError> {
    sim.reset();

    let (trig, mut conf) = trigger_tx(&bundle.trigger)?;
    if let Some(tx) = trig {
        let a = apply_one(&mut sim.db, env, tx)?;
        if !a.success {
            error!(reason = ?a.output, "trigger tx reverted");
            return Err(SimError::Revert { reason: a.output });
        }
    }

    if let Some(probe) = &bundle.health {
        let probe_tx = SimTx {
            caller: probe.caller,
            to: probe.to,
            value: U256::ZERO,
            data: probe.data.clone(),
            gas_limit: 1_000_000,
            access_list: Vec::new(),
        };
        let a = apply_one(&mut sim.db, env, &probe_tx)?;
        if !a.success {
            error!(reason = ?a.output, "health probe reverted");
            return Err(SimError::Revert { reason: a.output });
        }
        let hf = word_to_ray(&a.output)?;
        if hf >= probe.liquidatable_lt {
            return Err(SimError::GuardRejected { hf });
        }
    }

    let before = profit_held(sim, env)?;

    let mut gas_used: u64 = 0;
    let mut gas_spent: u64 = 0;
    for call in &bundle.calls {
        let a = apply_one(&mut sim.db, env, call)?;
        gas_used = gas_used
            .checked_add(a.gas_used)
            .ok_or(SimError::Malformed("gas overflow"))?;
        gas_spent = gas_spent
            .checked_add(a.gas_spent)
            .ok_or(SimError::Malformed("gas overflow"))?;
        if !a.success {
            error!(reason = ?a.output, "sim revert — investigate");
            return Err(SimError::Revert { reason: a.output });
        }
    }

    if matches!(
        &bundle.trigger,
        Trigger::Svr {
            reconstructed: None,
            hint,
            ..
        } if hint.call_data.is_none()
    ) {
        conf = Confidence(0);
    }

    let after = profit_held(sim, env)?;

    let net = signed_delta(after, before)?;
    if after < before.saturating_add(bundle.min_profit) && bundle.min_profit > U256::ZERO {
        return Err(SimError::ProfitBelowFloor { net });
    }

    Ok(SimOutcome {
        gas_used,
        gas_spent,
        net_profit_wei: net,
        confidence: conf,
        weth_after: after,
    })
}

fn signed_delta(after: U256, before: U256) -> Result<I256, SimError> {
    let a = I256::try_from(after).map_err(|_| SimError::Malformed("profit after"))?;
    let b = I256::try_from(before).map_err(|_| SimError::Malformed("profit before"))?;
    a.checked_sub(b).ok_or(SimError::Malformed("profit delta"))
}

/// Bounded variant fan-out (GUIDE 11 Step 4b). One simulator per variant;
/// scoped threads borrow the plans. Pinning is WP 16A.
pub fn verify_variants<P: DatabaseRef<Error = SimError> + Send + Sync>(
    sims: &mut [Simulator<P>],
    variants: &[Bundle],
    env: &SimEnv,
) -> Result<Vec<Result<SimOutcome, SimError>>, SimError> {
    if sims.len() != variants.len() {
        return Err(SimError::Malformed("variant/worker count mismatch"));
    }
    verify_variants_split(sims, variants, env)
}

fn verify_variants_split<P: DatabaseRef<Error = SimError> + Send + Sync>(
    sims: &mut [Simulator<P>],
    variants: &[Bundle],
    env: &SimEnv,
) -> Result<Vec<Result<SimOutcome, SimError>>, SimError> {
    if sims.is_empty() {
        return Ok(Vec::new());
    }
    let n = sims.len();
    if n == 1 {
        let sim = sims.get_mut(0).ok_or(SimError::Malformed("empty sims"))?;
        let b = variants
            .first()
            .ok_or(SimError::Malformed("empty variants"))?;
        return Ok(vec![verify(sim, b, env)]);
    }
    let mid = n.checked_div(2).ok_or(SimError::Malformed("empty sims"))?;
    let (left_s, right_s) = sims.split_at_mut(mid);
    let (left_v, right_v) = variants.split_at(mid);
    let mut left = Ok(Vec::new());
    let mut right = Ok(Vec::new());
    std::thread::scope(|s| {
        s.spawn(|| {
            left = verify_variants_split(left_s, left_v, env);
        });
        s.spawn(|| {
            right = verify_variants_split(right_s, right_v, env);
        });
    });
    let mut o = left?;
    o.extend(right?);
    Ok(o)
}

/// 05B seam: refuse until the archive path exists. Never fabricates fills.
pub fn verify_historical(_archive_dir: &std::path::Path) -> Result<(), SimError> {
    Err(SimError::ArchiveUnavailable)
}

/// `BlockEnv` at a known height/time. Does not invent base fee or gas limit.
#[must_use]
pub fn block_env_at(number: u64, timestamp: u64) -> BlockEnv {
    BlockEnv {
        number: U256::from(number),
        timestamp: U256::from(timestamp),
        ..BlockEnv::default()
    }
}
