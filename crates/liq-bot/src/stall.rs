//! Node-stall heartbeat (GUIDE 14).
//!
//! [`crate::drain::DrainJoin::observe_parent_header`] only runs when a header
//! arrives. This thread measures silence since that arrival. A stalled node
//! produces no header, so the lag is `elapsed / 12s`. No arrival yet → no
//! call. The slot length is Ethereum's 12 seconds.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use liq_risk::RiskGate;

/// Ethereum proof-of-stake slot, seconds.
pub const SLOT_SECS: u64 = 12;

/// Last header arrival. Shared by the hot thread and the stall thread.
pub struct HeaderClock {
    seen: AtomicBool,
    last_ms: AtomicU64,
}

impl HeaderClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            seen: AtomicBool::new(false),
            last_ms: AtomicU64::new(0),
        }
    }

    pub fn note_now(&self) {
        if let Some(ms) = unix_ms() {
            self.note(ms);
        }
    }

    pub fn note(&self, now_ms: u64) {
        self.last_ms.store(now_ms, Ordering::Release);
        self.seen.store(true, Ordering::Release);
    }

    /// Whole slots since the last header. `None` before the first header
    /// or if the clock goes backwards.
    #[must_use]
    pub fn lag_blocks(&self, now_ms: u64) -> Option<u64> {
        if !self.seen.load(Ordering::Acquire) {
            return None;
        }
        let last = self.last_ms.load(Ordering::Acquire);
        let elapsed_ms = now_ms.checked_sub(last)?;
        let elapsed_secs = elapsed_ms.checked_div(1_000)?;
        elapsed_secs.checked_div(SLOT_SECS)
    }
}

impl Default for HeaderClock {
    fn default() -> Self {
        Self::new()
    }
}

fn unix_ms() -> Option<u64> {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let secs = d.as_secs();
    let ms = secs.checked_mul(1_000)?;
    let sub = u64::from(d.subsec_millis());
    ms.checked_add(sub)
}

/// Wakes every slot and reports silence to the gate. Does not invent a lag
/// before the first header.
pub fn spawn(clock: Arc<HeaderClock>, gate: &'static RiskGate) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("liq-bot-stall".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(SLOT_SECS));
            let Some(now) = unix_ms() else {
                tracing::error!("system clock before epoch — stall lag withheld");
                continue;
            };
            if let Some(lag) = clock.lag_blocks(now) {
                gate.observe_node_lag(lag);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_slots_and_unseen_is_none() {
        let c = HeaderClock::new();
        assert!(c.lag_blocks(50_000).is_none());
        c.note(0);
        assert_eq!(c.lag_blocks(0), Some(0));
        assert_eq!(c.lag_blocks(12_000), Some(1));
        assert_eq!(c.lag_blocks(36_000), Some(3));
        c.note(50_000);
        assert!(c.lag_blocks(1).is_none(), "clock backwards is not a lag");
    }
}
