//! Trace identity and stage boundary emit (GUIDE 00 §5). The hot path never
//! links `liq-obs`; GUIDE 09's subscriber turns these events into histograms.

/// Created at the triggering input, carried through every stage.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceId(u64);

impl TraceId {
    /// Wrap a raw id. The allocator lives in later WPs; this is the constructor.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw id, for logging fields.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Hot-path stage boundaries (GUIDE 09 §1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    PriceTick,
    Candidate,
    Quote,
    RouteSolved,
    SimVerified,
    Signed,
    VenueAck,
    Inclusion,
}

/// Every stage boundary calls this. Emits a `tracing` event; GUIDE 09's
/// subscriber (in `liq-obs`) turns them into histograms.
#[inline]
pub fn stage(trace: TraceId, stage: Stage) {
    tracing::trace!(trace = trace.raw(), ?stage);
}
