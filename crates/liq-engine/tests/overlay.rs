//! Protocol-reported prices ([`liq_engine::ProtocolPrices`]): the price a
//! protocol's own oracle reports decides health for that protocol's
//! positions, without touching the canonical vector, and moves of it are
//! swept through their own threshold index. Same real Aave V4 rig as
//! `engine.rs`; the oracle is always the adapter called directly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod common;

use common::*;
use liq_engine::{ProtocolPriceMove, ProtocolPrices, World};
use liq_flash::{DepthOnlyRouteCache, Haircut};
use liq_protocol::Protocol;
use liq_types::{AssetId, Band, MarketId, PositionId, ProtocolId, Ray, SourceKind};

/// One market's protocol prices.
struct Overlay {
    protocol: ProtocolId,
    market: MarketId,
    patch: Vec<(AssetId, Ray)>,
}

impl ProtocolPrices for Overlay {
    fn patch(&self, protocol: ProtocolId, market: MarketId) -> &[(AssetId, Ray)] {
        if protocol == self.protocol && market == self.market {
            &self.patch
        } else {
            &[]
        }
    }
}

fn bands_universe() -> Vec<Borrower> {
    borrowers(7, |i| {
        [2_100, 2_040, 1_900, 1_500, 1_000, 0, 700][i as usize]
    })
}

const ETH_MINUS_30: u64 = ETH_USD_P8 - ETH_USD_P8 * 30 / 100;
const ETH_MINUS_52: u64 = ETH_USD_P8 - ETH_USD_P8 * 52 / 100;

fn spoke_overlay(weth_p8: u64) -> Overlay {
    Overlay {
        protocol: PROTOCOL,
        market: SPOKE_MARKET,
        patch: vec![(WETH, ray_of_p8(weth_p8))],
    }
}

fn run<R>(
    rig: &mut Rig,
    ov: &Overlay,
    f: impl FnOnce(&mut liq_engine::Engine, &World<'_>) -> R,
) -> R {
    let protocols: [&dyn Protocol; 1] = [&rig.p];
    let routes = DepthOnlyRouteCache(&rig.flash);
    let w = World {
        view: rig.st.view(T0),
        protocols: &protocols,
        flash: &rig.flash,
        routes: &routes,
        haircut: Haircut::NONE,
        overlay: Some(ov),
    };
    f(&mut rig.engine, &w)
}

/// The spoke's oracle reports WETH 30 % under the canonical feed: health
/// follows the protocol, so exactly the positions the adapter calls
/// liquidatable at that price are emitted — and the canonical vector the
/// engine holds is unchanged afterwards.
#[test]
fn protocol_price_decides_health_and_leaves_canonical_alone() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    let ov = spoke_overlay(ETH_MINUS_30);
    run(&mut rig, &ov, |e, w| e.resync(w).unwrap());
    let got = ids(rig.engine.candidates());
    let want = rig.expected(T0, &rig.px_with(WETH, ETH_MINUS_30));
    assert_eq!(got, want);
    assert_eq!(got, vec![pid(0), pid(1), pid(2), pid(3)]);
    assert_eq!(
        rig.engine.prices().0[usize::from(WETH.0)].price,
        ray_of_p8(ETH_USD_P8),
        "overlay restored after every fold"
    );
    assert!(
        rig.engine.overlay_index().registered(WETH).next().is_some(),
        "overlaid WETH thresholds live in the protocol-price index"
    );
    assert!(
        rig.engine.index().registered(WETH).next().is_none(),
        "and none in the canonical one"
    );
}

/// A `Cold` position priced by its protocol is caught when the *protocol*
/// price crosses its threshold. A canonical tick of the same size does not
/// sweep it: its threshold is in protocol units, not canonical ones.
#[test]
fn cold_position_is_caught_by_a_protocol_price_move() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    let at_par = spoke_overlay(ETH_USD_P8);
    run(&mut rig, &at_par, |e, w| e.resync(w).unwrap());
    rig.engine.candidates().count();
    assert_eq!(rig.engine.band(pid(4)), Some(Band::Cold));

    // Canonical crash with the protocol still at par: nothing new.
    let crash = rig.tick(WETH, ETH_MINUS_52, SourceKind::Canonical);
    run(&mut rig, &at_par, |e, w| {
        e.on_price_tick(w, &crash).unwrap()
    });
    assert!(
        !ids(rig.engine.candidates()).contains(&pid(4)),
        "the protocol, not the canonical feed, prices this market"
    );

    // The protocol's own price falls 52 %: the Cold one crosses.
    let fallen = spoke_overlay(ETH_MINUS_52);
    let moves = [ProtocolPriceMove {
        protocol: PROTOCOL,
        market: SPOKE_MARKET,
        asset: WETH,
        old: ray_of_p8(ETH_USD_P8),
        new: ray_of_p8(ETH_MINUS_52),
    }];
    run(&mut rig, &fallen, |e, w| {
        e.on_protocol_prices(w, &moves).unwrap()
    });
    let got = ids(rig.engine.candidates());
    assert!(
        got.contains(&pid(4)),
        "Cold #4 caught via the overlay index"
    );
    assert!(!got.contains(&pid(6)), "700 DAI at hf 1.41 did not cross");
    assert_eq!(rig.engine.band(pid(4)), Some(Band::Hot));
}

/// An overlay on another market leaves these positions on canonical prices.
#[test]
fn overlay_on_another_market_changes_nothing() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    let elsewhere = Overlay {
        protocol: PROTOCOL,
        market: MarketId(SPOKE_MARKET.0 + 99),
        patch: vec![(WETH, ray_of_p8(ETH_MINUS_30))],
    };
    run(&mut rig, &elsewhere, |e, w| e.resync(w).unwrap());
    let got = ids(rig.engine.candidates());
    assert_eq!(got, rig.expected(T0, &pinned_prices()));
    assert_eq!(got, vec![PositionId(0)]);
    assert!(rig.engine.overlay_index().registered(WETH).next().is_none());
}
