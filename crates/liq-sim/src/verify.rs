//! Bundle verification: trigger tx first, then our calls (GUIDE 11 Step 3).

use crate::warm::Simulator;
use crate::{SimError, SimOutcome};
use alloy_primitives::{Address, Bytes, I256, U256};
use alloy_sol_types::{sol, SolCall};
use liq_types::{Confidence, MevShareHint, Ray};
use revm::context::result::{ExecutionResult, Output};
use revm::context::{BlockEnv, TxEnv};
use revm::database_interface::DatabaseRef;
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::{Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext};
use tracing::error;

sol! {
    interface IExecutor {
        function execute(bytes calldata plan) external payable;
    }
}

/// One EVM call. Trigger txs and our `execute` share this shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimTx {
    pub caller: Address,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub gas_limit: u64,
}

/// Trigger applied *before* our calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// MEV-Share SVR. Hint with `call_data` → reconstructed tx. Absent
    /// calldata → predicted tx, outcome flagged low-confidence.
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
    TxEnv::builder()
        .caller(env.caller)
        .kind(TxKind::Call(env.to))
        .value(env.value)
        .data(env.data.clone())
        .gas_limit(env.gas_limit)
        .gas_price(0)
        .build_fill()
}

fn apply_one<P: DatabaseRef<Error = SimError>>(
    db: &mut revm::database::CacheDB<std::sync::Arc<P>>,
    block: &BlockEnv,
    tx: &SimTx,
) -> Result<(u64, Bytes, bool), SimError> {
    let txenv = to_tx(tx);
    let exec = {
        let mut evm = Context::mainnet()
            .with_db(&mut *db)
            .with_block(block.clone())
            .modify_cfg_chained(|cfg| {
                cfg.spec = SpecId::CANCUN;
                cfg.disable_nonce_check = true;
                cfg.tx_gas_limit_cap = Some(100_000_000);
            })
            .build_mainnet();
        evm.transact(txenv).map_err(|e| {
            tracing::error!(error = %e, "sim transact");
            SimError::StateUnavailable
        })?
    };
    db.commit(exec.state);
    let gas_used = exec.result.tx_gas_used();
    match exec.result {
        ExecutionResult::Success { output, .. } => {
            let bytes = match output {
                Output::Call(b) | Output::Create(b, _) => b,
            };
            Ok((gas_used, bytes, true))
        }
        ExecutionResult::Revert { output, .. } => Ok((gas_used, output, false)),
        ExecutionResult::Halt { reason, .. } => {
            error!(?reason, gas_used, "sim halt");
            Err(SimError::Revert {
                reason: Bytes::new(),
            })
        }
    }
}

fn trigger_tx(trigger: &Trigger) -> Result<(Option<&SimTx>, Confidence), SimError> {
    match trigger {
        Trigger::Svr {
            hint,
            reconstructed,
            predicted,
        } => {
            if hint.call_data.is_some() {
                let tx = reconstructed.as_deref().ok_or(SimError::Malformed(
                    "SVR hint has calldata but no reconstructed tx",
                ))?;
                Ok((Some(tx), Confidence::CERTAIN))
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
pub fn verify<P: DatabaseRef<Error = SimError> + Send + Sync>(
    sim: &mut Simulator<P>,
    bundle: &Bundle,
    at: BlockEnv,
) -> Result<SimOutcome, SimError> {
    sim.reset();

    let (trig, mut conf) = trigger_tx(&bundle.trigger)?;
    if let Some(tx) = trig {
        let (_, out, ok) = apply_one(&mut sim.db, &at, tx)?;
        if !ok {
            error!(reason = ?out, "trigger tx reverted");
            return Err(SimError::Revert { reason: out });
        }
    }

    if let Some(probe) = &bundle.health {
        let probe_tx = SimTx {
            caller: probe.caller,
            to: probe.to,
            value: U256::ZERO,
            data: probe.data.clone(),
            gas_limit: 1_000_000,
        };
        let (_, out, ok) = apply_one(&mut sim.db, &at, &probe_tx)?;
        if !ok {
            error!(reason = ?out, "health probe reverted");
            return Err(SimError::Revert { reason: out });
        }
        let hf = word_to_ray(&out)?;
        if hf >= probe.liquidatable_lt {
            return Err(SimError::GuardRejected { hf });
        }
    }

    let before = sim
        .db
        .basic_ref(sim.profit_account)
        .map_err(|_| SimError::StateUnavailable)?
        .map(|a| a.balance)
        .unwrap_or(U256::ZERO);

    let mut gas_used: u64 = 0;
    for call in &bundle.calls {
        let (g, out, ok) = apply_one(&mut sim.db, &at, call)?;
        gas_used = gas_used
            .checked_add(g)
            .ok_or(SimError::Malformed("gas overflow"))?;
        if !ok {
            error!(reason = ?out, "sim revert — investigate");
            return Err(SimError::Revert { reason: out });
        }
    }

    if matches!(
        bundle.trigger,
        Trigger::Svr {
            reconstructed: None,
            ..
        }
    ) {
        conf = Confidence(0);
    }

    let after = sim
        .db
        .basic_ref(sim.profit_account)
        .map_err(|_| SimError::StateUnavailable)?
        .map(|a| a.balance)
        .unwrap_or(U256::ZERO);

    let net = signed_delta(after, before)?;
    if after < before.saturating_add(bundle.min_profit) && bundle.min_profit > U256::ZERO {
        return Err(SimError::ProfitBelowFloor { net });
    }

    Ok(SimOutcome {
        gas_used,
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
    at: BlockEnv,
) -> Result<Vec<Result<SimOutcome, SimError>>, SimError> {
    if sims.len() != variants.len() {
        return Err(SimError::Malformed("variant/worker count mismatch"));
    }
    verify_variants_split(sims, variants, &at)
}

fn verify_variants_split<P: DatabaseRef<Error = SimError> + Send + Sync>(
    sims: &mut [Simulator<P>],
    variants: &[Bundle],
    at: &BlockEnv,
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
        return Ok(vec![verify(sim, b, at.clone())]);
    }
    let mid = n.checked_div(2).ok_or(SimError::Malformed("empty sims"))?;
    let (left_s, right_s) = sims.split_at_mut(mid);
    let (left_v, right_v) = variants.split_at(mid);
    let mut left = Ok(Vec::new());
    let mut right = Ok(Vec::new());
    std::thread::scope(|s| {
        s.spawn(|| {
            left = verify_variants_split(left_s, left_v, at);
        });
        s.spawn(|| {
            right = verify_variants_split(right_s, right_v, at);
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
