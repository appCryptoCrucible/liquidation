//! Watcher ↔ engine join. Consumes **emitted** engine records, not `liq-engine` types.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, U256};
use liq_types::{MarketId, PositionKey, ProtocolId, Ray};
use liq_watch::join::EngineJoin;
use liq_watch::types::DecodedLiquidation;
use parking_lot::RwLock;

use crate::alert::{AlarmSink, DigestSink};
use crate::error::{ObsError, Result};
use crate::outcome::Outcome;

/// One engine decision as logged by hot-path crates (`target = "liq_engine_join"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineEmit {
    pub protocol: u16,
    pub market: u32,
    pub user: Address,
    pub kind: EngineKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineKind {
    /// Engine flagged the position liquidatable (`computed < 1`).
    Flagged { computed: Ray },
    /// Engine knew the position and declined with a reason (digest, not alarm).
    Declined { reason: String },
    /// Engine published a health factor. `computed >= 1` on a real liquidation is `HealthWrong`.
    Health { computed: Ray, actual: Ray },
}

impl EngineEmit {
    #[must_use]
    pub fn key(&self) -> PositionKey {
        PositionKey {
            protocol: ProtocolId(self.protocol),
            market: MarketId(self.market),
            user: self.user,
        }
    }
}

pub struct JoinState {
    by_position: HashMap<PositionKey, EngineEmit>,
}

impl JoinState {
    fn new() -> Self {
        Self {
            by_position: HashMap::new(),
        }
    }
}

/// Second opinion: watcher `DecodedLiquidation` vs engine emits.
pub struct WatchEngineJoin {
    inner: RwLock<JoinState>,
    alarm: Arc<dyn AlarmSink>,
    digest: Arc<dyn DigestSink>,
    our_liquidator: Option<Address>,
}

impl WatchEngineJoin {
    #[must_use]
    pub fn new(
        alarm: Arc<dyn AlarmSink>,
        digest: Arc<dyn DigestSink>,
        our_liquidator: Option<Address>,
    ) -> Self {
        Self {
            inner: RwLock::new(JoinState::new()),
            alarm,
            digest,
            our_liquidator,
        }
    }

    /// Ingest a parsed engine emit (from the tracing subscriber, never from `liq-engine` types).
    pub fn ingest(&self, emit: EngineEmit) {
        self.inner.write().by_position.insert(emit.key(), emit);
    }

    pub fn classify(&self, ev: &DecodedLiquidation) -> Result<Outcome> {
        let key = PositionKey {
            protocol: ProtocolId(ev.protocol),
            market: MarketId(ev.market),
            user: ev.user,
        };
        let guard = self.inner.read();
        let Some(emit) = guard.by_position.get(&key) else {
            return Ok(Outcome::NotTracked { position: key });
        };
        match &emit.kind {
            EngineKind::Declined { reason } => Ok(Outcome::Declined {
                reason: reason.clone(),
            }),
            EngineKind::Health { computed, actual } => {
                if computed.raw() >= Ray::ONE.raw() {
                    Ok(Outcome::HealthWrong {
                        computed: *computed,
                        actual: *actual,
                    })
                } else if let Some(us) = self.our_liquidator {
                    if ev.liquidator == us {
                        tracing::error!(
                            "Won classification requires realized pnl + bid on the emit; absent"
                        );
                        Err(ObsError::Emit("won_pnl_bid"))
                    } else {
                        Ok(Outcome::LostToCompetitor {
                            winner: ev.liquidator,
                        })
                    }
                } else {
                    Ok(Outcome::LostToCompetitor {
                        winner: ev.liquidator,
                    })
                }
            }
            EngineKind::Flagged { computed } => {
                if computed.raw() >= Ray::ONE.raw() {
                    tracing::error!(
                        computed = %computed.raw(),
                        "Flagged emit with HF >= 1 is HealthWrong; actual HF missing on Flagged"
                    );
                    Err(ObsError::Emit("flagged_healthwrong_needs_actual"))
                } else if let Some(us) = self.our_liquidator {
                    if ev.liquidator == us {
                        tracing::error!("Won requires realized pnl + bid; emit has neither");
                        Err(ObsError::Emit("won_pnl_bid"))
                    } else {
                        Ok(Outcome::LostToCompetitor {
                            winner: ev.liquidator,
                        })
                    }
                } else {
                    Ok(Outcome::LostToCompetitor {
                        winner: ev.liquidator,
                    })
                }
            }
        }
    }

    pub fn observe_classified(&self, ev: &DecodedLiquidation) -> Result<Outcome> {
        let outcome = self.classify(ev)?;
        metrics::counter!("liq_outcome", "name" => outcome.name()).increment(1);
        if outcome.alarms() {
            self.alarm.alarm(&outcome)?;
        } else if outcome.to_digest() {
            self.digest.digest(&outcome)?;
        }
        Ok(outcome)
    }
}

impl EngineJoin for WatchEngineJoin {
    fn observe(&self, ev: &DecodedLiquidation) {
        if let Err(e) = self.observe_classified(ev) {
            tracing::error!(error = %e, "watcher↔engine join failed");
        }
    }
}

/// Parse `liq_engine_join` tracing fields into [`EngineEmit`]. Missing fields fail.
pub fn parse_engine_emit(
    protocol: Option<u16>,
    market: Option<u32>,
    user: Option<Address>,
    kind: Option<&str>,
    computed: Option<U256>,
    actual: Option<U256>,
    reason: Option<String>,
) -> Result<EngineEmit> {
    let protocol = protocol.ok_or(ObsError::Emit("protocol"))?;
    let market = market.ok_or(ObsError::Emit("market"))?;
    let user = user.ok_or(ObsError::Emit("user"))?;
    let kind = kind.ok_or(ObsError::Emit("kind"))?;
    let ekind = match kind {
        "flagged" => {
            let c = computed.ok_or(ObsError::Emit("computed"))?;
            EngineKind::Flagged {
                computed: Ray::from_raw(c),
            }
        }
        "declined" => EngineKind::Declined {
            reason: reason.ok_or(ObsError::Emit("reason"))?,
        },
        "health" => {
            let c = computed.ok_or(ObsError::Emit("computed"))?;
            let a = actual.ok_or(ObsError::Emit("actual"))?;
            EngineKind::Health {
                computed: Ray::from_raw(c),
                actual: Ray::from_raw(a),
            }
        }
        other => {
            tracing::error!(other, "unknown engine join kind");
            return Err(ObsError::Emit("kind"));
        }
    };
    Ok(EngineEmit {
        protocol,
        market,
        user,
        kind: ekind,
    })
}

#[cfg(test)]
fn sample_ev(user: Address, liquidator: Address) -> DecodedLiquidation {
    use alloy_primitives::B256;
    use liq_watch::types::{CoverageDims, TriggerClass};
    DecodedLiquidation {
        family: "aave-v3".into(),
        instance: "core".into(),
        protocol: 1,
        market: 0,
        block: 26_018_679,
        block_hash: B256::ZERO,
        tx_hash: B256::ZERO,
        tx_index: 0,
        log_index: 0,
        user,
        liquidator,
        repay_asset: Address::ZERO,
        repay_amount: "1".into(),
        seize_asset: Address::ZERO,
        seize_amount: "1".into(),
        raw: serde_json::json!({}),
        coverage: CoverageDims {
            instance: "core".into(),
            collateral_family: "weth".into(),
            trigger_class: TriggerClass::Unobserved,
            realized_vol_ray: "0".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{CapturingAlarm, CapturingDigest};
    use alloy_primitives::address;

    const USER: Address = address!("0x00000000000000000000000000000000000000aa");
    const LIQ: Address = address!("0x00000000000000000000000000000000000000bb");

    fn join() -> (WatchEngineJoin, Arc<CapturingAlarm>, Arc<CapturingDigest>) {
        let alarm = Arc::new(CapturingAlarm::default());
        let digest = Arc::new(CapturingDigest::default());
        let j = WatchEngineJoin::new(alarm.clone(), digest.clone(), None);
        (j, alarm, digest)
    }

    #[test]
    fn inject_not_tracked_alarms() {
        let (j, alarm, digest) = join();
        let o = j.observe_classified(&sample_ev(USER, LIQ)).unwrap();
        assert!(matches!(o, Outcome::NotTracked { .. }));
        assert_eq!(alarm.names(), vec!["NotTracked"]);
        assert!(digest.names().is_empty());
    }

    #[test]
    fn inject_health_wrong_alarms() {
        let (j, alarm, digest) = join();
        j.ingest(EngineEmit {
            protocol: 1,
            market: 0,
            user: USER,
            kind: EngineKind::Health {
                computed: Ray::ONE,
                actual: Ray::ZERO,
            },
        });
        let o = j.observe_classified(&sample_ev(USER, LIQ)).unwrap();
        assert!(matches!(o, Outcome::HealthWrong { .. }));
        assert_eq!(alarm.names(), vec!["HealthWrong"]);
        assert!(digest.names().is_empty());
    }

    #[test]
    fn inject_declined_goes_to_digest_not_alarm() {
        let (j, alarm, digest) = join();
        j.ingest(EngineEmit {
            protocol: 1,
            market: 0,
            user: USER,
            kind: EngineKind::Declined {
                reason: "unprofitable".into(),
            },
        });
        let o = j.observe_classified(&sample_ev(USER, LIQ)).unwrap();
        assert!(matches!(o, Outcome::Declined { .. }));
        assert!(alarm.names().is_empty());
        assert_eq!(digest.names(), vec!["Declined"]);
    }
}
