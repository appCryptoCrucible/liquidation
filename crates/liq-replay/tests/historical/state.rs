//! Chain state at a historical moment: the state after block N−1, read
//! lazily from the upstream node, with chosen transactions of block N applied
//! on top in revm. Every other run works on a copy.

use super::rpc::{big, hex, num, parse, Upstream};
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use liq_sim::SimError;
use revm::context::result::{ExecutionResult, Output};
use revm::context::{BlockEnv, TxEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::context_interface::transaction::{
    AccessList, AccessListItem, Authorization, SignedAuthorization,
};
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::state::{AccountInfo, Bytecode, EvmState};
use revm::{Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext};
use serde_json::{json, Value};
use std::sync::Arc;

/// Chain state after block `number`, read lazily over RPC.
#[derive(Clone)]
pub struct AtBlock {
    pub up: Arc<Upstream>,
    pub number: u64,
}

impl AtBlock {
    fn get(&self, method: &str, params: Value) -> Result<Value, SimError> {
        self.up.call(method, params).map_err(|e| {
            eprintln!("state read at {}: {e}", self.number);
            SimError::StateUnavailable
        })
    }
}

fn bad<E>(_: E) -> SimError {
    SimError::StateUnavailable
}

impl DatabaseRef for AtBlock {
    type Error = SimError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, SimError> {
        let a = format!("{address:#x}");
        let tag = hex(self.number);
        let answers = self
            .up
            .call_many(&[
                ("eth_getBalance", json!([a, tag])),
                ("eth_getTransactionCount", json!([a, tag])),
                ("eth_getCode", json!([a, tag])),
            ])
            .map_err(|e| {
                eprintln!("account read at {}: {e}", self.number);
                SimError::StateUnavailable
            })?;
        let balance = big(&answers[0]).map_err(bad)?;
        let nonce = num(&answers[1]).map_err(bad)?;
        let code: Bytes = parse(&answers[2]).map_err(bad)?;
        if balance.is_zero() && nonce == 0 && code.is_empty() {
            return Ok(None);
        }
        let code = Bytecode::new_raw_checked(code).map_err(bad)?;
        Ok(Some(AccountInfo {
            balance,
            nonce,
            code_hash: code.hash_slow(),
            code: Some(code),
            account_id: None,
        }))
    }

    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, SimError> {
        Err(SimError::StateUnavailable)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, SimError> {
        let slot = format!("{:#x}", B256::from(index));
        big(&self.get(
            "eth_getStorageAt",
            json!([format!("{address:#x}"), slot, hex(self.number)]),
        )?)
        .map_err(bad)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, SimError> {
        let b = self.get("eth_getBlockByNumber", json!([hex(number), false]))?;
        parse(&b["hash"]).map_err(bad)
    }
}

/// The parts of a block header a transaction can observe.
#[derive(Clone, Debug)]
pub struct Header {
    pub number: u64,
    pub hash: B256,
    pub timestamp: u64,
    pub base_fee: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub miner: Address,
    pub mix: B256,
    pub excess_blob_gas: u64,
}

impl Header {
    pub fn fetch(up: &Upstream, number: u64) -> Result<Self, String> {
        let h = up.call("eth_getBlockByNumber", json!([hex(number), false]))?;
        Ok(Self {
            number,
            hash: parse(&h["hash"])?,
            timestamp: num(&h["timestamp"])?,
            base_fee: num(&h["baseFeePerGas"])?,
            gas_limit: num(&h["gasLimit"])?,
            gas_used: num(&h["gasUsed"])?,
            miner: parse(&h["miner"])?,
            mix: parse(&h["mixHash"])?,
            excess_blob_gas: if h["excessBlobGas"].is_null() {
                0
            } else {
                num(&h["excessBlobGas"])?
            },
        })
    }

    pub fn spec(&self) -> SpecId {
        liq_sim::mainnet_spec(self.timestamp).unwrap_or(SpecId::OSAKA)
    }

    /// The block's environment; `base_fee` replaced (zero for calls, as a
    /// node runs `eth_call`).
    pub fn env(&self, base_fee: u64) -> BlockEnv {
        BlockEnv {
            number: U256::from(self.number),
            beneficiary: self.miner,
            timestamp: U256::from(self.timestamp),
            gas_limit: self.gas_limit,
            basefee: base_fee,
            difficulty: U256::ZERO,
            prevrandao: Some(self.mix),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice::new_with_spec(
                self.excess_blob_gas,
                self.spec(),
            )),
            ..BlockEnv::default()
        }
    }
}

/// A mined transaction, ready to run.
#[derive(Clone, Debug)]
pub struct Tx {
    pub hash: B256,
    pub index: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub kind: u64,
    pub env: TxEnv,
}

fn quantity128(v: &Value) -> Result<u128, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    u128::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn access_list(v: &Value) -> Result<AccessList, String> {
    let mut items = Vec::new();
    for i in v.as_array().into_iter().flatten() {
        let mut keys = Vec::new();
        for k in i["storageKeys"].as_array().into_iter().flatten() {
            keys.push(parse(k)?);
        }
        items.push(AccessListItem {
            address: parse(&i["address"])?,
            storage_keys: keys,
        });
    }
    Ok(AccessList(items))
}

fn authorizations(v: &Value) -> Result<Vec<SignedAuthorization>, String> {
    let mut out = Vec::new();
    for a in v.as_array().into_iter().flatten() {
        let inner = Authorization {
            chain_id: big(&a["chainId"])?,
            address: parse(&a["address"])?,
            nonce: num(&a["nonce"])?,
        };
        let y = u8::try_from(num(&a["yParity"])?).map_err(|e| e.to_string())?;
        out.push(SignedAuthorization::new_unchecked(
            inner,
            y,
            big(&a["r"])?,
            big(&a["s"])?,
        ));
    }
    Ok(out)
}

impl Tx {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let kind = if v["type"].is_null() {
            0
        } else {
            num(&v["type"])?
        };
        let from: Address = parse(&v["from"])?;
        let to: Option<Address> = if v["to"].is_null() {
            None
        } else {
            Some(parse(&v["to"])?)
        };
        let (gas_price, priority) = if kind >= 2 {
            (
                quantity128(&v["maxFeePerGas"])?,
                Some(quantity128(&v["maxPriorityFeePerGas"])?),
            )
        } else {
            (quantity128(&v["gasPrice"])?, None)
        };
        let chain_id = if v["chainId"].is_null() {
            None
        } else {
            Some(num(&v["chainId"])?)
        };
        let mut b = TxEnv::builder()
            .tx_type(Some(u8::try_from(kind).map_err(|e| e.to_string())?))
            .caller(from)
            .kind(to.map_or(TxKind::Create, TxKind::Call))
            .value(big(&v["value"])?)
            .data(parse(&v["input"])?)
            .gas_limit(num(&v["gas"])?)
            .gas_price(gas_price)
            .gas_priority_fee(priority)
            .nonce(num(&v["nonce"])?)
            .chain_id(chain_id)
            .access_list(access_list(&v["accessList"])?);
        if kind == 3 {
            let mut hashes = Vec::new();
            for h in v["blobVersionedHashes"].as_array().into_iter().flatten() {
                hashes.push(parse(h)?);
            }
            b = b
                .blob_hashes(hashes)
                .max_fee_per_blob_gas(quantity128(&v["maxFeePerBlobGas"])?);
        }
        if kind == 4 {
            b = b.authorization_list_signed(authorizations(&v["authorizationList"])?);
        }
        Ok(Self {
            hash: parse(&v["hash"])?,
            index: num(&v["transactionIndex"])?,
            from,
            to,
            kind,
            env: b.build_fill(),
        })
    }

    pub fn fetch(up: &Upstream, hash: B256) -> Result<Self, String> {
        Self::from_json(&up.call("eth_getTransactionByHash", json!([format!("{hash:#x}")]))?)
    }
}

/// One execution, not yet committed.
pub struct Ran {
    pub success: bool,
    pub gas_used: u64,
    pub output: Bytes,
    pub logs: Vec<Log>,
    pub state: EvmState,
}

/// As the chain ran it (fees and balances checked; nonces are not, since
/// other transactions of the block are not replayed), or as a node runs
/// `eth_call` (zero base fee, zero gas price).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Chain,
    Call,
}

pub fn run<D: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<D>,
    header: &Header,
    env: TxEnv,
    mode: Mode,
) -> Result<Ran, String> {
    let spec = header.spec();
    let base_fee = if mode == Mode::Call {
        0
    } else {
        header.base_fee
    };
    let exec = {
        let mut evm = Context::mainnet()
            .with_db(&mut *db)
            .with_block(header.env(base_fee))
            .modify_cfg_chained(|cfg| {
                cfg.spec = spec;
                cfg.chain_id = 1;
                cfg.disable_nonce_check = true;
                if mode == Mode::Call {
                    cfg.tx_gas_limit_cap = Some(u64::MAX);
                }
            })
            .build_mainnet();
        evm.transact(env).map_err(|e| format!("transact: {e}"))?
    };
    let gas_used = exec.result.gas().tx_gas_used();
    let logs = exec.result.logs().to_vec();
    let (success, output) = match exec.result {
        ExecutionResult::Success { output, .. } => (
            true,
            match output {
                Output::Call(b) | Output::Create(b, _) => b,
            },
        ),
        ExecutionResult::Revert { output, .. } => (false, output),
        ExecutionResult::Halt { .. } => (false, Bytes::new()),
    };
    Ok(Ran {
        success,
        gas_used,
        output,
        logs,
        state: exec.state,
    })
}

/// A read-only call against `db`, as `eth_call`.
pub fn call<D: DatabaseRef<Error = SimError>>(
    db: &D,
    header: &Header,
    from: Address,
    to: Address,
    data: Bytes,
    gas: u64,
) -> Result<Ran, String> {
    let mut scratch = CacheDB::new(db);
    let env = TxEnv::builder()
        .caller(from)
        .kind(TxKind::Call(to))
        .data(data)
        .gas_limit(gas)
        .gas_price(0)
        .chain_id(Some(1))
        .build_fill();
    run(&mut scratch, header, env, Mode::Call)
}

/// Chainlink `AnswerUpdated` and OCR2 `NewTransmission`: a price feed moved.
const ORACLE_TOPICS: [&str; 2] = [
    "0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f",
    "0xc797025feeeaf2cd924c99e9205acb8ec04d5cad21c41ce637a38fb6dee6016a",
];

/// What the pre-state had to include for the real liquidation to work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// Nothing: liquidatable at the bot's tip (block N−1).
    Tip,
    /// Block N's earlier oracle updates (this many transactions).
    OracleUpdates(usize),
    /// All of block N's earlier transactions (this many).
    Prefix(usize),
}

/// The state after block N−1, then whichever of block N's transactions are
/// applied, in order.
pub struct PreState {
    /// Block N−1: the state the bot's tip is at.
    pub parent: Header,
    /// Block N: the environment its transactions run in.
    pub block: Header,
    pub db: CacheDB<AtBlock>,
    pub applied: Vec<B256>,
    /// What the applied transactions changed, as an `eth_call` state
    /// override over block N−1: the pre-state is block N−1 plus exactly this.
    pub overrides: Overrides,
}

/// Per account: balance, nonce, code, and changed storage slots, after
/// every applied transaction (a later one overwrites an earlier one).
#[derive(Clone, Debug, Default)]
pub struct Overrides(pub std::collections::BTreeMap<Address, AccountDiff>);

#[derive(Clone, Debug, Default)]
pub struct AccountDiff {
    pub balance: U256,
    pub nonce: u64,
    pub code: Option<Bytes>,
    pub storage: std::collections::BTreeMap<U256, U256>,
}

impl Overrides {
    fn record(&mut self, state: &EvmState) {
        for (addr, acc) in state {
            if !acc.is_touched() {
                continue;
            }
            let d = self.0.entry(*addr).or_default();
            d.balance = acc.info.balance;
            d.nonce = acc.info.nonce;
            if let Some(c) = &acc.info.code {
                if !c.is_empty() {
                    d.code = Some(c.original_bytes());
                }
            }
            for (slot, v) in &acc.storage {
                if v.is_changed() {
                    d.storage.insert(*slot, v.present_value);
                }
            }
        }
    }

    /// The `stateOverride` object `eth_call` takes (geth's shape); `None`
    /// when nothing was applied.
    #[must_use]
    pub fn to_json(&self) -> Option<Value> {
        if self.0.is_empty() {
            return None;
        }
        let mut out = serde_json::Map::new();
        for (addr, d) in &self.0 {
            let mut o = serde_json::Map::new();
            o.insert("balance".into(), json!(format!("{:#x}", d.balance)));
            o.insert("nonce".into(), json!(hex(d.nonce)));
            if let Some(c) = &d.code {
                o.insert("code".into(), json!(c));
            }
            if !d.storage.is_empty() {
                let diff: serde_json::Map<String, Value> = d
                    .storage
                    .iter()
                    .map(|(k, v)| {
                        (
                            format!("{:#x}", B256::from(*k)),
                            json!(format!("{:#x}", B256::from(*v))),
                        )
                    })
                    .collect();
                o.insert("stateDiff".into(), Value::Object(diff));
            }
            out.insert(format!("{addr:#x}"), Value::Object(o));
        }
        Some(Value::Object(out))
    }
}

impl PreState {
    pub fn before(up: &Arc<Upstream>, block: u64) -> Result<Self, String> {
        Ok(Self {
            parent: Header::fetch(up, block - 1)?,
            block: Header::fetch(up, block)?,
            db: CacheDB::new(AtBlock {
                up: Arc::clone(up),
                number: block - 1,
            }),
            applied: Vec::new(),
            overrides: Overrides::default(),
        })
    }

    /// Run a transaction of block N as the chain did, and keep its effects.
    pub fn apply(&mut self, tx: &Tx) -> Result<bool, String> {
        let ran = run(&mut self.db, &self.block, tx.env.clone(), Mode::Chain)?;
        self.overrides.record(&ran.state);
        self.db.commit(ran.state);
        self.applied.push(tx.hash);
        Ok(ran.success)
    }

    /// The state the real liquidation `hash` (index `index` of block N) ran
    /// on, as near as the bot could have known it: the bot's tip if the
    /// liquidation already works there; else after block N's earlier oracle
    /// updates (what a backrun of them sees); else after all of block N's
    /// earlier transactions.
    pub fn establish(
        up: &Arc<Upstream>,
        block: u64,
        hash: B256,
        index: u64,
    ) -> Result<(Self, Basis), String> {
        let theirs = Tx::fetch(up, hash)?;
        let pre = Self::before(up, block)?;
        if pre.try_run(theirs.env.clone(), Mode::Chain)?.success {
            return Ok((pre, Basis::Tip));
        }
        let receipts = up.call("eth_getBlockReceipts", json!([hex(block)]))?;
        let receipts = receipts.as_array().ok_or("no receipts")?;
        let oracle: Vec<B256> = receipts
            .iter()
            .filter(|r| num(&r["transactionIndex"]).is_ok_and(|i| i < index))
            .filter(|r| {
                r["logs"].as_array().into_iter().flatten().any(|l| {
                    l["topics"][0]
                        .as_str()
                        .is_some_and(|t| ORACLE_TOPICS.contains(&t))
                })
            })
            .filter_map(|r| parse(&r["transactionHash"]).ok())
            .collect();
        if !oracle.is_empty() {
            let mut pre = Self::before(up, block)?;
            for h in &oracle {
                pre.apply(&Tx::fetch(up, *h)?)?;
            }
            if pre.try_run(theirs.env.clone(), Mode::Chain)?.success {
                return Ok((pre, Basis::OracleUpdates(oracle.len())));
            }
        }
        let mut pre = Self::before(up, block)?;
        let full = up.call("eth_getBlockByNumber", json!([hex(block), true]))?;
        let mut n = 0;
        for t in full["transactions"].as_array().into_iter().flatten() {
            let tx = Tx::from_json(t)?;
            if tx.index >= index {
                break;
            }
            pre.apply(&tx)?;
            n += 1;
        }
        if pre.try_run(theirs.env, Mode::Chain)?.success {
            return Ok((pre, Basis::Prefix(n)));
        }
        Err("does not reproduce even after the whole prefix".into())
    }

    /// Run on a copy of this state, in block N's environment.
    pub fn try_run(&self, env: TxEnv, mode: Mode) -> Result<Ran, String> {
        let mut db = self.db.clone();
        run(&mut db, &self.block, env, mode)
    }
}

/// A call frame that did not succeed: depth, caller, target, the 4-byte
/// selector it was called with, how it ended, and what it returned.
#[derive(Clone, Debug)]
pub struct Frame {
    pub depth: usize,
    pub from: Address,
    pub to: Address,
    pub selector: [u8; 4],
    pub input: Bytes,
    pub result: String,
    pub output: Bytes,
}

/// Records call frames — every one, or only those that end without
/// success — and every log, in order (`events` interleaves them).
#[derive(Default)]
struct Reverts {
    all: bool,
    stack: Vec<Bytes>,
    frames: Vec<Frame>,
    events: Vec<String>,
}

impl<CTX: revm::context_interface::ContextTr> revm::Inspector<CTX> for Reverts {
    fn log(&mut self, _context: &mut CTX, log: Log) {
        // ERC-20 `Transfer(from, to, value)`.
        let t = log.topics();
        if t.len() == 3
            && t[0]
                == alloy_primitives::b256!(
                    "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                )
            && log.data.data.len() == 32
        {
            self.events.push(format!(
                "{}Transfer {:#x}: {:#x} -> {:#x} {}",
                "  ".repeat(self.stack.len()),
                log.address,
                Address::from_word(t[1]),
                Address::from_word(t[2]),
                U256::from_be_slice(&log.data.data)
            ));
        }
    }

    fn call(
        &mut self,
        context: &mut CTX,
        inputs: &mut revm::interpreter::CallInputs,
    ) -> Option<revm::interpreter::CallOutcome> {
        self.stack.push(inputs.input.bytes(context));
        None
    }

    fn call_end(
        &mut self,
        _context: &mut CTX,
        inputs: &revm::interpreter::CallInputs,
        outcome: &mut revm::interpreter::CallOutcome,
    ) {
        let input = self.stack.pop().unwrap_or_default();
        if self.all {
            let mut s = String::new();
            for b in input.iter().take(4) {
                s.push_str(&format!("{b:02x}"));
            }
            self.events.push(format!(
                "{}end {:#x} -> {:#x} sel 0x{s} {:?} gas {}",
                "  ".repeat(self.stack.len()),
                inputs.caller,
                inputs.target_address,
                outcome.result.result,
                outcome.result.gas.total_gas_spent()
            ));
        }
        if !outcome.result.result.is_ok() {
            let mut selector = [0u8; 4];
            for (i, b) in input.iter().take(4).enumerate() {
                selector[i] = *b;
            }
            self.frames.push(Frame {
                depth: self.stack.len(),
                from: inputs.caller,
                to: inputs.target_address,
                selector,
                input,
                result: format!("{:?}", outcome.result.result),
                output: outcome.result.output.clone(),
            });
        }
    }
}

/// `env` as a transaction in `header`'s block on `db`, with every frame
/// that failed along the way (innermost first, as they ended).
pub fn trace_reverts<D: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<D>,
    header: &Header,
    env: TxEnv,
) -> Result<(bool, Vec<Frame>), String> {
    trace(db, header, env, false).map(|(ok, f, _)| (ok, f))
}

/// [`trace_reverts`], optionally with every frame's end and every Transfer
/// in order.
pub fn trace<D: DatabaseRef<Error = SimError>>(
    db: &mut CacheDB<D>,
    header: &Header,
    env: TxEnv,
    all: bool,
) -> Result<(bool, Vec<Frame>, Vec<String>), String> {
    use revm::InspectEvm;
    let spec = header.spec();
    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(header.env(header.base_fee))
        .modify_cfg_chained(|cfg| {
            cfg.spec = spec;
            cfg.chain_id = 1;
            cfg.disable_nonce_check = true;
        })
        .build_mainnet_with_inspector(Reverts {
            all,
            ..Reverts::default()
        });
    let res = evm
        .inspect_one_tx(env)
        .map_err(|e| format!("inspect: {e}"))?;
    let ok = res.is_success();
    Ok((
        ok,
        std::mem::take(&mut evm.inspector.frames),
        std::mem::take(&mut evm.inspector.events),
    ))
}
