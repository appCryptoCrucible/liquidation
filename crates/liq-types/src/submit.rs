//! Submission contract (GUIDE 13 §1, GUIDE 09 §4). No signing type: signing is
//! `liq-exec`'s concern (WP 13A). `ShadowRecorder` implements [`Submitter`].

use crate::trace::TraceId;
use alloy_primitives::{Bytes, U256};

/// Builder identity for [`Venue::BuilderBundle`] (GUIDE 13 §1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BuilderId(pub u16);

/// Submission venue. No public-mempool variant (D19, D54).
///
/// Relay / endpoint strings are `&'static str`, not `String`/`Url`: venues are
/// fixed at config load (`config/venues.toml`, `config/builders.toml`, leaked
/// once like `&'static Shared`), and an [`IntendedSubmission`] is built on the
/// hot thread per candidate — RUST-CONVENTIONS §6 forbids an allocation there.
/// `Copy` follows.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Venue {
    /// SVR-protected oracle backruns (Aave on mainnet).
    MevShare { relay: &'static str },
    /// Direct to builders. Non-SVR triggers.
    BuilderBundle {
        endpoint: &'static str,
        builder: BuilderId,
    },
}

/// What would be sent: plan bytes, bid, venue, deadline, [`TraceId`].
/// No signing material (GUIDE 09 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntendedSubmission {
    pub plan: Bytes,
    pub bid: U256,
    pub venue: Venue,
    pub deadline: u64,
    pub trace: TraceId,
}

/// Immediate submit result.
///
/// * [`Self::Shadow`] — 09A `ShadowRecorder` JSONL write (unchanged).
/// * [`Self::Recorded`] — 13A path signed and recorded; HTTP not sent.
/// * [`Self::Denied`] — `RiskAllow` refused; HTTP not sent.
/// * [`Self::Accepted`] — live POST received a JSON-RPC `result`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SubmitReceipt {
    Shadow,
    Recorded,
    Denied,
    Accepted,
}

/// Implemented by `liq-obs::ShadowRecorder` (09A) and `liq-exec` (13A).
pub trait Submitter: Send + Sync {
    type Error;
    fn submit(&self, submission: &IntendedSubmission) -> Result<SubmitReceipt, Self::Error>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::SubmitReceipt;

    #[test]
    fn shadow_variant_still_exists() {
        assert_eq!(SubmitReceipt::Shadow, SubmitReceipt::Shadow);
        assert_ne!(SubmitReceipt::Shadow, SubmitReceipt::Recorded);
        assert_ne!(SubmitReceipt::Shadow, SubmitReceipt::Denied);
        assert_ne!(SubmitReceipt::Shadow, SubmitReceipt::Accepted);
    }
}
