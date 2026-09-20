//! `X-Flashbots-Signature`: EIP-191 of keccak256(body).hex() (Flashbots spec).

use super::{MevShareError, Result};
use alloy_primitives::{keccak256, Address};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

/// Searcher identity key. Holds no funds (GUIDE 13 §1 / D20).
#[derive(Clone)]
pub struct SearcherKey {
    signer: PrivateKeySigner,
}

impl SearcherKey {
    /// 32-byte secp256k1 secret. Zero key is rejected by the signer library.
    pub fn from_secret(secret: alloy_primitives::B256) -> Result<Self> {
        let signer = PrivateKeySigner::from_bytes(&secret)
            .map_err(|e| MevShareError::Signature(e.to_string()))?;
        Ok(Self { signer })
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.signer.address()
    }
}

/// keccak256(utf-8 body) as `0x`-prefixed hex — `ethers.id(body)`.
#[must_use]
pub fn body_id_hex(body: &[u8]) -> String {
    format!("{:#x}", keccak256(body))
}

/// Sign `body` and format `X-Flashbots-Signature: address:signature`.
pub fn sign_body(key: &SearcherKey, body: &[u8]) -> Result<String> {
    let id = body_id_hex(body);
    let sig = key
        .signer
        .sign_message_sync(id.as_bytes())
        .map_err(|e| MevShareError::Signature(e.to_string()))?;
    Ok(flashbots_header(key.address(), &sig.to_string()))
}

#[must_use]
pub fn flashbots_header(address: Address, signature_hex: &str) -> String {
    format!("{address:#x}:{signature_hex}")
}

/// Recover the signer and check it matches the header address.
pub fn verify_header(header: &str, body: &[u8]) -> Result<Address> {
    let (addr_s, sig_s) = header.split_once(':').ok_or(MevShareError::BadAuthHeader)?;
    let header_addr: Address = addr_s.parse().map_err(|_| MevShareError::BadAuthHeader)?;
    let sig: alloy_primitives::Signature = sig_s
        .parse()
        .map_err(|e: alloy_primitives::SignatureError| MevShareError::Signature(e.to_string()))?;
    let id = body_id_hex(body);
    let recovered = sig
        .recover_address_from_msg(id.as_bytes())
        .map_err(|e| MevShareError::Signature(e.to_string()))?;
    if recovered != header_addr {
        return Err(MevShareError::SignerMismatch {
            recovered,
            header: header_addr,
        });
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::{sign_body, verify_header, SearcherKey};
    use alloy_primitives::b256;

    /// Anvil account 0. Test key only.
    const SECRET: alloy_primitives::B256 =
        b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    /// Oracle: Flashbots auth — sign then recover the same address.
    #[test]
    fn flashbots_sign_verify_round_trip() {
        let key = SearcherKey::from_secret(SECRET).unwrap();
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"mev_sendBundle","params":[]}"#;
        let header = sign_body(&key, body).unwrap();
        let recovered = verify_header(&header, body).unwrap();
        assert_eq!(recovered, key.address());
        assert!(
            header.starts_with(&format!("{:#x}:", key.address())),
            "oracle: GUIDE-13 header is address:signature; got {header}"
        );
        assert!(
            verify_header(&header, b"tampered").is_err(),
            "oracle: Def — wrong body must not verify"
        );
    }
}
