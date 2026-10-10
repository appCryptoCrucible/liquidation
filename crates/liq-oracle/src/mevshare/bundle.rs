//! `mev_sendBundle` body + identity signing. Live POST is WP 13A.

use super::sign::{sign_body, SearcherKey};
use super::{MevShareError, Result, FLASHBOTS_RELAY};
use alloy_primitives::{Bytes, B256};
use liq_types::{IntendedSubmission, SubmitReceipt, Submitter, Venue};
use serde_json::json;

/// `mev_sendBundle` schema version. mev-share-node accepts exactly
/// `"v0.1"` or `"beta-1"` and rejects anything else, an absent field
/// included, with `unsupported bundle version`
/// (`mevshare/bundle_validation.go`, `validateBundleInner`).
pub const MEV_SHARE_VERSION: &str = "v0.1";

/// Bundle params. Hinted oracle tx by hash, then one signed liquidation.
///
/// No `validity.refund` and no `refundConfig`. The matchmaker writes
/// `validity.refund` when it replaces the hash: the oracle bundle's
/// `privacy.wantRefund`, otherwise the node default. That percent is a
/// split of this liquidation's coinbase payment, and the SSE hint does
/// not carry it. `refundConfig` is the originator's address split of
/// that refund.
///
/// `builders` becomes `privacy.builders`. In mev-share-node the internal
/// builder (Flashbots' orderflow into BuilderNet) always receives the
/// bundle; the names select external builders from Flashbots' registry
/// (`flashbots/dowg` `builder-registrations.json`, lowercased by the node).
/// The node intersects this list with the oracle bundle's own
/// `privacy.builders` when it replaces the hash, so a builder the origin
/// did not allow is dropped there. Empty means internal builders only.
/// No `privacy.hints`: an unmatched (`{hash}`-first) bundle that sets hints
/// is refused (`invalid bundle privacy`).
#[derive(Clone, Debug)]
pub struct SendBundle {
    pub block: u64,
    pub max_block: u64,
    pub hint_hash: B256,
    pub signed_liquidation: Bytes,
    pub builders: &'static [&'static str],
}

/// Signed JSON-RPC body + `X-Flashbots-Signature` value.
#[derive(Clone, Debug)]
pub struct SignedRelayRequest {
    pub relay: &'static str,
    pub body: Vec<u8>,
    pub signature_header: String,
}

/// JSON-RPC `mev_sendBundle` UTF-8 body (the bytes that are signed).
pub fn rpc_send_bundle(b: &SendBundle) -> Result<Vec<u8>> {
    let mut params = json!({
        "version": MEV_SHARE_VERSION,
        "inclusion": {
            "block": format!("0x{:x}", b.block),
            "maxBlock": format!("0x{:x}", b.max_block),
        },
        "body": [
            { "hash": format!("{:#x}", b.hint_hash) },
            { "tx": format!("{:#x}", b.signed_liquidation), "canRevert": false },
        ],
    });
    if !b.builders.is_empty() {
        if let Some(obj) = params.as_object_mut() {
            obj.insert("privacy".into(), json!({ "builders": b.builders }));
        }
    }
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "mev_sendBundle",
        "params": [params],
    });
    serde_json::to_vec(&payload).map_err(|e| MevShareError::HintJson(e.to_string()))
}

/// Venue-typed submitter. Adding another auction is a new `Submitter` impl.
pub struct MevShareSubmitter {
    pub relay: &'static str,
    key: SearcherKey,
}

impl MevShareSubmitter {
    #[must_use]
    pub fn new(relay: &'static str, key: SearcherKey) -> Self {
        Self { relay, key }
    }

    #[must_use]
    pub fn flashbots(key: SearcherKey) -> Self {
        Self::new(FLASHBOTS_RELAY, key)
    }

    /// Leak the relay string once (00C Venue `&'static str`).
    #[must_use]
    pub fn from_config_relay(relay: String, key: SearcherKey) -> Self {
        Self::new(super::leak_endpoint(relay), key)
    }

    pub fn sign_send_bundle(&self, bundle: &SendBundle) -> Result<SignedRelayRequest> {
        let body = rpc_send_bundle(bundle)?;
        let signature_header = sign_body(&self.key, &body)?;
        Ok(SignedRelayRequest {
            relay: self.relay,
            body,
            signature_header,
        })
    }
}

impl Submitter for MevShareSubmitter {
    type Error = MevShareError;

    fn submit(&self, submission: &IntendedSubmission) -> Result<SubmitReceipt, MevShareError> {
        let Venue::MevShare { relay } = submission.venue else {
            return Err(MevShareError::WrongVenue);
        };
        if relay != self.relay {
            return Err(MevShareError::RelayMismatch);
        }
        Err(MevShareError::LiveSendIs13A)
    }
}

/// POST an already-signed request. 13A calls this; tests use a mock relay.
pub async fn post_signed(
    client: &reqwest::Client,
    req: &SignedRelayRequest,
) -> Result<serde_json::Value> {
    let resp = client
        .post(req.relay)
        .header("Content-Type", "application/json")
        .header("X-Flashbots-Signature", &req.signature_header)
        .body(req.body.clone())
        .send()
        .await
        .map_err(|e| MevShareError::Http(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| MevShareError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(MevShareError::Http(format!("relay {status}: {text}")));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| MevShareError::Rpc(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(MevShareError::Rpc(err.to_string()));
    }
    if v.get("result").is_none() {
        return Err(MevShareError::Rpc("JSON-RPC result missing".into()));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::{rpc_send_bundle, MevShareSubmitter, SendBundle};
    use crate::mevshare::sign::{verify_header, SearcherKey};
    use alloy_primitives::{b256, Bytes};
    use liq_types::{IntendedSubmission, Submitter, TraceId, Venue};

    const SECRET: alloy_primitives::B256 =
        b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    #[test]
    fn send_bundle_signs_and_verifies() {
        let key = SearcherKey::from_secret(SECRET).unwrap();
        let sub = MevShareSubmitter::flashbots(key);
        let bundle = SendBundle {
            block: 20_000_000,
            max_block: 20_000_002,
            hint_hash: b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            signed_liquidation: Bytes::from(vec![0x02, 0xf8]),
            builders: &[],
        };
        let signed = sub.sign_send_bundle(&bundle).unwrap();
        assert_eq!(signed.relay, crate::mevshare::FLASHBOTS_RELAY);
        verify_header(&signed.signature_header, &signed.body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&signed.body).unwrap();
        assert_eq!(v["method"], "mev_sendBundle");
        assert_eq!(
            v["params"][0]["body"][0]["hash"],
            format!("{:#x}", bundle.hint_hash)
        );
        assert_eq!(v["params"][0]["body"][1]["canRevert"], false);
        assert!(v["params"][0].get("validity").is_none());
        // mev-share-node: `Version != "beta-1" && Version != "v0.1"` is refused.
        assert_eq!(v["params"][0]["version"], "v0.1");
        assert!(
            v["params"][0].get("privacy").is_none(),
            "no builders named: no privacy object (internal builders only)"
        );

        let intended = IntendedSubmission {
            plan: Bytes::new(),
            bid: alloy_primitives::U256::from(1u64),
            venue: Venue::MevShare {
                relay: crate::mevshare::FLASHBOTS_RELAY,
            },
            deadline: 1,
            trace: TraceId::from_raw(1),
        };
        assert!(matches!(
            sub.submit(&intended),
            Err(crate::mevshare::MevShareError::LiveSendIs13A)
        ));
        let _ = rpc_send_bundle;
    }

    /// Shape checked against mev-share-node `SendMevBundleArgs` /
    /// `MevBundlePrivacy` json tags (`version`, `privacy.builders`) and its
    /// validation: builders allowed on an unmatched bundle, hints are not.
    #[test]
    fn builders_ride_in_privacy_without_hints() {
        let bundle = SendBundle {
            block: 0x10,
            max_block: 0x12,
            hint_hash: b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            signed_liquidation: Bytes::from(vec![0x02, 0xf8]),
            builders: &["flashbots", "titan", "quasar"],
        };
        let v: serde_json::Value =
            serde_json::from_slice(&rpc_send_bundle(&bundle).unwrap()).unwrap();
        let p = &v["params"][0];
        assert_eq!(p["version"], "v0.1");
        assert_eq!(p["inclusion"]["block"], "0x10");
        assert_eq!(p["inclusion"]["maxBlock"], "0x12");
        assert_eq!(
            p["privacy"]["builders"],
            serde_json::json!(["flashbots", "titan", "quasar"])
        );
        assert!(
            p["privacy"].get("hints").is_none(),
            "unmatched bundle with hints is refused"
        );
        assert!(p["privacy"].get("wantRefund").is_none());
    }
}
