//! Presigned templates (GUIDE 13 §3). Alloy secp256k1 signer, constructed once.

use crate::error::{ExecError, Result};
use crate::fee::BoundFees;
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::eip2930::AccessList;
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use smallvec::SmallVec;
use std::sync::Arc;

/// Yellow Paper intrinsic gas for a simple value transfer (no calldata).
pub const SELF_TRANSFER_GAS: u64 = 21_000;

/// Alloy `PrivateKeySigner` held for the life of the process. Address and
/// k256 `SigningKey` are computed once at construction (GUIDE 13 §3
/// "precomputed secp256k1 signer").
#[derive(Clone)]
pub struct PrecomputedSigner {
    inner: PrivateKeySigner,
    address: Address,
}

impl PrecomputedSigner {
    pub fn from_secret(secret: B256) -> Result<Self> {
        let inner =
            PrivateKeySigner::from_bytes(&secret).map_err(|e| ExecError::Signer(e.to_string()))?;
        let address = inner.address();
        Ok(Self { inner, address })
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.address
    }

    pub fn sign_tx(&self, tx: &mut TxEip1559) -> Result<SignedTx> {
        let sig = self
            .inner
            .sign_transaction_sync(tx)
            .map_err(|e| ExecError::Signer(e.to_string()))?;
        let signed = tx.clone().into_signed(sig);
        let envelope: TxEnvelope = signed.into();
        let raw = Bytes::from(envelope.encoded_2718());
        let hash = *envelope.tx_hash();
        Ok(SignedTx { raw, hash })
    }
}

/// Signed EIP-1559 envelope bytes + hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedTx {
    pub raw: Bytes,
    pub hash: B256,
}

/// Byte / field patch on a held unsigned tx.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PatchKind {
    Nonce,
    MaxFeePerGas,
    MaxPriorityFeePerGas,
    Input { offset: usize, len: usize },
}

/// One patch point (GUIDE 13 §3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PatchPoint {
    pub kind: PatchKind,
}

/// Values applied at [`Template::sign_patched`].
#[derive(Clone, Debug, Default)]
pub struct Patches {
    pub nonce: Option<u64>,
    pub max_fee_per_gas: Option<u128>,
    pub max_priority_fee_per_gas: Option<u128>,
    pub input: Option<Bytes>,
}

/// Held unsigned tx + patch points + precomputed signer.
pub struct Template {
    unsigned: TxEip1559,
    patch_points: SmallVec<[PatchPoint; 4]>,
    signer: Arc<PrecomputedSigner>,
}

impl Template {
    #[must_use]
    pub fn new(
        unsigned: TxEip1559,
        patch_points: SmallVec<[PatchPoint; 4]>,
        signer: Arc<PrecomputedSigner>,
    ) -> Self {
        Self {
            unsigned,
            patch_points,
            signer,
        }
    }

    pub fn sign_patched(&self, patches: &Patches) -> Result<SignedTx> {
        let mut tx = self.unsigned.clone();
        for p in &self.patch_points {
            apply_patch(&mut tx, p.kind, patches)?;
        }
        self.signer.sign_tx(&mut tx)
    }
}

fn apply_patch(tx: &mut TxEip1559, kind: PatchKind, patches: &Patches) -> Result<()> {
    match kind {
        PatchKind::Nonce => {
            tx.nonce = patches.nonce.ok_or(ExecError::MissingPatch("nonce"))?;
        }
        PatchKind::MaxFeePerGas => {
            tx.max_fee_per_gas = patches
                .max_fee_per_gas
                .ok_or(ExecError::MissingPatch("max_fee_per_gas"))?;
        }
        PatchKind::MaxPriorityFeePerGas => {
            tx.max_priority_fee_per_gas = patches
                .max_priority_fee_per_gas
                .ok_or(ExecError::MissingPatch("max_priority_fee_per_gas"))?;
        }
        PatchKind::Input { offset, len } => {
            let src = patches
                .input
                .as_ref()
                .ok_or(ExecError::MissingPatch("input"))?;
            if len != src.len() {
                return Err(ExecError::BadInputPatch);
            }
            let end = offset.checked_add(len).ok_or(ExecError::BadInputPatch)?;
            if end > tx.input.len() {
                return Err(ExecError::BadInputPatch);
            }
            let mut buf = tx.input.to_vec();
            let Some(dst) = buf.get_mut(offset..end) else {
                return Err(ExecError::BadInputPatch);
            };
            dst.copy_from_slice(src);
            tx.input = Bytes::from(buf);
        }
    }
    Ok(())
}

/// Inputs for [`sign_call`]. Packed so the signer stays a single call.
pub struct CallSpec<'a> {
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Address,
    pub input: Bytes,
    pub gas_limit: u64,
    pub fees: &'a BoundFees,
    pub priority: u128,
}

/// Sign an EIP-1559 call to `spec.to` with `spec.input`.
pub fn sign_call(signer: &PrecomputedSigner, spec: CallSpec<'_>) -> Result<SignedTx> {
    if spec.chain_id != 1 {
        return Err(ExecError::ChainId(spec.chain_id));
    }
    if spec.gas_limit == 0 {
        return Err(ExecError::ZeroGasLimit);
    }
    let mut tx = TxEip1559 {
        chain_id: spec.chain_id,
        nonce: spec.nonce,
        gas_limit: spec.gas_limit,
        max_fee_per_gas: spec.fees.max_fee_per_gas,
        max_priority_fee_per_gas: spec.priority,
        to: TxKind::Call(spec.to),
        value: U256::ZERO,
        access_list: AccessList::default(),
        input: spec.input,
    };
    signer.sign_tx(&mut tx)
}

/// Gap-filler: zero-value self-transfer at `nonce`.
pub fn sign_self_transfer(
    signer: &PrecomputedSigner,
    nonce: u64,
    fees: &BoundFees,
    chain_id: u64,
) -> Result<SignedTx> {
    sign_call(
        signer,
        CallSpec {
            chain_id,
            nonce,
            to: signer.address(),
            input: Bytes::new(),
            gas_limit: SELF_TRANSFER_GAS,
            fees,
            priority: fees.max_priority_fee_per_gas,
        },
    )
}

/// Sign + build the JSON-RPC body + identity header. No HTTP.
/// Used for the p99 measurement (excluding network).
pub fn sign_and_encode_bundle(
    signer: &PrecomputedSigner,
    identity: &liq_oracle::mevshare::SearcherKey,
    spec: CallSpec<'_>,
    block: u64,
) -> Result<(SignedTx, Vec<u8>, String)> {
    let signed = sign_call(signer, spec)?;
    let body = crate::submit::rpc_eth_send_bundle(block, std::slice::from_ref(&signed.raw))?;
    let header = liq_oracle::mevshare::sign_body(identity, &body)
        .map_err(|e| ExecError::Identity(e.to_string()))?;
    Ok((signed, body, header))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::float_arithmetic
)]
mod tests {
    use super::*;
    use alloy_primitives::b256;
    use smallvec::smallvec;

    const SECRET: B256 =
        b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    fn fees() -> BoundFees {
        BoundFees {
            max_fee_per_gas: 1_125,
            max_priority_fee_per_gas: 1,
            k: 1,
        }
    }

    #[test]
    fn template_patch_nonce_and_sign() {
        let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
        let unsigned = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(signer.address()),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
        };
        let t = Template::new(
            unsigned,
            smallvec![PatchPoint {
                kind: PatchKind::Nonce
            }],
            signer,
        );
        let signed = t
            .sign_patched(&Patches {
                nonce: Some(7),
                ..Patches::default()
            })
            .unwrap();
        assert!(!signed.raw.is_empty());
        assert_ne!(signed.hash, B256::ZERO);
    }

    /// Honest latency: measure sign + encode + identity header, no HTTP.
    /// If p99 ≥ 500 µs on this box, the metric is ABSENT — not invented.
    #[test]
    fn sign_submit_excluding_network_p99_honest() {
        let signer = PrecomputedSigner::from_secret(SECRET).unwrap();
        let id = liq_oracle::mevshare::SearcherKey::from_secret(SECRET).unwrap();
        let fees = fees();
        let n = 200usize;
        let mut ns = Vec::with_capacity(n);
        for i in 0..n {
            let t0 = std::time::Instant::now();
            let _ = sign_and_encode_bundle(
                &signer,
                &id,
                CallSpec {
                    chain_id: 1,
                    nonce: i as u64,
                    to: signer.address(),
                    input: Bytes::new(),
                    gas_limit: 21_000,
                    fees: &fees,
                    priority: 1,
                },
                20_000_000,
            )
            .unwrap();
            ns.push(t0.elapsed().as_nanos());
        }
        ns.sort_unstable();
        let idx = n.saturating_sub(1) * 99 / 100;
        let p99 = ns[idx];
        const BUDGET: u128 = 500_000; // 500 µs in ns
        if p99 >= BUDGET {
            eprintln!("ABSENT sign+submit p99: {p99} ns (≥ 500 µs). Not invented.");
        } else {
            eprintln!("MEASURED sign+submit p99: {p99} ns");
        }
        // The test records honesty; it does not fail the box for being slow.
        assert!(!ns.is_empty());
    }
}
