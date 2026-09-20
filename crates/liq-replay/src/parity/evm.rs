//! revm execution of the compiled official math oracles.

use alloy_primitives::{hex, Address, Bytes, U256};
use revm::context::result::{ExecutionResult, Output};
use revm::context::TxEnv;
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::state::{AccountInfo, Bytecode};
use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

use super::ParityError;

/// Synthetic address the oracle bytecode is inserted at. Not a mainnet pool.
pub const ORACLE: Address = Address::repeat_byte(0x51);

const UNI_HEX: &str = include_str!("bytecode/uni_oracle.bin-runtime");
const KYBER_HEX: &str = include_str!("bytecode/kyber_oracle.bin-runtime");

fn decode_hex(s: &str) -> Result<Bytes, ParityError> {
    let t = s.trim();
    hex::decode(t)
        .map(Bytes::from)
        .map_err(|_| ParityError::Abi("oracle bytecode hex"))
}

pub fn uni_oracle_runtime() -> Result<Bytes, ParityError> {
    decode_hex(UNI_HEX)
}

pub fn kyber_oracle_runtime() -> Result<Bytes, ParityError> {
    decode_hex(KYBER_HEX)
}

pub fn insert_runtime(db: &mut CacheDB<EmptyDB>, at: Address, runtime: Bytes) {
    let code = Bytecode::new_raw(runtime);
    db.insert_account_info(
        at,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: code.hash_slow(),
            code: Some(code),
            account_id: None,
        },
    );
}

/// Pure/view call. Revert → error (not a guessed output).
pub fn call_pure(
    db: &mut CacheDB<EmptyDB>,
    to: Address,
    data: Bytes,
) -> Result<Bytes, ParityError> {
    let tx = TxEnv::builder()
        .caller(Address::repeat_byte(0xA0))
        .kind(TxKind::Call(to))
        .value(U256::ZERO)
        .data(data)
        .gas_limit(8_000_000)
        .gas_price(0)
        .build_fill();
    let exec = {
        let mut evm = Context::mainnet()
            .with_db(&mut *db)
            .modify_cfg_chained(|cfg| {
                cfg.spec = SpecId::SHANGHAI;
                cfg.disable_nonce_check = true;
                cfg.tx_gas_limit_cap = Some(100_000_000);
            })
            .build_mainnet();
        evm.transact(tx)
            .map_err(|e| ParityError::Revm(e.to_string()))?
    };
    match exec.result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Call(b) | Output::Create(b, _) => Ok(b),
        },
        ExecutionResult::Revert { output, .. } => Err(ParityError::Revm(format!(
            "revert {}",
            hex::encode(&output)
        ))),
        ExecutionResult::Halt { reason, .. } => Err(ParityError::Revm(format!("halt {reason:?}"))),
    }
}
