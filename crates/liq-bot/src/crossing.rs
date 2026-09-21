//! 08B wiring: decode payload actions, attach crossing, then fire.
//! AccessManager skip is logged in `liq-oracle::poll_matured` — no invented Rays.

use liq_config::Intern;
use liq_engine::{
    attach_crossing, fire_param_change, registered_set, Engine, ThresholdIndex, World,
};
use liq_oracle::{classify_action, ClassifiedAction, PayloadAction};
use liq_types::{AssetId, Ray, ScheduledParamChange};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CrossingError {
    #[error("payload actions undecodable — refusing empty crossing")]
    Undecodable,
    #[error("payload action asset not interned")]
    UnknownAsset,
    #[error(transparent)]
    Trigger(#[from] liq_engine::TriggerError),
}

/// Decode + classify. Any unknown selector fails the whole payload.
pub fn classify_actions(actions: &[PayloadAction]) -> Result<Vec<ClassifiedAction>, CrossingError> {
    if actions.is_empty() {
        return Err(CrossingError::Undecodable);
    }
    let mut out = Vec::with_capacity(actions.len());
    for a in actions {
        out.push(classify_action(a).map_err(|_| CrossingError::Undecodable)?);
    }
    Ok(out)
}

/// Attach crossings. LT/LTV without RAY → [`registered_set`]. Both Rays →
/// [`attach_crossing`]. Never invent RAY from bps.
pub fn attach_from_actions(
    mut ev: ScheduledParamChange,
    index: &ThresholdIndex,
    intern: &Intern,
    actions: &[ClassifiedAction],
    ray_pairs: &[(AssetId, Ray, Ray)],
) -> Result<ScheduledParamChange, CrossingError> {
    if actions.is_empty() && ray_pairs.is_empty() {
        return Err(CrossingError::Undecodable);
    }
    let mut ids = Vec::new();
    for a in actions {
        match a {
            ClassifiedAction::ConfigureCollateral { asset, .. } => {
                let id = intern.asset(*asset).ok_or(CrossingError::UnknownAsset)?;
                ids.extend(registered_set(index, id));
            }
        }
    }
    for (asset, old, new) in ray_pairs {
        ev = attach_crossing(ev, index, *asset, *old, *new);
        ids.extend(ev.crossing.iter().copied());
    }
    ids.sort_unstable();
    ids.dedup();
    ev.crossing = ids;
    Ok(ev)
}

/// `attach_*` then [`fire_param_change`]. Empty crossing from a failed decode
/// must not reach here.
pub fn fire_wired(
    engine: &mut Engine,
    world: &World<'_>,
    ev: &ScheduledParamChange,
) -> Result<(), CrossingError> {
    Ok(fire_param_change(engine, world, ev)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, U256};
    use liq_config::{Intern, Registry};
    use liq_engine::{crossing_set, Side};
    use liq_types::{MarketId, PositionId, ProtocolId, TraceId};
    use std::path::PathBuf;

    fn r(v: u64) -> Ray {
        Ray::from_raw(U256::from(v))
    }

    fn ev() -> ScheduledParamChange {
        ScheduledParamChange {
            protocol: ProtocolId(0),
            market: MarketId(0),
            execution_block: 1,
            crossing: Vec::new(),
            trace: TraceId::from_raw(1),
        }
    }

    #[test]
    fn empty_or_unknown_action_refuses() {
        assert!(matches!(
            classify_actions(&[]),
            Err(CrossingError::Undecodable)
        ));
        let bad = PayloadAction {
            target: Address::repeat_byte(1),
            calldata: Bytes::from_static(&[0x00, 0x00, 0x00, 0x00]),
        };
        assert!(matches!(
            classify_actions(&[bad]),
            Err(CrossingError::Undecodable)
        ));
    }

    #[test]
    fn lt_without_rays_uses_registered_set() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let weth = alloy_primitives::address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let asset = intern.asset(weth).expect("WETH interned");
        let n_assets = usize::from(asset.0).saturating_add(1);
        let mut idx = ThresholdIndex::new(n_assets, 8);
        idx.begin(PositionId(1));
        idx.register(PositionId(1), asset, Side::Falling, r(100))
            .unwrap();
        idx.begin(PositionId(2));
        idx.register(PositionId(2), asset, Side::Rising, r(200))
            .unwrap();
        let classified = [ClassifiedAction::ConfigureCollateral {
            asset: weth,
            ltv: U256::from(7500u64),
            liquidation_threshold: U256::from(8000u64),
            liquidation_bonus: U256::from(10500u64),
        }];
        let got = attach_from_actions(ev(), &idx, &intern, &classified, &[]).unwrap();
        assert_eq!(got.crossing, vec![PositionId(1), PositionId(2)]);
    }

    #[test]
    fn both_rays_use_attach_crossing() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let intern = Intern::from_registry(
            &Registry::from_path(&root.join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let a = AssetId(0);
        let mut idx = ThresholdIndex::new(1, 8);
        idx.begin(PositionId(1));
        idx.register(PositionId(1), a, Side::Falling, r(100))
            .unwrap();
        idx.begin(PositionId(2));
        idx.register(PositionId(2), a, Side::Falling, r(50))
            .unwrap();
        let got = attach_from_actions(ev(), &idx, &intern, &[], &[(a, r(120), r(40))]).unwrap();
        assert_eq!(got.crossing, crossing_set(&idx, a, r(120), r(40)));
    }
}
