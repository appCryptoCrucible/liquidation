//! Single-writer triple-buffer publish of the canonical [`PriceVector`]
//! (GUIDE 06 §7b). Hot-thread reads are wait-free.

use liq_types::PriceVector;
use triple_buffer::{Input, Output, TripleBuffer};

/// Writer half. Oracle / fusion thread only.
pub struct PricePublish {
    input: Input<PriceVector>,
}

/// Reader half. Any number of hot-path consumers; each owns its `Output`.
pub struct PriceRead {
    output: Output<PriceVector>,
}

/// Split a triple buffer seeded with `init`. `init` is cloned three times
/// by the crate; it must already be sized (never a fabricated price).
#[must_use]
pub fn split(init: &PriceVector) -> (PricePublish, PriceRead) {
    let (input, output) = TripleBuffer::new(init).split();
    (PricePublish { input }, PriceRead { output })
}

impl PricePublish {
    /// Publish a complete vector. Wait-free for the reader.
    #[inline]
    pub fn write(&mut self, vector: PriceVector) {
        self.input.write(vector);
    }
}

impl PriceRead {
    /// Latest complete vector. Wait-free; never a torn read.
    #[inline]
    pub fn read(&mut self) -> &PriceVector {
        self.output.read()
    }
}

#[cfg(test)]
mod tests {
    use super::split;
    use liq_types::fixed::Ray;
    use liq_types::{AssetId, Price, PriceVector, SourceKind};

    fn px(asset: u16, raw: u64, block: u64, ts: u64) -> Price {
        Price {
            asset: AssetId(asset),
            price: Ray::from_raw(alloy_primitives::U256::from(raw)),
            source: SourceKind::Canonical,
            block,
            ts,
        }
    }

    /// Oracle: GUIDE 06 §7b — the reader sees the latest complete write, not
    /// a queue of superseded vectors.
    #[test]
    fn reader_sees_latest_complete_vector() {
        let init = PriceVector(vec![px(0, 1, 1, 1)]);
        let (mut w, mut r) = split(&init);
        assert_eq!(r.read().0[0].ts, 1);
        w.write(PriceVector(vec![px(0, 2, 2, 2)]));
        w.write(PriceVector(vec![px(0, 3, 3, 3)]));
        let got = r.read();
        assert_eq!(got.0.len(), 1, "oracle: Def — one slot");
        assert_eq!(got.0[0].ts, 3, "oracle: GUIDE-06 §7b latest complete");
        assert_eq!(
            got.0[0].price,
            Ray::from_raw(alloy_primitives::U256::from(3u64))
        );
        assert!(matches!(got.0[0].source, SourceKind::Canonical));
    }
}
